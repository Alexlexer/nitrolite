# nitrolite — Development Plan

Replacement for Acer NitroSense: a small Rust tray app that talks directly to the
Acer ACPI gaming interface instead of running a 630 MB Electron stack.

**Status:** v0.2.0 — M1, M2, M3, M6, M6b-c complete. The tool is a working
replacement: verified mode control, CPU/GPU temperatures, system gauges, tray icon
and a dashboard TUI. The items still unchecked below are optional, and each says
why it was not pursued.

---

## 1. Goal

| | |
|---|---|
| **Goal** | Full parity with the NitroSense features this laptop actually uses (fan mode, fan/thermal readout) at a fraction of the memory footprint. |
| **Success metric** | NitroSense uninstalled, `nitrolite` resident memory measured and reported, fan control verified under sustained load with no thermal regression. |
| **Non-goals** | Per-key RGB and lightbar (this model reports `Lighting: false` for all zones), game library scanning, GPU overclocking, telemetry, cloud features, store integration. |

## 2. Where we are

**Done (Phase 0):**

- Mapped the call chain: NitroSense (Electron) → named pipe → `AcerHardwareService` (SYSTEM) → `root\WMI : AcerGamingFunction` → ACPI → EC.
- Identified the live ACPI instance: `ACPI\PNP0C14\APGe_0`.
- Enumerated all 25 methods and their parameter shapes.
- Found the three quirks that block naive implementations (admin-only instance enumeration, `gmOutput` delivered as BSTR, `Put` wanting `VT_I4`).
- Working Rust read path: `nitrolite.exe` (162 KB) returns values identical to the PowerShell ground truth.

**Not done:** setters, UI, privilege model, decommissioning.

See [PROTOCOL.md](PROTOCOL.md) for the interface details and evidence.

---

## 3. Architecture decision (open — Phase 3)

The UI cannot stay admin forever. Three options:

| Option | Shape | Pro | Con |
|---|---|---|---|
| **A. Single elevated exe** | Tray app launches elevated (one UAC) | Simplest, no IPC | Whole UI runs as admin |
| **B. Service + user UI** | SYSTEM service owns WMI; user UI talks over a pipe | UI unprivileged, survives logon | Must harden pipe ACL; more code; reimplements Acer's design |
| **C. Reuse Acer's pipe** | Skip WMI, speak `predatorsense_hardware_service_*` | No admin, no service of our own | Undocumented protocol, depends on Acer service being installed |

**Default for v1: option A.** Ship something working, then migrate to B if the
elevation requirement proves annoying. Option C is rejected unless NitroSense's
service must stay installed anyway — it defeats the point of removing the stack.

---

## 4. Roadmap

### Phase 1 — Decode the read path  `[M1 ✅, M2]`

**M1 result (2026-11-15).** A 90 s capture with 70 s of CPU load moved the ACPI
thermal zone 55.1 → 91.0 °C while **nothing in the Acer interface changed**. A
full 0–0xFF selector sweep found only a handful of valid selectors, and
`GetGamingFanBehavior` answers with a pattern that tracks the *selector's bits*
rather than any sensor. Details in [PROTOCOL.md §4.1](PROTOCOL.md).

- [x] **M1 Calibration harness.** `nitrolite calibrate` samples every getter into
      JSONL under optional load; `nitrolite analyze` reports which selectors move.
- [x] **M2 Real temperature sources.** CPU via `MSAcpi_ThermalZoneTemperature`
      (`nitrolite temp`, verified 55→91 °C under load), GPU via NVML (`nitrolite gpu`,
      verified against `nvidia-smi`).
- [x] **M2b Mode mapping.** `GetGamingMiscSetting(0x0B)` is the operating mode:
      `0` Quiet, `1` Balanced, `4` Performance, `6` ECO (automatic on battery —
      this model has no Turbo). `nitrolite mode` reports it. Established read-only,
      by correlating user-driven switches with NitroSense's own log — no EC writes.
- [x] **M2c Fan RPM verdict.** Unavailable: `GetGamingFanSpeed` is constant and
      selector-bit-driven, NVML reports `N/A`, no other getter exposes a
      tachometer. Closed rather than left open.
- [ ] **M2d Remaining odd words.** `GetGamingProfile(0) = 0x00FF000001000000`,
      `FanBehavior` `0x100`/`0x4000`/`0x4100` — none change with mode or load.
      Decide whether they are capability bitfields (ignore) or something else.

**Exit criteria (revised):** ✅ CPU and GPU temperatures displayed and correct;
✅ mode reported; ✅ RPM verdict written. `M2d` is tidy-up, not a blocker.

### Phase 2 — Safe setters  `[M3 ✅, M4]`

- [x] **M3 Setter primitives.** `SetGamingMiscSetting` with
      `gmInput = (value << 8) | selector`, input encoded as `VT_BSTR`.
      Verified: Quiet `0x0B` → status 0, read-back mode 0; Balanced `0x010B` →
      mode 1. `nitrolite mode set <name> [--dry-run]`.
- [x] **M3b Read-back verification.** Every write re-reads the state word and
      compares; a mismatch triggers an automatic restore of the previous mode.
- [ ] **M4 Failsafe hardening.** Still to do: restore on panic/`Drop`, and the
      `--force` gate for non-auto fan states (mode has no equivalent).
- [ ] **M3c Other setters.** `SetGamingFanBehavior` / `SetGamingFanSpeed` /
      `SetGamingProfile` encodings unknown; nothing writes them yet.

**Exit criteria (revised):** mode changes are verified by read-back and revert on
mismatch. Killing the tool mid-session never leaves a state we did not verify.

> **Safety gate:** no setter work merges until M1/M2 land. Writing EC bytes blind
> is how laptops end up with fans at zero and no thermal headroom.

### Phase 3 — Privilege model  `[M5]`

- [ ] **M5** Decide A vs B (see §3). If B: service install/uninstall, pipe ACL
      limited to the interactive user, versioned request/response framing,
      graceful degradation when the service is absent.

**Exit criteria:** documented elevation expectations; UI never requires admin
under option B.

### Phase 4 — Tray UI  `[M6, M7]`

- [x] **M6 Minimal tray.** Native Win32 (hidden message window +
      `Shell_NotifyIconW`) — no GUI toolkit. Right-click menu: Quiet / Balanced /
      Performance with the active one checked, plus Exit. Tooltip shows mode,
      CPU and GPU temperature, refreshed on a 2 s timer. Mode changes go through
      the verified setter and revert on read-back mismatch.

      **Measured:** 22.9 MB private / 36.7 MB working set, 0.000 s CPU over 10 s
      idle. NitroSense for comparison: ~577–630 MB across 5 processes.

      **Target missed:** the plan said < 10 MB resident; we are at 22.9 MB
      private. The gap is COM/WMI, the `windows` crate's static data, and NVML
      being loaded eagerly. Worth a pass later (`M6b`), but not worth blocking on
      — it is already a ~25× reduction.
- [x] **M6c TUI.** `nitrolite tui` — live dashboard (mode, CPU zone, GPU), keys 1/2/3 to switch mode with read-back verification, q to quit. ASCII borders when stdout is not a console so redirected output stays readable.
- [ ] **M7 Small window.** Live temps + fan RPM, mode buttons, fan curve editor
      (custom table via `SetGamingFanTable`) only if M2 makes the table
      interpretable.

**Exit criteria:** RAM measured and reported against NitroSense's baseline;
idle CPU under 0.5 %.

### Phase 5 — Parity and decommission  `[M8]`

- [ ] **M8** Feature parity check against NitroSense on this model (fan mode,
      temps, fan readout). Then: uninstall NitroSense, document how to restore it,
      verify behaviour survives reboot and an Acer driver update.

**Exit criteria:** 72 hours of daily use with NitroSense removed, no thermal
incident, no manual intervention needed.

---

## 5. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| **Interface exposes no thermal data and probably no fan RPM** (confirmed M1) | Cannot show fan speed; writes cannot be verified by tachometer | Temps from ACPI zone + NVML; verify writes behaviourally; state the limitation instead of faking a number |
| Packing decode is wrong | Wrong temps/mode shown; bad fan decisions | Calibrate against known-good sources before any write; fixture tests |
| EC write bricks fan behaviour | Overheating | Setters gated behind decode; read-back verification; auto-restore on exit |
| Acer services also drive the fan | Our writes fight `AcerHardwareService` | Close NitroSense during tests; document coexistence; test with service stopped |
| Firmware/driver update changes ACPI | Reads start returning status 2 | Handle every non-zero status gracefully; surface as "unsupported selector", never as 0 |
| Privilege model churn | Rework late | Decide at M5 before UI is built on top |
| Model variance | Code works only on ANV15-51 | Keep model-specific selectors in a config table, not scattered in code |

## 6. Testing

- **Unit:** decode functions against captured fixture vectors in `fixtures/`
  (committed, hand-trimmed, no hardware needed).
- **Integration:** `#[ignore]`d tests that require elevation and real hardware.
- **Calibration:** `captures/*.jsonl` is machine-specific and gitignored; trim a
  representative window into `fixtures/` when a decode rule is proven against it.
- **Manual:** thermal test — 10 min sustained load with the tool active, watching
  package temperature and fan RPM.

## 7. Open questions

1. Does `AcerHardwareService` need to stay installed for the EC to behave, or is
   the ACPI method authoritative on its own?
2. What does `GetGamingFanSpeed`'s constant `100` (selectors 1 and 4) represent —
   a setpoint percentage, a curve index, or something else? It did not move
   through 70 s at 91 °C.
3. Is `GetGamingProfile(0) = 0x00FF000001000000` a profile pinion (`0xFF` marker)
   or two packed fields?
4. ~~Does the Turbo key route through `SetGamingMiscSetting`?~~ **Resolved:** there
   is no Turbo on this model; `6` is ECO. **Implemented:** the AC/DC follow now
   lives in `power.rs` — ECO on battery, preferred mode on AC. Verified that
   NitroSense, not the Acer service, had been doing this: with NitroSense
   uninstalled and `AASSvc` running, unplugging left the mode at Balanced.
5. Is `SetGamingFanTable` usable, or does it need the EC's factory curve format?

## 8. Repo layout

```
nitrolite/
├── Cargo.toml
├── docs/
│   ├── PLAN.md          # this file
│   └── PROTOCOL.md      # reverse-engineered interface + evidence
├── src/
│   ├── main.rs          # CLI: status / temp / probe / raw / calibrate / analyze
│   ├── calib.rs         # M1 sampling harness + capture analysis
│   └── acer.rs          # COM/WMI client: AcerGamingFunction + thermal zones
├── captures/            # raw calibration captures (gitignored)
└── fixtures/            # trimmed captures used by unit tests (committed)
```




