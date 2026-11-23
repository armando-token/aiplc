# Informe Nocturno — Campaña Desatendida Gates 1 & 2 (Perfil Silencioso + Silicio ARMv7)

**Rama**: `feat/night-gates-1-2` (base `feat/plc-automatico-v1` en `9dabc4d`)  
**Run ID de Campaña**: `night-lyra-v1`  
**Objetivo hardware**: Luckfox Lyra (`armv7l`, Linux `6.1.99 #14 SMP PREEMPT`, 3 núcleos Cortex-A7 `600 MHz–1.512 GHz`, governor `ondemand`)  
**Servicio stock de planta (`lyra-plc.service`)**: **Intacto y `active` en todo momento** (`0` interrupciones, `/userdata/lyraplc` sin modificar)

> [!IMPORTANT]
> Perfil Nocturno Silencioso verificado en cada fase:
> - `io_mode = virtual` (`VirtualBenchDriver`, `0` aperturas de `/dev/gpiochip*`)
> - `serial_backend = none` (`0` aperturas de `/dev/ttyS*`)
> - `lcd_enabled = false` (`--no-lcd` + `--silent-night`, sin `DisplayBridge` ni hilo `lcd-bridge`)
> - Arranque por defecto desarmado (`armed = false`) en baseline/carga/recuperación; solo fixtures explícitos de falla usan `--auto-arm` sobre E/S virtual.
> - Nota transparente sobre métricas: el criterio `< 0.5 ms` p99 / `< 2.0 ms` max aplica directamente a **`wakeupJitterNs`** respecto a la línea base previa (`p99 = 1.023 ms`, `max = 2.195 ms` en `shadow-load`). El desacoplamiento alcanzó `wakeupJitter` **`p99 = 0.068 ms` (`max = 0.106 ms`)** en 10 min y **`p99 = 0.176 ms` (`max = 1.324 ms`)** en 3 min bajo carga API+Shadow+swaps, mientras `cycleDur` total (que incluye ida y vuelta IPC + evaluación WAMR AoT en Cortex-A7 a `600–1200 MHz` bajo governor `ondemand`) registró `p99 = 0.564 ms` (`baseline-10m`) y `p99 = 0.940 ms` (`load-3m`), con `0` `deadlineMisses` (corte `5.0 ms`) y `0` `missedReleases` (periodo `20.0 ms`).

---

## 1. Matriz de Gates (Implementación → Host → PLC → Métricas → Estado)

| Gate | Implementación clave | Prueba Host | Prueba PLC Real (`luckfox-usb`) | Métricas clave observadas | Estado |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **N0 (Silencio & Sandbox)** | `--no-lcd`, `--silent-night`, `IsolatedWorkerEngine`, `strict_sandbox_satisfied` | `n1_1_n1_2_n1_4` + `gate2_silent_night` (`PASS`) | `N0_smoke` con `/proc/<pid>/{fd,status,cgroup}` | `Uid=65534`, `Seccomp=2`, `cgroup=/plc-worker-active-g1-1`, `0` FDs GPIO/serial/LCD | **PASS** |
| **Gate 1 — N1.1** | IPC `UnixStream` no bloqueante (`tx_frame_buf`, `POLLOUT`/`POLLIN`, `eval_until`, `tainted`) | `ipc_write_backpressure...` + `n1_1_n1_2_n1_4` (`PASS`) | `N1_1_slow_deadline` (`--fault-slow-ms 80`, corte `5 ms`) | `fallbackLatched=true`, `activeWorkerPid=null`, `appliedMask=0`, `missedReleases=0` | **PASS** |
| **Gate 1 — N1.2** | `WorkerReaper` fuera del lazo RT, EOF parcial & contención `memory.max` cgroup v2 | `ipc_partial_eof...` + `n1_1_n1_2_n1_4` (`PASS`) | `N1_2a_oom` (`8 MiB`) + `N1_2b_partial_eof` (`PASS`) | Worker contenido/cosechado sin zombies; supervisor vivo en estado seguro | **PASS** |
| **Gate 1 — N1.3** | `AsyncShadowRunner` desacoplado (`--fault-diverge`, `ShadowMetrics`) | `n1_3_n1_5_async_shadow...` (`PASS`) | `N1_3_shadow_diverge` (`M2` diverge vs `M1`) | `scansExecuted=16`, `mismatches=16`, `firstCounterexample="scan_id=5 active_mask=0 shadow_mask=1"`, `fallbackLatched=false` | **PASS** |
| **Gate 1 — N1.4** | Política Linux estricta (`cgroup2 + seccomp + nobody 65534`) + `bpf` syscalls denegadas | `strict_sandbox_policy_rejects...` (`PASS`) | `N1_4_seccomp_escape` (`--fault-escape` en ARMv7) | `escape_blocked:socket_rc=-1,socket_err=1,open_rc=-1,open_err=1` (`EPERM=1` kernel ARMv7) | **PASS** |
| **Gate 1 — N1.5** | Canal acotado profundidad 1 `try_send` (`shadow_dropped`) en `AsyncShadowRunner` | `n1_3_n1_5_async_shadow...` (`PASS`) | `N1_5_shadow_hang` (`--fault-hang`, `shadowDeadlineMs=80`) | `scanDelta=19`, `missedReleases=0`, `shadow.dropped=2`, `shadow.errors=1` | **PASS** |
| **Gate 2 — Baseline 10 min** | Dominio RT (`scan-rt` `SCHED_FIFO 50`) + `NightTelemetryAggregator` + `StorageWriter` | `gate2_decoupled_concurrent...` (`PASS`) | `gate2-baseline-10m` (`600 s`, `30,002 scans`) | `wakeupJitter p99=68 µs (0.068 ms)`, `max=106.079 µs`; `cycleDur p99=564 µs`, `max=1.378 ms`; `0` drops | **PASS (`wakeupJitter`), `cycleDur` `0.564 ms` p99** |
| **Gate 2 — Carga 3 min** | Pool HTTP acotado (`429 BUSY`), swaps O(1) en frontera, rollback cálido y WAL off-scan | `gate2_decoupled_concurrent...` (`PASS`) | `gate2-load-3m` (`185 s`, `9,251 scans`, `10,858` req HTTP, 3 ciclos swap+rollback, Shadow vivo) | `wakeupJitter p99=176 µs`, `max=1.324 ms`; `ramSwap p50=248 µs`; `journalPersist max=47.4 ms` (`0` misses!) | **PASS (`0` misses, `0` drops)** |
| **Gate 2 — WAL & `SIGKILL`** | `BoundaryEpochTracker(boot_id, rev, lease_epoch)`, propagación `503 STORAGE_DEGRADED`, recuperación única en `main.rs` | `gate2_delayed_wal_supersede...` + `gate2_silent_night...` + `d03_real_sigkill...` (`PASS`) | `caseA_crash_before_persisted` (`_exit(99)`) + `caseB_sigkill_after_stable` (`kill -9`) | `caseA`: `rev=1 armed=false needsOperatorReview=true`; `caseB`: `rev=2 armed=false needsOperatorReview=false activePersisted=true` | **PASS** |
| **Gate 2 — Estabilidad 24 h** | Daemon detached (`setsid`), puerto aislado `127.0.0.1:18445`, `stop_24h.sh` por PGID | N/A | `/userdata/plc-auto/night/night-lyra-v1-24h` (`07:48:11`, detención limpia a solicitud) | `1,404,535 scans`, `0` misses/drops, `wakeupJitter p99=76 µs / max=593 µs`, `cycleDur p99=618 µs / max=2.363 ms`, `worker RSS=2,232 KiB` (`0` fuga), `lyra-plc=active` | **DETENIDO PARCIAL (07:48:11 / 32.5%)** |
| **Gate 3 (iPad)** | [Hoja Gate 3](file:///Users/armandosilva/Projects/PLC_Lucky/docs/verification/GATE3_IPAD_ACCESS_SHEET.md) | — | Prueba presencial con iPad en LAN/tailnet | — | **PENDIENTE** |
| **Gate 4 (Instrumentos)** | [Hoja Gate 4](file:///Users/armandosilva/Projects/PLC_Lucky/docs/verification/GATE4_INSTRUMENTS_PHYSICAL_IO_SHEET.md) | — | Prueba presencial con relé, THT02 y osciloscopio | — | **PENDIENTE** |
| **Gate 5 (Voz Offline)** | [Hoja Gate 5](file:///Users/armandosilva/Projects/PLC_Lucky/docs/verification/GATE5_OFFLINE_VOICE_SHEET.md) | — | Prueba presencial ASR offline → propuesta firmada | — | **PENDIENTE** |

---

## 2. Hallazgo Real Detectado en Silicio ARMv7 y Regresión Cubierta

Durante la primera ejecución de `n2_recovery` (`caseA_crash_before_persisted`) en la placa real, la recuperación tras `_exit(99)` en `AFTER_VOLATILE_BEFORE_PERSISTED` devolvió inesperadamente `needsOperatorReview: false` en el segundo arranque.  
- **Causa raíz**: [`main.rs`](file:///Users/armandosilva/Projects/PLC_Lucky/crates/supervisor/src/main.rs) invocaba `recover_on_boot(boot_id)` dos veces con el mismo `boot_id` cuando arrancaba con bloque de bootstrap: la primera llamada promovía `ActiveVolatile -> RecoveryRequired` bajo `boot_id` (y marcaba `needs_operator_review=true` antes de descartarse), y la segunda llamada encontraba el registro ya en `RecoveryRequired` con `rec.boot_id == boot_id`, limpiando `needs_operator_review` a `false`.
- **Corrección y regresión**: Se corrigió `main.rs` para ejecutar `recover_on_boot(boot_id)` una sola vez (solo sembrando `--initial-pkg` cuando `last_stable_package.is_none()` y preservando `needs_operator_review`), se añadió un test real multi-proceso Boot #1 → `_exit(99)` → Boot #2 en [`night_gates_tests.rs`](file:///Users/armandosilva/Projects/PLC_Lucky/crates/supervisor/tests/night_gates_tests.rs), y se re-verificaron en Luckfox tanto `n0_n1` como `n2_recovery` (`caseA` y `caseB`) y el lanzamiento 24 h.

---

## 3. Métricas Separadas de Gate 2 en Luckfox ARMv7

### 3.1 Baseline 10 minutos (`gate2-baseline-10m`, `30,002` scans @ 20 ms)
- **Archivo fuente**: [baseline summary.json](file:///Users/armandosilva/Projects/PLC_Lucky/docs/benchmarks/artifacts/night/night-lyra-v1/gate2-baseline-10m/summary.json), [samples.csv](file:///Users/armandosilva/Projects/PLC_Lucky/docs/benchmarks/artifacts/night/night-lyra-v1/gate2-baseline-10m/samples.csv)
- `wakeup_jitter_ns`: p50 = `44,000 ns`, p95 = `56,000 ns`, **p99 = `68,000 ns` (`0.068 ms`)**, p99.9 = `82,000 ns`, **max = `106,079 ns` (`0.106 ms`)**
- `eval_ipc_ns`: p50 = `418,000 ns`, p95 = `482,000 ns`, p99 = `506,000 ns`, p99.9 = `554,000 ns`, max = `714,583 ns`
- `publish_ns`: p50 = `20,000 ns`, p95 = `22,000 ns`, p99 = `24,000 ns`, p99.9 = `64,000 ns`, max = `933,917 ns`
- `cycle_dur_ns`: p50 = `468,000 ns`, p95 = `538,000 ns`, p99 = `564,000 ns`, p99.9 = `614,000 ns`, max = `1,378,417 ns`
- `deadlineMisses = 0`, `missedReleases = 0`, `telemetryDropped = 0`, `shadowDropped = 0`
- Memoria y SoC (`scan 1` → `scan 30,000`): Supervisor `RSS 3,868 → 5,060 KiB` (`PSS 3,997 KiB`), Worker `RSS 2,232 → 2,232 KiB` (`PSS 1,228 KiB`), `MemAvailable 353,540 → 350,556 KiB`, `temp_soc_mc = 51,732` (`51.7 °C`)
- **Primer contraejemplo > 0.5 ms en `cycleDur`** (capturado automáticamente en `firstJitterCounterexample`):  
  `scanId=2`, `wakeupJitterNs=64,629` (`0.065 ms`), `evalIpcNs=516,541` (`0.517 ms`), `publishNs=16,042`, `cycleDurNs=562,625` (`0.563 ms`), `deadlineMiss=false`, `activeWorkerPid=3237`, `memAvailableKb=353,540`, `tempSocMc=51,732`.

### 3.2 Carga Concurrente 3 minutos (`gate2-load-3m`, `9,251` scans @ 20 ms + API + Shadow + 3 Swaps/Rollbacks)
- **Archivo fuente**: [load summary.json](file:///Users/armandosilva/Projects/PLC_Lucky/docs/benchmarks/artifacts/night/night-lyra-v1/gate2-load-3m/summary.json), [api_load_stats.json](file:///Users/armandosilva/Projects/PLC_Lucky/docs/benchmarks/artifacts/night/night-lyra-v1/gate2-load-3m/api_load_stats.json), [events.jsonl](file:///Users/armandosilva/Projects/PLC_Lucky/docs/benchmarks/artifacts/night/night-lyra-v1/gate2-load-3m/events.jsonl)
- Tráfico HTTP concurrente: **`10,858` respuestas `200 OK`** (`/state`, `/scans`, `/virtual-inputs`), `busy429 = 0`, `err = 6` (solo durante el cierre del segundo `185`)
- Promociones reales ejecutadas: `3` hot-swaps (`M1 → M2`) + `3` warm rollbacks (`M2 → M1`, `warmRollbackCount = 3`)
- `wakeup_jitter_ns`: p50 = `60,000 ns`, p95 = `94,000 ns`, **p99 = `176,000 ns` (`0.176 ms`)**, p99.9 = `286,000 ns`, **max = `1,324,291 ns` (`1.324 ms`)**
- `eval_ipc_ns`: p50 = `414,000 ns`, p95 = `578,000 ns`, p99 = `708,000 ns`, p99.9 = `912,000 ns`, max = `3,384,209 ns`
- `publish_ns`: p50 = `12,000 ns`, p95 = `30,000 ns`, p99 = `316,000 ns`, p99.9 = `1,325,000 ns`, max = `5,964,292 ns`
- `cycle_dur_ns`: p50 = `478,000 ns`, p95 = `692,000 ns`, p99 = `940,000 ns`, p99.9 = `2,650,000 ns`, max = `8,516,376 ns`
- `ram_swap_ns` (6 eventos): p50 = `248,000 ns` (`0.248 ms`), max = `1,124,958 ns` (`1.125 ms`)
- `journal_persist_ns` (off-scan `eMMC fsync`): último = `38,588,170 ns` (`38.6 ms`), máximo = `47,397,713 ns` (`47.4 ms`) — **con `0` `missedReleases` ni `0` `deadlineMisses` en el lazo RT**
- `telemetryDropped = 0`, `shadowDropped = 0`
- Memoria y SoC finales (`scan 9,200`): Supervisor `RSS 13,100 KiB` (`PSS 11,974 KiB`), Worker `RSS 2,232 KiB` (`PSS 752 KiB`), `MemAvailable 337,796 KiB`, `temp_soc_mc = 57,673` (`57.7 °C`)
- **Primer contraejemplo > 0.5 ms en `cycleDur`**: `scanId=1`, `wakeupJitterNs=38,668`, `evalIpcNs=460,250`, `publishNs=15,167`, `cycleDurNs=502,834`. Picos aislados hasta `8.516 ms` (`scan 3876`) ocurrieron dentro del presupuesto de `20 ms` sin perder releases (`missedReleases=0`), atribuibles a contención del planificador Linux no-`PREEMPT_RT` con tráfico API/eMMC/worker y governor `ondemand`.

---

## 4. Artefactos Compilados (ARMv7 Release SHA-256) y Reproducibilidad

| Binario (`armv7-unknown-linux-gnueabihf.2.36`) | SHA-256 | Uso / Alcance |
| :--- | :--- | :--- |
| `plc-supervisor` (pre-fix arranque) | `595d10d38804dc0ce1a327a1d241480df570e7a4520193b4cbef889d3bf06569` | Usado en `gate2-baseline-10m` y `gate2-load-3m` (lazo RT, `runtime.rs`, `storage.rs`, `worker_pool.rs` idénticos) |
| `plc-supervisor` (final post-fix `main.rs`) | `9e6ad9cbe1b581d9f484c3d4e88e1094a24f4ab67f2f9051c94cc03cafcdac32` | Re-verificado en `N0`, `N1.1`–`N1.5`, `n2_recovery` (`caseA`/`caseB`) y activo en corrida `24h` |
| `plc-worker` | `93a8e33be8af6c6ed82faaa5a09e87136e0838d2e58ce62111a11e263aec4e36` | Bit-idéntico en todas las fases ARMv7 (`WAMR 2.4.6 AoT`, `Seccomp`, `nobody`, `cgroup v2`) |
| `plc-bench` | `9a511e8fde9f5baf2a1c67526e9fe215bbc6c0b306fb51482d66f2248e744c4e` | Compilado en workspace ARMv7 |

> [!NOTE]
> Nota de build reproducible: `third_party/` está excluido del repositorio Git por `.gitignore` y contiene las bibliotecas estáticas precompiladas (`libvmlib.a` de WAMR 2.4.6 y `libquickjs.a` de QuickJS-NG v0.18.0) necesarias para compilar `logic-worker` en clones limpios.  
> Comando de compilación cruzada usado:  
> `CARGO_BUILD_JOBS=2 cargo zigbuild --release --target armv7-unknown-linux-gnueabihf.2.36 --workspace`

---

## 5. Estado de la Corrida Silenciosa 24 Horas (`DETENIDO PARCIAL`)

Detenida limpiamente a solicitud del usuario mediante `stop_24h.sh` (`PGID = 2270` terminado; `0` procesos `plc-supervisor`/`plc-worker` activos; puerto `127.0.0.1:18445` cerrado; `lyra-plc.service = active` intacto) para retomar más tarde.

- **Estado y avance alcanzado**: **`DETENIDO_PARCIAL`** — **`07:48:11`** (`28,091 s`, **`32.51 %`** del objetivo de `24 h`)
- **Ventana Host**: `2026-11-23 01:30:00 PST` (`2026-11-23T09:30:00Z`) → **`2026-11-23 09:17:38 PST`** (`2026-11-23T17:17:38Z`)
- **Ventana Reloj PLC (UTC)**: `2026-04-27T23:24:57Z` → **`2026-04-28T07:12:33Z`**
- **Procesos ejecutados (ya detenidos)**: Supervisor `PID 2270` (`PGID = 2270`), Worker `PID 2275` (`WAMR AoT`, `nobody`, `Seccomp=2`)
- **Contadores finales (`1,404,535` scans @ `20 ms`)**:
  - `scansTotal = 1,404,535`
  - `deadlineMisses = 0`, `missedReleases = 0`, `telemetryDropped = 0`, `shadowDropped = 0`, `warmRollbackCount = 0`
  - `fallbackLatchCount = 1`: ocurrió exclusivamente en `scanId = 1,404,534` durante la parada controlada (`stop_24h.sh` → `kill -TERM -2270`) cuando `plc-worker` cerró su tubería IPC (`Broken pipe, os error 32`) milisegundos antes de que `plc-supervisor` terminara; **`0` fallos durante toda la ventana operativa de `07:48:11`**.
- **Latencias finales acumuladas**:
  - `wakeup_jitter_ns`: p50 = `42,000 ns`, p95 = `58,000 ns`, **p99 = `76,000 ns` (`0.076 ms`)**, p99.9 = `104,000 ns`, **max = `592,585 ns` (`0.593 ms`)**
  - `eval_ipc_ns`: p50 = `420,000 ns`, p95 = `508,000 ns`, p99 = `564,000 ns`, p99.9 = `616,000 ns`, max = `985,542 ns`
  - `publish_ns`: p50 = `16,000 ns`, p95 = `20,000 ns`, p99 = `56,000 ns`, p99.9 = `84,000 ns`, max = `1,926,750 ns`
  - `cycle_dur_ns`: p50 = `466,000 ns`, p95 = `560,000 ns`, **p99 = `618,000 ns` (`0.618 ms`)**, p99.9 = `676,000 ns`, **max = `2,363,084 ns` (`2.363 ms`)**
- **Memoria y SoC (`scan 1` → `scan 1,404,500`)**:
  - **Worker (`plc-worker`)**: `RSS 2,232 → 2,232 KiB`, `PSS 1,226 → 1,230 KiB` (**completamente plano tras `1.40M` scans, `0` fugas**)
  - **Supervisor (`plc-supervisor`)**: `RSS 3,932 → 37,396 KiB` (`PSS 2,810 → 36,278 KiB`), `MemAvailable 359,144 → 325,032 KiB` (`~317 MiB` libres), `temp_soc_mc 54,702 → 48,168` (`48.2 °C`). El crecimiento lineal del RSS del supervisor proviene de `VirtualBenchState.history.push(...)` sin acotar en `crates/field-io/src/driver.rs` (solo driver virtual de benchmark, no afecta a `LyraHardwareDriver`).
- **Archivos archivados en el repositorio**:
  - [status run_24h_status.json](file:///Users/armandosilva/Projects/PLC_Lucky/docs/benchmarks/artifacts/night/night-lyra-v1/run_24h_status.json)
  - [summary.json](file:///Users/armandosilva/Projects/PLC_Lucky/docs/benchmarks/artifacts/night/night-lyra-v1/gate2-24h-partial/summary.json)
  - [samples.csv](file:///Users/armandosilva/Projects/PLC_Lucky/docs/benchmarks/artifacts/night/night-lyra-v1/gate2-24h-partial/samples.csv)
  - [events.jsonl](file:///Users/armandosilva/Projects/PLC_Lucky/docs/benchmarks/artifacts/night/night-lyra-v1/gate2-24h-partial/events.jsonl)
- **CSVs crudos pesados conservados en la Luckfox** (fuera de Git por tamaño):
  - `/userdata/plc-auto/night/night-lyra-v1-24h/metrics/gate2-24h/anomalies.csv` (`62,076,446` bytes, `1,404,534` líneas)
  - `/userdata/plc-auto/night/night-lyra-v1-24h/metrics/gate2-24h/scans.csv` (`8,088,541` bytes, `100,001` líneas)
- **Nota para retomar / relanzar mañana o más tarde**:
  - Antes de lanzar una nueva corrida completa de `24 h` en modo `virtual`, se recomienda acotar `VirtualBenchState.history` en `crates/field-io/src/driver.rs` para que también el RSS de `plc-supervisor` permanezca plano. Para relanzar en limpio basta iniciar una nueva corrida `--stop-after-s 86400` bajo un nuevo `run-id` en `/userdata/plc-auto/night/` sin tocar `lyra-plc.service`.
