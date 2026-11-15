//! Calibration harness (milestone M1).
//!
//! Samples every getter across a selector range at a fixed interval and writes
//! one JSON object per tick. Optionally generates CPU load in-process so the
//! capture contains a known idle -> load transition, which is what lets us tell
//! temperature fields (they track load) from fan fields (they lag it) and from
//! constant/status fields.
//!
//! The point is to stop guessing at the packed-word layout.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::acer::AcerWmi;

pub const GETTERS: [&str; 4] = [
    "GetGamingProfile",
    "GetGamingFanBehavior",
    "GetGamingFanSpeed",
    "GetGamingSysInfo",
];

pub const DEFAULT_SELECTORS: u32 = 8;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Reading {
    pub m: String,
    /// Selector passed as gmInput.
    pub sel: u32,
    pub raw: String,
    pub status: u8,
    pub value: u32,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Row {
    /// Milliseconds since capture start.
    pub t: u64,
    /// "idle" or "load".
    pub phase: String,
    /// ACPI thermal zones in Celsius.
    pub tz: Vec<f64>,
    pub r: Vec<Reading>,
}

pub struct CalibOpts {
    pub seconds: u64,
    pub interval_ms: u64,
    pub selectors: u32,
    pub load: bool,
    pub out: String,
    pub idle_seconds: u64,
}

pub fn run(wmi: &AcerWmi, opts: &CalibOpts) -> Result<(), String> {
    let file = File::create(&opts.out)
        .map_err(|e| format!("cannot create {}: {e}", opts.out))?;
    let mut out = BufWriter::new(file);

    let stop = Arc::new(AtomicBool::new(false));
    let mut burners = Vec::new();
    let mut load_started: Option<Instant> = None;

    let start = Instant::now();
    let interval = Duration::from_millis(opts.interval_ms);
    let mut next = start;
    let mut rows = 0u64;

    println!(
        "capturing {}s ({}s idle, {}s load) -> {}",
        opts.seconds, opts.idle_seconds, opts.seconds.saturating_sub(opts.idle_seconds), opts.out
    );

    while start.elapsed() < Duration::from_secs(opts.seconds) {
        let in_idle = start.elapsed() < Duration::from_secs(opts.idle_seconds);
        let phase = if in_idle { "idle" } else { "load" };

        // Start the burn threads once, when the idle phase ends.
        if !in_idle && opts.load && load_started.is_none() {
            load_started = Some(Instant::now());
            let threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            println!("  [{}s] load on ({threads} threads)", start.elapsed().as_secs());
            for _ in 0..threads {
                let flag = stop.clone();
                burners.push(std::thread::spawn(move || {
                    let mut acc = 0.0f64;
                    while !flag.load(Ordering::Relaxed) {
                        for i in 0..100_000u64 {
                            acc += (i as f64).sqrt().sin();
                        }
                        std::hint::black_box(acc);
                    }
                }));
            }
        }

        let mut readings = Vec::with_capacity((GETTERS.len() as u32 * opts.selectors) as usize);
        for method in GETTERS {
            for sel in 0..opts.selectors {
                match wmi.get(method, sel) {
                    Ok(raw) => readings.push(Reading {
                        m: method.to_string(),
                        sel,
                        raw: format!("{raw:#018X}"),
                        status: (raw & 0xFF) as u8,
                        value: ((raw >> 8) & 0xFFFF) as u32,
                    }),
                    Err(e) => readings.push(Reading {
                        m: method.to_string(),
                        sel,
                        raw: format!("ERR:{e}"),
                        status: 0xFF,
                        value: 0,
                    }),
                }
            }
        }

        let row = Row {
            t: start.elapsed().as_millis() as u64,
            phase: phase.to_string(),
            tz: wmi.thermal_zones().unwrap_or_default(),
            r: readings,
        };
        serde_json::to_writer(&mut out, &row).map_err(|e| format!("write failed: {e}"))?;
        out.write_all(b"\n").map_err(|e| format!("write failed: {e}"))?;
        rows += 1;

        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }

    stop.store(true, Ordering::Relaxed);
    for b in burners {
        let _ = b.join();
    }
    if let Some(t) = load_started {
        println!("  load ran for {}s", t.elapsed().as_secs());
    }
    out.flush().map_err(|e| format!("flush failed: {e}"))?;

    println!("wrote {rows} rows");
    Ok(())
}

/// Summarise a capture: which (method, selector) pairs actually changed, and how.
pub fn analyze(path: &str) -> Result<(), String> {
    let data = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let rows: Vec<Row> = data
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(|e| format!("bad row: {e}")))
        .collect::<Result<_, _>>()?;

    if rows.is_empty() {
        return Err("no rows in capture".to_string());
    }

    println!("capture: {path}");
    println!("rows: {}  duration: {}ms\n", rows.len(), rows.last().map(|r| r.t).unwrap_or(0));

    // Thermal zones first: this is the known-good reference signal.
    let tz_min = rows
        .iter()
        .flat_map(|r| r.tz.iter().copied())
        .fold(f64::INFINITY, f64::min);
    let tz_max = rows
        .iter()
        .flat_map(|r| r.tz.iter().copied())
        .fold(f64::NEG_INFINITY, f64::max);
    if tz_min.is_finite() {
        println!("ACPI thermal zone: {tz_min:.1} C -> {tz_max:.1} C (delta {:.1} C)\n", tz_max - tz_min);
    } else {
        println!("ACPI thermal zone: no readings\n");
    }

    // Group by (method, selector).
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<(String, u32), Vec<&Reading>> = BTreeMap::new();
    for row in &rows {
        for r in &row.r {
            groups.entry((r.m.clone(), r.sel)).or_default().push(r);
        }
    }

    println!("{:<24} {:>3}  {:>6}  {:>18}  {:>18}  {}", "method", "sel", "uniq", "idle raw", "load raw", "verdict");
    for ((method, sel), readings) in &groups {
        let uniq: std::collections::BTreeSet<&str> =
            readings.iter().map(|r| r.raw.as_str()).collect();
        if uniq.len() <= 1 {
            continue; // constant: status-only or unsupported
        }

        // First and last distinct raw values are a decent proxy for idle vs load extremes.
        let first = readings.first().map(|r| r.raw.clone()).unwrap_or_default();
        let last = readings.last().map(|r| r.raw.clone()).unwrap_or_default();
        let statuses: std::collections::BTreeSet<u8> = readings.iter().map(|r| r.status).collect();
        let verdict = if statuses.iter().all(|s| *s != 0) {
            "status only"
        } else if first != last {
            "VARIES"
        } else {
            "varies"
        };
        println!(
            "{method:<24} {sel:>3}  {:>6}  {first:>18}  {last:>18}  {verdict}",
            uniq.len()
        );
    }

    println!("\nconstant selectors omitted. 'VARIES' = first/last sample differ.");
    Ok(())
}

