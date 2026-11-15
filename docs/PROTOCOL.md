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
