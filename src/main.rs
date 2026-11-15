mod acer;

use acer::{AcerWmi, DEFAULT_INSTANCE};

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

    let wmi = match AcerWmi::connect(&instance) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("connect failed: {e}");
            eprintln!("(this needs an elevated shell; the ACPI instance is admin-only)");
            std::process::exit(1);
        }
    };

    let result = match cmd {
        "status" => status(&wmi),
        "raw" => {
            let method = args.get(1).map(String::as_str).unwrap_or("GetGamingProfile");
            let sel: u32 = args
                .get(2)
                .and_then(|s| parse_num(s))
                .unwrap_or(0) as u32;
            match wmi.get(method, sel) {
                Ok(v) => {
                    println!("{method}(in={sel}) = {v:#018X}");
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        "probe" => probe(&wmi),
        other => {
            eprintln!("unknown command: {other}");
            eprintln!("usage: nitro [status|probe|raw <method> <input>]");
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Packed word layout, guessed so far: byte0 = ACPI status (0 ok, 1/2 error),
/// byte1 = primary value, higher bytes = additional fields.
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

    for method in ["GetGamingProfile", "GetGamingFanBehavior", "GetGamingFanSpeed", "GetGamingSysInfo"] {
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
    Ok(())
}

fn probe(wmi: &AcerWmi) -> Result<(), String> {
    for method in GETTERS {
        println!("### {method}");
        for sel in 0..=0x20u32 {
            match wmi.get(method, sel) {
                Ok(raw) => {
                    let w = Word(raw);
                    println!("  in={sel:<4} {}  status={} value={}", w.raw(), w.status(), w.value());
                }
                Err(_) => {}
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
