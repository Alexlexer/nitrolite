//! System gauges that the dashboard can show honestly: CPU load, memory and
//! disk usage. All three come from documented Windows APIs, so unlike fan RPM
//! these numbers are real.

use windows::core::PCWSTR;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

#[derive(Default, Clone, Copy)]
pub struct Gauges {
    pub cpu_load_pct: Option<f32>,
    pub ram_used_gb: Option<f64>,
    pub ram_total_gb: Option<f64>,
    pub ram_used_pct: Option<f32>,
    pub disk_used_gb: Option<f64>,
    pub disk_total_gb: Option<f64>,
    pub disk_used_pct: Option<f32>,
}

fn filetime_ticks(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// Keeps the previous CPU sample so load is a delta rather than a lifetime average.
/// (total GB, free GB, used %) cached between refreshes.
type DiskSample = Option<(f64, f64, f32)>;

pub struct SysMeter {
    previous: Option<(u64, u64)>, // (busy, total)
    /// Disk usage changes slowly; re-query it every few seconds rather than
    /// every UI refresh. RAM stays per-sample because it is a cheap call.
    disk: Option<(std::time::Instant, DiskSample)>,
}

impl Default for SysMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl SysMeter {
    pub fn new() -> Self {
        Self {
            previous: None,
            disk: None,
        }
    }

    pub fn sample(&mut self) -> Gauges {
        Gauges {
            cpu_load_pct: self.cpu_load(),
            ..self.static_gauges()
        }
    }

    fn cpu_load(&mut self) -> Option<f32> {
        unsafe {
            let (mut idle, mut kernel, mut user) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).ok()?;

            let idle = filetime_ticks(idle);
            // Kernel time includes idle time.
            let total = filetime_ticks(kernel) + filetime_ticks(user);
            let busy = total.saturating_sub(idle);

            let previous = self.previous.replace((busy, total));
            let (prev_busy, prev_total) = previous?;

            let delta_total = total.saturating_sub(prev_total);
            if delta_total == 0 {
                return None;
            }
            let delta_busy = busy.saturating_sub(prev_busy);
            Some((delta_busy as f64 / delta_total as f64 * 100.0).clamp(0.0, 100.0) as f32)
        }
    }

    fn static_gauges(&mut self) -> Gauges {
        let mut gauges = Gauges::default();

        unsafe {
            let mut mem = MEMORYSTATUSEX {
                dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
                ..Default::default()
            };
            if GlobalMemoryStatusEx(&mut mem).is_ok() {
                let total = mem.ullTotalPhys as f64 / 1024.0 / 1024.0 / 1024.0;
                let avail = mem.ullAvailPhys as f64 / 1024.0 / 1024.0 / 1024.0;
                gauges.ram_total_gb = Some(total);
                gauges.ram_used_gb = Some(total - avail);
                gauges.ram_used_pct = Some(((total - avail) / total * 100.0) as f32);
            }

            // Disk usage moves slowly; refresh it every few seconds instead of
            // on every UI tick. RAM stays per-sample because it is a cheap call
            // and visibly changes.
            let stale = self
                .disk
                .as_ref()
                .map(|(at, _)| at.elapsed() >= std::time::Duration::from_secs(10))
                .unwrap_or(true);

            if stale {
                let path: Vec<u16> = "C:\\".encode_utf16().chain(std::iter::once(0)).collect();
                let (mut free, mut total) = (0u64, 0u64);
                let value = if GetDiskFreeSpaceExW(
                    PCWSTR(path.as_ptr()),
                    Some(&mut free),
                    Some(&mut total),
                    None,
                )
                .is_ok()
                    && total > 0
                {
                    let total_gb = total as f64 / 1024.0 / 1024.0 / 1024.0;
                    let free_gb = free as f64 / 1024.0 / 1024.0 / 1024.0;
                    Some((
                        total_gb,
                        free_gb,
                        ((total_gb - free_gb) / total_gb * 100.0) as f32,
                    ))
                } else {
                    None
                };
                self.disk = Some((std::time::Instant::now(), value));
            }

            if let Some((_, Some((total_gb, free_gb, pct)))) = self.disk {
                gauges.disk_total_gb = Some(total_gb);
                gauges.disk_used_gb = Some(total_gb - free_gb);
                gauges.disk_used_pct = Some(pct);
            }
        }

        gauges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_cpu_sample_has_no_load_yet() {
        let mut meter = SysMeter::new();
        // No previous sample: a load figure would be meaningless.
        assert_eq!(meter.cpu_load(), None);
    }

    #[test]
    fn second_cpu_sample_produces_a_percentage() {
        let mut meter = SysMeter::new();
        let _ = meter.cpu_load();
        std::thread::sleep(std::time::Duration::from_millis(120));
        if let Some(load) = meter.cpu_load() {
            assert!((0.0..=100.0).contains(&load), "load out of range: {load}");
        }
    }

    #[test]
    fn gauges_are_sane_on_this_machine() {
        let mut meter = SysMeter::new();
        let g = meter.sample();
        if let Some(pct) = g.ram_used_pct {
            assert!((0.0..=100.0).contains(&pct));
        }
        if let Some(total) = g.disk_total_gb {
            assert!(total > 0.0);
        }
    }
}
