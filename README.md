<img src="assets/nitrolite-256.png" width="96" align="right" alt="nitrolite icon">

# nitrolite

A minimal replacement for Acer NitroSense on the Nitro ANV15-51 (and hopefully
sibling models): talks straight to the Acer ACPI gaming interface instead of
running an Electron app to do it.

NitroSense on this machine: **~630 MB across 4 processes**. `nitrolite.exe`: **162 KB**.

```
NitroSense  →  named pipe  →  AcerHardwareService (SYSTEM)  →  root\WMI  →  ACPI  →  EC
nitrolite       ─────────────────────────────────────────────────→  root\WMI  →  ACPI  →  EC
```

## Status

**v0.2.1 — a working replacement.** Direct ACPI/WMI access with no NitroSense
dependency, verified against hardware:

- operating mode: read *and* write, with read-back verification and automatic
  revert if the EC does not take the value
- CPU temperature from the ACPI thermal zone; GPU temperature, utilisation,
  clock, power and VRAM from NVML
- CPU load, memory and disk usage from documented Windows APIs
- tray icon, and a full-screen TUI dashboard with mode-driven fan animation
- ECO is selectable while unplugged (`[4]` in the TUI, menu entry in the tray) and is
  never stored as a preference
- AC/DC follow: ECO is applied when the charger is removed and your chosen mode
  is restored when it comes back (this logic lived in NitroSense, not the Acer
  service stack)
- ~22.8 MB private, ~0.3% of one core, versus NitroSense's ~600 MB across five
  processes

**Fan RPM is now read for real.** The earlier conclusion that it was unreadable
was wrong: `AcerSysMonitorService` exposes `CPU_FANSPEED`, `GPU1_FANSPEED` and
`SYS1_FANSPEED` over a small `ACER`-framed JSON protocol on localhost:46753
([docs/PROTOCOL.md §11](docs/PROTOCOL.md)). `nitrolite fans`, the dashboard and
the tray tooltip all show measured speeds. The fan *animation* stays decorative
and mode-driven — only the numbers are real.

See [docs/PLAN.md](docs/PLAN.md) for what is left and why.

> **Do not uninstall NitroSense yet.** It is the reference implementation until
> `nitrolite` reaches parity.

## Requirements

- Windows 10/11, Acer hardware exposing `root\WMI : AcerGamingFunction`
- **An elevated shell** — the ACPI instance is admin-only; a non-elevated run
  fails at `ExecMethod` with `Access denied (0x80041003)`
- Rust (tested with the toolchain at `%USERPROFILE%\.cargo\bin`)

## Starting it

```powershell
nitrolite tui     # terminal dashboard — prompts for UAC, opens elevated
nitrolite tray    # tray icon — prompts for UAC
```

Both self-elevate, so run them from any shell (or via the Desktop / Start Menu
shortcuts). One-shot commands (`mode`, `temp`, `status`) do **not** self-elevate —
an elevated child gets its own console and its output would vanish — so run those
from an elevated terminal.

The TUI is a full-screen dashboard: CPU and GPU panels with live gauges and
sparklines, memory and storage bars, a temperature history chart, and a cooling
panel with two animated fans. It shows two animated fans — CPU and GPU. They are **decoration**: rotation
speed follows the selected mode (Quiet 15%, Balanced 50%, Performance 100%), the
LED ring brightens with it, and the active mode is printed underneath. No RPM
figure is displayed, estimated or implied, because this machine exposes no fan
tachometer to userland ([docs/PROTOCOL.md §11](docs/PROTOCOL.md)). A fabricated
number would be worse than no number.
The TUI does not scroll: it draws in the alternate screen, adapts to the terminal
height, and restores your scrollback on exit.
## Build

```powershell
cargo build --release
# -> target\release\nitrolite.exe
```

## Usage

```powershell
nitrolite tray                            # tray icon: temps in tooltip, mode menu
nitrolite tui                             # terminal dashboard: temps + mode keys
nitrolite status                          # raw values for all getters
nitrolite temp                            # ACPI thermal zones + GPU temperature
nitrolite mode                            # current operating mode
nitrolite mode set quiet --dry-run        # show payload, write nothing
nitrolite mode set performance            # write + verify (reverts on mismatch)
nitrolite probe                           # sweep input selectors 0..0x20
nitrolite raw GetGamingProfile 0          # single call, hex output
nitrolite calibrate --seconds 90 --idle 20 --load
nitrolite analyze captures\calib-*.jsonl  # which selectors actually move
```

`tray`, `mode`, `temp` and `status` need elevation; `gpu`, `analyze` do not.

`calibrate` and `analyze` are the M1 investigation tools: capture under load and
see which values track it. `analyze` needs no elevation.

## What the interface does not give us

A 90 s load capture moved the ACPI thermal zone 55 → 91 °C while nothing in the
Acer interface changed. Temperatures therefore come from
`MSAcpi_ThermalZoneTemperature` (CPU) and NVML (GPU), not from the gaming
interface — and live fan RPM appears not to be exposed at all. See
[docs/PROTOCOL.md §4.1](docs/PROTOCOL.md).

Set `NITROLITE_INSTANCE` to override the default ACPI instance
(`ACPI\PNP0C14\APGe_0`) on a different model.

## Interface notes

The ACPI interface has three traps that make naive implementations fail — instance
enumeration needs admin, `gmOutput` arrives as a BSTR string, and `Put(gmInput)`
needs `VT_I4` rather than the `VT_UI4` the MOF declares. Details and evidence:
[docs/PROTOCOL.md](docs/PROTOCOL.md).

## Dependencies

- Windows + the Acer ACPI driver stack (provides the `ACPI\PNP0C14\APGe_0`
  firmware device)
- NVIDIA driver for GPU temperatures (optional — degrades to CPU-only)
- an elevated shell

**NitroSense is not required.** Verified three ways: all four NitroSense processes killed, the entire Acer service stack (`AASSvc` / `AcerHardwareService.exe`) stopped, and finally the package **uninstalled outright** — reads, temperatures and verified mode writes all still worked each time. See [docs/PROTOCOL.md §10](docs/PROTOCOL.md).

## Hardware support

Tested on exactly one machine: **Acer Nitro ANV15-51** (i5-13420H, RTX 4060
Laptop, Windows 11 26H2 build 26300).

That matters, because the interface is model- and firmware-specific:

- The ACPI instance is `ACPI\PNP0C14\APGe_0`. Override with the
  `NITROLITE_INSTANCE` environment variable if another machine exposes a
  different path.
- Every value in this repo — mode `0/1/4/6`, the fan/behaviour words, the
  `(value << 8) | selector` setter packing — was derived **empirically** on this
  laptop. Do not assume the same numbers mean the same things elsewhere; on a
  different model `4` may not be "Performance".
- Fan RPM is not read at all; see [docs/PROTOCOL.md §11](docs/PROTOCOL.md) for
  why the data exists but is unreachable.
- Confirmed absent on this model: keyboard/lightbar RGB, Turbo key, MUX switch,
  Whisper mode.

If it misbehaves on other hardware, `nitrolite probe` and `nitrolite raw` exist
to dump what that machine actually returns before trusting anything else.
## Layout

```
src/main.rs    CLI: status / probe / raw
src/acer.rs    COM/WMI client for AcerGamingFunction
docs/PLAN.md   roadmap, risks, milestones
docs/PROTOCOL.md  reverse-engineered protocol + captured samples
```

## License

MIT














