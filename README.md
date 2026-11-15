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

**Phase 0 + M1 — read path working, first calibration done.** The protocol is
mapped, the Rust client matches the PowerShell ground truth, and the M1 capture
proved the Acer interface carries no thermal data (see below).

Not yet: setters, UI, GPU temps. See [docs/PLAN.md](docs/PLAN.md).

> **Do not uninstall NitroSense yet.** It is the reference implementation until
> `nitrolite` reaches parity.

## Requirements

- Windows 10/11, Acer hardware exposing `root\WMI : AcerGamingFunction`
- **An elevated shell** — the ACPI instance is admin-only; a non-elevated run
  fails at `ExecMethod` with `Access denied (0x80041003)`
- Rust (tested with the toolchain at `%USERPROFILE%\.cargo\bin`)

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

## Layout

```
src/main.rs    CLI: status / probe / raw
src/acer.rs    COM/WMI client for AcerGamingFunction
docs/PLAN.md   roadmap, risks, milestones
docs/PROTOCOL.md  reverse-engineered protocol + captured samples
```

## License

MIT




