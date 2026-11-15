//! Client for Acer's system-monitor service — the one place fan speed is
//! actually readable.
//!
//! Wire format, recovered from the NitroSense client (`app.asar`):
//!
//! ```text
//! "ACER"                    4 bytes ASCII magic
//! uint32 little-endian      command id (GET_MONITOR_DATA = 10)
//! UTF-8 JSON                request payload, `{}` for monitor data
//! ```
//!
//! The reply is a JSON object whose `data` carries `CPU_FANSPEED`,
//! `GPU1_FANSPEED` and `SYS1_FANSPEED` in RPM. Encryption (AES-256-ECB with a
//! key from `SOFTWARE\OEM\AcerAgentService\AESkey`) is only applied when
//! `SupportAES` is non-zero; it is absent on this machine, so frames are
//! plaintext. See docs/PROTOCOL.md §11.
//!
//! Read-only: this module never sends `SET_DEVICE_DATA`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde::Deserialize;

const ADDRESS: &str = "127.0.0.1:46753";
const MAGIC: &[u8; 4] = b"ACER";
const CMD_GET_MONITOR_DATA: u32 = 10;

#[derive(Debug, Clone, Default)]
pub struct Monitor {
    pub cpu_fan_rpm: Option<u32>,
    pub gpu_fan_rpm: Option<u32>,
    pub sys_fan_rpm: Option<u32>,
    /// The service's own CPU/GPU sensors, which differ from the ACPI thermal
    /// zone — shown alongside rather than substituted.
    pub cpu_temp_c: Option<f64>,
    pub gpu_temp_c: Option<f64>,
}

#[derive(Deserialize)]
struct Envelope {
    data: Option<Payload>,
    result: Option<String>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "CPU_FANSPEED")]
    cpu_fan: Option<f64>,
    #[serde(rename = "GPU1_FANSPEED")]
    gpu_fan: Option<f64>,
    #[serde(rename = "SYS1_FANSPEED")]
    sys_fan: Option<f64>,
    #[serde(rename = "CPU_TEMPERATURE")]
    cpu_temp: Option<f64>,
    #[serde(rename = "GPU1_TEMPERATURE")]
    gpu_temp: Option<f64>,
}

fn rpm(value: Option<f64>) -> Option<u32> {
    // The service reports speeds as floats; anything absurd is treated as noise
    // rather than displayed as a fan spinning at 60000 RPM.
    value
        .filter(|v| (0.0..=20000.0).contains(v))
        .map(|v| v.round() as u32)
}

fn frame(command: u32, payload: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&command.to_le_bytes());
    out.extend_from_slice(payload.as_bytes());
    out
}

/// Count braces so a reply split across reads is not parsed half-finished.
fn json_complete(bytes: &[u8]) -> bool {
    let mut depth = 0i32;
    let mut started = false;
    for &b in bytes {
        match b {
            b'{' => {
                depth += 1;
                started = true;
            }
            b'}' => depth -= 1,
            _ => {}
        }
        if started && depth == 0 {
            return true;
        }
    }
    false
}

pub fn parse_reply(text: &str) -> Option<Monitor> {
    let envelope: Envelope = serde_json::from_str(text).ok()?;
    // "0" is success; anything else means the service rejected the request.
    if envelope.result.as_deref() != Some("0") {
        return None;
    }
    let data = envelope.data?;

    Some(Monitor {
        cpu_fan_rpm: rpm(data.cpu_fan),
        gpu_fan_rpm: rpm(data.gpu_fan),
        sys_fan_rpm: rpm(data.sys_fan),
        cpu_temp_c: data.cpu_temp,
        gpu_temp_c: data.gpu_temp,
    })
}

/// One request/response exchange with the monitor service.
///
/// Returns `None` whenever the service is unavailable — it is part of the Acer
/// stack, so it may be stopped, and the caller must degrade rather than fail.
pub fn read() -> Option<Monitor> {
    let address: SocketAddr = ADDRESS.parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(1000)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(2000)))
        .ok()?;

    stream.write_all(&frame(CMD_GET_MONITOR_DATA, "{}")).ok()?;

    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if json_complete(&buf) {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    parse_reply(&String::from_utf8_lossy(&buf))
}

/// Poll the service on a background thread and publish the latest snapshot.
///
/// The service takes ~700 ms to answer (it samples on its own cadence), which
/// is far too long to sit in a render loop. Callers read the mutex, which is
/// effectively free, and the animation never stalls.
pub fn shared(interval: Duration) -> std::sync::Arc<std::sync::Mutex<Option<Monitor>>> {
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let writer = std::sync::Arc::clone(&slot);

    std::thread::spawn(move || loop {
        if let Some(snapshot) = read() {
            if let Ok(mut guard) = writer.lock() {
                *guard = Some(snapshot);
            }
        }
        std::thread::sleep(interval);
    });

    slot
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from the real service on an Acer Nitro ANV15-51.
    const CAPTURE: &str = r#"{
        "data" : {
            "CPU_CORE_AMOUNT" : 12,
            "CPU_FANSPEED" : 4464,
            "CPU_TEMPERATURE" : 79,
            "CPU_USAGE" : 16.36,
            "GPU1_FANSPEED" : 3993.0,
            "GPU1_TEMPERATURE" : 82,
            "SYS1_FANSPEED" : 4445
        },
        "request" : "GET_MONITOR_DATA",
        "result" : "0"
    }"#;

    #[test]
    fn frame_matches_the_recovered_format() {
        let f = frame(CMD_GET_MONITOR_DATA, "{}");
        assert_eq!(&f[..4], b"ACER");
        assert_eq!(u32::from_le_bytes(f[4..8].try_into().unwrap()), 10);
        assert_eq!(&f[8..], b"{}");
    }

    #[test]
    fn parses_fan_speeds_from_a_real_reply() {
        let m = parse_reply(CAPTURE).expect("capture must parse");
        assert_eq!(m.cpu_fan_rpm, Some(4464));
        assert_eq!(m.gpu_fan_rpm, Some(3993));
        assert_eq!(m.sys_fan_rpm, Some(4445));
        assert_eq!(m.cpu_temp_c, Some(79.0));
        assert_eq!(m.gpu_temp_c, Some(82.0));
    }

    #[test]
    fn non_zero_result_is_a_failure_not_a_zero() {
        let bad = r#"{"result":"3"}"#;
        assert!(parse_reply(bad).is_none());
    }

    #[test]
    fn absurd_speeds_are_discarded_rather_than_shown() {
        let noisy = r#"{"result":"0","data":{"CPU_FANSPEED": 60000, "GPU1_FANSPEED": 3993}}"#;
        let m = parse_reply(noisy).unwrap();
        assert_eq!(m.cpu_fan_rpm, None, "60000 RPM is noise, not a reading");
        assert_eq!(m.gpu_fan_rpm, Some(3993));
    }

    #[test]
    fn detects_a_complete_json_object() {
        assert!(!json_complete(b"{\"a\":1"));
        assert!(json_complete(b"{\"a\":1}"));
        assert!(json_complete(b"{\"a\":{\"b\":2}} trailing"));
    }
}
