# Operation and Recovery Runbook — Luckfox Lyra PLC

## 1. Safety Principles & Startup Guarantees
- **Always Start Disarmed**: Every supervisor boot or crash recovery starts with `control_armed = false` (`armed_on_boot = false`) and physical GPIO lines OFF (`0`). Physical outputs never energize until an operator explicitly triggers `POST /v1/devices/{id}/rearm` (or launches with `--auto-arm` in controlled maintenance/bench runs).
- **Isolated Hardware Boundaries**:
  - `lyra-bench-01` lives in `/userdata/plc-auto/{bin,profiles,packages,data,benchmarks,console}`.
  - Stock vendor PLC software in `/userdata/lyraplc` (`lyra-plc.service`) and kernel/boot partitions (`/boot`) MUST NEVER be overwritten or modified.
  - Because only ONE process can hold `/dev/gpiochip0` lines `17` & `18` cleanly via Linux GPIO chardev v2 (`EBUSY` otherwise), temporarily `systemctl stop lyra-plc.service` before running `plc-supervisor --hardware-gpio` or `plc-bench hil --hardware-gpio`, and restart `systemctl start lyra-plc.service` afterward.
  - Verify `tower.blue` (`bit 4`) is strictly **VIRTUAL** (`physical: false`, no wired GPIO relay on stock PCB).

## 2. Normal Deployment Workflow (Two-Step Human Confirmation)
1. **Stage & Shadow Verify (`Author` / `Engineer`)**: `POST /v1/devices/{id}/proposals` submits `{txId, package, shadowTrace?}` where `package` is a signed `ModulePackageV1` carrying `package.baseRevision`. Rejects `409 BASE_REVISION_CONFLICT` if `package.baseRevision` is stale (`D01`) or `409 IDEMPOTENCY_CONFLICT` if the same `txId` is re-submitted with a different body hash.
2. **Explicit Authorization (`Operator`)**: `POST /v1/proposals/{tx}/authorizations` supplies `operatorId`, `expectedPackageHash`, and bounded `ttlMs` after reviewing diff and Shadow result.
3. **Boundary Activation (`Operator`)**: `POST /v1/proposals/{tx}/activate` returns `409 CONTROL_DISARMED_OR_LATCHED` if disarmed or fault-latched; otherwise queues activation for the exact next scan boundary (`202 Accepted`, repeat is idempotent `200`).
4. **Abort Before Boundary**: `POST /v1/proposals/{tx}/cancel` transitions an armed transaction to `REJECTED` before scan swap.

## 3. Crash, Power-Loss & Network Recovery Procedures
### A. Power Cut / `SIGKILL` During Promotion (`D03`)
- WAL records in `<data-dir>/journal.wal` are CRC32-checksummed JSON lines written with `sync_all()` + directory fsync (`atomic_write_synced` for immutable packages); incomplete trailing lines from crash mid-write are safely truncated on boot before recovery logging.
- On restart after an abrupt crash right after `ACTIVE_VOLATILE` before `STABLE`:
  1. Supervisor marks interrupted `ACTIVE_VOLATILE`/`RECOVERING` entries as `RECOVERY_REQUIRED`.
  2. Supervisor sets `armed_on_boot = false` (`controlArmed = false`), `fallbackLatched = true`, and `needs_operator_review = true`, and loads the last `STABLE` package revision (remaining physical OFF).
  3. Operator inspects `GET /v1/devices/{id}/state`, confirms plant readiness, and issues `POST /v1/devices/{id}/rearm` (or rolls back if needed).

### B. Safety Latch Clear & Manual Rollback (`D01` / `F01–F03` / `S03`)
- On a single-scan worker overrun (`F01`), cgroup OOM kill (`F02`), or malformed IPC frame (`F03`), `ScanSupervisor` revokes the worker (`evalResult = None`, `worker_ok = false`), reconciles fallback arena state, and commands physical outputs `0` (`WORKER_TIMEOUT_OR_FAULT`).
- `fallback_latched = true` latches automatically in four conditions: (1) a failed boundary engine swap, (2) a worker failure on the very first scan of a newly swapped package (`swapped_pkg_here && !worker_ok`), (3) any physical driver failure (`obs.driver_status != DriverStatus::Applied`, triggering `force_safe_off`), or (4) boot recovery with `needs_operator_review = true` (`DeploymentManager` also tracks anti-flapping consecutive fault budget `max_faults_before_latch = 3`).
- To restore automatic control:
  1. `POST /v1/devices/{id}/rollbacks` (`{txId, baseRevision, reason}`) requires armed control (`409` if disarmed/latched without rearm) and queues a rollback to the previous `STABLE` hash at a **strictly monotonic new revision**.
  2. `POST /v1/devices/{id}/rearm` clears `fallback_latched`, `needs_operator_review`, and `recent_fault_count` once hardware/module health is confirmed.

### C. Client Wi-Fi Drop Mid-Transaction (`D02` / `V02`)
- Offline web console (`localStorage` key `plc_auto_pending_tx`) and iPad client (`PendingTxStore`) persist pending `txId` before sending.
- On reconnect, client shows `"Verificando estado..."` and polls `GET /v1/proposals/{txId}` and `GET /v1/devices/{id}/state` with the **same `txId`** rather than generating duplicate mutations.

### D. Physical Output Safe-Off Verification
- Terminating `plc-supervisor` via `SIGINT` or `SIGTERM` executes `sup.disarm_and_safe_off()` before releasing `/dev/gpiochip0` line FDs.
- Always confirm physical relay indicators are OFF (`appliedMask == 0`) before handoff.
