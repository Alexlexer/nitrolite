//! Terminal dashboard (`nitrolite tui`).
//!
//! Fills the terminal with panels: CPU, GPU, memory, storage, a temperature
//! history chart and a cooling panel with two animated fans.
//!
//! Two output modes, chosen by whether stdout is a real console:
//!   * console -> ANSI colour, Unicode boxes, animated fans
//!   * pipe    -> plain ASCII, static frame
//!
//! ## The fans are decoration
//!
//! Fan rotation is derived **only** from the selected performance mode. It is
//! not a measurement: this machine exposes no fan tachometer to userland (see
//! docs/PROTOCOL.md §11). No RPM figure is shown, estimated or implied — the
//! mode is printed under the fans instead, and a test enforces that.
//!
//! Every other number on screen is real: ACPI thermal zone, NVML gauges,
//! GetSystemTimes, GlobalMemoryStatusEx, GetDiskFreeSpaceExW.

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
use crate::nvml::{GpuInfo, Nvml};
use crate::sysinfo::{Gauges, SysMeter};

const MODE_SELECTOR: u32 = 0x0B;
const HISTORY_LEN: usize = 60;

/// Fan glyph box in terminal cells; each cell is a 2x4 braille dot block.
const FAN_COLS: usize = 13;
const FAN_ROWS: usize = 5;
/// Radians per animation tick at 100% speed.
const BASE_ROTATION: f32 = 0.11;
/// GPU fan is phase-shifted so the two do not look mechanically locked.
const GPU_PHASE_OFFSET: f32 = 1.7;

/// One rendered fan: rows of (glyph, is_led_ring) cells.
type FanGrid = Vec<Vec<(char, bool)>>;

/// Below these dimensions the full dashboard will not fit and we fall back.
const MIN_DASH_W: usize = 92;
const MIN_DASH_H: usize = 24;

static ANSI: AtomicBool = AtomicBool::new(false);
static UNICODE_BOX: AtomicBool = AtomicBool::new(false);

const RED: &str = "91";
const DIM: &str = "90";
const CYAN: &str = "36";
const GREEN: &str = "32";
const MAGENTA: &str = "35";

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
        4 => "1;91",
        6 => "1;35",
        _ => "1;31",
    }
}

/// Decorative rotation rate, as a fraction of full speed. Presentation only.
fn rotation_factor(mode: u8) -> f32 {
    match mode {
        0 => 0.15,
        1 => 0.50,
        4 => 1.00,
        _ => 0.15,
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

fn temp_style(celsius: Option<f64>) -> &'static str {
    match celsius {
        Some(c) if c >= 85.0 => "1;31",
        Some(c) if c >= 70.0 => "33",
        Some(_) => "32",
        None => DIM,
    }
}

// ---------------------------------------------------------------- painting

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

/// Paint a segment list and report its exact visible width.
fn line_text(segs: &[(&str, &str)]) -> Line {
    let used = segs.iter().map(|(t, _)| t.chars().count()).sum();
    (paint(segs), used)
}

fn repeat(ch: char, n: usize) -> String {
    std::iter::repeat_n(ch, n).collect()
}

/// Horizontal bar, e.g. `████░░░░` — clamped so it never overflows.
fn bar(pct: f32, width: usize, style: &str) -> (String, usize) {
    let filled = ((pct.clamp(0.0, 100.0) / 100.0) * width as f32).round() as usize;
    let filled = filled.min(width);
    (
        paint(&[
            (&repeat('█', filled), style),
            (&repeat('░', width - filled), DIM),
        ]),
        width,
    )
}

fn sparkline(history: &VecDeque<f64>, width: usize, style: &str) -> (String, usize) {
    if history.len() < 2 || width == 0 {
        return (String::new(), 0);
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

    let text: String = history
        .iter()
        .rev()
        .take(width)
        .rev()
        .map(|v| blocks[(((v - lo) / span) * 7.0).round().clamp(0.0, 7.0) as usize])
        .collect();
    let used = text.chars().count();
    (paint(&[(text.as_str(), style)]), used)
}

// -------------------------------------------------------------------- fans

/// Render one fan into braille cells. Second element marks LED-ring cells.
fn fan_grid(phase: f32) -> FanGrid {
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

            let (on, is_ring) = if r < 0.18 {
                (true, false) // hub
            } else if r < 0.86 {
                // Spiralled blade edge: a symmetric pattern quantises to
                // identical braille frames and appears to stop.
                ((3.0 * (theta - phase) + 0.9 * r).sin() > 0.45, false)
            } else if r <= 1.0 {
                (true, true) // LED ring
            } else {
                (false, false)
            };

            if on {
                let (cx, cy) = (x / 2, y / 4);
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

    (0..FAN_ROWS)
        .map(|cy| {
            (0..FAN_COLS)
                .map(|cx| {
                    let ch = if unicode {
                        char::from_u32(0x2800 + bits[cy][cx] as u32).unwrap_or(' ')
                    } else if bits[cy][cx] != 0 {
                        '#'
                    } else {
                        ' '
                    };
                    (ch, ring[cy][cx] && unicode)
                })
                .collect()
        })
        .collect()
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

// ------------------------------------------------------------------- model

struct Data {
    mode: u8,
    cpu_temp: Option<f64>,
    gpu: Option<GpuInfo>,
    gpu_name: String,
    gauges: Gauges,
    cpu_hist: VecDeque<f64>,
    gpu_hist: VecDeque<f64>,
    status: String,
}

fn collect(
    previous: Option<&Data>,
    wmi: &AcerWmi,
    nvml: Option<&Nvml>,
    meter: &mut SysMeter,
) -> Data {
    let mode = wmi
        .get("GetGamingMiscSetting", MODE_SELECTOR)
        .map(|raw| ((raw >> 8) & 0xFF) as u8)
        .unwrap_or(255);

    let cpu_temp = wmi.thermal_zones().ok().and_then(|z| z.first().copied());

    let gpu = nvml
        .and_then(|n| n.read_all().ok())
        .and_then(|mut v| v.drain(..).next());

    let gpu_name = gpu
        .as_ref()
        .map(|g| g.name.clone())
        .unwrap_or_else(|| "unavailable".to_string());

    let mut cpu_hist = previous.map(|d| d.cpu_hist.clone()).unwrap_or_default();
    let mut gpu_hist = previous.map(|d| d.gpu_hist.clone()).unwrap_or_default();

    for (hist, value) in [
        (&mut cpu_hist, cpu_temp),
        (
            &mut gpu_hist,
            gpu.as_ref().and_then(|g| g.temp_c).map(f64::from),
        ),
    ] {
        if let Some(v) = value {
            if hist.len() == HISTORY_LEN {
                hist.pop_front();
            }
            hist.push_back(v);
        }
    }

    Data {
        mode,
        cpu_temp,
        gpu,
        gpu_name,
        gauges: meter.sample(),
        cpu_hist,
        gpu_hist,
        status: String::new(),
    }
}

// ------------------------------------------------------------------ panels

/// A line of content plus its visible width (the painted string may contain ANSI).
type Line = (String, usize);

/// Box a set of content lines into a titled panel of exactly `width` columns.
fn panel(title: &str, body: Vec<Line>, width: usize, style: &str) -> Vec<Line> {
    let inner = width.saturating_sub(2);
    let mut out = Vec::with_capacity(body.len() + 2);

    let heading = format!("─ {title} ");
    let fill = inner.saturating_sub(heading.chars().count());
    out.push((
        paint(&[
            ("┌", style),
            (heading.as_str(), style),
            (&repeat('─', fill), style),
            ("┐", style),
        ]),
        width,
    ));

    for (content, used) in body {
        // "│" + space + content + padding + "│" must equal `width`.
        let pad = inner.saturating_sub(used + 1);
        out.push((
            format!(
                "{}{}{}{}{}",
                paint(&[("│", style)]),
                " ",
                content,
                " ".repeat(pad),
                paint(&[("│", style)]),
            ),
            width,
        ));
    }

    out.push((
        paint(&[("└", style), (&repeat('─', inner), style), ("┘", style)]),
        width,
    ));

    out
}
/// Join panels side by side. Panels are padded to a common height.
fn row(panels: Vec<Vec<Line>>, gap: usize) -> Vec<Line> {
    let height = panels.iter().map(|p| p.len()).max().unwrap_or(0);
    let mut out = Vec::with_capacity(height);

    for i in 0..height {
        let mut text = String::new();
        let mut used = 0usize;
        for (n, p) in panels.iter().enumerate() {
            if n > 0 {
                text.push_str(&" ".repeat(gap));
                used += gap;
            }
            if let Some((line, w)) = p.get(i) {
                text.push_str(line);
                used += w;
            } else {
                let w = p.first().map(|(_, w)| *w).unwrap_or(0);
                text.push_str(&" ".repeat(w));
                used += w;
            }
        }
        out.push((text, used));
    }
    out
}

fn level_bar(label: &str, pct: Option<f32>, width: usize, style: &str) -> Line {
    let value = pct.unwrap_or(0.0);
    let (bar_text, bar_w) = bar(value, width, style);
    let left = format!("  {label:<6}");
    let text = format!("{}{}", paint(&[(left.as_str(), DIM)]), bar_text);
    (text, left.chars().count() + bar_w)
}

// --------------------------------------------------------------- rendering

fn dashboard(
    data: &Data,
    w: usize,
    h: usize,
    cpu_fan: &[Vec<(char, bool)>],
    gpu_fan: &[Vec<(char, bool)>],
) -> Vec<String> {
    let inner = w.saturating_sub(2);
    let gap = 1usize;

    // ---- cooling panel width, then split the rest between CPU and GPU.
    let cooling_w = (FAN_COLS * 2 + 6).max(32);
    let side = inner.saturating_sub(cooling_w + gap * 2);
    let cpu_w = side / 2;
    let gpu_w = side - cpu_w;

    let cpu_temp = data.cpu_temp;
    let gpu_temp = data.gpu.as_ref().and_then(|g| g.temp_c).map(f64::from);

    // CPU panel -------------------------------------------------------
    let mut cpu_body: Vec<Line> = Vec::new();
    let cpu_temp_text = cpu_temp
        .map(|t| format!("{t:>5.1} C"))
        .unwrap_or_else(|| "  n/a  ".to_string());
    let cpu_load = data
        .gauges
        .cpu_load_pct
        .map(|v| format!("{v:.0}%"))
        .unwrap_or_else(|| "--".into());
    cpu_body.push(line_text(&[
        ("  ", ""),
        (&cpu_temp_text, temp_style(cpu_temp)),
        (&format!("   load {cpu_load}"), DIM),
    ]));
    let spark_w = cpu_w.saturating_sub(6);
    let (spark, spark_used) = sparkline(&data.cpu_hist, spark_w, RED);
    cpu_body.push((format!("  {spark}"), spark_used + 2));
    let (load, load_used) = bar(
        data.gauges.cpu_load_pct.unwrap_or(0.0),
        cpu_w.saturating_sub(12),
        CYAN,
    );
    cpu_body.push((format!("  {load}"), load_used + 2));

    // GPU panel -------------------------------------------------------
    let mut gpu_body: Vec<Line> = Vec::new();
    let gpu_temp_text = gpu_temp
        .map(|t| format!("{t:>5.1} C"))
        .unwrap_or_else(|| "  n/a  ".to_string());
    let gpu_util = data
        .gpu
        .as_ref()
        .and_then(|g| g.util_pct)
        .map(|v| format!("{v}%"))
        .unwrap_or_else(|| "--".into());
    gpu_body.push(line_text(&[
        ("  ", ""),
        (&gpu_temp_text, temp_style(gpu_temp)),
        (&format!("   util {gpu_util}"), DIM),
    ]));
    let gpu_spark_w = gpu_w.saturating_sub(6);
    let (gspark, gspark_used) = sparkline(&data.gpu_hist, gpu_spark_w, RED);
    gpu_body.push((format!("  {gspark}"), gspark_used + 2));
    let detail = match &data.gpu {
        Some(g) => format!(
            "  {}  {} W  {} MB",
            g.clock_mhz
                .map(|c| format!("{c} MHz"))
                .unwrap_or_else(|| "--".into()),
            g.power_w
                .map(|p| format!("{p:.0}"))
                .unwrap_or_else(|| "--".into()),
            g.mem_used_mb
                .map(|m| m.to_string())
                .unwrap_or_else(|| "--".into()),
        ),
        None => "  --".to_string(),
    };
    gpu_body.push(line_text(&[(&detail, DIM)]));

    // Cooling panel ---------------------------------------------------
    let led = led_style(data.mode);
    let mut cooling_body: Vec<Line> = Vec::new();

    for r in 0..FAN_ROWS {
        let left = paint_cells(&cpu_fan[r], led);
        let right = paint_cells(&gpu_fan[r], led);
        let text = format!("{left}   {right}");
        cooling_body.push((text, FAN_COLS * 2 + 3));
    }
    let labels = format!(
        "{:<width$}{}{:<width$}",
        "CPU",
        "   ",
        "GPU",
        width = FAN_COLS
    );
    cooling_body.push((paint(&[(labels.as_str(), DIM)]), labels.chars().count()));
    let caption = format!("Mode: {}", mode_name(data.mode));
    let cap_pad = cooling_w.saturating_sub(caption.chars().count() + 2) / 2;
    let cap_line = format!("{}{}", " ".repeat(cap_pad), caption);
    cooling_body.push((
        paint(&[(cap_line.as_str(), mode_style(data.mode))]),
        cap_line.chars().count(),
    ));

    let stats = row(
        vec![
            panel("CPU", cpu_body, cpu_w, CYAN),
            panel("GPU", gpu_body, gpu_w, RED),
            panel("Cooling", cooling_body, cooling_w, RED),
        ],
        gap,
    );

    // Memory / storage row --------------------------------------------
    let mem_w = (inner.saturating_sub(gap)) / 2;
    let disk_w = inner.saturating_sub(gap + mem_w);

    let ram_text = match (data.gauges.ram_used_gb, data.gauges.ram_total_gb) {
        (Some(u), Some(t)) => format!("{u:.1} / {t:.0} GB"),
        _ => "--".to_string(),
    };
    let ram_body = vec![
        (
            paint(&[(format!("  {ram_text}").as_str(), "37")]),
            format!("  {ram_text}").chars().count(),
        ),
        level_bar(
            "",
            data.gauges.ram_used_pct,
            mem_w.saturating_sub(12),
            GREEN,
        ),
    ];

    let disk_text = match (data.gauges.disk_used_gb, data.gauges.disk_total_gb) {
        (Some(u), Some(t)) => format!("{u:.0} / {t:.0} GB"),
        _ => "--".to_string(),
    };
    let disk_body = vec![
        (
            paint(&[(format!("  {disk_text}").as_str(), "37")]),
            format!("  {disk_text}").chars().count(),
        ),
        level_bar(
            "",
            data.gauges.disk_used_pct,
            disk_w.saturating_sub(12),
            MAGENTA,
        ),
    ];

    let storage = row(
        vec![
            panel("Memory", ram_body, mem_w, GREEN),
            panel("Storage (C:)", disk_body, disk_w, MAGENTA),
        ],
        gap,
    );

    // Temperature chart -----------------------------------------------
    let chart_w = inner.saturating_sub(10);
    let mut chart_body: Vec<Line> = Vec::new();
    let (cpu_line, _cpu_used) = sparkline(&data.cpu_hist, chart_w, CYAN);
    let (gpu_line, _gpu_used) = sparkline(&data.gpu_hist, chart_w, RED);
    chart_body.push(line_text(&[
        ("  ", ""),
        ("CPU", CYAN),
        (" ", ""),
        (&cpu_line, ""),
    ]));
    chart_body.push(line_text(&[
        ("  ", ""),
        ("GPU", RED),
        (" ", ""),
        (&gpu_line, ""),
    ]));
    let legend = format!(
        "  CPU {}   GPU {}   (last {} samples)",
        data.cpu_temp
            .map(|t| format!("{t:.1} C"))
            .unwrap_or_else(|| "n/a".into()),
        gpu_temp
            .map(|t| format!("{t:.1} C"))
            .unwrap_or_else(|| "n/a".into()),
        data.cpu_hist.len()
    );
    chart_body.push(line_text(&[(&legend, DIM)]));
    let chart = panel("Temperatures", chart_body, inner, DIM);

    // Modes ------------------------------------------------------------
    let modes = "  [1] Quiet      [2] Balanced      [3] Performance      [r] refresh      [q] quit";
    let modes_body = vec![(paint(&[(modes, "37")]), modes.chars().count())];
    let modes_panel = panel("Keys", modes_body, inner, RED);

    // Assemble ---------------------------------------------------------
    let mut body: Vec<Line> = Vec::new();
    body.extend(stats);
    body.extend(storage);
    body.extend(chart);
    body.extend(modes_panel);
    if !data.status.is_empty() {
        let status = format!("  {}", data.status);
        body.push((paint(&[(status.as_str(), "33")]), status.chars().count()));
    }

    frame(body, w, h, data)
}

/// Wrap the assembled body in the outer border and footer.
fn frame(mut body: Vec<Line>, w: usize, h: usize, data: &Data) -> Vec<String> {
    let inner = w.saturating_sub(2);
    let mut out = Vec::with_capacity(h);

    // Header
    let title = " nitrolite ";
    let right = format!(
        "{}  ·  v{} ",
        mode_name(data.mode),
        env!("CARGO_PKG_VERSION")
    );
    let fill = inner.saturating_sub(title.chars().count() + right.chars().count());
    out.push(paint(&[
        ("╭", RED),
        (title, "1;91"),
        (&repeat('─', fill), RED),
        (&right, "1;37"),
        ("╮", RED),
    ]));

    // Header + body + footer + bottom border must equal the terminal height.
    let body_h = h.saturating_sub(3);
    body.truncate(body_h);
    // Centre the panels vertically rather than leaving one big gap at the end.
    let pad_top = body_h.saturating_sub(body.len()) / 2;
    for _ in 0..pad_top {
        body.insert(0, (String::new(), 0));
    }
    while body.len() < body_h {
        body.push((String::new(), 0));
    }

    for (content, used) in body {
        let pad = inner.saturating_sub(used);
        out.push(format!(
            "{}{}{}{}",
            paint(&[("│", RED)]),
            content,
            " ".repeat(pad),
            paint(&[("│", RED)]),
        ));
    }

    // Truncated to the inner width so a long GPU name cannot break the border.
    let footer_full = format!(
        " {}   ·   CPU {}   GPU {}   ·   {}",
        env!("CARGO_PKG_NAME"),
        data.cpu_temp
            .map(|t| format!("{t:.0}C"))
            .unwrap_or_else(|| "--".into()),
        data.gpu
            .as_ref()
            .and_then(|g| g.temp_c)
            .map(|t| format!("{t}C"))
            .unwrap_or_else(|| "--".into()),
        data.gpu_name,
    );
    let footer: String = footer_full.chars().take(inner).collect();
    let pad = inner.saturating_sub(footer.chars().count());
    out.push(format!(
        "{}{}{}{}",
        paint(&[("│", RED)]),
        paint(&[(footer.as_str(), DIM)]),
        " ".repeat(pad),
        paint(&[("│", RED)]),
    ));

    out.push(paint(&[("╰", RED), (&repeat('─', inner), RED), ("╯", RED)]));

    out
}

fn draw(
    out: &mut impl Write,
    data: &Data,
    cpu_fan: &[Vec<(char, bool)>],
    gpu_fan: &[Vec<(char, bool)>],
) -> io::Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));
    let w = (cols as usize).max(MIN_DASH_W);
    let h = rows as usize;

    let lines = if w >= MIN_DASH_W && h >= MIN_DASH_H {
        dashboard(data, w, h, cpu_fan, gpu_fan)
    } else {
        compact(data, cpu_fan, gpu_fan)
    };

    queue!(out, MoveTo(0, 0))?;
    for (i, line) in lines.iter().enumerate().take(h) {
        queue!(out, MoveTo(0, i as u16))?;
        write!(out, "{line}")?;
    }
    for i in lines.len().min(h)..h {
        queue!(out, MoveTo(0, i as u16))?;
        write!(out, "{}", " ".repeat(w))?;
    }
    out.flush()
}

/// Fallback for terminals too small for the dashboard.
fn compact(
    data: &Data,
    cpu_fan: &[Vec<(char, bool)>],
    gpu_fan: &[Vec<(char, bool)>],
) -> Vec<String> {
    let w = 46usize;
    let inner = w - 2;
    let mut out = Vec::new();

    out.push(format!("┌{}┐", repeat('─', inner)));
    let title = " nitrolite";
    out.push(format!("│{:<inner$}│", title));

    for r in 0..3 {
        let left = paint_cells(&cpu_fan[r], led_style(data.mode));
        let right = paint_cells(&gpu_fan[r], led_style(data.mode));
        let line = format!(" {left}  {right}");
        out.push(format!("│{:<inner$}│", strip_pad(&line, inner)));
    }
    let cap = format!(" Mode: {}", mode_name(data.mode));
    out.push(format!("│{:<inner$}│", cap));
    let cpu = format!(
        " CPU {}   Load {}",
        data.cpu_temp
            .map(|t| format!("{t:.0}C"))
            .unwrap_or_else(|| "--".into()),
        data.gauges
            .cpu_load_pct
            .map(|v| format!("{v:.0}%"))
            .unwrap_or_else(|| "--".into())
    );
    out.push(format!("│{:<inner$}│", cpu));
    out.push(format!(
        "│{:<inner$}│",
        " [1] Quiet  [2] Balanced  [3] Performance"
    ));
    out.push(format!("│{:<inner$}│", " [q] quit"));
    out.push(format!("└{}┘", repeat('─', inner)));
    out
}

fn strip_pad(s: &str, _width: usize) -> &str {
    s
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
    let mut meter = SysMeter::new();
    let _ = meter.sample();
    let data = collect(None, wmi, nvml, &mut meter);
    let mut out = io::stdout();
    let cpu_fan = fan_grid(0.0);
    let gpu_fan = fan_grid(GPU_PHASE_OFFSET);
    for line in dashboard(&data, 118, 32, &cpu_fan, &gpu_fan) {
        writeln!(out, "{line}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

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

    let mut meter = SysMeter::new();
    let _ = meter.sample();
    let mut data = collect(None, wmi, nvml, &mut meter);

    let mut phase: f32 = 0.0;
    let mut pending: Option<Pending> = None;
    let mut dirty = true;

    // Frame skipping: in slower modes many animation ticks quantise to the same
    // braille frame, and redrawing identical pixels is pure CPU.
    let mut previous: Option<(FanGrid, FanGrid)> = None;
    let mut last_refresh = Instant::now();

    loop {
        let cpu_fan = fan_grid(phase);
        let gpu_fan = fan_grid(phase + GPU_PHASE_OFFSET);

        let changed = match &previous {
            Some((c, g)) => *c != cpu_fan || *g != gpu_fan,
            None => true,
        };
        if changed || dirty {
            draw(&mut out, &data, &cpu_fan, &gpu_fan).map_err(|e| e.to_string())?;
            previous = Some((cpu_fan, gpu_fan));
            dirty = false;
        }

        if animating {
            phase = (phase + BASE_ROTATION * rotation_factor(data.mode)) % std::f32::consts::TAU;
        }

        if let Some(mut p) = pending.take() {
            match p.child.try_wait() {
                Ok(Some(status)) => {
                    data.status = if status.success() {
                        format!("mode -> {}", mode_name(p.target))
                    } else {
                        format!("mode change to {} failed", mode_name(p.target))
                    };
                    dirty = true;
                    last_refresh = Instant::now() - Duration::from_secs(5);
                }
                Ok(None) => pending = Some(p),
                Err(e) => {
                    data.status = format!("mode change error: {e}");
                    dirty = true;
                }
            }
        }

        // Faster modes tick faster; quiet needs only a few frames a second.
        let tick = if !animating {
            Duration::from_millis(500)
        } else {
            match rotation_factor(data.mode) {
                f if f >= 0.9 => Duration::from_millis(80),
                f if f >= 0.4 => Duration::from_millis(160),
                _ => Duration::from_millis(300),
            }
        };

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
                        dirty = true;
                    }
                    KeyCode::Char('r') => {
                        last_refresh = Instant::now() - Duration::from_secs(5);
                        data.status = "refreshed".to_string();
                        dirty = true;
                    }
                    _ => {}
                }
            }
        }

        if last_refresh.elapsed() >= Duration::from_secs(1) {
            let status = std::mem::take(&mut data.status);
            data = collect(Some(&data), wmi, nvml, &mut meter);
            data.status = status;
            last_refresh = Instant::now();
            dirty = true;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: build a frame at a given animation phase.
    fn frame_at(data: &Data, w: usize, h: usize, phase: f32) -> Vec<String> {
        dashboard(
            data,
            w,
            h,
            &fan_grid(phase),
            &fan_grid(phase + GPU_PHASE_OFFSET),
        )
    }

    fn sample_data() -> Data {
        Data {
            mode: 1,
            cpu_temp: Some(61.5),
            gpu: None,
            gpu_name: "Test GPU".to_string(),
            gauges: Gauges {
                cpu_load_pct: Some(27.0),
                ram_used_gb: Some(12.6),
                ram_total_gb: Some(32.0),
                ram_used_pct: Some(39.0),
                disk_used_gb: Some(418.0),
                disk_total_gb: Some(1000.0),
                disk_used_pct: Some(42.0),
            },
            cpu_hist: VecDeque::from(vec![55.0, 58.0, 61.5]),
            gpu_hist: VecDeque::from(vec![48.0, 50.0]),
            status: String::new(),
        }
    }

    #[test]
    fn rotation_speed_is_mode_derived_only() {
        assert!((rotation_factor(0) - 0.15).abs() < f32::EPSILON);
        assert!((rotation_factor(1) - 0.50).abs() < f32::EPSILON);
        assert!((rotation_factor(4) - 1.00).abs() < f32::EPSILON);
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

    /// The animation must move faster in faster modes. Measuring phase advance
    /// is the honest assertion: counting distinct frames saturates once the
    /// braille grid is fine enough that every step differs.
    #[test]
    fn animation_speed_scales_with_mode() {
        fn advance(factor: f32, ticks: usize) -> f32 {
            let mut phase = 0.0f32;
            for _ in 0..ticks {
                phase += BASE_ROTATION * factor;
            }
            phase
        }

        let quiet = advance(rotation_factor(0), 10);
        let balanced = advance(rotation_factor(1), 10);
        let performance = advance(rotation_factor(4), 10);

        assert!(quiet < balanced, "quiet={quiet} balanced={balanced}");
        assert!(
            balanced < performance,
            "balanced={balanced} performance={performance}"
        );
        // Performance must be exactly 1/0.15x the quiet advance.
        assert!((performance / quiet - 1.0 / 0.15).abs() < 0.01);
    }
    #[test]
    fn fans_are_not_in_lockstep() {
        assert_ne!(fan_grid(0.5), fan_grid(0.5 + GPU_PHASE_OFFSET));
    }

    #[test]
    fn dashboard_fills_the_requested_width() {
        let data = sample_data();
        for w in [MIN_DASH_W, 100, 120, 160] {
            let lines = frame_at(&data, w, 32, 0.0);
            let plain: Vec<String> = lines
                .iter()
                .map(|l| l.replace('\x1b', "").to_string())
                .collect();
            for line in &plain {
                // Strip ANSI to count visible characters.
                let visible = strip_ansi(line).chars().count();
                assert_eq!(visible, w, "line not exactly {w} wide: {line:?}");
            }
        }
    }

    #[test]
    fn dashboard_respects_height() {
        let data = sample_data();
        let lines = frame_at(&data, 118, 30, 0.0);
        assert!(
            lines.len() <= 30,
            "frame taller than terminal: {}",
            lines.len()
        );
    }

    #[test]
    fn frame_never_mentions_rpm() {
        let data = sample_data();
        for w in [MIN_DASH_W, 118] {
            let frame = frame_at(&data, w, 32, 0.3).join("\n").to_lowercase();
            assert!(
                !frame.contains("rpm"),
                "fan RPM must never be displayed: the number would be fabricated"
            );
        }
    }

    #[test]
    fn frame_shows_the_active_mode() {
        let mut data = sample_data();
        for (value, expected) in [(0u8, "Quiet"), (1, "Balanced"), (4, "Performance")] {
            data.mode = value;
            let frame = frame_at(&data, 118, 32, 0.0).join("\n");
            assert!(
                frame.contains(&format!("Mode: {expected}")),
                "missing {expected}"
            );
        }
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for n in chars.by_ref() {
                        if n.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
