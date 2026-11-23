# Test Matrix & Verification Report (`H01–R01`)

> [!IMPORTANT]
> This report strictly distinguishes between **real physical hardware verification on the Luckfox Lyra PLC** (`luckfox`, RK3506 ARMv7, Linux `6.1.99`) and **simulated external bench peripherals** (RS-485 THT-02 slave simulated over OS PTY; third indicator channel `tower.blue` modeled as `VIRTUAL`).

## 1. Execution Scope Summary
- **Real PLC Hardware Executed (`ssh luckfox-usb`, `/userdata/plc-auto/`)**:
  - Physical GPIO chardev v2 on `/dev/gpiochip0` lines `17` (`tower.red`) and `18` (`tower.green`)
  - Real Linux kernel `6.1.99` `cgroup v2` (`8 MiB` `memory.max` OOM killer), `Seccomp-BPF` (`AUDIT_ARCH_ARM`), `UID 65534 nobody`, `NO_NEW_PRIVS`, closed FDs
  - Real `SIGKILL` crash & eMMC WAL recovery on `plc-supervisor`
  - Real ARMv7 Cortex-A7 jitter & engine benchmarks (`A0`, `A1-interp`, `A1-aot`, `A2`, `W0–W5`)
  - 1,000-swap eMMC soak & live REST API + offline `/console/` + physical GPIO `M1 -> M2 -> Rollback` demo
  - Stock `/userdata/lyraplc` untouched; `lyra-plc.service` verified `active` after completion with GPIO `17/18` returned to `0` (`OFF`).
- **Simulated / Host Environments**:
  - **THT-02 Modbus RTU temperature/humidity sensor**: Real Modbus RTU FC03 byte frames and CRC-16 over a real Linux/macOS PTY (`/dev/pts/*`) because no physical RS-485 THT-02 sensor was wired.
  - **`tower.blue` (`maskBit=2`)**: Explicitly declared `physical: false` (`VIRTUAL`) because stock Luckfox Lyra PLC provides 2 physical relay outputs (`gpiochip0:17` and `18`).
  - **Engine `A3` (`wasmtime` Cranelift)**: Evaluated on macOS ARM64 (`host-darwin-arm64`) and cross-compiled for `aarch64-unknown-linux-gnu.2.36`; 32-bit `armv7` is unsupported upstream by Cranelift (`no supported isa found for arch armv7`).

## 2. Full Acceptance Matrix

| ID | Category | Method & Environment | Evidence Source | Result |
| --- | --- | --- | --- | --- |
| **H01** | Exclusive `/dev/gpiochip0` lines 17 & 18 + safe-off | Real PLC (`plc-bench hil --hardware-gpio`) | `docs/benchmarks/artifacts/plc-lyra/hil/hil_report.json` (`secondOpenBlocked: true`, `observedOnMask: 1`, `observedOffMask: 0`, `hardware: true`) | **PASS (Real HW)** |
| **H02** | Modbus RTU FC03, CRC-16, `0x7FFF` sentinel, `1500ms` TTL `GOOD->STALE` | Real PLC PTY (`hil_report.json`) + `field-io` suite (`4` tests) | `hil_report.json` (`goodValue: 452`, `faultQuality: Invalid`), `cargo test -p field-io` | **PASS (PTY Sim)** |
| **H03** | Capability/profile validation (`tower.blue` VIRTUAL, 32-signal cap, capabilities rejection) | Schema validation + `W3` 32-sig/16-timer run on PLC + Rust `worker_tests` | `control-ir` (`3` tests), `plc-lyra/a1-aot-W3-active/summary.json`, `console` (`2` tests) & `apps/ipad` (`4` tests) | **PASS (PLC + Host Tests)** |
| **F01** | Worker hang / eval deadline overrun | Real PLC (`plc-worker --fault-hang`) & host IPC test | `hil_report.json` (`F01 passed: true`), `logic-worker::ipc_worker_process_*` | **PASS (Real PLC)** |
| **F02** | Cgroup v2 `8 MiB` `memory.max` OOM containment | Real PLC Linux 6.1.99 (`plc-worker --fault-oom`) | `hil_report.json` (`cgroupApplied: true`, `oomConfirmed: true`, `oom_events: 2`, `peak_bytes: 8388608`, `current_bytes: 8192`) | **PASS (Real PLC Kernel)** |
| **F03** | Malformed/corrupted IPC frame rejection | Real PLC (`plc-worker --fault-bad-frame`) & host IPC test | `hil_report.json` (`F03 passed: true`), `worker_tests` | **PASS (Real PLC)** |
| **F04** | Failing shadow module isolated from active outputs | Supervisor `run_shadow_replay` & `ScanSupervisor` tests | `supervisor_tests` + `deploy.rs` shadow validation | **PASS** |
| **F05** | 500ms min dwell anti-chatter vs physical inhibit bypass priority | `supervisor_tests::f05_shield_dwell_chatter_and_contingency_off_priority` + post-dwell interlock recheck | `supervisor_tests` (`7 passed`) | **PASS** |
| **F06** | Seccomp-BPF + `UID 65534 nobody` + closed FDs block `socket()` & `open("/dev/gpiochip0")` | Real PLC (`plc-worker --fault-escape`) | `hil_report.json` (`seccomp_applied: true`, `uid_dropped: true`, `effective_uid: 65534`, `cgroup_applied: true`, `socket_rc=-1,socket_err=1`, `open_rc=-1,open_err=1`) | **PASS (Real PLC Kernel)** |
| **D01** | Stale `baseRevision` conflict (`409 Conflict`) | Supervisor HTTP API test | `supervisor_tests::d01_d02_d04_and_api_routes_work_end_to_end` | **PASS** |
| **D02** | Idempotent `txId` + reconnect reconciliation (`"verificando estado"`) | Supervisor API + Web Console + Swift iPad tests | `supervisor_tests`, `console.test.ts` (`2 passed`), `PlcAutoCoreTests` (`4 passed`) | **PASS** |
| **D03** | Abrupt crash (`AFTER_VOLATILE_BEFORE_PERSISTED` & real `SIGKILL`) starts disarmed on last stable rev | Real PLC eMMC (`hil_report.json`) + host binary subprocess tests | `hil_report.json` (`D03 recoveredRev: 1, startedDisarmed: true`), `supervisor_tests::d03_real_sigkill_exit_crash_injection` | **PASS (Real PLC eMMC)** |
| **D04** | Separate 4-stage evidence & witness discrepancy detection | Live PLC GPIO readback (`live_demo_result.json`) + Swift/Console schema & discrepancy tests | `live_demo_result.json` (`selectedInScan: true`, `driverApplied: "APPLIED"`, `physicalObservedMask: 0`, `activePersisted: true`), `supervisor_tests`, `PlcAutoCoreTests::testDecodesRealRustSupervisorStateWithEvidenceBundle` | **PASS (Real PLC)** |
| **S01** | §8.2 Golden trace `10s -> 5s` (`RESET` at 11.98s/12.0s & `PRESERVE_ELAPSED` at 9.48s/9.5s) | Rust Supervisor & TS Simulator parity | `supervisor_tests::golden_trace_*`, `simulator.test.ts` (`3 passed`) | **PASS** |
| **S02** | Reject incompatible `TON -> TOF` and duplicate `stableId` | `StateArena` migration planner | `supervisor_tests::s02_rejects_ton_to_tof_and_duplicate_stable_id` | **PASS** |
| **S03** | Rollback creates monotonic rev (`1 -> 2 -> 3`) across A1 & A2 QuickJS | Real PLC live demo (`A1` M1->M2->Rollback) & real PLC `S03-A2` QuickJS M1->M2->Rollback (`hil_report.json`) + host test | `live_demo_result.json` (`rev0: 1, revAfterM2: 2, revAfterRollback: 3`), `hil_report.json` (`S03-A2`: `m1Rev: 1, m2Rev: 2, rollbackRev: 3`), `supervisor_tests::s03_*` | **PASS (Real PLC)** |
| **V01** | Spanish intent normalizer across 120 cases (`dev`+`eval`+40+ ASR) | Automated corpus runner (`@plc-auto/intent`) validated against `site-profile.json` | `intent.test.ts`: **120/120 exact outcome matches, 0 false accepts, 45/45 `ACCEPT` ruleSets schema-validated** | **PASS** |
| **V02** | Offline `es-PE` speech service + disconnect TX reconciliation | `apps/ipad` Swift Package (`MockSpeechCaptureService` + `StubTransport` unit tests) | `swift test`: `4/4 passed` (mocked speech / HTTP stub scope) | **PASS (Mocked Unit Tests)** |
| **I01** | ST & PLCopen XML LD import with trace equivalence & offline SVG | Differential trace check vs `@plc-auto/simulator` | `iec.test.ts` (`I01` passed) | **PASS** |
| **I02** | Reject `%I/%Q`, `WHILE`, unknown calls, XXE `<!DOCTYPE>/<!ENTITY>`, duplicate coils | Negative security parser tests (`@plc-auto/iec-import`) | `iec.test.ts` (`I02` passed) | **PASS** |
| **R01** | Engine selection benchmarks (`W0–W5`, jitter, shadow, `A0–A3`, 1,000-swap soak) | Real PLC ARMv7 (`34` bench dirs + `soak-1000`) & macOS ARM64 host (`A0–A3`) + `aarch64-linux-gnu` cross-build | `docs/benchmarks/ENGINE_SELECTION_RECORD.md`, `0` deadline misses across all engine runs | **PASS (Real PLC A0–A2 + Host A3)** |
