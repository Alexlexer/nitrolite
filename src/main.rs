mod acer;
mod calib;

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
        std::env::var("NITRO_INSTANCE").unwrap_or_else(|_| DEFAULT_INSTANCE.to_string());

    // `analyze` works on a file and needs no hardware access.
    if cmd == "analyze" {
        let path = match args.get(1) {
            Some(p) => p,
            None => {
                eprintln!("usage: nitro analyze <capture.jsonl>");
                std::process::exit(2);
            }
        };
        if let Err(e) = calib::analyze(path) {
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
        other => {
            eprintln!("unknown command: {other}");
            eprintln!("usage: nitro [status|probe|raw|calibrate|analyze]");
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
                println!("nitro calibrate [--seconds N] [--idle N] [--interval-ms N] [--selectors N] [--load] [--out FILE]");
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

fn parse_num(s: &str) -> Option<u32> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        s.parse().ok()
    }
}
