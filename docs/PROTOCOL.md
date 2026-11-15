# Protocol notes — Acer ACPI gaming interface

Reverse-engineered from NitroSense 5.1.385 on an **Acer Nitro ANV15-51**
(i5-13420H, RTX 4060 Laptop, Windows 11 26H2, build 26300.9974).

Everything below is evidence-backed unless marked **hypothesis**.

---

## 1. Call chain

```
NitroSense.exe                    Electron (166 MB exe, ~630 MB RAM across 4 procs)
 └─ app.asar                      132 MB, contains no WMI strings
 └─ xsense_native.node            generic helpers: WMI value reads, brightness,
                                  display info, process lists, Darfon \\.\LCD
 └─ named pipe                    predatorsense_hardware_service_*
      └─ AcerHardwareService.exe  from predatorservice.inf, runs as SYSTEM
           └─ SELECT * FROM AcerGamingFunction        [root\WMI]
                └─ ACPI\PNP0C14\APGe_0
                     └─ EC
```

Evidence: `AcerHardwareService.exe` string table contains
`SELECT * FROM AcerGamingFunction`, `GetGamingFanSpeed`, `SetGamingFanBehavior`,
`namedpipe_agent_service_function::*`, `treadstone::TsClientCommandProcessor::wmi_*`,
and format strings such as `WMI GetGamingFanGroupSpeedFunction input: 0x%I32X output: 0x%I64X`.

NitroSense itself never touches WMI for fans — that is why we skip straight to the
last hop.

---

## 2. The WMI class

- Namespace: `root\WMI`
- Class: `AcerGamingFunction`
- Live instance: **`ACPI\PNP0C14\APGe_0`** (single, singleton)
- Object path used with `ExecMethod`:
  `AcerGamingFunction.InstanceName="ACPI\\PNP0C14\\APGe_0"`

### Methods (25)

| Method | In | Out |
|---|---|---|
| `GetGamingProfile` / `SetGamingProfile` | u32 / u64 | u64 / u32 |
| `GetGamingFanBehavior` / `SetGamingFanBehavior` | u32 / u64 | u64 / u32 |
| `GetGamingFanSpeed` / `SetGamingFanSpeed` | u32 / u64 | u64 / u32 |
| `GetGamingFanTable` / `SetGamingFanTable` | — / u64 | u64 / u32 |
| `GetGamingSysInfo` | u32 | u64 |
| `GetGamingProfileSetting` / `SetGamingProfileSetting` | u32 / u64 | u64 / u32 |
| `GetGamingMiscSetting` / `SetGamingMiscSetting` | u32 / u64 | u64 / u32 |
| `GetGamingLED*`, `SetGamingLED*`, `GetGamingRgbKb`, `SetGamingKBBacklight` | — | — |
| `GetCPUOverclockingProfile` / `SetCPUOverclockingProfile` | byte[] | byte[] |

This model reports `Lighting: {keyboard: false, lightbar: false, logo: false}` in
`app\win32\Nitro\Nitro ANV15-51.json`, so LED methods are out of scope.

---

## 3. Three quirks that block naive implementations

1. **Instance enumeration requires administrator.** Class metadata
   (`Get-CimClass`, `GetObject`) is readable by any user, so the interface *looks*
   present; enumerating instances returns `WBEM_E_ACCESS_DENIED (0x80041003)`,
   and every `ExecMethod` fails with `Impersonation failed - Access denied`.
   Symptom to watch for: "Invalid method Parameter(s)" when invoking by class name
   with no instance context.

2. **`gmOutput` is a BSTR, not a UInt64.** The out-param comes back with
   `vt=8 (VT_BSTR)` and CIM type `21 (UINT64)` — the ACPI mapper stringifies the
   value. Parse the string ("12345" or "0x3009") to get the number.

3. **`Put(gmInput)` requires `VT_I4`.** The MOF declares `uint32`, but a
   `VT_UI4` VARIANT fails with `WBEM_E_TYPE_MISMATCH (0x80041005)`. Use `i32`.

Confirmed workarounds live in [src/acer.rs](../src/acer.rs).

---

## 4. Packed output word

`gmOutput` layout **as far as proven**:

```
bits   63..24        23..16     15..8       7..0
       value fields   ?         value       status
```

- `status` (byte 0): `0` = ok, `1`/`2` = selector not supported / error.
  Never interpret a non-zero status as a value.
- `value` (byte 1) fits the fan data seen so far.

**Hypothesis, not proven:** byte 1 is a percentage (0–100) for fan speed and a
mode index for fan behavior. Fuel for calibration — do not ship as fact.

### 4.1 What M1 calibration actually found (2026-11-15)

Capture: 90 s, 20 s idle + 70 s of in-process CPU load on all 12 threads,
1 Hz, selectors 0–7 (`captures/calib-load.jsonl`).

**The ACPI thermal zone tracked the load: 55.1 °C → 91.0 °C.**

**The Acer interface did not change at all.** Only one selector produced a
varying value across the whole run:

| Selector | Behaviour under 70 s load |
|---|---|
| `GetGamingSysInfo(2)` | toggles `0x0000000000000000` ↔ `0x0000010000000000` — a flag (bit 40), not a measurement |
| everything else | constant for the entire capture |

Wide sweep over selectors 0–0xFF confirms the valid set is tiny:

| Method | Valid selectors | Value |
|---|---|---|
| `GetGamingSysInfo` | 0, 1, 2 | `0x...0227000000`, `0x0`, `0x10000000000` |
| `GetGamingFanSpeed` | **1, 4** | `0x6400` (100), constant |
| `GetGamingFanBehavior` | many | alternates `0x100` / `0x4000` / `0x4100` — pattern follows the **selector's bits**, not a sensor index |
| `GetGamingProfile` | 0 | `0x00FF000001000000`, constant |
| `GetGamingProfileSetting` | none | status 1/2 for all |
| `GetGamingFanTable` (no input) | — | `0x01` |

`GetGamingFanBehavior` answering *every* odd selector with the same `0x100`, and
`bit 3` setting flipping the answer to `0x4000`/`0x4100`, is the signature of an
echoed bitfield — the EC is reflecting selector bits, not reporting fans. A
sensor array would not behave this way.

**Consequences:**

1. **Temperatures do not come from this interface.** CPU temperature must come
   from `MSAcpi_ThermalZoneTemperature`; GPU from NVML/NVAPI (NitroSense's
   service uses `NvAPI_GPU_GetThermalSettings`, visible in its strings).
2. **Fan RPM is probably not exposed here.** The `100` on selectors 1/4 never
   moved through 70 s at 91 °C, so it is a configured setpoint or an index, not a
   tachometer. Treat live RPM as unavailable until proven otherwise.
3. **Fan control may still work** (`SetGamingFanBehavior`), but a write can no
   longer be verified by reading RPM back. Verification must be behavioural:
   write, then confirm `GetGamingFanBehavior` / `GetGamingProfile` reflects the
   new state, and confirm thermal response separately.

### 4.2 Access requirements are wider than expected

`MSAcpi_ThermalZoneTemperature` instance enumeration is **also admin-only** on
this machine — `Get-CimInstance`, `System.Management` and our COM client all fail
with `Access denied` as a normal user, and all succeed elevated. Class metadata
stays readable either way, which is what makes this confusing to discover.

Net effect: **the whole tool needs elevation**, not just the fan writes.

### 4.3 Operating mode — solved (M2)

`GetGamingMiscSetting` is where the live state actually lives. Its valid
selectors are sparse:

| Selector | Value seen | Meaning |
|---|---|---|
| `0x07` | `0xFF` (255) | constant so far — likely "auto" / unlimited |
| `0x08` | `1` | constant so far |
| `0x0A` | `83` | constant under load — **not** a temperature (see below) |
| **`0x0B`** | **0 / 1 / 4 / 6** | **operating mode** |

Mode mapping, established by correlating a user-driven switch in NitroSense with
both our recorder and NitroSense's own log:

| Mode | Value | Evidence |
|---|---|---|
| Quiet | **0** | switched Performance→Balanced→Quiet, `0x0B` went 4→1→0 |
| Balanced | **1** | same sequence (the app calls this `DEFAULT` internally) |
| Performance | **4** | same sequence |
| ECO | **6** | automatic, not user-selectable — see below |

There is **no Turbo** on this model: the app exposes only Quiet, Balanced and
Performance, and the model profile carries `ModeSwitchKey: false`. An earlier
draft of these notes guessed `6` was Turbo; that was wrong.

`6` is ECO, the mode the Acer service applies on power-source change. Every one
of the 82 occurrences in the log follows the same pattern:

```
LockMode: AdaptorChange, newMode: ECO, oldMode: DEFAULT
[FUNCRW] setModeWithoutScenario_And_RefreshUI ===== getData(): 1, mode: 6
setDataWithoutScenario: OPERATING_MODE,v:6
```

That matches `"ACDCModeSet": true` in `win32\Nitro\Nitro ANV15-51.json`: pick up
the charger or drop to battery and the mode changes itself. Any replacement must
either reproduce this or explicitly opt out — leaving it unimplemented means the
OS/EC will still switch underneath us and `nitro mode` will report ECO.

All `OPERATING_MODE` values across the log's history are drawn from
`{0, 1, 4, 6}` — three user-selectable modes plus ECO.

**Correction to §4.1:** the earlier conclusion that "the Acer interface carries
no live state" was too strong. The Phase-1 probe set covered `Profile`,
`FanBehavior`, `FanSpeed`, `SysInfo` — none of which change with mode — and
missed `MiscSetting` entirely. The interface does carry live state; it just does
not carry **temperatures**, which is the claim that actually holds.

`0x0A` was tested as a temperature candidate and rejected: it stayed at `83`
through a 60 s load cycle while the ACPI zone moved 85–91 °C.

## 4.4 Verdict on fan RPM

Three independent routes checked, all closed on this model:

1. `GetGamingFanSpeed` — constant `100` under load, and responds to selector
   *bit patterns* rather than indices (see §4.1).
2. `nvmlDeviceGetFanSpeed` — returns `N/A`; the chassis fans are on the EC, not
   the GPU.
3. No other Acer getter exposes a tachometer value.

Fan RPM is therefore **not obtainable** through any interface investigated.
Treat it as unavailable rather than leaving the question open; verification of
fan writes must be behavioural (§Phase 2 in the plan).

### Captured samples (idle, 2026-11-15)

| Call | Raw | Reading |
|---|---|---|
| `GetGamingProfile(0)` | `0x00FF000001000000` | unknown: `0xFF` marker + `0x01` field |
| `GetGamingProfile(1..40)` | `0x0000000000000002` | unsupported |
| `GetGamingFanBehavior(1)` | `0x0000000000000100` | value 1, status ok |
| `GetGamingFanBehavior(3)` | `0x0000000000000100` | value 1, status ok |
| `GetGamingFanBehavior(0,2,4)` | `0x0000000000000001` / `...0002` | unsupported / error |
| `GetGamingFanSpeed(1)` | `0x0000000000006400` | value 100 |
| `GetGamingFanSpeed(4)` | `0x0000000000006400` | value 100 |
| `GetGamingFanSpeed(2,3)` | `0x0000000000000001` | unsupported |
| `GetGamingSysInfo(0)` | `0x0000000227000000` | unknown; `0x27` (39) plausible temp |
| `GetGamingSysInfo(2)` | `0x0000010000000000` | unknown |
| `GetGamingSysInfo(3,4)` | `0x0000000000000002` | unsupported |

For reference, `MSAcpi_ThermalZoneTemperature` read **48.1 °C** while
`GetGamingSysInfo(0)` was captured, so `0x27` is *not* the CPU package temp —
another field, or a different unit.

Fan speed of `100` on selectors 1 and 4 may be a percentage setpoint or an index,
not RPM. Unresolved — that is milestone M1.

---

## 5. Profile names

From `AcerHardwareService.exe` strings: `Quiet`, `Balanced`, `Performance`,
`Turbo`, `Default`. Also present: `Current OP mode = %d, Enabled = %d, Turbo key = %d`.

The mapping from these names to `SetGamingProfile`'s u64 payload is **not yet
established** — that is milestone M3.

---

## 6. Named-pipe alternative (unused, documented for completeness)

If we ever want to avoid admin without writing a service, Acer's own pipe
protocol is the route. Prefixes seen: `predatorsense_hardware_service_`,
`predatorsense_agent_service_`. Command enum names appear in the binary as
`kSvcCmdWMIGetGamingFanGroupSpeed`, `kSvcCmdWMISetGamingFanBehavior`,
`kSvcCmdWMIGetGamingSystemInformation`, etc. — names visible, **values not yet
recovered**. Depends on Acer's service remaining installed, which conflicts with
the goal of removing that stack.

---

## 7. Reproduction

```powershell
# class metadata (no elevation needed)
Get-CimClass -Namespace root\WMI -ClassName AcerGamingFunction |
  ForEach-Object { $_.CimClassMethods }

# instances (ELEVATION REQUIRED)
Get-CimInstance -Namespace root\WMI -ClassName AcerGamingFunction

# invoke on the instance, not the class
$i = Get-CimInstance -Namespace root\WMI -ClassName AcerGamingFunction
Invoke-CimMethod -InputObject $i -MethodName GetGamingProfile -Arguments @{ gmInput = [uint32]0 }

# or via the Rust client
nitro status
```

## 8. Artifacts

Raw captures and string dumps are in `C:\DEV\nitro-re\` on the analysis machine
(`strings-node.txt`, `sweep.log`, `instance-probe.log`, `status.out`). Not part of
the repo.
