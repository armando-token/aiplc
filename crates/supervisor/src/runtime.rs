use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use control_abi::crypto::{canonical_json_bytes, profile_hash, sha256_hex, verify_module_package_signature};
use control_abi::schema::{
    DeploymentRevision, DeploymentState, ModulePackageV1, SiteProfileV1,
};
use control_abi::types::{
    DriverStatus, EvalStatus, OutputFrame, SignalQuality, WorkerMode, ABI_VERSION, MAX_BLOCKS,
    SIGNAL_PHYSICAL_INHIBIT, SIGNAL_TEMP_PROCESS,
};
use control_abi::wire::{EvalRequest, EvalResult};
use field_io::FieldIoDriver;
use logic_worker::engine::LogicEngine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::{AuthTokens, Role};
use crate::arena::{PreparedMigration, StateArena};
use crate::clock::ControlClock;
use crate::deploy::{AuthorizationRecord, ShadowReport, TransactionRecord};
use crate::journal::RecoveredState;
use crate::scan_loop::ScanTelemetryRow;
use crate::shield::{ManualLease, ReactiveShield};
use crate::storage::{BoundaryEpochTracker, StorageWriter, StorageWriterHandle};
use crate::telemetry_out::{
    now_unix_ms, NightRunMetadata, NightScanSample, NightTelemetryAggregator, SharedRtScalars,
    TelemetryHandle,
};
use crate::worker_pool::{
    create_warmed_engine_off_scan, AsyncShadowRunner, ShadowEvalSample, WorkerReaper,
    WorkerReaperHandle, WorkerSpawnSpec,
};

#[derive(Debug, Clone)]
pub struct VirtualInputCmd {
    pub temp_decic: Option<i32>,
    pub temp_quality: Option<SignalQuality>,
    pub temp_age_ms: Option<u32>,
    pub physical_inhibit: Option<bool>,
    pub inject_fault: Option<Option<DriverStatus>>,
    pub witness_override_mask: Option<Option<u32>>,
}

pub enum RtControlCmd {
    Swap(BoundarySwapCmd),
    VirtualInput(VirtualInputCmd),
    ManualLease(Option<ManualLease>),
    Rearm,
    Disarm,
}

pub struct BoundarySwapCmd {
    pub tx_id: String,
    pub is_rollback: bool,
    pub revision: DeploymentRevision,
    pub package: ModulePackageV1,
    pub migration: PreparedMigration,
    pub prewarmed_engine: Box<dyn LogicEngine>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeAuditEntry {
    pub ts_unix_ms: u128,
    pub boot_id: u64,
    pub actor: String,
    pub action: String,
    pub tx_id: Option<String>,
    pub package_hash: Option<String>,
    pub revision: String,
    pub outcome: String,
}

/// Management control plane owned ONLY by HTTP / API handlers (never locked by RT scan thread).
pub struct ControlPlaneState {
    pub device_id: String,
    pub boot_id: u64,
    pub profile: SiteProfileV1,
    pub profile_hash_str: String,
    pub trusted_pub_hex: String,
    pub highest_revision: u64,
    pub armed_tx_id: Option<String>,
    pub prev_stable: Option<(u64, ModulePackageV1, String)>,
    pub rev_packages: BTreeMap<u64, (ModulePackageV1, String)>,
    pub transactions: BTreeMap<String, TransactionRecord>,
    pub audit_log: VecDeque<RuntimeAuditEntry>,
}

impl ControlPlaneState {
    pub fn from_recovered(
        profile: SiteProfileV1,
        trusted_pub_hex: String,
        recovered: RecoveredState,
        boot_id: u64,
    ) -> Result<Self, String> {
        let profile_hash_str = profile_hash(&profile).map_err(|e| e.to_string())?;
        let mut transactions = BTreeMap::new();
        let mut rev_packages = BTreeMap::new();

        for rev in &recovered.revisions {
            if let Some(ref pkg) = recovered.last_stable_package {
                if recovered
                    .last_stable_revision
                    .as_ref()
                    .map(|r| r.package_hash == rev.package_hash)
                    .unwrap_or(false)
                {
                    rev_packages.insert(rev.revision, (pkg.clone(), rev.package_hash.clone()));
                }
            }
            if let Some((pkg, hash)) = rev_packages.get(&rev.revision).cloned() {
                transactions.insert(
                    rev.tx_id.clone(),
                    TransactionRecord {
                        tx_id: rev.tx_id.clone(),
                        revision: rev.clone(),
                        request_digest: hash,
                        package: pkg,
                        shadow_report: None,
                        authorization: None,
                        reason: recovered.recovery_note.clone(),
                    },
                );
            }
        }

        if let (Some(ref stable_rev), Some(ref stable_pkg)) =
            (&recovered.last_stable_revision, &recovered.last_stable_package)
        {
            rev_packages.insert(
                stable_rev.revision,
                (stable_pkg.clone(), stable_rev.package_hash.clone()),
            );
        }

        Ok(Self {
            device_id: profile.device_id.clone(),
            boot_id,
            profile,
            profile_hash_str,
            trusted_pub_hex,
            highest_revision: recovered.highest_revision,
            armed_tx_id: None,
            prev_stable: None,
            rev_packages,
            transactions,
            audit_log: VecDeque::with_capacity(256),
        })
    }

    pub fn push_audit(
        &mut self,
        actor: &str,
        action: &str,
        tx_id: Option<&str>,
        pkg_hash: Option<&str>,
        rev: &str,
        outcome: &str,
    ) {
        if self.audit_log.len() >= 256 {
            self.audit_log.pop_front();
        }
        self.audit_log.push_back(RuntimeAuditEntry {
            ts_unix_ms: now_unix_ms(),
            boot_id: self.boot_id,
            actor: actor.to_string(),
            action: action.to_string(),
            tx_id: tx_id.map(str::to_string),
            package_hash: pkg_hash.map(str::to_string),
            revision: rev.to_string(),
            outcome: outcome.to_string(),
        });
    }
}

/// Complete decoupled 3-domain supervisor runtime:
/// Domain 1: HTTP/Management Plane
/// Domain 2: Lock-free RT Scan Loop thread
/// Domain 3: Exclusive WAL StorageWriter + Off-Scan TelemetryAggregator + WorkerReaper + AsyncShadowRunner
pub struct DecoupledRuntime {
    pub profile: SiteProfileV1,
    pub boot_id: u64,
    pub clock: Arc<dyn ControlClock>,
    pub worker_spec: WorkerSpawnSpec,
    pub io_mode: String,
    pub serial_backend: String,
    pub lcd_enabled: bool,
    pub tokens: AuthTokens,
    pub console_dir: Option<PathBuf>,
    pub stop_flag: Arc<AtomicBool>,

    pub scalars: Arc<SharedRtScalars>,
    pub boundary_epoch: Arc<BoundaryEpochTracker>,
    pub storage_handle: StorageWriterHandle,
    pub telemetry_handle: TelemetryHandle,
    pub reaper_handle: WorkerReaperHandle,
    pub shadow_runner: Arc<AsyncShadowRunner>,
    pub mgmt: Mutex<ControlPlaneState>,
    pub cmd_tx: mpsc::SyncSender<RtControlCmd>,
    pub active_pkg_meta: Mutex<Option<(u64, ModulePackageV1, String)>>,

    rt_thread: Option<JoinHandle<()>>,
    storage_owner: Option<StorageWriter>,
    telemetry_owner: Option<NightTelemetryAggregator>,
    reaper_owner: Option<WorkerReaper>,
}

pub struct DecoupledRuntimeConfig {
    pub profile: SiteProfileV1,
    pub trusted_pub_hex: String,
    pub boot_id: u64,
    pub recovered: RecoveredState,
    pub storage: StorageWriter,
    pub boundary_epoch: Arc<BoundaryEpochTracker>,
    pub driver: Box<dyn FieldIoDriver>,
    pub virtual_bench_shared: Option<Arc<Mutex<field_io::driver::VirtualBenchState>>>,
    pub clock: Arc<dyn ControlClock>,
    pub worker_spec: WorkerSpawnSpec,
    pub io_mode: String,
    pub serial_backend: String,
    pub lcd_enabled: bool,
    pub auto_arm: bool,
    pub rt_fifo: bool,
    pub metrics_meta: NightRunMetadata,
    pub console_dir: Option<PathBuf>,
    pub tokens: AuthTokens,
    pub stop_flag: Option<Arc<AtomicBool>>,
}

impl DecoupledRuntime {
    pub fn start(cfg: DecoupledRuntimeConfig) -> Result<Arc<Self>, String> {
        let reaper = WorkerReaper::start();
        let reaper_handle = reaper.handle();
        let shadow_runner = Arc::new(AsyncShadowRunner::start(reaper_handle.clone()));

        let initial_rev = cfg
            .recovered
            .last_stable_revision
            .as_ref()
            .map(|r| r.revision)
            .unwrap_or(0);
        let initial_pkg = cfg.recovered.last_stable_package.clone();
        let initial_hash = cfg
            .recovered
            .last_stable_revision
            .as_ref()
            .map(|r| r.package_hash.clone());

        let initial_engine: Option<Box<dyn LogicEngine>> = if let Some(ref pkg) = initial_pkg {
            Some(
                create_warmed_engine_off_scan(
                    &cfg.worker_spec,
                    "active",
                    1,
                    pkg,
                    &cfg.trusted_pub_hex,
                    &reaper_handle,
                )
                .map_err(|e| format!("failed spawning initial active worker: {e}"))?,
            )
        } else {
            None
        };

        let initial_worker_pid = initial_engine
            .as_ref()
            .and_then(|e| e.worker_pid())
            .unwrap_or(0);

        let needs_review = cfg.recovered.needs_operator_review;
        let armed_boot = cfg.auto_arm && !needs_review;

        let scalars = Arc::new(SharedRtScalars::default());
        scalars.active_revision.store(initial_rev, Ordering::Release);
        scalars.lease_epoch.store(1, Ordering::Release);
        scalars.generation.store(1, Ordering::Release);
        scalars.armed.store(armed_boot, Ordering::Release);
        scalars.fallback_latched.store(needs_review, Ordering::Release);
        scalars
            .needs_operator_review
            .store(needs_review, Ordering::Release);
        scalars
            .active_worker_pid
            .store(initial_worker_pid, Ordering::Release);

        let storage_handle = cfg.storage.handle();
        let telemetry = NightTelemetryAggregator::start(
            cfg.metrics_meta,
            Arc::clone(&cfg.storage.health),
            Arc::clone(&shadow_runner.metrics),
            Arc::clone(&scalars),
        );
        let telemetry_handle = telemetry.handle.clone();

        let mgmt = ControlPlaneState::from_recovered(
            cfg.profile.clone(),
            cfg.trusted_pub_hex.clone(),
            cfg.recovered,
            cfg.boot_id,
        )?;

        let active_pkg_meta = match (&initial_pkg, &initial_hash) {
            (Some(p), Some(h)) => Mutex::new(Some((initial_rev, p.clone(), h.clone()))),
            _ => Mutex::new(None),
        };

        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<RtControlCmd>(32);
        let stop_flag = cfg
            .stop_flag
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));

        let mut arena = StateArena::new(initial_rev);
        if let Some(ref pkg) = initial_pkg {
            if let Ok(plan) = arena.plan_migration(pkg, initial_rev) {
                arena.apply_migration(&plan, cfg.clock.now_ns());
            }
        }

        let rt_clock = Arc::clone(&cfg.clock);
        let rt_profile = cfg.profile.clone();
        let rt_boot_id = cfg.boot_id;
        let mut rt_driver = cfg.driver;
        let rt_bench = cfg.virtual_bench_shared;
        let mut rt_engine = initial_engine;
        let rt_scalars = Arc::clone(&scalars);
        let rt_epoch = Arc::clone(&cfg.boundary_epoch);
        let rt_storage = storage_handle.clone();
        let rt_telem = telemetry_handle.clone();
        let rt_shadow = Arc::clone(&shadow_runner);
        let rt_stop = Arc::clone(&stop_flag);
        let rt_fifo = cfg.rt_fifo;
        let mut rt_active_pkg = initial_pkg;
        let mut rt_shield = ReactiveShield::from_profile(&rt_profile);

        let rt_thread = thread::Builder::new()
            .name("scan-rt".into())
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                if rt_fifo {
                    try_enable_rt_fifo(&rt_telem);
                }

                let period_ns = (rt_profile.scan_period_ms.max(1) as u64) * 1_000_000;
                let eval_cutoff_rel_ns = (rt_profile.eval_deadline_ms.max(1) as u64) * 1_000_000;
                let mut next_release_ns = rt_clock.now_ns() + period_ns;
                let mut scan_id = 0u64;
                let mut output_seq = 0u64;
                let mut lease_epoch = 1u64;
                let mut generation = 1u64;
                let mut active_rev = initial_rev;
                let mut manual_lease: Option<ManualLease> = None;

                rt_telem.emit_event_nonblocking(
                    0,
                    "scan_loop_started",
                    format!(
                        "rev={} worker_pid={} armed={}",
                        active_rev, initial_worker_pid, armed_boot
                    ),
                );

                while !rt_stop.load(Ordering::Acquire) {
                    rt_clock.sleep_until_ns(next_release_ns);
                    if rt_stop.load(Ordering::Acquire) {
                        break;
                    }

                    let actual_start_ns = rt_clock.now_ns();
                    let raw_release_ns = next_release_ns;
                    let wakeup_jitter_ns = actual_start_ns.saturating_sub(raw_release_ns);

                    let release_ns = if actual_start_ns >= raw_release_ns + period_ns {
                        let skipped = (actual_start_ns - raw_release_ns) / period_ns;
                        rt_scalars.missed_releases.fetch_add(skipped, Ordering::Relaxed);
                        let aligned = raw_release_ns + skipped * period_ns;
                        next_release_ns = aligned + period_ns;
                        aligned
                    } else {
                        next_release_ns = raw_release_ns + period_ns;
                        raw_release_ns
                    };

                    scan_id += 1;
                    rt_scalars.scan_id.store(scan_id, Ordering::Release);

                    // 1. Drain non-blocking control/input commands
                    let mut ram_swap_ns = 0u64;
                    while let Ok(cmd) = cmd_rx.try_recv() {
                        match cmd {
                            RtControlCmd::VirtualInput(vi) => {
                                if let Some(ref vb) = rt_bench {
                                    if let Ok(mut st) = vb.lock() {
                                        if let Some(t) = vi.temp_decic {
                                            st.temp_decic = t;
                                        }
                                        if let Some(q) = vi.temp_quality {
                                            st.temp_quality = q;
                                        }
                                        if let Some(a) = vi.temp_age_ms {
                                            st.temp_age_ms = a;
                                        }
                                        if let Some(inh) = vi.physical_inhibit {
                                            st.physical_inhibit = inh;
                                        }
                                        if let Some(f) = vi.inject_fault {
                                            st.inject_fault = f;
                                        }
                                        if let Some(w) = vi.witness_override_mask {
                                            st.witness_override_mask = w;
                                        }
                                        st.input_seq += 1;
                                    }
                                }
                            }
                            RtControlCmd::ManualLease(l) => {
                                manual_lease = l;
                            }
                            RtControlCmd::Rearm => {
                                rt_scalars.armed.store(true, Ordering::Release);
                                rt_scalars.fallback_latched.store(false, Ordering::Release);
                                rt_scalars
                                    .needs_operator_review
                                    .store(false, Ordering::Release);
                                rt_telem.emit_event_nonblocking(
                                    scan_id,
                                    "operator_rearm",
                                    "rearmed control",
                                );
                            }
                            RtControlCmd::Disarm => {
                                rt_scalars.armed.store(false, Ordering::Release);
                                rt_telem.emit_event_nonblocking(
                                    scan_id,
                                    "operator_disarm",
                                    "disarmed control",
                                );
                            }
                            RtControlCmd::Swap(mut swap) => {
                                let t0 = rt_clock.now_ns();
                                let can_swap = rt_scalars.armed.load(Ordering::Acquire)
                                    && (!rt_scalars.fallback_latched.load(Ordering::Acquire)
                                        || swap.is_rollback)
                                    && !rt_scalars.needs_operator_review.load(Ordering::Acquire)
                                    && rt_driver.health().healthy;
                                if can_swap {
                                    if let Some(mut old) = rt_engine.take() {
                                        old.revoke();
                                    }
                                    rt_engine = Some(swap.prewarmed_engine);
                                    generation += 1;
                                    lease_epoch += 1;
                                    active_rev = swap.revision.revision;
                                    arena.apply_migration(&swap.migration, actual_start_ns);
                                    rt_active_pkg = Some(swap.package);
                                    if swap.is_rollback {
                                        rt_scalars.fallback_latched.store(false, Ordering::Release);
                                        rt_scalars
                                            .warm_rollback_count
                                            .fetch_add(1, Ordering::Relaxed);
                                    }
                                    let wpid = rt_engine
                                        .as_ref()
                                        .and_then(|e| e.worker_pid())
                                        .unwrap_or(0);
                                    rt_scalars
                                        .active_revision
                                        .store(active_rev, Ordering::Release);
                                    rt_scalars.lease_epoch.store(lease_epoch, Ordering::Release);
                                    rt_scalars.generation.store(generation, Ordering::Release);
                                    rt_scalars.active_worker_pid.store(wpid, Ordering::Release);
                                    rt_epoch.store(rt_boot_id, active_rev, lease_epoch);

                                    let mut rev = swap.revision;
                                    rev.active_scan_id = scan_id;
                                    let _ = rt_storage.commit_boundary_nonblocking(
                                        rev,
                                        lease_epoch,
                                        swap.is_rollback,
                                    );
                                    ram_swap_ns = rt_clock.now_ns().saturating_sub(t0);
                                    rt_telem.emit_event_nonblocking(
                                        scan_id,
                                        if swap.is_rollback {
                                            "boundary_warm_rollback"
                                        } else {
                                            "boundary_hot_swap"
                                        },
                                        format!(
                                            "tx={} rev={} lease={} ram_swap_ns={} worker_pid={}",
                                            swap.tx_id, active_rev, lease_epoch, ram_swap_ns, wpid
                                        ),
                                    );
                                } else {
                                    // Safety gate rejected swap -> retire unused prewarmed engine off-scan!
                                    swap.prewarmed_engine.revoke();
                                    rt_telem.emit_event_nonblocking(
                                        scan_id,
                                        "boundary_swap_rejected_rt",
                                        format!("tx={}", swap.tx_id),
                                    );
                                }
                            }
                        }
                    }

                    // 2. Sample inputs
                    let (input_seq, signals) = rt_driver.sample_inputs(actual_start_ns);
                    let io_healthy = rt_driver.health().healthy;

                    // 3. Evaluate active engine within bounded wall-clock eval deadline (never past scan period)
                    let eval_start_ns = rt_clock.now_ns();
                    let cutoff_abs_ns = eval_start_ns
                        .saturating_add(eval_cutoff_rel_ns)
                        .min(release_ns.saturating_add(period_ns));
                    let rem_ns = cutoff_abs_ns.saturating_sub(eval_start_ns);
                    let eval_deadline_instant = Instant::now() + Duration::from_nanos(rem_ns);
                    let mut worker_ok = false;
                    let mut eval_result: Option<EvalResult> = None;

                    if let Some(ref mut eng) = rt_engine {
                        let req = EvalRequest {
                            abi_version: ABI_VERSION,
                            mode: WorkerMode::Active,
                            boot_id: rt_boot_id,
                            lease_epoch,
                            generation,
                            scan_id,
                            logical_ns: actual_start_ns,
                            input_seq,
                            state_revision: arena.state_revision,
                            applied_output_mask: arena.last_applied_output_mask,
                            signals: signals.clone(),
                            blocks: arena.blocks.clone(),
                        };
                        match eng.eval_until(&req, eval_deadline_instant) {
                            Ok(res) => {
                                let after_eval = rt_clock.now_ns();
                                let within_cutoff =
                                    after_eval.saturating_sub(eval_start_ns) <= eval_cutoff_rel_ns
                                        && after_eval <= release_ns.saturating_add(period_ns);
                                if within_cutoff
                                    && res.status == EvalStatus::Ok
                                    && res.lease_epoch == lease_epoch
                                    && res.generation == generation
                                    && res.scan_id == scan_id
                                {
                                    worker_ok = true;
                                    eval_result = Some(res);
                                } else {
                                    eng.revoke();
                                    rt_engine = None;
                                    rt_scalars.active_worker_pid.store(0, Ordering::Release);
                                    rt_scalars.fallback_latched.store(true, Ordering::Release);
                                    rt_scalars
                                        .fallback_latch_count
                                        .fetch_add(1, Ordering::Relaxed);
                                    rt_telem.emit_event_nonblocking(
                                        scan_id,
                                        "worker_fault_fallback",
                                        "mismatched epoch/cutoff on worker reply",
                                    );
                                }
                            }
                            Err(e) => {
                                eng.revoke();
                                rt_engine = None;
                                rt_scalars.active_worker_pid.store(0, Ordering::Release);
                                rt_scalars.fallback_latched.store(true, Ordering::Release);
                                rt_scalars
                                    .fallback_latch_count
                                    .fetch_add(1, Ordering::Relaxed);
                                rt_telem.emit_event_nonblocking(
                                    scan_id,
                                    "worker_fault_fallback",
                                    format!("worker error/timeout: {e}"),
                                );
                            }
                        }
                    }
                    let eval_end_ns = rt_clock.now_ns();
                    let eval_ipc_ns = eval_end_ns.saturating_sub(eval_start_ns);
                    let deadline_miss = eval_ipc_ns > eval_cutoff_rel_ns
                        || eval_end_ns > release_ns.saturating_add(period_ns);

                    // 4. Offer non-blocking snapshot to AsyncShadowRunner (never blocks!)
                    rt_shadow.offer_nonblocking(ShadowEvalSample {
                        boot_id: rt_boot_id,
                        lease_epoch,
                        generation,
                        scan_id,
                        logical_ns: actual_start_ns,
                        input_seq,
                        state_revision: arena.state_revision,
                        applied_output_mask: arena.last_applied_output_mask,
                        signals: signals.clone(),
                        active_desired_mask: eval_result.as_ref().map(|r| r.desired_mask),
                        active_worker_ok: worker_ok,
                        eval_deadline_ms: rt_profile.eval_deadline_ms.max(1) as u64,
                    });

                    // 5. Reactive Shield & state confirmation
                    let proposed_mask = eval_result.as_ref().map(|r| r.desired_mask).unwrap_or(0);
                    let control_armed = rt_scalars.armed.load(Ordering::Acquire);
                    let fallback_latched = rt_scalars.fallback_latched.load(Ordering::Acquire);
                    let active_rs = rt_active_pkg.as_ref().map(|p| &p.rule_set);

                    let decision = rt_shield.evaluate(
                        actual_start_ns,
                        rt_boot_id,
                        control_armed,
                        fallback_latched,
                        io_healthy,
                        worker_ok,
                        &signals,
                        active_rs,
                        &mut manual_lease,
                        proposed_mask,
                    );

                    if decision.is_contingency_off || !worker_ok {
                        arena.reconcile_fallback();
                    } else if let Some(res) = eval_result {
                        let valid_blocks = res.state_revision == arena.state_revision
                            && res.blocks.len() <= MAX_BLOCKS
                            && res.blocks.len() == arena.blocks.len()
                            && res.blocks.iter().zip(arena.blocks.iter()).all(|(nb, ab)| {
                                nb.stable_id == ab.stable_id
                                    && nb.block_type == ab.block_type
                                    && nb.schema_version == ab.schema_version
                            });
                        if valid_blocks || arena.blocks.is_empty() {
                            arena.blocks = res.blocks;
                        } else {
                            arena.reconcile_fallback();
                            rt_scalars.fallback_latched.store(true, Ordering::Release);
                            rt_scalars
                                .fallback_latch_count
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    let decision_ns = rt_clock.now_ns();

                    // 6. Publish OutputFrame & enqueue telemetry non-blockingly
                    output_seq += 1;
                    let out_frame = OutputFrame {
                        boot_id: rt_boot_id,
                        scan_id,
                        lease_epoch,
                        active_revision: active_rev,
                        output_seq,
                        desired_mask: decision.desired_mask,
                        applied_mask: decision.applied_mask,
                        expiry_ns: release_ns.saturating_add(period_ns),
                        guard_reason: decision.guard_reason,
                    };
                    let publish_start_ns = rt_clock.now_ns();
                    let obs = rt_driver.apply_output(&out_frame, publish_start_ns);
                    let ack_driver_ns = rt_clock.now_ns();

                    if obs.driver_status == DriverStatus::Applied {
                        arena.last_applied_output_mask = decision.applied_mask;
                        rt_scalars
                            .applied_mask
                            .store(decision.applied_mask, Ordering::Release);
                    } else {
                        rt_scalars.fallback_latched.store(true, Ordering::Release);
                        rt_driver.force_safe_off(ack_driver_ns);
                        arena.last_applied_output_mask = 0;
                        rt_scalars.applied_mask.store(0, Ordering::Release);
                    }

                    let active_worker_pid = rt_engine
                        .as_ref()
                        .and_then(|e| e.worker_pid())
                        .unwrap_or(0);
                    rt_scalars
                        .active_worker_pid
                        .store(active_worker_pid, Ordering::Release);

                    let row = ScanTelemetryRow {
                        boot_id: rt_boot_id,
                        scan_id,
                        release_ns,
                        start_ns: actual_start_ns,
                        input_seq,
                        eval_start_ns,
                        eval_end_ns,
                        decision_ns,
                        publish_ns: publish_start_ns,
                        ack_driver_ns,
                        active_rev,
                        logic_hash: rt_active_pkg
                            .as_ref()
                            .and_then(|p| {
                                p.wasm_digest.clone().or_else(|| Some(p.ir_digest.clone()))
                            })
                            .unwrap_or_else(|| "none".into()),
                        lease_epoch,
                        deadline_miss,
                        fallback_reason: decision.guard_reason,
                        desired_mask: decision.desired_mask,
                        applied_mask: decision.applied_mask,
                        observed_mask: obs.observed_mask,
                        signals,
                    };

                    let cycle_end_ns = rt_clock.now_ns();
                    let publish_ns = cycle_end_ns.saturating_sub(publish_start_ns);
                    let cycle_dur_ns = cycle_end_ns.saturating_sub(actual_start_ns);

                    rt_telem.offer_scan_nonblocking(NightScanSample {
                        row,
                        wakeup_jitter_ns,
                        eval_ipc_ns,
                        publish_ns,
                        cycle_dur_ns,
                        ram_swap_ns,
                        worker_ok,
                        active_worker_pid,
                    });
                }

                // Cleanly retire active worker off-scan at loop exit
                if let Some(mut eng) = rt_engine.take() {
                    eng.revoke();
                }
            })
            .map_err(|e| format!("spawn scan-rt failed: {e}"))?;

        let runtime = Arc::new(Self {
            profile: cfg.profile,
            boot_id: cfg.boot_id,
            clock: cfg.clock,
            worker_spec: cfg.worker_spec,
            io_mode: cfg.io_mode,
            serial_backend: cfg.serial_backend,
            lcd_enabled: cfg.lcd_enabled,
            tokens: cfg.tokens,
            console_dir: cfg.console_dir,
            stop_flag,
            scalars,
            boundary_epoch: cfg.boundary_epoch,
            storage_handle,
            telemetry_handle,
            reaper_handle,
            shadow_runner,
            mgmt: Mutex::new(mgmt),
            cmd_tx,
            active_pkg_meta,
            rt_thread: Some(rt_thread),
            storage_owner: Some(cfg.storage),
            telemetry_owner: Some(telemetry),
            reaper_owner: Some(reaper),
        });

        Ok(runtime)
    }

    /// Graceful shutdown helper (used by tests and SIGINT/SIGTERM handling).
    pub fn shutdown(mut self) {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(h) = self.rt_thread.take() {
            let _ = h.join();
        }
        if let Ok(mut sh) = Arc::try_unwrap(self.shadow_runner) {
            sh.shutdown();
        }
        if let Some(mut st) = self.storage_owner.take() {
            st.shutdown();
        }
        if let Some(mut tm) = self.telemetry_owner.take() {
            tm.shutdown();
        }
        if let Some(mut rp) = self.reaper_owner.take() {
            rp.shutdown();
        }
    }

    pub fn request_stop(&self) {
        self.stop_flag.store(true, Ordering::Release);
    }

    pub fn join_threads(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(h) = self.rt_thread.take() {
            let _ = h.join();
        }
        self.shadow_runner.clear();
        if let Some(mut st) = self.storage_owner.take() {
            st.shutdown();
        }
        if let Some(mut tm) = self.telemetry_owner.take() {
            tm.shutdown();
        }
        if let Some(mut rp) = self.reaper_owner.take() {
            rp.shutdown();
        }
    }
}

fn try_enable_rt_fifo(telem: &TelemetryHandle) {
    #[cfg(target_os = "linux")]
    unsafe {
        let param = libc::sched_param { sched_priority: 50 };
        let rc = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
        if rc == 0 {
            telem.emit_event_nonblocking(0, "rt_fifo_enabled", "SCHED_FIFO prio=50 active");
        } else {
            let err = std::io::Error::last_os_error();
            telem.emit_event_nonblocking(0, "rt_fifo_fallback", format!("SCHED_FIFO unavailable: {err}"));
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        telem.emit_event_nonblocking(0, "rt_fifo_skipped", "non-linux target");
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProposeReq {
    tx_id: String,
    package: ModulePackageV1,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthReq {
    operator_id: String,
    expected_package_hash: String,
    #[serde(default = "default_ttl_ms")]
    ttl_ms: u64,
}

fn default_ttl_ms() -> u64 {
    60_000
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RollbackReq {
    #[serde(default)]
    tx_id: Option<String>,
    #[serde(default = "default_rb_reason")]
    reason: String,
}

fn default_rb_reason() -> String {
    "operator_rollback".into()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManualReq {
    mask: u32,
    valid_mask: u32,
    ttl_ms: u64,
    operator_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VirtualInputReq {
    #[serde(default)]
    temp_decic: Option<i32>,
    #[serde(default)]
    temp_quality: Option<SignalQuality>,
    #[serde(default)]
    temp_age_ms: Option<u32>,
    #[serde(default)]
    physical_inhibit: Option<bool>,
    #[serde(default)]
    inject_fault: Option<Option<DriverStatus>>,
    #[serde(default)]
    witness_override_mask: Option<Option<u32>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShadowReq {
    #[serde(default)]
    tx_id: Option<String>,
    #[serde(default)]
    package: Option<ModulePackageV1>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    wait_ms: Option<u64>,
    #[serde(default)]
    shadow_deadline_ms: Option<u64>,
    #[serde(default)]
    fault_args: Vec<String>,
}

impl DecoupledRuntime {
    pub fn handle_http_request(
        &self,
        method: &str,
        path: &str,
        auth_header: Option<&str>,
        body: &[u8],
    ) -> (u16, &'static str, Vec<u8>) {
        // Static console offline UI (only if configured)
        if method == "GET" && (path == "/" || path == "/console" || path.starts_with("/console/")) {
            if let Some(ref dir) = self.console_dir {
                let rel = if path == "/" || path == "/console" || path == "/console/" {
                    "index.html"
                } else {
                    path.trim_start_matches("/console").trim_start_matches('/')
                };
                if !rel.is_empty() && !rel.contains("..") && !rel.starts_with('/') {
                    if let (Ok(base_canon), Ok(target_canon)) =
                        (fs::canonicalize(dir), fs::canonicalize(dir.join(rel)))
                    {
                        if target_canon.starts_with(&base_canon) {
                            if let Ok(bytes) = fs::read(&target_canon) {
                                let ctype = if rel.ends_with(".js") {
                                    "application/javascript; charset=utf-8"
                                } else if rel.ends_with(".css") {
                                    "text/css; charset=utf-8"
                                } else {
                                    "text/html; charset=utf-8"
                                };
                                return (200, ctype, bytes);
                            }
                        }
                    }
                }
            }
            return json_resp(
                200,
                &json!({
                    "status": "ok",
                    "service": "plc-auto-supervisor-decoupled",
                    "ioMode": self.io_mode,
                    "serialBackend": self.serial_backend,
                    "lcdEnabled": self.lcd_enabled
                }),
            );
        }

        if method == "GET" && path == "/health" {
            let st = self.storage_handle.health.snapshot();
            return json_resp(
                200,
                &json!({
                    "ok": !st.degraded,
                    "storageDegraded": st.degraded,
                    "ioMode": self.io_mode,
                    "serialBackend": self.serial_backend,
                    "lcdEnabled": self.lcd_enabled,
                    "supervisorPid": std::process::id(),
                }),
            );
        }

        let (actor, role) = match self.tokens.authenticate(auth_header) {
            Some(v) => v,
            None => {
                return json_resp(
                    401,
                    &json!({"error": "UNAUTHORIZED", "message": "missing or invalid Bearer token"}),
                );
            }
        };

        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        if parts.is_empty() || parts[0] != "v1" {
            return json_resp(404, &json!({"error": "NOT_FOUND"}));
        }

        // GET /v1/devices/{id}/state
        if method == "GET" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "state" {
            if parts[2] != self.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            let pub_snap = self.telemetry_handle.snapshot.read().unwrap().clone();
            let st = self.storage_handle.health.snapshot();
            let sh = self.shadow_runner.metrics.snapshot();
            let active_rev = self.scalars.active_revision.load(Ordering::Acquire);
            let wpid = self.scalars.active_worker_pid.load(Ordering::Acquire);
            let pkg_meta = self.active_pkg_meta.lock().unwrap().clone();
            let active_pkg_hash = pkg_meta.as_ref().map(|(_, _, h)| h.clone());
            let logic_hash = pkg_meta
                .as_ref()
                .and_then(|(_, p, _)| p.wasm_digest.clone().or_else(|| Some(p.ir_digest.clone())));
            let temp = pub_snap.latest_row.as_ref().and_then(|r| {
                r.signals
                    .iter()
                    .find(|s| s.id == SIGNAL_TEMP_PROCESS)
                    .copied()
            });
            let inh = pub_snap.latest_row.as_ref().and_then(|r| {
                r.signals
                    .iter()
                    .find(|s| s.id == SIGNAL_PHYSICAL_INHIBIT)
                    .map(|s| s.value != 0)
            });

            return json_resp(
                200,
                &json!({
                    "deviceId": self.profile.device_id,
                    "bootId": self.boot_id.to_string(),
                    "scanId": self.scalars.scan_id.load(Ordering::Acquire),
                    "activeRevision": active_rev,
                    "activePackageHash": active_pkg_hash,
                    "activeLogicHash": logic_hash,
                    "leaseEpoch": self.scalars.lease_epoch.load(Ordering::Acquire),
                    "generation": self.scalars.generation.load(Ordering::Acquire),
                    "armed": self.scalars.armed.load(Ordering::Acquire),
                    "fallbackLatched": self.scalars.fallback_latched.load(Ordering::Acquire),
                    "needsOperatorReview": self.scalars.needs_operator_review.load(Ordering::Acquire),
                    "appliedMask": self.scalars.applied_mask.load(Ordering::Acquire),
                    "guardReason": pub_snap.guard_reason,
                    "missedReleases": self.scalars.missed_releases.load(Ordering::Acquire),
                    "telemetryDropped": self.scalars.telemetry_dropped.load(Ordering::Acquire),
                    "shadowDropped": sh.dropped,
                    "warmRollbackCount": self.scalars.warm_rollback_count.load(Ordering::Acquire),
                    "fallbackLatchCount": self.scalars.fallback_latch_count.load(Ordering::Acquire),
                    "activePersisted": st.active_persisted,
                    "storageDegraded": st.degraded,
                    "storageError": st.last_error,
                    "storageStatus": st,
                    "ioMode": self.io_mode,
                    "serialBackend": self.serial_backend,
                    "lcdEnabled": self.lcd_enabled,
                    "isolatedWorker": self.worker_spec.should_isolate_active(),
                    "supervisorPid": std::process::id(),
                    "activeWorkerPid": if wpid == 0 { Value::Null } else { json!(wpid) },
                    "shadow": sh,
                    "tempDecic": temp.map(|s| s.value),
                    "tempQuality": temp.map(|s| s.quality),
                    "physicalInhibit": inh.unwrap_or(false),
                }),
            );
        }

        // GET /v1/devices/{id}/scans
        if method == "GET" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "scans" {
            if parts[2] != self.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            let pub_snap = self.telemetry_handle.snapshot.read().unwrap().clone();
            let sum = self.telemetry_handle.summary.read().unwrap().clone();
            return json_resp(
                200,
                &json!({
                    "deviceId": self.profile.device_id,
                    "scans": pub_snap.recent_scans,
                    "nightSummary": sum,
                }),
            );
        }

        // GET /v1/devices/{id}/night-metrics
        if method == "GET"
            && parts.len() == 4
            && parts[1] == "devices"
            && parts[3] == "night-metrics"
        {
            if parts[2] != self.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            let sum = self.telemetry_handle.summary.read().unwrap().clone();
            let st = self.storage_handle.health.snapshot();
            let sh = self.shadow_runner.metrics.snapshot();
            return json_resp(
                200,
                &json!({
                    "summary": sum,
                    "storage": st,
                    "shadow": sh,
                }),
            );
        }

        // GET /v1/devices/{id}/evidence
        if method == "GET" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "evidence" {
            if parts[2] != self.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            let pub_snap = self.telemetry_handle.snapshot.read().unwrap().clone();
            let st = self.storage_handle.health.snapshot();
            let sh = self.shadow_runner.metrics.snapshot();
            return json_resp(
                200,
                &json!({
                    "deviceId": self.profile.device_id,
                    "bootId": self.boot_id,
                    "scanId": pub_snap.scan_id,
                    "activeRevision": pub_snap.active_revision,
                    "leaseEpoch": pub_snap.lease_epoch,
                    "guardReason": pub_snap.guard_reason,
                    "appliedMask": pub_snap.applied_mask,
                    "activePersisted": st.active_persisted,
                    "storageDegraded": st.degraded,
                    "storageError": st.last_error,
                    "latestRow": pub_snap.latest_row,
                    "shadow": sh,
                }),
            );
        }

        // GET /v1/devices/{id}/audit
        if method == "GET" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "audit" {
            if parts[2] != self.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            let mgmt = self.mgmt.lock().unwrap();
            let storage_events = self
                .storage_handle
                .health
                .recent_events
                .lock()
                .unwrap()
                .clone();
            let telem_events = self.telemetry_handle.recent_events.read().unwrap().clone();
            return json_resp(
                200,
                &json!({
                    "audit": mgmt.audit_log,
                    "storageEvents": storage_events,
                    "runtimeEvents": telem_events,
                }),
            );
        }

        // POST /v1/devices/{id}/proposals
        if method == "POST" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "proposals" {
            if parts[2] != self.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            if !can_author(role) {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            if let Err(err) = self.storage_handle.has_capacity_and_healthy() {
                return json_resp(503, &json!({"error": "STORAGE_DEGRADED", "detail": err}));
            }
            let req: ProposeReq = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => return json_resp(400, &json!({"error": "BAD_REQUEST", "detail": e.to_string()})),
            };
            return self.handle_propose(&actor, req);
        }

        // POST /v1/proposals/{txId}/shadow
        if method == "POST" && parts.len() == 4 && parts[1] == "proposals" && parts[3] == "shadow" {
            if !can_author(role) && !can_operate(role) {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            let req: ShadowReq = serde_json::from_slice(body).unwrap_or_else(|_| ShadowReq {
                wait_ms: Some(100),
                ..Default::default()
            });
            return self.handle_proposal_shadow(&actor, parts[2], req);
        }

        // POST /v1/devices/{id}/shadow
        if method == "POST" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "shadow" {
            if parts[2] != self.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            if !can_operate(role) && !can_author(role) {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            let req: ShadowReq = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => return json_resp(400, &json!({"error": "BAD_REQUEST", "detail": e.to_string()})),
            };
            return self.handle_device_shadow(req);
        }

        // POST /v1/proposals/{txId}/authorizations
        if method == "POST"
            && parts.len() == 4
            && parts[1] == "proposals"
            && parts[3] == "authorizations"
        {
            if !can_operate(role) {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            if let Err(err) = self.storage_handle.has_capacity_and_healthy() {
                return json_resp(503, &json!({"error": "STORAGE_DEGRADED", "detail": err}));
            }
            let req: AuthReq = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => return json_resp(400, &json!({"error": "BAD_REQUEST", "detail": e.to_string()})),
            };
            return self.handle_authorize(&actor, &role_label(role), parts[2], req);
        }

        // POST /v1/proposals/{txId}/activate
        if method == "POST"
            && parts.len() == 4
            && parts[1] == "proposals"
            && parts[3] == "activate"
        {
            if !can_operate(role) {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            if let Err(err) = self.storage_handle.has_capacity_and_healthy() {
                return json_resp(503, &json!({"error": "STORAGE_DEGRADED", "detail": err}));
            }
            return self.handle_activate(&actor, parts[2]);
        }

        // POST /v1/devices/{id}/rollback
        if method == "POST" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "rollback" {
            if parts[2] != self.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            if !can_operate(role) {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            if let Err(err) = self.storage_handle.has_capacity_and_healthy() {
                return json_resp(503, &json!({"error": "STORAGE_DEGRADED", "detail": err}));
            }
            let req: RollbackReq = serde_json::from_slice(body).unwrap_or(RollbackReq {
                tx_id: None,
                reason: "operator_rollback".into(),
            });
            return self.handle_rollback(&actor, req);
        }

        // POST /v1/devices/{id}/rearm
        if method == "POST" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "rearm" {
            if parts[2] != self.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            if !can_operate(role) {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            if let Err(err) = self.storage_handle.has_capacity_and_healthy() {
                return json_resp(503, &json!({"error": "STORAGE_DEGRADED", "detail": err}));
            }
            self.scalars.armed.store(true, Ordering::Release);
            self.scalars
                .needs_operator_review
                .store(false, Ordering::Release);
            self.scalars.fallback_latched.store(false, Ordering::Release);
            let _ = self.cmd_tx.try_send(RtControlCmd::Rearm);
            {
                let mut mgmt = self.mgmt.lock().unwrap();
                mgmt.push_audit(&actor, "rearm", None, None, "active", "armed");
            }
            return json_resp(200, &json!({"ok": true, "armed": true}));
        }

        // POST /v1/devices/{id}/manual
        if method == "POST" && parts.len() == 4 && parts[1] == "devices" && parts[3] == "manual" {
            if parts[2] != self.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            if !can_operate(role) {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            let req: ManualReq = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => return json_resp(400, &json!({"error": "BAD_REQUEST", "detail": e.to_string()})),
            };
            let now_ns = self.clock.now_ns();
            let lease = ManualLease {
                boot_id: self.boot_id,
                mask: req.mask,
                valid_mask: req.valid_mask,
                expires_logical_ns: now_ns.saturating_add(req.ttl_ms.saturating_mul(1_000_000)),
                operator_id: req.operator_id,
            };
            if self.cmd_tx.try_send(RtControlCmd::ManualLease(Some(lease))).is_err() {
                return json_resp(429, &json!({"error": "BUSY"}));
            }
            return json_resp(200, &json!({"ok": true}));
        }

        // POST /v1/devices/{id}/virtual-inputs (ONLY enabled when io_mode == "virtual")
        if method == "POST"
            && parts.len() == 4
            && parts[1] == "devices"
            && parts[3] == "virtual-inputs"
        {
            if parts[2] != self.profile.device_id {
                return json_resp(404, &json!({"error": "UNKNOWN_DEVICE"}));
            }
            if !can_operate(role) {
                return json_resp(403, &json!({"error": "FORBIDDEN"}));
            }
            if self.io_mode != "virtual" {
                return json_resp(
                    403,
                    &json!({"error": "FORBIDDEN_IN_HARDWARE_IO_MODE"}),
                );
            }
            let req: VirtualInputReq = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => return json_resp(400, &json!({"error": "BAD_REQUEST", "detail": e.to_string()})),
            };
            let cmd = RtControlCmd::VirtualInput(VirtualInputCmd {
                temp_decic: req.temp_decic,
                temp_quality: req.temp_quality,
                temp_age_ms: req.temp_age_ms,
                physical_inhibit: req.physical_inhibit,
                inject_fault: req.inject_fault,
                witness_override_mask: req.witness_override_mask,
            });
            if self.cmd_tx.try_send(cmd).is_err() {
                return json_resp(429, &json!({"error": "BUSY"}));
            }
            return json_resp(200, &json!({"ok": true}));
        }

        json_resp(404, &json!({"error": "NOT_FOUND"}))
    }

    fn handle_propose(&self, actor: &str, req: ProposeReq) -> (u16, &'static str, Vec<u8>) {
        let pkg_val = match serde_json::to_value(&req.package) {
            Ok(v) => v,
            Err(e) => return json_resp(400, &json!({"error": "BAD_PACKAGE", "detail": e.to_string()})),
        };
        let req_digest = match canonical_json_bytes(&pkg_val) {
            Ok(b) => sha256_hex(&b),
            Err(e) => return json_resp(400, &json!({"error": "BAD_PACKAGE", "detail": e.to_string()})),
        };
        let pkg_bytes = match canonical_json_bytes(&pkg_val) {
            Ok(b) => b,
            Err(e) => return json_resp(400, &json!({"error": "BAD_PACKAGE", "detail": e.to_string()})),
        };
        let computed_pkg_hash = sha256_hex(&pkg_bytes);
        let live_rev = self.scalars.active_revision.load(Ordering::Acquire);

        let mut mgmt = self.mgmt.lock().unwrap();
        if let Some(existing) = mgmt.transactions.get(&req.tx_id) {
            if existing.request_digest == req_digest {
                return json_resp(200, existing);
            } else {
                mgmt.push_audit(actor, "propose", Some(&req.tx_id), None, "n/a", "idempotency_conflict");
                return json_resp(409, &json!({"error": "IDEMPOTENCY_MISMATCH"}));
            }
        }

        if req.package.base_revision != live_rev {
            mgmt.push_audit(actor, "propose", Some(&req.tx_id), None, "n/a", "revision_conflict");
            return json_resp(
                409,
                &json!({"error": "REVISION_CONFLICT", "expectedBaseRevision": live_rev, "got": req.package.base_revision}),
            );
        }

        if req.package.profile_hash != mgmt.profile_hash_str {
            return json_resp(422, &json!({"error": "PROFILE_HASH_MISMATCH"}));
        }
        if !req.package.requested_capabilities.is_empty() {
            return json_resp(422, &json!({"error": "CAPABILITIES_FORBIDDEN"}));
        }
        if let Err(e) = verify_module_package_signature(&req.package, &mgmt.trusted_pub_hex) {
            return json_resp(422, &json!({"error": "SIGNATURE_INVALID", "detail": e.to_string()}));
        }

        // Validate schema compatibility against active live revision blocks
        let schema_arena = build_arena_schema(live_rev, mgmt.rev_packages.get(&live_rev).map(|(p, _)| p));
        let next_rev = mgmt.highest_revision + 1;
        if let Err(e) = schema_arena.plan_migration(&req.package, next_rev) {
            return json_resp(422, &json!({"error": "INCOMPATIBLE_MIGRATION", "detail": e.to_string()}));
        }

        let mut rev_staged = DeploymentRevision {
            tx_id: req.tx_id.clone(),
            device_id: mgmt.device_id.clone(),
            boot_id: mgmt.boot_id,
            revision: next_rev,
            base_revision: req.package.base_revision,
            package_hash: computed_pkg_hash.clone(),
            logic_hash: req
                .package
                .wasm_digest
                .clone()
                .unwrap_or_else(|| req.package.ir_digest.clone()),
            rule_set_hash: req.package.ir_digest.clone(),
            profile_hash: req.package.profile_hash.clone(),
            state_schema: req.package.state_schema.clone(),
            state: DeploymentState::Staged,
            active_scan_id: 0,
        };
        let mut rev_verified = rev_staged.clone();
        rev_verified.state = DeploymentState::Verified;

        let persisted_hash = match self.storage_handle.store_package_and_append_revisions(
            req.package.clone(),
            rev_staged.clone(),
            rev_verified.clone(),
        ) {
            Ok(h) => h,
            Err(e) => {
                mgmt.push_audit(actor, "propose", Some(&req.tx_id), None, "n/a", "storage_degraded");
                return json_resp(503, &json!({"error": "STORAGE_DEGRADED", "detail": e}));
            }
        };

        rev_staged.package_hash = persisted_hash.clone();
        rev_verified.package_hash = persisted_hash.clone();
        mgmt.highest_revision = next_rev;
        mgmt.rev_packages
            .insert(next_rev, (req.package.clone(), persisted_hash.clone()));

        let tx_rec = TransactionRecord {
            tx_id: req.tx_id.clone(),
            revision: rev_verified,
            request_digest: req_digest,
            package: req.package,
            shadow_report: None,
            authorization: None,
            reason: "verified".into(),
        };
        mgmt.transactions.insert(req.tx_id.clone(), tx_rec);
        mgmt.push_audit(
            actor,
            "propose",
            Some(&req.tx_id),
            Some(&persisted_hash),
            &next_rev.to_string(),
            "verified",
        );

        let rec = mgmt.transactions.get(&req.tx_id).unwrap();
        json_resp(201, rec)
    }

    fn handle_proposal_shadow(
        &self,
        actor: &str,
        tx_id: &str,
        req: ShadowReq,
    ) -> (u16, &'static str, Vec<u8>) {
        let (pkg, pub_hex, rev_num) = {
            let mgmt = self.mgmt.lock().unwrap();
            let Some(tx) = mgmt.transactions.get(tx_id) else {
                return json_resp(404, &json!({"error": "UNKNOWN_TX"}));
            };
            (
                tx.package.clone(),
                mgmt.trusted_pub_hex.clone(),
                tx.revision.revision,
            )
        };

        let mut spec = self.worker_spec.clone();
        spec.rt_fifo_active_worker = false;
        if !req.fault_args.is_empty() {
            spec.extra_args = req.fault_args;
        }
        if let Some(dl) = req.shadow_deadline_ms {
            spec.eval_deadline_ms = dl.max(1);
        }
        let baseline_scans = self.shadow_runner.metrics.scans_executed.load(Ordering::Acquire);
        self.shadow_runner
            .install_candidate(pkg, pub_hex, rev_num, spec);

        // Wait up to requested settle time (default 100ms) for initial live shadow scans off-scan
        let wait_ms = req.wait_ms.unwrap_or(100).min(1500);
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        while Instant::now() < deadline {
            if self.shadow_runner.metrics.scans_executed.load(Ordering::Acquire) > baseline_scans
                || self.shadow_runner.metrics.errors.load(Ordering::Acquire) > 0
            {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        let snap = self.shadow_runner.metrics.snapshot();
        let report = ShadowReport {
            passed: snap.mismatches == 0 && snap.errors == 0,
            scans_executed: snap.scans_executed.saturating_sub(baseline_scans),
            mismatches: snap.mismatches,
            vetoes: 0,
            errors: snap.errors,
            first_counterexample: snap.first_counterexample.clone(),
        };

        let mut mgmt = self.mgmt.lock().unwrap();
        let Some(tx) = mgmt.transactions.get_mut(tx_id) else {
            return json_resp(404, &json!({"error": "UNKNOWN_TX"}));
        };
        if matches!(
            tx.revision.state,
            DeploymentState::Verified | DeploymentState::Shadow | DeploymentState::Rejected
        ) {
            tx.revision.state = if report.passed {
                DeploymentState::Shadow
            } else {
                DeploymentState::Rejected
            };
            tx.shadow_report = Some(report);
            let rev_clone = tx.revision.clone();
            let rev_str = tx.revision.revision.to_string();
            let _ = self.storage_handle.append_revision_sync(rev_clone);
            mgmt.push_audit(actor, "shadow", Some(tx_id), None, &rev_str, "recorded");
        }
        let rec = mgmt.transactions.get(tx_id).unwrap();
        json_resp(200, rec)
    }

    fn handle_device_shadow(&self, req: ShadowReq) -> (u16, &'static str, Vec<u8>) {
        if req.action.as_deref() == Some("clear") {
            self.shadow_runner.clear();
            return json_resp(200, &json!({"ok": true, "shadow": self.shadow_runner.metrics.snapshot()}));
        }
        let mut spec = self.worker_spec.clone();
        spec.rt_fifo_active_worker = false;
        if !req.fault_args.is_empty() {
            spec.extra_args = req.fault_args;
        }
        if let Some(dl) = req.shadow_deadline_ms {
            spec.eval_deadline_ms = dl.max(1);
        }
        if let Some(pkg) = req.package {
            let pub_hex = self.mgmt.lock().unwrap().trusted_pub_hex.clone();
            let rev = req.wait_ms.unwrap_or(0);
            self.shadow_runner.install_candidate(pkg, pub_hex, rev, spec);
            return json_resp(200, &json!({"ok": true, "shadow": self.shadow_runner.metrics.snapshot()}));
        }
        if let Some(tx_id) = req.tx_id {
            let mgmt = self.mgmt.lock().unwrap();
            let Some(tx) = mgmt.transactions.get(&tx_id) else {
                return json_resp(404, &json!({"error": "UNKNOWN_TX"}));
            };
            self.shadow_runner.install_candidate(
                tx.package.clone(),
                mgmt.trusted_pub_hex.clone(),
                tx.revision.revision,
                spec,
            );
            return json_resp(200, &json!({"ok": true, "txId": tx_id}));
        }
        json_resp(400, &json!({"error": "BAD_REQUEST"}))
    }

    fn handle_authorize(
        &self,
        actor: &str,
        role_str: &str,
        tx_id: &str,
        req: AuthReq,
    ) -> (u16, &'static str, Vec<u8>) {
        let now_ns = self.clock.now_ns();
        let live_rev = self.scalars.active_revision.load(Ordering::Acquire);
        let mut mgmt = self.mgmt.lock().unwrap();

        let (base_rev, pkg_hash, state, rule_hash, shadow_opt, armed_tx) = {
            let Some(tx) = mgmt.transactions.get(tx_id) else {
                return json_resp(404, &json!({"error": "UNKNOWN_TX"}));
            };
            (
                tx.revision.base_revision,
                tx.revision.package_hash.clone(),
                tx.revision.state,
                tx.revision.rule_set_hash.clone(),
                tx.shadow_report.clone(),
                mgmt.armed_tx_id.clone(),
            )
        };

        if pkg_hash != req.expected_package_hash {
            return json_resp(409, &json!({"error": "PACKAGE_HASH_MISMATCH"}));
        }
        if !matches!(state, DeploymentState::Verified | DeploymentState::Shadow | DeploymentState::Armed) {
            return json_resp(409, &json!({"error": "INVALID_STATE", "state": state}));
        }
        if base_rev != live_rev {
            return json_resp(409, &json!({"error": "REVISION_CONFLICT", "expected": live_rev, "got": base_rev}));
        }
        if let Some(ref sh) = shadow_opt {
            if !sh.passed {
                return json_resp(409, &json!({"error": "SHADOW_FAILED"}));
            }
        }
        if let Some(ref armed) = armed_tx {
            if armed != tx_id {
                return json_resp(409, &json!({"error": "ALREADY_ARMED", "armedTx": armed}));
            }
        }

        let ev_digest = shadow_opt
            .as_ref()
            .and_then(|s| serde_json::to_vec(s).ok())
            .map(|b| sha256_hex(&b))
            .unwrap_or_else(|| sha256_hex(b"no-shadow"));

        let mut updated_rev = {
            let tx = mgmt.transactions.get_mut(tx_id).unwrap();
            tx.authorization = Some(AuthorizationRecord {
                operator_id: req.operator_id,
                role: role_str.to_string(),
                package_hash: pkg_hash.clone(),
                diff_hash: rule_hash,
                evidence_digest: ev_digest,
                expires_logical_ns: now_ns.saturating_add(req.ttl_ms.saturating_mul(1_000_000)),
                authorized_at_ns: now_ns,
            });
            tx.revision.state = DeploymentState::Armed;
            tx.revision.clone()
        };
        updated_rev.state = DeploymentState::Armed;

        if let Err(e) = self.storage_handle.append_revision_sync(updated_rev) {
            return json_resp(503, &json!({"error": "STORAGE_DEGRADED", "detail": e}));
        }
        mgmt.armed_tx_id = Some(tx_id.to_string());
        mgmt.push_audit(actor, "authorize", Some(tx_id), Some(&pkg_hash), &live_rev.to_string(), "armed");
        let rec = mgmt.transactions.get(tx_id).unwrap();
        json_resp(200, rec)
    }

    fn handle_activate(&self, actor: &str, tx_id: &str) -> (u16, &'static str, Vec<u8>) {
        if !self.scalars.armed.load(Ordering::Acquire) {
            return json_resp(409, &json!({"error": "CONTROL_DISARMED"}));
        }
        if self.scalars.needs_operator_review.load(Ordering::Acquire) {
            return json_resp(409, &json!({"error": "RECOVERY_REVIEW_REQUIRED"}));
        }

        let live_rev = self.scalars.active_revision.load(Ordering::Acquire);
        let now_ns = self.clock.now_ns();

        let (pkg, rev_copy, migration, pub_hex, prev_pkg_opt) = {
            let mgmt = self.mgmt.lock().unwrap();
            let Some(tx) = mgmt.transactions.get(tx_id) else {
                return json_resp(404, &json!({"error": "UNKNOWN_TX"}));
            };
            if live_rev == tx.revision.revision {
                return json_resp(200, &json!({"txId": tx_id, "status": "already_active"}));
            }
            if tx.revision.state != DeploymentState::Armed {
                return json_resp(409, &json!({"error": "NOT_ARMED", "state": tx.revision.state}));
            }
            if tx.revision.base_revision != live_rev {
                return json_resp(409, &json!({"error": "REVISION_CONFLICT"}));
            }
            let Some(ref auth) = tx.authorization else {
                return json_resp(409, &json!({"error": "NO_AUTHORIZATION"}));
            };
            if now_ns >= auth.expires_logical_ns || auth.package_hash != tx.revision.package_hash {
                return json_resp(409, &json!({"error": "AUTHORIZATION_EXPIRED"}));
            }
            let schema_arena =
                build_arena_schema(live_rev, mgmt.rev_packages.get(&live_rev).map(|(p, _)| p));
            let migration = match schema_arena.plan_migration(&tx.package, tx.revision.revision) {
                Ok(m) => m,
                Err(e) => return json_resp(422, &json!({"error": "INCOMPATIBLE_MIGRATION", "detail": e.to_string()})),
            };
            let prev = self.active_pkg_meta.lock().unwrap().clone();
            (
                tx.package.clone(),
                tx.revision.clone(),
                migration,
                mgmt.trusted_pub_hex.clone(),
                prev,
            )
        };

        // Pre-warm worker process 100% OFF-SCAN before touching RT thread!
        let prewarmed = match create_warmed_engine_off_scan(
            &self.worker_spec,
            "active",
            rev_copy.revision,
            &pkg,
            &pub_hex,
            &self.reaper_handle,
        ) {
            Ok(e) => e,
            Err(e) => {
                return json_resp(422, &json!({"error": "WORKER_WARM_FAILED", "detail": e.to_string()}));
            }
        };

        // Store target pkg in swap slot by updating active_pkg_meta & saving prev_stable in mgmt
        {
            let mut mgmt = self.mgmt.lock().unwrap();
            if let Some(prev) = prev_pkg_opt {
                mgmt.prev_stable = Some(prev);
            }
            mgmt.armed_tx_id = None;
            if let Some(tx) = mgmt.transactions.get_mut(tx_id) {
                tx.revision.state = DeploymentState::ActiveVolatile;
            }
            mgmt.push_audit(
                actor,
                "activate",
                Some(tx_id),
                Some(&rev_copy.package_hash),
                &rev_copy.revision.to_string(),
                "queued_boundary_swap",
            );
        }

        if self
            .cmd_tx
            .try_send(RtControlCmd::Swap(BoundarySwapCmd {
                tx_id: tx_id.to_string(),
                is_rollback: false,
                revision: rev_copy.clone(),
                package: pkg.clone(),
                migration,
                prewarmed_engine: prewarmed,
            }))
            .is_err()
        {
            return json_resp(429, &json!({"error": "SWAP_QUEUE_BUSY"}));
        }

        *self.active_pkg_meta.lock().unwrap() =
            Some((rev_copy.revision, pkg, rev_copy.package_hash));
        self.shadow_runner.clear();

        json_resp(202, &json!({"txId": tx_id, "status": "scheduled_at_boundary"}))
    }

    fn handle_rollback(&self, actor: &str, req: RollbackReq) -> (u16, &'static str, Vec<u8>) {
        if !self.scalars.armed.load(Ordering::Acquire) {
            return json_resp(409, &json!({"error": "CONTROL_DISARMED"}));
        }

        let live_rev = self.scalars.active_revision.load(Ordering::Acquire);
        let (target_pkg, target_hash, new_rev, pub_hex, tx_id) = {
            let mut mgmt = self.mgmt.lock().unwrap();
            let Some((_, prev_pkg, prev_hash)) = mgmt.prev_stable.clone() else {
                return json_resp(409, &json!({"error": "NO_PREVIOUS_STABLE_PACKAGE"}));
            };
            mgmt.highest_revision += 1;
            let new_rev_num = mgmt.highest_revision;
            let tid = req
                .tx_id
                .unwrap_or_else(|| format!("rollback-r{new_rev_num}"));
            let target_pkg = prev_pkg;
            let rev = DeploymentRevision {
                tx_id: tid.clone(),
                device_id: mgmt.device_id.clone(),
                boot_id: mgmt.boot_id,
                revision: new_rev_num,
                base_revision: live_rev,
                package_hash: prev_hash.clone(),
                logic_hash: target_pkg
                    .wasm_digest
                    .clone()
                    .unwrap_or_else(|| target_pkg.ir_digest.clone()),
                rule_set_hash: target_pkg.ir_digest.clone(),
                profile_hash: target_pkg.profile_hash.clone(),
                state_schema: target_pkg.state_schema.clone(),
                state: DeploymentState::Recovering,
                active_scan_id: 0,
            };
            mgmt.rev_packages
                .insert(new_rev_num, (target_pkg.clone(), prev_hash.clone()));
            mgmt.transactions.insert(
                tid.clone(),
                TransactionRecord {
                    tx_id: tid.clone(),
                    revision: rev.clone(),
                    request_digest: prev_hash.clone(),
                    package: target_pkg.clone(),
                    shadow_report: None,
                    authorization: None,
                    reason: req.reason.clone(),
                },
            );
            mgmt.push_audit(
                actor,
                "rollback",
                Some(&tid),
                Some(&prev_hash),
                &new_rev_num.to_string(),
                &req.reason,
            );
            (target_pkg, prev_hash, rev, mgmt.trusted_pub_hex.clone(), tid)
        };

        let schema_arena = build_arena_schema(live_rev, Some(&target_pkg));
        let migration = match schema_arena.plan_migration(&target_pkg, new_rev.revision) {
            Ok(m) => m,
            Err(e) => {
                return json_resp(
                    422,
                    &json!({"error": "ROLLBACK_MIGRATION_FAILED", "detail": e.to_string()}),
                );
            }
        };

        let prewarmed = match create_warmed_engine_off_scan(
            &self.worker_spec,
            "rollback",
            new_rev.revision,
            &target_pkg,
            &pub_hex,
            &self.reaper_handle,
        ) {
            Ok(e) => e,
            Err(e) => {
                return json_resp(
                    500,
                    &json!({"error": "ROLLBACK_WORKER_SPAWN_FAILED", "detail": e.to_string()}),
                );
            }
        };

        if self
            .cmd_tx
            .try_send(RtControlCmd::Swap(BoundarySwapCmd {
                tx_id: tx_id.clone(),
                is_rollback: true,
                revision: new_rev.clone(),
                package: target_pkg.clone(),
                migration,
                prewarmed_engine: prewarmed,
            }))
            .is_err()
        {
            return json_resp(429, &json!({"error": "SWAP_QUEUE_BUSY"}));
        }

        *self.active_pkg_meta.lock().unwrap() =
            Some((new_rev.revision, target_pkg, target_hash));
        self.shadow_runner.clear();

        json_resp(
            202,
            &json!({
                "txId": tx_id,
                "status": "rollback_scheduled",
                "targetRevision": new_rev.revision,
            }),
        )
    }
}

fn build_arena_schema(live_rev: u64, active_pkg: Option<&ModulePackageV1>) -> StateArena {
    let mut arena = StateArena::new(live_rev);
    if let Some(pkg) = active_pkg {
        for entry in &pkg.required_blocks {
            let mut b = control_abi::types::StateBlock::empty(entry.stable_id, entry.block_type);
            b.schema_version = entry.schema_version;
            b.retention_class = entry.retention_class;
            b.migration_policy = entry.migration_policy;
            arena.blocks.push(b);
        }
    }
    arena
}

fn can_author(role: Role) -> bool {
    matches!(role, Role::Author | Role::Engineer)
}

fn can_operate(role: Role) -> bool {
    matches!(role, Role::Operator | Role::Engineer)
}

fn role_label(role: Role) -> String {
    format!("{role:?}")
}

fn json_resp<T: Serialize>(code: u16, val: &T) -> (u16, &'static str, Vec<u8>) {
    let body = serde_json::to_vec(val).unwrap_or_else(|_| b"{}".to_vec());
    (code, "application/json; charset=utf-8", body)
}

/// Bounded HTTP server with fixed worker thread pool + inflight cap (`429 BUSY` when saturated).
pub fn serve_decoupled_http(listener: TcpListener, rt: Arc<DecoupledRuntime>, stop: Arc<AtomicBool>) {
    const MAX_WORKERS: usize = 8;
    const MAX_QUEUE: usize = 16;

    let _ = listener.set_nonblocking(true);
    let (conn_tx, conn_rx) = mpsc::sync_channel::<TcpStream>(MAX_QUEUE);
    let conn_rx = Arc::new(Mutex::new(conn_rx));
    let inflight = Arc::new(AtomicUsize::new(0));

    let mut workers = Vec::with_capacity(MAX_WORKERS);
    for idx in 0..MAX_WORKERS {
        let rx = Arc::clone(&conn_rx);
        let rt_w = Arc::clone(&rt);
        let st_w = Arc::clone(&stop);
        let infl_w = Arc::clone(&inflight);
        let h = thread::Builder::new()
            .name(format!("api-worker-{idx}"))
            .spawn(move || {
                while !st_w.load(Ordering::Acquire) {
                    let msg = {
                        let guard = rx.lock().unwrap();
                        guard.recv_timeout(Duration::from_millis(50))
                    };
                    match msg {
                        Ok(stream) => {
                            let _ = stream.set_nonblocking(false);
                            handle_http_client(&rt_w, stream);
                            infl_w.fetch_sub(1, Ordering::Release);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .expect("spawn api worker");
        workers.push(h);
    }

    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let cur = inflight.fetch_add(1, Ordering::AcqRel);
                if cur >= MAX_QUEUE + MAX_WORKERS {
                    inflight.fetch_sub(1, Ordering::Release);
                    let _ = stream.write_all(
                        b"HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"error\":\"BUSY\"}",
                    );
                    continue;
                }
                match conn_tx.try_send(stream) {
                    Ok(()) => {}
                    Err(mpsc::TrySendError::Full(mut s)) => {
                        inflight.fetch_sub(1, Ordering::Release);
                        let _ = s.write_all(
                            b"HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"error\":\"BUSY\"}",
                        );
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        inflight.fetch_sub(1, Ordering::Release);
                        break;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }

    drop(conn_tx);
    for w in workers {
        let _ = w.join();
    }
}

fn handle_http_client(rt: &DecoupledRuntime, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let mut raw = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let mut header_end = None;
    let mut content_len = 0usize;

    loop {
        if raw.len() > 1024 * 1024 {
            let _ = stream.write_all(b"HTTP/1.1 413 Payload Too Large\r\nConnection: close\r\n\r\n");
            return;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if header_end.is_none() {
                    if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                        let hdr_str = String::from_utf8_lossy(&raw[..pos]);
                        for line in hdr_str.lines() {
                            if let Some((k, v)) = line.split_once(':') {
                                if k.trim().eq_ignore_ascii_case("content-length") {
                                    content_len = v.trim().parse().unwrap_or(0);
                                }
                            }
                        }
                    }
                }
                if let Some(he) = header_end {
                    if raw.len() >= he + content_len {
                        break;
                    }
                }
            }
            Err(_) => return,
        }
    }

    let Some(he) = header_end else {
        return;
    };
    let hdr_str = String::from_utf8_lossy(&raw[..he - 4]);
    let mut lines = hdr_str.lines();
    let Some(req_line) = lines.next() else {
        return;
    };
    let mut req_parts = req_line.split_whitespace();
    let method = req_parts.next().unwrap_or("GET");
    let raw_path = req_parts.next().unwrap_or("/");
    let path = raw_path.split('?').next().unwrap_or(raw_path);

    let mut auth_header: Option<&str> = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("authorization") {
                auth_header = Some(v.trim());
            }
        }
    }

    let end = (he + content_len).min(raw.len());
    let body = &raw[he..end];
    let (status, ctype, resp_body) = rt.handle_http_request(method, path, auth_header, body);
    let reason = match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        resp_body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&resp_body);
}
