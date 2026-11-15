# nitro — Development Plan

Replacement for Acer NitroSense: a small Rust tray app that talks directly to the
Acer ACPI gaming interface instead of running a 630 MB Electron stack.

**Status:** Phase 0 complete — protocol mapped, read path working.

---

## 1. Goal

| | |
|---|---|
| **Goal** | Full parity with the NitroSense features this laptop actually uses (fan mode, fan/thermal readout) at a fraction of the memory footprint. |
| **Success metric** | NitroSense uninstalled, `nitro` resident memory measured and reported, fan control verified under sustained load with no thermal regression. |
| **Non-goals** | Per-key RGB and lightbar (this model reports `Lighting: false` for all zones), game library scanning, GPU overclocking, telemetry, cloud features, store integration. |

## 2. Where we are

**Done (Phase 0):**

- Mapped the call chain: NitroSense (Electron) → named pipe → `AcerHardwareService` (SYSTEM) → `root\WMI : AcerGamingFunction` → ACPI → EC.
- Identified the live ACPI instance: `ACPI\PNP0C14\APGe_0`.
- Enumerated all 25 methods and their parameter shapes.
- Found the three quirks that block naive implementations (admin-only instance enumeration, `gmOutput` delivered as BSTR, `Put` wanting `VT_I4`).
- Working Rust read path: `nitro.exe` (162 KB) returns values identical to the PowerShell ground truth.

**Not done:** packing decode, setters, UI, privilege model, decommissioning.

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

### Phase 1 — Decode the read path  `[M1, M2]`

The core unknown. `gmOutput` is a packed word; the low byte is an ACPI status
(0 ok, 1/2 unsupported) and the remaining bytes carry values in a layout we have
not proven yet.

- [ ] **M1 Calibration harness.** `nitro calibrate` — sample all four getters at
      1 Hz into JSONL, alongside `MSAcpi_ThermalZoneTemperature` and a synthetic
      CPU load. Correlate: temperature fields track load, fan fields lag it.
- [ ] **M2 Decode table.** Write down the proven byte layout per selector:
      which selectors are valid, what each byte means, units and scale. Encode it
      as pure functions with fixture tests taken from real captures.

**Exit criteria:** a `nitro temp` / `nitro fan` output whose numbers match what
NitroSense shows, for CPU temp, GPU temp, and both fans, at idle and under load.

### Phase 2 — Safe setters  `[M3, M4]`

- [ ] **M3 Setter primitives.** `SetGamingFanBehavior` and `SetGamingProfile`.
      Read-back verification after every write.
- [ ] **M4 Failsafe.** Refuse to leave fans in a non-auto state unless `--force`.
      Restore `auto` on panic / on exit / on `Drop`. Never write a value that the
      read path has not first confirmed is accepted.

**Exit criteria:** mode changes are observable in NitroSense (or in fan RPM after
decode), and killing the tool mid-session never leaves fans pinned.

> **Safety gate:** no setter work merges until M1/M2 land. Writing EC bytes blind
> is how laptops end up with fans at zero and no thermal headroom.

### Phase 3 — Privilege model  `[M5]`

- [ ] **M5** Decide A vs B (see §3). If B: service install/uninstall, pipe ACL
      limited to the interactive user, versioned request/response framing,
      graceful degradation when the service is absent.

**Exit criteria:** documented elevation expectations; UI never requires admin
under option B.

### Phase 4 — Tray UI  `[M6, M7]`

- [ ] **M6 Minimal tray.** Icon + tooltip (temps), menu: Quiet / Balanced /
      Performance / Turbo / Auto. Target **< 10 MB resident** — to be measured,
      not assumed.
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
| Packing decode is wrong | Wrong temps/mode shown; bad fan decisions | Calibrate against known-good sources before any write; fixture tests |
| EC write bricks fan behaviour | Overheating | Setters gated behind decode; read-back verification; auto-restore on exit |
| Acer services also drive the fan | Our writes fight `AcerHardwareService` | Close NitroSense during tests; document coexistence; test with service stopped |
| Firmware/driver update changes ACPI | Reads start returning status 2 | Handle every non-zero status gracefully; surface as "unsupported selector", never as 0 |
| Privilege model churn | Rework late | Decide at M5 before UI is built on top |
| Model variance | Code works only on ANV15-51 | Keep model-specific selectors in a config table, not scattered in code |

## 6. Testing

- **Unit:** decode functions against captured fixture vectors (no hardware).
- **Integration:** `#[ignore]`d tests that require elevation and real hardware.
- **Calibration:** `nitro calibrate` output is committed as a golden file for this
  machine; decode changes must keep it consistent.
- **Manual:** thermal test — 10 min sustained load with the tool active, watching
  package temperature and fan RPM.

## 7. Open questions

1. Does `AcerHardwareService` need to stay installed for the EC to behave, or is
   the ACPI method authoritative on its own?
2. Which selectors are the valid ones for `GetGamingFanSpeed`? Only 1 and 4 return
   plausible data; 2/3 return status 1.
3. Is `GetGamingProfile(0) = 0x00FF000001000000` a profile pinion (`0xFF` marker)
   or two packed fields?
4. Does the Turbo key (`ModeSwitchKey: false` on this model) route through
   `SetGamingMiscSetting`?
5. Is `SetGamingFanTable` usable, or does it need the EC's factory curve format?

## 8. Repo layout

```
nitro/
├── Cargo.toml
├── docs/
│   ├── PLAN.md          # this file
│   └── PROTOCOL.md      # reverse-engineered interface + evidence
├── src/
│   ├── main.rs          # CLI: status / probe / raw
│   └── acer.rs          # COM/WMI client for AcerGamingFunction
└── captures/            # calibration output (golden files), gitignored by default
```
