# Engine Selection Record — Luckfox Lyra PLC (`ARMv7` vs `ARM64`)

## 1. Test Hardware & Platform Profiles
1. **Target PLC (`luckfox`)**: Luckfox Lyra PLC, SoC Rockchip RK3506 (3× ARM Cortex-A7 `armv7l`, `armhf`, glibc `2.36`), `464 MiB` total RAM (`~356 MiB` free during active runs), Linux `6.1.99 #14 SMP PREEMPT` (standard `PREEMPT`, NOT `PREEMPT_RT`), cgroup v2 (`cpuset/cpu/memory`, no `pids`), Seccomp-BPF (`AUDIT_ARCH_ARM`), no `NET_NS`/`PID_NS`/`USER_NS`.
2. **Host Workstation (`darwin-arm64`)**: Apple Silicon ARM64 macOS host used for cross-architecture comparison with Cranelift (`A3` Wasmtime) and local CI tests.
3. **RT Scheduler Jitter Baseline on Real PLC (`plc-bench jitter`, 500 scans @ 20 ms)**:
   - `jitter-idle`: `0` deadline misses; wakeup jitter `p50 = 38.4 us`, `p95 = 58.9 us`, `p99 = 63.8 us`, `max = 74.4 us`; peak PSS `2,498 KiB` (`2.44 MiB`); peak SoC temp `48.8 C`.
   - `jitter-stress` (2 CPU load threads): `0` deadline misses; wakeup jitter `p50 = 21.0 us`, `p95 = 40.1 us`, `p99 = 516.2 us`, `max = 940.0 us` (`0.94 ms` inside `20 ms` window); peak PSS `2,598 KiB` (`2.54 MiB`); peak SoC temp `50.0 C`.

## 2. Measured Engine Matrix on Real Luckfox Lyra PLC (`docs/benchmarks/artifacts/plc-lyra/`)
All runs executed inside isolated `plc-worker` child processes (`UID 65534 nobody`, Seccomp-BPF enabled, Linux cgroup v2 `/sys/fs/cgroup/plc-worker-<slot>` `32 MiB` limit, closed FDs) communicating over Unix domain sockets with `16 KiB` `PLCQ`/`PLCR` scan framing (`0` dropped off-scan telemetry samples). Deadline misses check both `eval_dur_ns > 10 ms` (`evalDeadlineMs = 10`) and full cycle `> 20 ms` (`scanPeriodMs = 20`). Note that in `shadow` and `shadow-load` modes, both active and shadow worker processes execute concurrently on the PLC, while `Peak PSS` / `cgroup Peak` reflect the active worker PID (`plc-bench` attaches telemetry to `active_client.pid()`).

| Run ID | Workload | Mode | Scans | Misses | Eval `p50` (us) | Eval `p95` (us) | Eval `p99` (us) | Eval `max` (us) | Peak PSS (KiB) | Cgroup Peak (KiB) | Min MemAvail (KiB) | Max SoC Temp (C) |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `a0-empty-W0-active` | W0 (empty noop) | active | 400 | 0 | 434.0 | 477.8 | 500.8 | 528.8 | 1126 | 344 | 364444 | 48.8 |
| `a0-ir-W0-active` | W0 (simple threshold) | active | 350 | 0 | 441.0 | 520.3 | 534.6 | 812.3 | 1123 | 344 | 364144 | 47.6 |
| `a1-interp-W0-active` | W0 | active | 350 | 0 | 508.1 | 548.0 | 596.8 | 874.4 | 1275 | 372 | 364964 | 47.0 |
| `a1-aot-W0-active` | W0 | active | 350 | 0 | 503.7 | 534.0 | 572.8 | 630.6 | 1284 | 384 | 364700 | 47.0 |
| `a2-quickjs-W0-active` | W0 | active | 350 | 0 | 870.0 | 953.8 | 971.2 | 1315.1 | 2103 | 492 | 364844 | 47.0 |
| `a0-ir-W1-active` | W1 (M1 10s TON) | active | 350 | 0 | 425.5 | 495.5 | 536.7 | 542.5 | 1127 | 352 | 364744 | 47.0 |
| `a1-interp-W1-active` | W1 | active | 350 | 0 | 495.2 | 546.3 | 591.5 | 658.3 | 1283 | 380 | 364708 | 47.0 |
| **`a1-aot-W1-active`** | **W1 (M1 10s TON)** | **active** | **350** | **0** | **483.6** | **530.8** | **539.9** | **545.7** | **1295** | **392** | **364120** | **47.0** |
| `a2-quickjs-W1-active` | W1 | active | 350 | 0 | 774.4 | 1015.3 | 1078.9 | 1347.2 | 2114 | 500 | 363788 | 47.0 |
| `a1-aot-W1-shadow` | W1 | shadow | 350 | 0 | 806.2 | 999.2 | 1026.7 | 1162.9 | 785 | 392 | 363952 | 47.0 |
| `a2-quickjs-W1-shadow` | W1 | shadow | 350 | 0 | 1549.6 | 1804.0 | 1921.5 | 2057.7 | 1252 | 504 | 364000 | 47.0 |
| `a0-ir-W1-shadow-load` | W1 | shadow+load | 350 | 0 | 555.9 | 664.4 | 1044.2 | 1867.0 | 688 | 356 | 364484 | 50.0 |
| `a1-interp-W1-shadow-load` | W1 | shadow+load | 350 | 0 | 651.9 | 924.3 | 1224.1 | 1978.4 | 776 | 376 | 363736 | 50.0 |
| **`a1-aot-W1-shadow-load`** | **W1** | **shadow+load** | **350** | **0** | **613.7** | **849.0** | **1039.5** | **2844.6** | **788** | **392** | **364384** | **50.5** |
| `a2-quickjs-W1-shadow-load` | W1 | shadow+load | 350 | 0 | 1053.8 | 1248.9 | 1741.5 | 2401.6 | 1255 | 504 | 363984 | 51.1 |
| `a0-ir-W2-active` | W2 (M2 TON+pulse) | active | 350 | 0 | 447.4 | 503.7 | 531.7 | 557.4 | 1162 | 380 | 364088 | 47.0 |
| `a1-interp-W2-active` | W2 | active | 350 | 0 | 402.8 | 456.2 | 499.0 | 509.0 | 1315 | 412 | 364536 | 47.0 |
| `a1-aot-W2-active` | W2 | active | 350 | 0 | 455.3 | 534.3 | 549.5 | 605.8 | 1327 | 424 | 364072 | 47.0 |
| `a2-quickjs-W2-active` | W2 | active | 350 | 0 | 833.3 | 898.6 | 1010.0 | 1147.1 | 2131 | 520 | 364764 | 47.0 |
| `a0-ir-W3-active` | W3 (32 sig/16 timer) | active | 350 | 0 | 588.9 | 630.9 | 663.2 | 690.7 | 1646 | 868 | 363604 | 47.0 |
| `a1-interp-W3-active` | W3 | active | 350 | 0 | 622.4 | 687.2 | 711.4 | 784.0 | 1791 | 896 | 364140 | 47.0 |
| **`a1-aot-W3-active`** | **W3 (32 sig/16 timer)** | **active** | **350** | **0** | **634.7** | **748.7** | **777.3** | **871.5** | **1823** | **920** | **363248** | **47.0** |
| **`a2-quickjs-W3-active`** | **W3 (32 sig/16 timer)** | **active** | **350** | **0** | **3732.8** | **3976.0** | **4095.3** | **4260.1** | **2463** | **856** | **364184** | **47.0** |
| `a0-ir-W4-active` | W4 (5s PRESERVE_ELAPSED + pulse) | active | 350 | 0 | 475.1 | 515.7 | 546.9 | 845.5 | 1167 | 388 | 364656 | 47.0 |
| `a1-interp-W4-active` | W4 | active | 350 | 0 | 416.5 | 465.8 | 502.0 | 563.5 | 1322 | 416 | 365076 | 47.0 |
| `a1-aot-W4-active` | W4 | active | 350 | 0 | 382.4 | 445.7 | 521.2 | 907.1 | 1331 | 428 | 364516 | 47.0 |
| `a2-quickjs-W4-active` | W4 | active | 350 | 0 | 763.3 | 1052.6 | 1075.1 | 1382.5 | 2134 | 520 | 364596 | 47.0 |
| `a0-ir-W5-active` | W5 (TON + CTU proxy; continuous PID/CBF in `@plc-auto/simulator`) | active | 350 | 0 | 461.1 | 521.2 | 541.6 | 603.8 | 1159 | 380 | 364500 | 47.0 |
| `a1-interp-W5-active` | W5 | active | 350 | 0 | 525.3 | 593.8 | 618.0 | 754.5 | 1315 | 412 | 364084 | 47.0 |
| `a1-aot-W5-active` | W5 | active | 350 | 0 | 400.8 | 529.4 | 550.7 | 593.8 | 1327 | 424 | 364424 | 47.0 |
| `a2-quickjs-W5-active` | W5 | active | 350 | 0 | 844.1 | 961.0 | 999.8 | 1137.8 | 2134 | 524 | 363812 | 47.0 |

## 3. Host ARM64 Comparison Including `A3` Wasmtime (`docs/benchmarks/artifacts/host-darwin-arm64/`)
On 32-bit `armv7-unknown-linux-gnueabihf`, `cranelift-codegen v0.114.0` panics at build time (`"no supported isa found for arch armv7"`), whereas `wasmtime-engine` compiles and runs on 64-bit targets (`darwin-arm64` tested, `aarch64-linux-gnu.2.36` cross-built cleanly):

| Host Engine (`W1`, 250 scans) | Misses | Eval `p50` (us) | Eval `p95` (us) | Eval `p99` (us) | Eval `max` (us) |
| --- | ---: | ---: | ---: | ---: | ---: |
| `a0-ir-W1-active` | 0 | 90.0 | 125.0 | 148.0 | 172.0 |
| `a1-interp-W1-active` | 0 | 97.0 | 166.0 | 210.0 | 402.0 |
| **`a1-aot-W1-active`** | **0** | **96.0** | **135.0** | **160.0** | **221.0** |
| `a2-quickjs-W1-active` | 0 | 124.0 | 180.0 | 264.0 | 1617.0 |
| `a3-wasmtime-W1-active` | 0 | 98.0 | 143.0 | 178.0 | 264.0 |

## 4. Accelerated 1,000-Swap Soak on PLC eMMC (`soak-1000`)
- Executed `1,000` alternating M1 <-> M2 promotions & rollbacks (`1,001` lines / all `1,000` scans recorded in `scans.csv`) with `A1-aot` and CRC32 WAL (`DurableJournal` `fsync` + `sync_dir` on RK3506 eMMC): `finalRevision = 1001`, `peakPssKb = 19213` (`18.76 MiB`), `peakRssKb = 20064`, `minMemAvailableKb = 348752` (`340.6 MiB` free), `maxTempSocMc = 49356` (`49.4 C`), `swapLatencyNs p50 = 17.88 ms`, `p95 = 21.46 ms`, `p99 = 25.20 ms`, `max = 40.93 ms`.
- **Harness disclosure**: The accelerated soak uses `VirtualClock` (`100 ms` per step) and resets the deployment rollback anti-flapping counter (`rearm_operator()`, bypassing production `max_faults_before_latch = 3`) solely so 500 consecutive M2 -> M1 rollbacks can execute automatically; `swapLatencyNs` includes synchronous WAL `fsync` and inline engine warm inside the loop, whereas in live scan operation `standby_engine` pre-warms off-scan and journal persistence completes at the transition boundary.

## 5. Final Architectural Selection (`R01`)
- **Primary Selected Engine: `A1-aot` (`WAMR 2.4.6` AoT)** with `A0` (`native-ir`) as deterministic reference verifier and **`A2` (`QuickJS-NG v0.18.0`)** kept as secondary script/fallback option:
  1. **ARMv7 & ARM64 support**: Works natively on Luckfox RK3506 ARMv7 Cortex-A7 (`wasmtime`/`A3` does not support 32-bit `armv7`).
  2. **Predictable tail well within the `10 ms` `evalDeadlineMs` cutoff**: At `W3` (32 signals + 16 independent timers), `a1-aot` stays at `p50 = 634.7 us`, `p99 = 777.3 us`, `max = 871.5 us`, which is **5.27x faster at `p99`** (`4,095.3 / 777.3`) and **4.89x faster at `max`** (`4,260.1 / 871.5`) than `a2-quickjs` (`p99 = 4,095.3 us`, `max = 4,260.1 us`, leaving <1 ms margin under a 5 ms budget and >41% of the 10 ms `evalDeadlineMs` window), while using **26.0% less PSS** (`1,823 KiB` vs `2,463 KiB`).
  3. **Memory footprint & zero WASI**: `1,295 KiB` (`1.26 MiB`) PSS and `392 KiB` cgroup v2 `memory.peak` on `W1`, leaving `~355.6 MiB` (`364,120 KiB`) RAM free on the PLC with strong sandbox guarantees (`Seccomp-BPF`, `cgroup v2`, `UID 65534 nobody`).
