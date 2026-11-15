<img src="assets/nitrolite-256.png" width="96" align="right" alt="nitrolite icon">

# nitrolite

A minimal replacement for Acer NitroSense on the Nitro ANV15-51 (and hopefully
sibling models): it talks straight to the hardware instead of running an Electron
app to do it.

NitroSense: **~600 MB across five processes**. `nitrolite.exe`: **736 KB**,
**~23 MB resident**, **~0.3% of one core**.

```
NitroSense  →  Electron → named pipe → AcerHardwareService → WMI → ACPI → EC
nitrolite   ────────────────────────────────→ WMI → ACPI → EC        (control)
nitrolite   ────────────────────────────────→ monitor service        (telemetry)
```

## Status

Working, verified on hardware:

- **Operating mode** — read and write, with read-back verification and automatic
  revert if the EC does not take the value.
- **Fan speed** — real RPM for CPU, GPU and SYS from Acer's monitor service
  ([§11](docs/PROTOCOL.md)). No estimates, no decoration in the numbers.
- **Temperatures** — CPU from the monitor service sensor (the ACPI thermal zone as fallback; it is coarse and can sit on one value for minutes), GPU from NVML (temperature,
  utilisation, clock, power, VRAM).
- **System gauges** — CPU load, memory, disk.
- **AC/DC follow** — ECO engages when the charger is removed and your chosen mode
  returns when it is plugged back in. This logic lived in NitroSense, not in the
  Acer service stack.
- **ECO** — selectable while unplugged (`[4]` in the TUI, menu entry in the tray),
  never stored as a preference.
- **Two surfaces** — a tray icon and a full-screen TUI dashboard.

See [docs/PLAN.md](docs/PLAN.md) for what is deliberately left out and why.

## Requirements

- Windows 10/11 on Acer hardware exposing `root\WMI : AcerGamingFunction`
- **Elevated** for anything touching ACPI — the instance is admin-only and a
  non-elevated run fails at `ExecMethod` with `Access denied (0x80041003)`
- NVIDIA driver (optional) for GPU gauges — degrades to CPU-only
- Acer monitor service running (optional) for fan RPM — degrades to "unavailable"
- Rust 1.98.0 (pinned in `rust-toolchain.toml`)

## Starting it

```powershell
nitrolite tui     # terminal dashboard — prompts for UAC, opens elevated
nitrolite tray    # tray icon — prompts for UAC
```

Both self-elevate, so run them from any shell, or use the Desktop / Start Menu
shortcuts. One-shot commands (`mode`, `temp`, `status`, `fans`) do **not**
self-elevate — an elevated child gets its own console and its output would
vanish — so run those from an elevated terminal.

The TUI fills the terminal with panels: CPU and GPU gauges with sparklines,
memory and storage bars, a temperature history chart, and a cooling panel with
two fans whose blades are driven by the **measured RPM** — the CPU and GPU fans
spin independently at their own speeds, so a hot GPU visibly outpaces an idle
CPU. When the monitor service is unavailable the rotation falls back to the
selected mode and the panel says "service unavailable" rather than inventing a
number.

The TUI never scrolls: it draws in the alternate screen, adapts to the terminal
height, and restores your scrollback on exit.

## Build

```powershell
cargo build --release
# -> target\release\nitrolite.exe
```

## Usage

```powershell
nitrolite tray                            # tray icon: temps, RPM, mode menu
nitrolite tui                             # terminal dashboard
nitrolite fans                            # CPU / GPU / SYS fan RPM
nitrolite temp                            # ACPI thermal zones + GPU temperature
nitrolite mode                            # current operating mode
nitrolite mode set quiet --dry-run        # show payload, write nothing
nitrolite mode set performance            # write + verify (reverts on mismatch)
nitrolite gpu                             # NVML detail
nitrolite status                          # raw values for all getters
nitrolite probe                           # sweep input selectors 0..0x20
nitrolite raw GetGamingProfile 0          # single call, hex output
nitrolite calibrate --seconds 90 --idle 20 --load
nitrolite analyze captures\calib-*.jsonl  # which selectors actually move
```

`tray`, `tui`, `mode`, `temp` and `status` need elevation; `fans`, `gpu` and
`analyze` do not.

`calibrate` and `analyze` are the reverse-engineering tools: capture every getter
under load and see which values actually move.

## Fan speed

The ACPI gaming interface exposes no tachometer. Fan RPM comes from a **different
component**, `AcerSysMonitorService`, over a small framed protocol on
`127.0.0.1:46753`:

```
"ACER" + uint32 LE command id + UTF-8 JSON
GET_MONITOR_DATA (10)  →  CPU_FANSPEED, GPU1_FANSPEED, SYS1_FANSPEED
```

Recovered from NitroSense's own client code, including the fact that its payload
encryption is conditional (`SupportAES`) and off by default here. The service
answers in ~700 ms, so it is polled on a background thread and the UI reads the
latest snapshot — a synchronous read would stall the render loop. Full detail and
captured samples: [docs/PROTOCOL.md §11](docs/PROTOCOL.md).

## Interface notes

The ACPI interface has four traps that break naive implementations — instance
enumeration needs admin, `gmOutput` arrives as a **BSTR string**, `Put(gmInput)`
needs `VT_I4` rather than the `VT_UI4` the MOF declares, and 64-bit setter input
needs **`VT_BSTR`**. Details and evidence: [docs/PROTOCOL.md](docs/PROTOCOL.md).

## Dependencies

- Windows + the Acer ACPI driver stack (provides `ACPI\PNP0C14\APGe_0`)
- NVIDIA driver for GPU gauges (optional)
- Acer monitor service for fan RPM (optional)
- an elevated shell

**NitroSense is not required.** Verified three ways: all four NitroSense
processes killed, the entire Acer service stack (`AASSvc` /
`AcerHardwareService.exe`) stopped, and finally the package **uninstalled
outright** — reads, temperatures and verified mode writes kept working each
time. See [docs/PROTOCOL.md §10](docs/PROTOCOL.md).

## Hardware support

Tested on exactly one machine: **Acer Nitro ANV15-51** (i5-13420H, RTX 4060
Laptop, Windows 11 26H2 build 26300).

The interface is model- and firmware-specific:

- The ACPI instance is `ACPI\PNP0C14\APGe_0`. Override with `NITROLITE_INSTANCE`
  if another machine exposes a different path.
- Every value here — mode `0/1/4/6`, the fan/behaviour words, the
  `(value << 8) | selector` packing — was derived **empirically** on this laptop.
  Do not assume the same numbers mean the same things elsewhere; on another model
  `4` may not be "Performance".
- Confirmed absent on this model: keyboard/lightbar RGB, Turbo key, MUX switch,
  Whisper mode.

If it misbehaves on other hardware, `nitrolite probe` and `nitrolite raw` dump
what that machine actually returns before you trust anything else.

## Layout

```
src/main.rs     CLI: status / fans / temp / mode / probe / raw / calibrate
src/acer.rs     COM/WMI client for AcerGamingFunction (control path)
src/monitor.rs  ACER-framed client for fan RPM (telemetry path)
src/power.rs    AC/DC rule: ECO on battery, preferred mode on AC
src/sysinfo.rs  CPU load, memory, disk
src/nvml.rs     GPU gauges via nvml.dll
src/tui.rs      full-screen dashboard
src/tray.rs     notification-area icon
docs/PLAN.md        roadmap, risks, milestones
docs/PROTOCOL.md    reverse-engineered protocols + captured samples
tools/make_icon.py  regenerates assets/nitrolite.ico
```

## License

MIT



