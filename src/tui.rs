//! Terminal dashboard (`nitrolite tui`).
//!
//! Deliberately hand-drawn with crossterm rather than ratatui: the drawing code
//! is ~100 lines and the dependency is one crate, which suits a tool whose whole
//! selling point is being small.
//!
//! `--once` renders a single frame and exits without touching terminal modes,
//! which makes the rendering testable without an interactive TTY.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{poll, read, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{execute, queue};

use crate::acer::AcerWmi;
use crate::nvml::Nvml;

const MODE_SELECTOR: u32 = 0x0B;

fn mode_name(value: u8) -> &'static str {
    match value {
        0 => "Quiet",
        1 => "Balanced",
        4 => "Performance",
        6 => "ECO (battery)",
        _ => "Unknown",
    }
}

struct Data {
    mode: u8,
    zones: Vec<f64>,
    gpu: Option<u32>,
    gpu_name: String,
    status: String,
}

fn collect(wmi: &AcerWmi, nvml: Option<&Nvml>) -> Data {
    let mode = wmi
        .get("GetGamingMiscSetting", MODE_SELECTOR)
        .map(|raw| ((raw >> 8) & 0xFF) as u8)
        .unwrap_or(255);

    let zones = wmi.thermal_zones().unwrap_or_default();

    let (gpu, gpu_name) = match nvml.and_then(|n| n.read_all().ok()) {
        Some(gpus) if !gpus.is_empty() => (gpus[0].temp_c, gpus[0].name.clone()),
        _ => (None, "unavailable".to_string()),
    };

    Data {
        mode,
        zones,
        gpu,
        gpu_name,
        status: String::new(),
    }
}

/// Pad/truncate to a fixed width so the frame doesn't jitter as numbers change.
fn cell(text: &str, width: usize) -> String {
    let mut s: String = text.chars().take(width).collect();
    while s.chars().count() < width {
        s.push(' ');
    }
    s
}

fn render(data: &Data, width: usize) -> Vec<String> {
    let unicode = UNICODE_BOX.load(Ordering::Relaxed);
    let (tl, tr, bl, br, h, v) = if unicode {
        ('┌', '┐', '└', '┘', '─', '│')
    } else {
        ('+', '+', '+', '+', '-', '|')
    };

    let inner = width.saturating_sub(2);
    let bar: String = std::iter::repeat(h).take(inner).collect();

    let row = |text: String| format!("{v}{}{v}", cell(&text, inner));

    let mut lines = Vec::new();
    lines.push(format!("{tl}{bar}{tr}"));
    lines.push(row(" nitrolite - Acer ACPI".to_string()));

    let cpu = data
        .zones
        .first()
        .map(|z| format!("{z:.1} C"))
        .unwrap_or_else(|| "n/a".to_string());

    let rows = [
        ("Mode", mode_name(data.mode).to_string()),
        ("CPU (ACPI zone)", cpu),
        (
            "GPU",
            data.gpu
                .map(|t| format!("{t} C"))
                .unwrap_or_else(|| "n/a".to_string()),
        ),
        ("GPU name", data.gpu_name.clone()),
    ];

    for (label, value) in rows {
        lines.push(row(format!("  {label:<16}{value}")));
    }

    lines.push(row(String::new()));
    lines.push(row("  [1] Quiet   [2] Balanced   [3] Performance".to_string()));
    lines.push(row("  [r] refresh                    [q] quit".to_string()));

    if !data.status.is_empty() {
        lines.push(row(String::new()));
        lines.push(row(format!("  {}", data.status)));
    }

    lines.push(format!("{bl}{bar}{br}"));
    lines
}

fn draw(out: &mut impl Write, data: &Data) -> io::Result<()> {
    let (cols, _) = crossterm::terminal::size().unwrap_or((60, 20));
    let width = (cols as usize).min(64).max(30);
    let lines = render(data, width);

    queue!(out, MoveTo(0, 0))?;
    for (i, line) in lines.iter().enumerate() {
        queue!(out, MoveTo(0, i as u16))?;
        write!(out, "{line}")?;
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
                    "write rejected (asked {value}, got {after}, status {}) — restored {current}",
                    r.status
                )
            }
        }
        Err(e) => format!("write failed: {e}"),
    }
}

/// Renders one frame to stdout and exits. Used for testing and for piping.
/// Set at startup: true only when stdout is a real console that we switched to
/// UTF-8. Pipes and files get ASCII borders, so redirected output stays readable.
static UNICODE_BOX: AtomicBool = AtomicBool::new(false);

/// Windows consoles do not default to UTF-8; without this the box drawing
/// characters arrive as mojibake in cmd.exe / Windows PowerShell. When stdout is
/// a pipe (redirected or captured), codepage switching has no effect, so fall
/// back to ASCII instead of emitting bytes the consumer will mis-decode.
fn set_utf8_console() {
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleOutputCP, CONSOLE_MODE, STD_OUTPUT_HANDLE,
    };

    let is_console = unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE).unwrap_or_default();
        let mut mode = CONSOLE_MODE::default();
        GetConsoleMode(handle, &mut mode).is_ok()
    };

    if is_console {
        unsafe {
            let _ = SetConsoleOutputCP(65001);
        }
        UNICODE_BOX.store(true, Ordering::Relaxed);
    }
}

pub fn run_once(wmi: &AcerWmi, nvml: Option<&Nvml>) -> Result<(), String> {
    set_utf8_console();
    let data = collect(wmi, nvml);
    let mut out = io::stdout();
    for line in render(&data, 60) {
        writeln!(out, "{line}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn run(wmi: &AcerWmi, nvml: Option<&Nvml>) -> Result<(), String> {
    // Restore the terminal even on error/panic paths.
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
        }
    }

    set_utf8_console();
    enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, Hide, Clear(ClearType::All))
        .map_err(|e| format!("alt screen: {e}"))?;
    let _guard = Guard;

    let mut data = collect(wmi, nvml);
    let mut last_refresh = Instant::now();

    loop {
        draw(&mut out, &data).map_err(|e| e.to_string())?;

        if poll(Duration::from_millis(250)).map_err(|e| e.to_string())? {
            if let Event::Key(KeyEvent { code, modifiers, .. }) =
                read().map_err(|e| e.to_string())?
            {
                match code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
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
            data = collect(wmi, nvml);
            data.status = status;
            last_refresh = Instant::now();
        }
    }
}



