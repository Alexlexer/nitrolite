//! Terminal dashboard (`nitrolite tui`).
//!
//! Hand-drawn rather than ratatui: the drawing code is small and the dependency
//! is one crate, which suits a tool whose selling point is being lightweight.
//!
//! Two output modes, chosen by whether stdout is a real console:
//!   * console -> ANSI colour, Unicode box drawing, animated fans
//!   * pipe    -> plain ASCII, static frame (animation is meaningless when piped)
//!
//! The frame is capped to the terminal height and drawn in the alternate screen,
//! so it never wraps and never enters the scrollback.
//!
//! ## The fans are decoration
//!
//! The fan graphics rotate at a rate derived **only** from the selected
//! performance mode. They are not a measurement: this machine exposes no fan
//! tachometer to userland (see docs/PROTOCOL.md §11). No RPM figure is shown,
//! estimated or implied anywhere — the mode is printed under the fans instead.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{poll, read, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, queue};

use crate::acer::AcerWmi;
use crate::nvml::Nvml;

const MODE_SELECTOR: u32 = 0x0B;
const HISTORY_LEN: usize = 20;

/// Fan glyph box, in terminal cells. Each cell is 2x4 braille dots.
const FAN_COLS: usize = 11;
const FAN_ROWS: usize = 4;
/// Radians per animation tick at 100% speed.
const BASE_ROTATION: f32 = 0.11;
/// GPU fan is offset so the two do not look mechanically locked together.
const GPU_PHASE_OFFSET: f32 = 1.7;

static ANSI: AtomicBool = AtomicBool::new(false);
static UNICODE_BOX: AtomicBool = AtomicBool::new(false);

fn mode_name(value: u8) -> &'static str {
    match value {
        0 => "Quiet",
        1 => "Balanced",
        4 => "Performance",
        6 => "ECO (battery)",
        _ => "Unknown",
    }
}

fn mode_style(value: u8) -> &'static str {
    match value {
        0 => "1;36",
        1 => "1;32",
        4 => "1;33",
        6 => "1;35",
        _ => "1;31",
    }
}

/// Decorative rotation rate for the fans, as a fraction of full speed.
///
/// This is presentation, not telemetry: it is a constant per mode so the
/// animation speed changes the instant the mode changes.
fn rotation_factor(mode: u8) -> f32 {
    match mode {
        0 => 0.15, // Quiet
        1 => 0.50, // Balanced
        4 => 1.00, // Performance
        _ => 0.15, // ECO / unknown: treat as the quiet setting
    }
}

/// LED ring style per mode: dim, moderate, bright red.
fn led_style(mode: u8) -> &'static str {
    match mode {
        4 => "1;91",
        1 => "31",
        _ => "2;31",
    }
}

fn temp_style(celsius: f64) -> &'static str {
    if celsius >= 85.0 {
        "1;31"
    } else if celsius >= 70.0 {
        "33"
    } else {
        "32"
    }
}

struct Data {
    mode: u8,
    zones: Vec<f64>,
    gpu: Option<u32>,
    gpu_name: String,
    status: String,
    history: VecDeque<f64>,
}

fn collect(previous: Option<&Data>, wmi: &AcerWmi, nvml: Option<&Nvml>) -> Data {
    let mode = wmi
        .get("GetGamingMiscSetting", MODE_SELECTOR)
        .map(|raw| ((raw >> 8) & 0xFF) as u8)
        .unwrap_or(255);

    let zones = wmi.thermal_zones().unwrap_or_default();

    let (gpu, gpu_name) = match nvml.and_then(|n| n.read_all().ok()) {
        Some(gpus) if !gpus.is_empty() => (gpus[0].temp_c, gpus[0].name.clone()),
        _ => (None, "unavailable".to_string()),
    };

    let mut history = previous
        .map(|d| d.history.clone())
        .unwrap_or_else(|| VecDeque::with_capacity(HISTORY_LEN));

    if let Some(cpu) = zones.first() {
        if history.len() == HISTORY_LEN {
            history.pop_front();
        }
        history.push_back(*cpu);
    }

    Data {
        mode,
        zones,
        gpu,
        gpu_name,
        status: String::new(),
        history,
    }
}

fn visible(segs: &[(&str, &str)]) -> usize {
    segs.iter().map(|(text, _)| text.chars().count()).sum()
}

fn paint(segs: &[(&str, &str)]) -> String {
    let ansi = ANSI.load(Ordering::Relaxed);
    let mut out = String::new();
    for (text, style) in segs {
        if ansi && !style.is_empty() {
            out.push_str("\x1b[");
            out.push_str(style);
            out.push('m');
            out.push_str(text);
            out.push_str("\x1b[0m");
        } else {
            out.push_str(text);
        }
    }
    out
}

fn sparkline(history: &VecDeque<f64>, width: usize) -> String {
    if history.len() < 2 {
        return String::new();
    }
    let unicode = UNICODE_BOX.load(Ordering::Relaxed);
    let blocks: [char; 8] = if unicode {
        ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█']
    } else {
        ['.', ':', '-', '=', '+', '*', '#', '%']
    };

    let lo = history.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = history.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let span = (hi - lo).max(1.0);

    history
        .iter()
        .take(width)
        .map(|v| {
            let idx = (((v - lo) / span) * 7.0).round().clamp(0.0, 7.0) as usize;
            blocks[idx]
        })
        .collect()
}

/// Render one fan into braille cells.
///
/// The returned grid is `FAN_ROWS` rows of `FAN_COLS` cells; the second tuple
/// element marks cells that belong to the LED ring so they can be coloured.
fn fan_grid(phase: f32) -> Vec<Vec<(char, bool)>> {
    let unicode = UNICODE_BOX.load(Ordering::Relaxed);
    let w = FAN_COLS * 2;
    let h = FAN_ROWS * 4;

    let mut bits = vec![vec![0u8; FAN_COLS]; FAN_ROWS];
    let mut ring = vec![vec![false; FAN_COLS]; FAN_ROWS];

    for y in 0..h {
        for x in 0..w {
            let fx = (x as f32 + 0.5) / w as f32 * 2.0 - 1.0;
            let fy = (y as f32 + 0.5) / h as f32 * 2.0 - 1.0;
            let r = (fx * fx + fy * fy).sqrt();
            let theta = fy.atan2(fx);

            let mut on = false;
            let mut is_ring = false;

            if r < 0.20 {
                on = true; // hub
            } else if r < 0.88 {
                // Three blades swept by phase. The +0.9*r term spirals the edge,
                // which breaks the 3-fold symmetry enough that rotation stays
                // visible after the grid quantises it to braille dots.
                on = (3.0 * (theta - phase) + 0.9 * r).sin() > 0.45;
            } else if r <= 1.0 {
                on = true;
                is_ring = true;
            }

            if on {
                let cx = x / 2;
                let cy = y / 4;
                let bit = match (x % 2, y % 4) {
                    (0, 0) => 0x01,
                    (0, 1) => 0x02,
                    (0, 2) => 0x04,
                    (0, _) => 0x40,
                    (1, 0) => 0x08,
                    (1, 1) => 0x10,
                    (1, 2) => 0x20,
                    (_, _) => 0x80,
                };
                bits[cy][cx] |= bit;
                if is_ring {
                    ring[cy][cx] = true;
                }
            }
        }
    }

    let mut out = Vec::with_capacity(FAN_ROWS);
    for cy in 0..FAN_ROWS {
        let mut row = Vec::with_capacity(FAN_COLS);
        for cx in 0..FAN_COLS {
            let ch = if unicode {
                char::from_u32(0x2800 + bits[cy][cx] as u32).unwrap_or(' ')
            } else if bits[cy][cx] != 0 {
                '#'
            } else {
                ' '
            };
            row.push((ch, ring[cy][cx] && unicode));
        }
        out.push(row);
    }
    out
}

fn paint_cells(cells: &[(char, bool)], led: &'static str) -> String {
    if !ANSI.load(Ordering::Relaxed) {
        return cells.iter().map(|(c, _)| *c).collect();
    }
    let mut out = String::new();
    let mut current: Option<&str> = None;
    for (ch, is_ring) in cells {
        let style = if *is_ring { led } else { "37" };
        if current != Some(style) {
            out.push_str("\x1b[0m\x1b[");
            out.push_str(style);
            out.push('m');
            current = Some(style);
        }
        out.push(*ch);
    }
    out.push_str("\x1b[0m");
    out
}

/// Build the two-fan row block plus labels and the mode caption.
///
/// Returns (painted lines, visible width of each).
fn fan_block(mode: u8, phase: f32) -> Vec<(String, usize)> {
    let led = led_style(mode);
    let cpu = fan_grid(phase);
    let gpu = fan_grid(phase + GPU_PHASE_OFFSET);

    let gap = 5usize;
    let total = FAN_COLS * 2 + gap;
    let mut lines = Vec::with_capacity(FAN_ROWS + 2);

    for r in 0..FAN_ROWS {
        let left = paint_cells(&cpu[r], led);
        let right = paint_cells(&gpu[r], led);
        let raw = format!("{left}{}{right}", " ".repeat(gap));
        lines.push((raw, total));
    }

    let labels = format!(
        "{:<width$}{}{:<width$}",
        "CPU",
        " ".repeat(gap),
        "GPU",
        width = FAN_COLS
    );
    lines.push((paint(&[(labels.as_str(), "90")]), total));

    let caption = format!("Mode: {}", mode_name(mode));
    lines.push((
        paint(&[(caption.as_str(), mode_style(mode))]),
        caption.chars().count(),
    ));

    lines
}

/// Build the frame as painted lines.
fn render(data: &Data, width: usize, compact: bool, phase: f32) -> Vec<String> {
    let unicode = UNICODE_BOX.load(Ordering::Relaxed);
    let (tl, tr, bl, br, h, v) = if unicode {
        ('┌', '┐', '└', '┘', '─', '│')
    } else {
        ('+', '+', '+', '+', '-', '|')
    };

    let inner = width.saturating_sub(2);
    let bar: String = std::iter::repeat_n(h, inner).collect();
    let border = |c: char, text: &str| paint(&[(c.to_string().as_str(), "36"), (text, "36")]);

    let plain_row = |content: String, used: usize| {
        let pad = inner.saturating_sub(used);
        format!(
            "{}{}{}{}",
            paint(&[(v.to_string().as_str(), "36")]),
            content,
            " ".repeat(pad),
            paint(&[(v.to_string().as_str(), "36")])
        )
    };

    let mut lines = Vec::new();
    lines.push(format!(
        "{}{}{}",
        border(tl, ""),
        paint(&[(bar.as_str(), "36")]),
        border(tr, "")
    ));

    let dot = if unicode { "·" } else { "-" };
    let title = format!(" nitrolite  {dot}  Acer ACPI");
    lines.push(plain_row(
        paint(&[(title.as_str(), "1;36")]),
        title.chars().count(),
    ));

    if !compact {
        for (raw, used) in fan_block(data.mode, phase) {
            let left_pad = inner.saturating_sub(used) / 2;
            let content = format!("{}{}", " ".repeat(left_pad), raw);
            lines.push(plain_row(content, used + left_pad));
        }
        lines.push(plain_row(paint(&[("", "")]), 0));
    }

    let cpu = data.zones.first().copied();
    let cpu_text = cpu
        .map(|z| format!("{z:.1} C"))
        .unwrap_or_else(|| "n/a".to_string());

    let mut rows: Vec<(String, String, &'static str)> = Vec::new();
    rows.push((
        "CPU".to_string(),
        cpu_text,
        cpu.map(temp_style).unwrap_or("90"),
    ));
    rows.push((
        "GPU".to_string(),
        data.gpu
            .map(|t| format!("{t} C"))
            .unwrap_or_else(|| "n/a".to_string()),
        data.gpu.map(|t| temp_style(t as f64)).unwrap_or("90"),
    ));
    if !compact {
        rows.push(("GPU name".to_string(), data.gpu_name.clone(), "90"));
    }

    let spark = sparkline(&data.history, inner.saturating_sub(22));

    for (i, (label, value, style)) in rows.iter().enumerate() {
        let mut segs: Vec<(&str, &str)> = Vec::new();
        let label_padded = format!("  {label:<12}");
        segs.push((label_padded.as_str(), "90"));
        segs.push((value.as_str(), style));
        if i == 1 && !spark.is_empty() {
            segs.push(("  ", ""));
            segs.push((spark.as_str(), "36"));
        }
        let used = visible(&segs);
        lines.push(plain_row(paint(&segs), used));
    }

    if !compact {
        lines.push(plain_row(String::new(), 0));
    }
    // Two short lines: a single combined hint line overflowed the box at 62 cols.
    let keys_modes = "  [1] Quiet   [2] Balanced   [3] Performance";
    lines.push(plain_row(
        paint(&[(keys_modes, "90")]),
        keys_modes.chars().count(),
    ));
    if !compact {
        let keys_actions = "  [r] refresh                    [q] quit";
        lines.push(plain_row(
            paint(&[(keys_actions, "90")]),
            keys_actions.chars().count(),
        ));
    }

    if !data.status.is_empty() {
        let status = format!("  {}", data.status);
        lines.push(plain_row(
            paint(&[(status.as_str(), "33")]),
            status.chars().count(),
        ));
    }

    lines.push(format!(
        "{}{}{}",
        border(bl, ""),
        paint(&[(bar.as_str(), "36")]),
        border(br, "")
    ));

    lines
}

fn draw(out: &mut impl Write, data: &Data, phase: f32) -> io::Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((60, 24));
    let width = (cols as usize).clamp(34, 66);
    let height = rows as usize;

    let mut lines = render(data, width, false, phase);
    if lines.len() > height {
        lines = render(data, width, true, phase);
    }
    lines.truncate(height);

    queue!(out, MoveTo(0, 0))?;
    for (i, line) in lines.iter().enumerate() {
        queue!(out, MoveTo(0, i as u16))?;
        write!(out, "{line}")?;
    }
    for i in lines.len()..height {
        queue!(out, MoveTo(0, i as u16))?;
        write!(out, "{}", " ".repeat(width))?;
    }
    out.flush()
}

/// A mode change running in a child process.
///
/// The write + read-back verification takes ~400 ms. Doing it inline would
/// stall the animation, so each request is dispatched to a child `nitrolite
/// mode set <name>` (already elevated, no second UAC) and polled from the loop.
struct Pending {
    child: Child,
    target: u8,
}

fn spawn_mode_change(value: u8) -> Result<Pending, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let name = match value {
        0 => "quiet",
        1 => "balanced",
        4 => "performance",
        _ => return Err("unsupported mode".to_string()),
    };

    let child = Command::new(exe)
        .args(["mode", "set", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;

    Ok(Pending {
        child,
        target: value,
    })
}

fn init_console() {
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, SetConsoleOutputCP, CONSOLE_MODE,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };

    if std::env::var("NITROLITE_FORCE_ANSI").is_ok() {
        ANSI.store(true, Ordering::Relaxed);
        UNICODE_BOX.store(true, Ordering::Relaxed);
        return;
    }

    unsafe {
        let handle = match GetStdHandle(STD_OUTPUT_HANDLE) {
            Ok(h) => h,
            Err(_) => return,
        };
        let mut mode = CONSOLE_MODE::default();
        if GetConsoleMode(handle, &mut mode).is_err() {
            return;
        }
        let _ = SetConsoleOutputCP(65001);
        let _ = SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        ANSI.store(true, Ordering::Relaxed);
        UNICODE_BOX.store(true, Ordering::Relaxed);
    }
}

pub fn run_once(wmi: &AcerWmi, nvml: Option<&Nvml>) -> Result<(), String> {
    init_console();
    let data = collect(None, wmi, nvml);
    let mut out = io::stdout();
    // Static frame: one animation step, no loop.
    for line in render(&data, 62, false, 0.0) {
        writeln!(out, "{line}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn run(wmi: &AcerWmi, nvml: Option<&Nvml>) -> Result<(), String> {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
        }
    }

    init_console();
    let animating = ANSI.load(Ordering::Relaxed);

    enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, Hide, Clear(ClearType::All))
        .map_err(|e| format!("alt screen: {e}"))?;
    let _guard = Guard;

    let mut data = collect(None, wmi, nvml);
    let mut phase: f32 = 0.0;
    let mut pending: Option<Pending> = None;

    let tick = if animating {
        Duration::from_millis(80)
    } else {
        Duration::from_millis(250)
    };
    let mut last_refresh = Instant::now();

    loop {
        draw(&mut out, &data, phase).map_err(|e| e.to_string())?;

        if animating {
            phase = (phase + BASE_ROTATION * rotation_factor(data.mode)) % std::f32::consts::TAU;
        }

        // A mode change must never block the loop: poll it, then move on.
        if let Some(mut p) = pending.take() {
            match p.child.try_wait() {
                Ok(Some(status)) => {
                    data.status = if status.success() {
                        format!("mode -> {}", mode_name(p.target))
                    } else {
                        format!("mode change to {} failed", mode_name(p.target))
                    };
                    last_refresh = Instant::now() - Duration::from_secs(5);
                }
                Ok(None) => pending = Some(p),
                Err(e) => {
                    data.status = format!("mode change error: {e}");
                }
            }
        }

        if poll(tick).map_err(|e| e.to_string())? {
            if let Event::Key(KeyEvent {
                code, modifiers, ..
            }) = read().map_err(|e| e.to_string())?
            {
                match code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(())
                    }
                    KeyCode::Char(c @ ('1' | '2' | '3')) => {
                        let value = match c {
                            '1' => 0u8,
                            '2' => 1,
                            _ => 4,
                        };
                        if pending.is_some() {
                            data.status = "mode change already in flight".to_string();
                        } else {
                            match spawn_mode_change(value) {
                                Ok(p) => {
                                    data.status = format!("switching to {}...", mode_name(value));
                                    pending = Some(p);
                                }
                                Err(e) => data.status = e,
                            }
                        }
                    }
                    KeyCode::Char('r') => {
                        last_refresh = Instant::now() - Duration::from_secs(5);
                        data.status = "refreshed".to_string();
                    }
                    _ => {}
                }
            }
        }

        if last_refresh.elapsed() >= Duration::from_secs(1) {
            let status = std::mem::take(&mut data.status);
            data = collect(Some(&data), wmi, nvml);
            data.status = status;
            last_refresh = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperature_thresholds_pick_the_right_colour() {
        assert_eq!(temp_style(50.0), "32");
        assert_eq!(temp_style(69.9), "32");
        assert_eq!(temp_style(70.0), "33");
        assert_eq!(temp_style(84.9), "33");
        assert_eq!(temp_style(85.0), "1;31");
    }

    #[test]
    fn modes_have_stable_names_and_colours() {
        assert_eq!(mode_name(0), "Quiet");
        assert_eq!(mode_name(1), "Balanced");
        assert_eq!(mode_name(4), "Performance");
        assert_eq!(mode_name(6), "ECO (battery)");
        assert_eq!(mode_name(9), "Unknown");
        assert_eq!(mode_style(1), "1;32");
    }

    #[test]
    fn rotation_speed_is_mode_derived_only() {
        assert!((rotation_factor(0) - 0.15).abs() < f32::EPSILON);
        assert!((rotation_factor(1) - 0.50).abs() < f32::EPSILON);
        assert!((rotation_factor(4) - 1.00).abs() < f32::EPSILON);
        // Anything else falls back to the quiet rate, never to a made-up value.
        assert!((rotation_factor(6) - 0.15).abs() < f32::EPSILON);
        assert!((rotation_factor(255) - 0.15).abs() < f32::EPSILON);
    }

    #[test]
    fn led_ring_gets_brighter_with_mode() {
        assert_eq!(led_style(0), "2;31");
        assert_eq!(led_style(1), "31");
        assert_eq!(led_style(4), "1;91");
    }

    #[test]
    fn fan_grid_has_expected_dimensions() {
        let grid = fan_grid(0.0);
        assert_eq!(grid.len(), FAN_ROWS);
        assert!(grid.iter().all(|row| row.len() == FAN_COLS));
    }

    #[test]
    fn fan_rotates_with_phase() {
        let frames: std::collections::HashSet<String> = (0..8)
            .map(|i| format!("{:?}", fan_grid(i as f32 * BASE_ROTATION)))
            .collect();
        assert!(frames.len() > 1, "rotation must actually change the frame");
    }

    /// The whole point of the animation: how fast it moves is a function of the
    /// selected mode, nothing else. Count distinct frames over an equal number
    /// of ticks and require it to scale quiet < balanced < performance.
    #[test]
    fn animation_speed_scales_with_mode() {
        fn distinct_frames(factor: f32) -> usize {
            let mut seen = std::collections::HashSet::new();
            let mut phase = 0.0f32;
            for _ in 0..24 {
                phase += BASE_ROTATION * factor;
                seen.insert(format!("{:?}", fan_grid(phase)));
            }
            seen.len()
        }

        let quiet = distinct_frames(rotation_factor(0));
        let balanced = distinct_frames(rotation_factor(1));
        let performance = distinct_frames(rotation_factor(4));

        assert!(quiet < balanced, "quiet={quiet} balanced={balanced}");
        assert!(
            balanced < performance,
            "balanced={balanced} performance={performance}"
        );
    }

    #[test]
    fn fans_are_not_in_lockstep() {
        // The GPU fan is phase-shifted, so the two grids differ at the same phase.
        let cpu = fan_grid(0.5);
        let gpu = fan_grid(0.5 + GPU_PHASE_OFFSET);
        assert_ne!(cpu, gpu);
    }

    fn sample_data() -> Data {
        Data {
            mode: 1,
            zones: vec![61.5],
            gpu: Some(55),
            gpu_name: "Test GPU".to_string(),
            status: String::new(),
            history: VecDeque::from(vec![55.0, 58.0, 61.5]),
        }
    }

    #[test]
    fn frame_never_mentions_rpm() {
        let data = sample_data();
        for compact in [false, true] {
            let frame = render(&data, 62, compact, 0.3).join("\n").to_lowercase();
            assert!(
                !frame.contains("rpm"),
                "fan RPM must never be displayed: the number would be fabricated"
            );
        }
    }

    #[test]
    fn frame_shows_the_active_mode_under_the_fans() {
        let mut data = sample_data();
        for (value, expected) in [(0u8, "Quiet"), (1, "Balanced"), (4, "Performance")] {
            data.mode = value;
            let frame = render(&data, 62, false, 0.0).join("\n");
            assert!(
                frame.contains(&format!("Mode: {expected}")),
                "expected the fan caption to name {expected}"
            );
        }
    }

    #[test]
    fn sparkline_needs_two_samples_and_bounds_its_width() {
        let mut history = VecDeque::new();
        assert!(sparkline(&history, 10).is_empty());
        history.push_back(50.0);
        assert!(sparkline(&history, 10).is_empty());

        for v in [55.0, 60.0, 70.0, 85.0] {
            history.push_back(v);
        }
        assert_eq!(sparkline(&history, 10).chars().count(), history.len());
        assert_eq!(sparkline(&history, 2).chars().count(), 2);
    }
}
