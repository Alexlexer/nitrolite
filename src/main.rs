mod acer;
mod calib;
mod elevate;
mod monitor;
mod nvml;
mod power;
mod sysinfo;
mod tray;
mod tui;

use acer::{AcerWmi, DEFAULT_INSTANCE};
use calib::{CalibOpts, DEFAULT_SELECTORS};

const GETTERS: [&str; 5] = [
    "GetGamingProfile",
    "GetGamingFanBehavior",
    "GetGamingFanSpeed",
    "GetGamingSysInfo",
    "GetGamingProfileSetting",
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("status");
    let instance =
        std::env::var("NITROLITE_INSTANCE").unwrap_or_else(|_| DEFAULT_INSTANCE.to_string());

    // Interactive commands need admin (the ACPI instance is admin-only).
    // Relaunch elevated rather than making the user find an admin terminal.
    let interactive = match cmd {
        "tray" => true,
        "tui" => !args.iter().any(|a| a == "--once"),
        _ => false,
    };
    if interactive && !elevate::is_elevated() {
        match elevate::relaunch_elevated() {
            Ok(_) => {
                println!("elevation required - launched in a new elevated window");
                return;
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }

    // `analyze` works on a file and needs no hardware access.
    if cmd == "analyze" {
        let path = match args.get(1) {
            Some(p) => p,
            None => {
                eprintln!("usage: nitrolite analyze <capture.jsonl>");
                std::process::exit(2);
            }
        };
        if let Err(e) = calib::analyze(path) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }

    // `fans` talks to the Acer monitor service over localhost: no WMI, no
    // elevation, and it is the only source of real fan RPM.
    if cmd == "fans" {
        match crate::monitor::read() {
            Some(m) => {
                let fmt = |v: Option<u32>| {
                    v.map(|r| format!("{r} RPM"))
                        .unwrap_or_else(|| "n/a".into())
                };
                println!("CPU fan : {}", fmt(m.cpu_fan_rpm));
                println!("GPU fan : {}", fmt(m.gpu_fan_rpm));
                println!("SYS fan : {}", fmt(m.sys_fan_rpm));
                if let Some(t) = m.cpu_temp_c {
                    println!("CPU temp (service): {t:.0} C");
                }
                if let Some(t) = m.gpu_temp_c {
                    println!("GPU temp (service): {t:.0} C");
                }
            }
            None => {
                eprintln!("monitor service unavailable — is the Acer service stack running?");
                std::process::exit(1);
            }
        }
        return;
    }

    // `gpu` uses NVML only — no WMI, no elevation.
    if cmd == "gpu" {
        if let Err(e) = gpu_report() {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }

    // `tray` manages its own WMI connection and message loop.
    if cmd == "tray" {
        if let Err(e) = tray::run(&instance) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }

    let wmi = match AcerWmi::connect(&instance) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("connect failed: {e}");
            std::process::exit(1);
        }
    };

    let result = match cmd {
        "status" => status(&wmi),
        "temp" => match wmi.thermal_zones() {
            Ok(zones) if zones.is_empty() => {
                println!("no ACPI thermal zones reported");
                Ok(())
            }
            Ok(zones) => {
                for (i, z) in zones.iter().enumerate() {
                    println!("zone {i}: {z:.1} C");
                }
                match nvml::Nvml::load() {
                    Ok(nv) => match nv.read_all() {
                        Ok(gpus) => {
                            for g in gpus {
                                if let Some(t) = g.temp_c {
                                    println!("gpu {} ({}): {t} C", g.index, g.name);
                                }
                            }
                        }
                        Err(e) => println!("gpu: {e}"),
                    },
                    Err(e) => println!("gpu: {e}"),
                }
                Ok(())
            }
            Err(e) => Err(e),
        },
        "probe" => probe(&wmi),
        "raw" => {
            let method = args.get(1).map(String::as_str).unwrap_or("GetGamingProfile");
            let sel: u32 = args.get(2).and_then(|s| parse_num(s)).unwrap_or(0) as u32;
            match wmi.get(method, sel) {
                Ok(v) => {
                    println!("{method}(in={sel}) = {v:#018X}");
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        "calibrate" => {
            let opts = parse_calib_opts(&args);
            calib::run(&wmi, &opts)
        }
        "watch" => watch(&wmi, &args),
        "tui" => {
            let nvml = nvml::Nvml::load().ok();
            if args.iter().any(|a| a == "--once") {
                tui::run_once(&wmi, nvml.as_ref())
            } else {
                tui::run(&wmi, nvml.as_ref())
            }
        }
        "mode" => match args.get(1).map(String::as_str) {
            None => match wmi.get("GetGamingMiscSetting", 0x0B) {
                Ok(raw) => {
                    let value = ((raw >> 8) & 0xFF) as u8;
                    println!("mode {value} ({})", mode_name(value));
                    Ok(())
                }
                Err(e) => Err(e),
            },
            Some("set") => {
                let name = args.get(2).map(String::as_str).unwrap_or("");
                let dry_run = args.iter().any(|a| a == "--dry-run");
                set_mode(&wmi, name, dry_run)
            }
            Some(other) => Err(format!(
                "unknown mode subcommand {other}; usage: nitrolite mode [set <quiet|balanced|performance> [--dry-run]]"
            )),
        },
        other => {
            eprintln!("unknown command: {other}");
            eprintln!("usage: nitrolite [status|probe|raw|calibrate|analyze]");
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn parse_calib_opts(args: &[String]) -> CalibOpts {
    let mut opts = CalibOpts {
        seconds: 120,
        idle_seconds: 15,
        interval_ms: 1000,
        selectors: DEFAULT_SELECTORS,
        load: false,
        out: default_capture_path(),
    };

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--seconds" | "-s" => {
                if let Some(v) = args.get(i + 1) {
                    opts.seconds = v.parse().unwrap_or(opts.seconds);
                    i += 1;
                }
            }
            "--idle" => {
                if let Some(v) = args.get(i + 1) {
                    opts.idle_seconds = v.parse().unwrap_or(opts.idle_seconds);
                    i += 1;
                }
            }
            "--interval-ms" => {
                if let Some(v) = args.get(i + 1) {
                    opts.interval_ms = v.parse().unwrap_or(opts.interval_ms);
                    i += 1;
                }
            }
            "--selectors" => {
                if let Some(v) = args.get(i + 1) {
                    opts.selectors = v.parse().unwrap_or(opts.selectors);
                    i += 1;
                }
            }
            "--out" | "-o" => {
                if let Some(v) = args.get(i + 1) {
                    opts.out = v.clone();
                    i += 1;
                }
            }
            "--load" => opts.load = true,
            "--help" | "-h" => {
                println!("nitrolite calibrate [--seconds N] [--idle N] [--interval-ms N] [--selectors N] [--load] [--out FILE]");
                std::process::exit(0);
            }
            other => {
                eprintln!("ignoring unknown calibrate option: {other}");
            }
        }
        i += 1;
    }
    opts
}

fn default_capture_path() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::create_dir_all("captures");
    format!("captures/calib-{secs}.jsonl")
}

/// Packed word layout as far as proven: byte 0 = ACPI status,
/// byte 1 = primary value. Everything above that is hypothesis until M2.
struct Word(u64);

impl Word {
    fn status(&self) -> u8 {
        (self.0 & 0xFF) as u8
    }
    fn value(&self) -> u32 {
        ((self.0 >> 8) & 0xFFFF) as u32
    }
    fn raw(&self) -> String {
        format!("{:#018X}", self.0)
    }
    fn ok(&self) -> bool {
        self.status() == 0
    }
}

fn status(wmi: &AcerWmi) -> Result<(), String> {
    println!("Acer gaming interface (root\\WMI :: AcerGamingFunction)\n");

    for method in [
        "GetGamingProfile",
        "GetGamingFanBehavior",
        "GetGamingFanSpeed",
        "GetGamingSysInfo",
    ] {
        println!("{method}");
        println!("  in   raw                  status   value");
        for sel in 0..=4u32 {
            match wmi.get(method, sel) {
                Ok(raw) => {
                    let w = Word(raw);
                    println!(
                        "  {sel:<4} {}  {:>6}   {:>5}{}",
                        w.raw(),
                        w.status(),
                        w.value(),
                        if w.ok() { "" } else { "   (unsupported)" }
                    );
                }
                Err(e) => println!("  {sel:<4} ERROR: {e}"),
            }
        }
        println!();
    }

    let zones = wmi.thermal_zones().unwrap_or_default();
    if zones.is_empty() {
        println!("ACPI thermal zones: none");
    } else {
        let list: Vec<String> = zones.iter().map(|z| format!("{z:.1} C")).collect();
        println!("ACPI thermal zones: {}", list.join(", "));
    }
    Ok(())
}

fn probe(wmi: &AcerWmi) -> Result<(), String> {
    for method in GETTERS {
        println!("### {method}");
        for sel in 0..=0x20u32 {
            if let Ok(raw) = wmi.get(method, sel) {
                let w = Word(raw);
                println!(
                    "  in={sel:<4} {}  status={} value={}",
                    w.raw(),
                    w.status(),
                    w.value()
                );
            }
        }
    }
    Ok(())
}

/// Poll the interesting words and print a line only when something changes.
/// Used to observe what an external mode switch (NitroSense UI) does to the
/// ACPI state, which is how we map mode names to values without writing.
fn watch(wmi: &AcerWmi, args: &[String]) -> Result<(), String> {
    let seconds: u64 = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let interval_ms: u64 = args
        .iter()
        .position(|a| a == "--interval-ms")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);

    println!("watching for {seconds}s (changes only)\n");

    let probes: [(&str, &[u32]); 5] = [
        ("GetGamingProfile", &[0]),
        ("GetGamingFanBehavior", &[1, 3]),
        ("GetGamingFanSpeed", &[1, 4]),
        ("GetGamingSysInfo", &[0, 1, 2]),
        // MiscSetting carries live state (0x07=255, 0x08=1, 0x0A=83, 0x0B toggles)
        ("GetGamingMiscSetting", &[7, 8, 10, 11]),
    ];

    let start = std::time::Instant::now();
    let mut prev: Option<String> = None;

    while start.elapsed() < std::time::Duration::from_secs(seconds) {
        let mut parts = Vec::new();
        for (method, sels) in probes {
            for sel in sels {
                if let Ok(raw) = wmi.get(method, *sel) {
                    parts.push(format!("{method}({sel})={raw:#018X}"));
                }
            }
        }
        let line = parts.join("  ");
        if prev.as_deref() != Some(line.as_str()) {
            println!("[{:>5}s] {line}", start.elapsed().as_secs());
            prev = Some(line);
        }
        std::thread::sleep(std::time::Duration::from_millis(interval_ms));
    }

    println!("\nwatch finished");
    Ok(())
}

/// Operating mode, as reported by `GetGamingMiscSetting(0x0B)`.
///
/// Only three modes are user-selectable on the Nitro V 15 (`ModeSwitchKey: false`
/// in the model profile — there is no Turbo). `6` is ECO, the mode the Acer
/// service switches to automatically on adaptor change / battery.
/// Evidence: docs/PROTOCOL.md §4.3.
fn mode_name(value: u8) -> &'static str {
    match value {
        0 => "Quiet",
        1 => "Balanced",
        4 => "Performance",
        6 => "ECO (battery)",
        _ => "Unknown",
    }
}

fn gpu_report() -> Result<(), String> {
    let nv = nvml::Nvml::load()?;
    let gpus = nv.read_all()?;
    if gpus.is_empty() {
        println!("no NVIDIA GPUs reported by NVML");
        return Ok(());
    }
    for g in gpus {
        println!("GPU {}: {}", g.index, g.name);
        println!(
            "  temperature: {}",
            g.temp_c
                .map(|t| format!("{t} C"))
                .unwrap_or_else(|| "unavailable".to_string())
        );
        println!(
            "  fan:         {}",
            g.fan_pct
                .map(|f| format!("{f} %"))
                .unwrap_or_else(|| "not exposed by this driver/GPU".to_string())
        );
    }
    Ok(())
}

const MISC_MODE_SELECTOR: u64 = 0x0B;

/// Setter payload layout: value in the high byte, selector in the low byte.
/// Confirmed against the hardware in M3 (0x010B → Balanced).
fn mode_payload(value: u8) -> u64 {
    ((value as u64) << 8) | MISC_MODE_SELECTOR
}

fn read_mode(wmi: &AcerWmi) -> Result<u8, String> {
    let raw = wmi.get("GetGamingMiscSetting", MISC_MODE_SELECTOR as u32)?;
    Ok(((raw >> 8) & 0xFF) as u8)
}

/// Only the three user-selectable modes. ECO (6) is applied by the Acer service
/// on adaptor change and is deliberately not settable here.
fn mode_value(name: &str) -> Option<u8> {
    match name.to_ascii_lowercase().as_str() {
        "quiet" => Some(0),
        "balanced" => Some(1),
        "performance" => Some(4),
        // ECO is normally applied automatically when the charger is removed,
        // but it is a real mode and must be reachable when starting on battery.
        "eco" => Some(crate::power::ECO),
        _ => None,
    }
}

fn set_mode(wmi: &AcerWmi, name: &str, dry_run: bool) -> Result<(), String> {
    let value = mode_value(name)
        .ok_or_else(|| format!("unknown mode '{name}'; known: quiet, balanced, performance"))?;

    let current = read_mode(wmi)?;
    println!("current mode: {current} ({})", mode_name(current));
    if current == value {
        println!("already {name} ({value}); nothing to do");
        return Ok(());
    }

    // Setters pack (value << 8) | selector, mirroring the getter output layout.
    let payload = mode_payload(value);
    println!("payload: 0x{payload:016X}  (selector 0x{MISC_MODE_SELECTOR:02X}, value {value})");

    if dry_run {
        println!("dry run: nothing written");
        return Ok(());
    }

    let ret = wmi.set("SetGamingMiscSetting", payload)?;
    println!(
        "SetGamingMiscSetting accepted gmInput as {} and returned status {status}",
        ret.encoding,
        status = ret.status
    );
    std::thread::sleep(std::time::Duration::from_millis(400));

    let after = read_mode(wmi)?;
    if after == value {
        println!("verified: mode {current} -> {after} ({name})");
        Ok(())
    } else {
        // Never leave the machine in a state we did not verify.
        let restore = ((current as u64) << 8) | MISC_MODE_SELECTOR;
        let r = wmi.set("SetGamingMiscSetting", restore);
        Err(format!(
            "read-back mismatch: asked for {value}, got {after}; restore to {current} -> {r:?}"
        ))
    }
}

fn parse_num(s: &str) -> Option<u32> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        s.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_modes_map_to_their_words() {
        assert_eq!(mode_value("quiet"), Some(0));
        assert_eq!(mode_value("BALANCED"), Some(1));
        assert_eq!(mode_value("Performance"), Some(4));
    }

    #[test]
    fn unknown_modes_are_rejected_but_eco_is_reachable() {
        // ECO is applied automatically on battery, but it is a real mode and
        // must be selectable when the machine starts unplugged.
        assert_eq!(mode_value("eco"), Some(6));
        assert_eq!(mode_value("turbo"), None);
        assert_eq!(mode_value(""), None);
    }
    #[test]
    fn payload_packs_value_above_selector() {
        assert_eq!(mode_payload(0), 0x0000_0000_0000_000B);
        assert_eq!(mode_payload(1), 0x0000_0000_0000_010B);
        assert_eq!(mode_payload(4), 0x0000_0000_0000_040B);
    }

    #[test]
    fn word_decodes_status_and_value() {
        let ok = Word(0x0000_0000_0000_6400);
        assert_eq!(ok.status(), 0);
        assert_eq!(ok.value(), 100);
        assert!(ok.ok());

        let unsupported = Word(0x0000_0000_0000_0002);
        assert_eq!(unsupported.status(), 2);
        assert!(
            !unsupported.ok(),
            "non-zero status must never read as a value"
        );
    }

    #[test]
    fn parse_num_accepts_decimal_and_hex() {
        assert_eq!(parse_num("42"), Some(42));
        assert_eq!(parse_num("0x2A"), Some(42));
        assert_eq!(parse_num("0X2a"), Some(42));
        assert_eq!(parse_num("nope"), None);
    }
}
