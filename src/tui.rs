//! Terminal dashboard (`nitrolite tui`).
//!
//! Hand-drawn rather than ratatui: the drawing code is small and the dependency
//! is one crate, which suits a tool whose selling point is being lightweight.
//!
//! Two output modes, chosen by whether stdout is a real console:
//!   * console -> ANSI colour + Unicode box drawing (VT processing enabled)
//!   * pipe    -> plain ASCII, so redirected output stays readable
//!
//! The frame is capped to the terminal height and drawn in the alternate screen,
//! so it never wraps and never enters the scrollback.

use std::collections::VecDeque;
use std::io::{self, Write};
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

/// Colour for the mode value, so the state is legible at a glance.
fn mode_style(value: u8) -> &'static str {
    match value {
        0 => "1;36",
        1 => "1;32",
        4 => "1;33",
        6 => "1;35",
        _ => "1;31",
    }
}

/// Green under 70 C, yellow to 85 C, red above.
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

/// Build the frame as a list of already-painted lines.
fn render(data: &Data, width: usize, compact: bool) -> Vec<String> {
    let unicode = UNICODE_BOX.load(Ordering::Relaxed);
    let (tl, tr, bl, br, h, v) = if unicode {
        ('┌', '┐', '└', '┘', '─', '│')
    } else {
        ('+', '+', '+', '+', '-', '|')
    };

    let inner = width.saturating_sub(2);
    let bar: String = std::iter::repeat_n(h, inner).collect();

    let row = |segs: &[(&str, &str)]| {
        let used = visible(segs);
        let pad = inner.saturating_sub(used);
        let mut out = paint(&[(v.to_string().as_str(), "36")]);
        out.push_str(&paint(segs));
        out.push_str(&" ".repeat(pad));
        out.push_str(&paint(&[(v.to_string().as_str(), "36")]));
        out
    };

    let mut lines = Vec::new();
    lines.push(format!(
        "{}{}{}",
        paint(&[(tl.to_string().as_str(), "36")]),
        paint(&[(bar.as_str(), "36")]),
        paint(&[(tr.to_string().as_str(), "36")])
    ));

    // Title row.
    let dot = if unicode { "·" } else { "-" };
    let title = format!(" nitrolite  {dot}  Acer ACPI");
    lines.push(row(&[(title.as_str(), "1;36")]));

    let cpu = data.zones.first().copied();
    let cpu_text = cpu
        .map(|z| format!("{z:.1} C"))
        .unwrap_or_else(|| "n/a".to_string());

    let mut rows: Vec<(String, String, &'static str)> = Vec::new();
    rows.push((
        "Mode".to_string(),
        mode_name(data.mode).to_string(),
        mode_style(data.mode),
    ));
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

    // Sparkline sits under the CPU row.
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
        lines.push(row(&segs));
    }

    if !compact {
        lines.push(row(&[]));
    }
    lines.push(row(&[
        ("  [1] ", "1;36"),
        ("Quiet", "36"),
        ("   [2] ", "1;36"),
        ("Balanced", "32"),
        ("   [3] ", "1;36"),
        ("Performance", "33"),
    ]));
    lines.push(row(&[
        ("  [r] refresh", "90"),
        ("              [q] quit", "90"),
    ]));

    if !data.status.is_empty() {
        if !compact {
            lines.push(row(&[]));
        }
        lines.push(row(&[(format!("  {}", data.status).as_str(), "33")]));
    }

    lines.push(format!(
        "{}{}{}",
        paint(&[(bl.to_string().as_str(), "36")]),
        paint(&[(bar.as_str(), "36")]),
        paint(&[(br.to_string().as_str(), "36")])
    ));

    lines
}

fn draw(out: &mut impl Write, data: &Data) -> io::Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((60, 20));
    let width = (cols as usize).clamp(34, 66);
    let height = rows as usize;

    let mut lines = render(data, width, false);
    if lines.len() > height {
        lines = render(data, width, true);
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

fn apply_mode(wmi: &AcerWmi, value: u8) -> String {
    let current = match wmi.get("GetGamingMiscSetting", MODE_SELECTOR) {
        Ok(raw) => ((raw >> 8) & 0xFF) as u8,
        Err(e) => return format!("read failed: {e}"),
    };
    if current == value {
        return format!("already {}", mode_name(value));
    }

    let payload = ((value as u64) << 8) | MODE_SELECTOR as u64;
    match wmi.set("SetGamingMiscSetting", payload) {
        Ok(r) => {
            std::thread::sleep(Duration::from_millis(400));
            let after = wmi
                .get("GetGamingMiscSetting", MODE_SELECTOR)
                .map(|raw| ((raw >> 8) & 0xFF) as u8)
                .unwrap_or(current);
            if after == value {
                format!("mode {} -> {} ({})", current, after, mode_name(after))
            } else {
                let restore = ((current as u64) << 8) | MODE_SELECTOR as u64;
                let _ = wmi.set("SetGamingMiscSetting", restore);
                format!(
                    "write rejected (asked {value}, got {after}, status {}) - restored {current}",
                    r.status
                )
            }
        }
        Err(e) => format!("write failed: {e}"),
    }
}

/// Decide the output flavour: console (colour + Unicode) or pipe (plain ASCII).
fn init_console() {
    // Testing hook: render colour even when piped, so the layout can be checked
    // without an interactive terminal.
    if std::env::var("NITROLITE_FORCE_ANSI").is_ok() {
        ANSI.store(true, Ordering::Relaxed);
        UNICODE_BOX.store(true, Ordering::Relaxed);
        return;
    }
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, SetConsoleOutputCP, CONSOLE_MODE,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };

    unsafe {
        let handle = match GetStdHandle(STD_OUTPUT_HANDLE) {
            Ok(h) => h,
            Err(_) => return,
        };
        let mut mode = CONSOLE_MODE::default();
        if GetConsoleMode(handle, &mut mode).is_err() {
            return; // redirected: keep plain ASCII
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
    for line in render(&data, 62, false) {
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
    enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, Hide, Clear(ClearType::All))
        .map_err(|e| format!("alt screen: {e}"))?;
    let _guard = Guard;

    let mut data = collect(None, wmi, nvml);
    let mut last_refresh = Instant::now();

    loop {
        draw(&mut out, &data).map_err(|e| e.to_string())?;

        if poll(Duration::from_millis(250)).map_err(|e| e.to_string())? {
            if let Event::Key(KeyEvent {
                code, modifiers, ..
            }) = read().map_err(|e| e.to_string())?
            {
                match code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(())
                    }
                    KeyCode::Char('1') => {
                        data.status = apply_mode(wmi, 0);
                        last_refresh = Instant::now() - Duration::from_secs(5);
                    }
                    KeyCode::Char('2') => {
                        data.status = apply_mode(wmi, 1);
                        last_refresh = Instant::now() - Duration::from_secs(5);
                    }
                    KeyCode::Char('3') => {
                        data.status = apply_mode(wmi, 4);
                        last_refresh = Instant::now() - Duration::from_secs(5);
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
    fn sparkline_needs_two_samples_and_bounds_its_width() {
        let mut history = VecDeque::new();
        assert!(
            sparkline(&history, 10).is_empty(),
            "one sample is not a trend"
        );
        history.push_back(50.0);
        assert!(sparkline(&history, 10).is_empty());

        for v in [55.0, 60.0, 70.0, 85.0] {
            history.push_back(v);
        }
        assert_eq!(sparkline(&history, 10).chars().count(), history.len());
        assert_eq!(
            sparkline(&history, 2).chars().count(),
            2,
            "must respect width"
        );
    }
}
