use std::collections::VecDeque;
use std::sync::Arc;

use control_abi::schema::{EvidenceBundle, ModulePackageV1, SiteProfileV1};
use control_abi::types::{
    DriverStatus, EvalStatus, GuardReason, InputSignal, OutputFrame, WorkerMode,
    ABI_VERSION, SIGNAL_TEMP_PROCESS,
};
use control_abi::wire::{EvalRequest, EvalResult};
use field_io::FieldIoDriver;
use logic_worker::engine::LogicEngine;

use crate::arena::StateArena;
use crate::clock::ControlClock;
use crate::deploy::{DeploymentManager, PendingActivation, ShadowReport};
use crate::shield::{ManualLease, ReactiveShield};

#[derive(Debug, Clone)]
pub struct ScanTelemetryRow {
    pub boot_id: u64,
    pub scan_id: u64,
    pub release_ns: u64,
    pub start_ns: u64,
    pub input_seq: u64,
    pub eval_start_ns: u64,
    pub eval_end_ns: u64,
    pub decision_ns: u64,
    pub publish_ns: u64,
    pub ack_driver_ns: u64,
    pub active_rev: u64,
    pub logic_hash: String,
    pub lease_epoch: u64,
    pub deadline_miss: bool,
    pub fallback_reason: GuardReason,
    pub desired_mask: u32,
    pub applied_mask: u32,
    pub observed_mask: Option<u32>,
    pub signals: Vec<InputSignal>,
}

/// Coordinates deterministic boundary scans, active/shadow evaluators, reactive shield,
/// field I/O publication, and boundary hot-swap transitions (§5.2).
pub struct ScanSupervisor<C: ControlClock, D: FieldIoDriver> {
    pub clock: Arc<C>,
    pub driver: D,
    pub profile: SiteProfileV1,
    pub arena: StateArena,
    pub shield: ReactiveShield,
    pub deploy: DeploymentManager,

    pub boot_id: u64,
    pub scan_id: u64,
    pub lease_epoch: u64,
    pub generation: u64,
    pub output_seq: u64,

    pub control_armed: bool,
    pub manual_lease: Option<ManualLease>,

    pub active_engine: Option<Box<dyn LogicEngine>>,
    pub standby_engine: Option<Box<dyn LogicEngine>>,
    pub shadow_engine: Option<Box<dyn LogicEngine>>,
    pub shadow_blocks: Vec<control_abi::StateBlock>,
    pub shadow_mismatches: u64,
    pub shadow_scans: u64,
    pub shadow_errors: u64,

    pub pending_activation: Option<PendingActivation>,
    pub last_evidence: Option<EvidenceBundle>,
    pub recent_scans: VecDeque<ScanTelemetryRow>,
    pub missed_releases: u64,
    next_release_ns: u64,
}

impl<C: ControlClock, D: FieldIoDriver> ScanSupervisor<C, D> {
    pub fn new(
        clock: Arc<C>,
        driver: D,
        profile: SiteProfileV1,
        deploy: DeploymentManager,
        boot_id: u64,
    ) -> Self {
        let initial_rev = deploy.active_revision;
        let shield = ReactiveShield::from_profile(&profile);
        let mut arena = StateArena::new(initial_rev);
        if let Some(ref pkg) = deploy.active_package {
            let init_plan = crate::arena::PreparedMigration {
                target_revision: initial_rev,
                entries: pkg.required_blocks.clone(),
            };
            arena.apply_migration(&init_plan, 0);
        }
        let now_ns = clock.now_ns();
        Self {
            clock,
            driver,
            profile,
            arena,
            shield,
            deploy,
            boot_id,
            scan_id: 0,
            lease_epoch: 1,
            generation: 1,
            output_seq: 0,
            control_armed: false,
            manual_lease: None,
            active_engine: None,
            standby_engine: None,
            shadow_engine: None,
            shadow_blocks: Vec::new(),
            shadow_mismatches: 0,
            shadow_scans: 0,
            shadow_errors: 0,
            pending_activation: None,
            last_evidence: None,
            recent_scans: VecDeque::with_capacity(256),
            missed_releases: 0,
            next_release_ns: now_ns,
        }
    }

    pub fn arm_control(&mut self) {
        self.deploy.rearm_operator();
        self.control_armed = true;
    }

    pub fn disarm_and_safe_off(&mut self) {
        self.control_armed = false;
        let now = self.clock.now_ns();
        self.driver.force_safe_off(now);
    }

    pub fn install_active_engine(&mut self, engine: Box<dyn LogicEngine>) {
        self.lease_epoch += 1;
        self.generation += 1;
        self.active_engine = Some(engine);
    }

    pub fn install_shadow_engine(&mut self, engine: Box<dyn LogicEngine>) {
        self.shadow_blocks = self.arena.blocks.clone();
        self.shadow_scans = 0;
        self.shadow_mismatches = 0;
        self.shadow_errors = 0;
        self.shadow_engine = Some(engine);
    }

    /// Runs a Shadow simulation / replay verification across a trace of `(logical_ns, signals)`
    /// using a private arena copy so active field I/O is never touched (§5.5 step 3 & F04).
    pub fn run_shadow_replay(
        &self,
        shadow: &mut dyn LogicEngine,
        candidate: &ModulePackageV1,
        trace: &[(u64, Vec<InputSignal>)],
    ) -> ShadowReport {
        if let Err(e) = shadow
            .prepare(candidate, &self.deploy.trusted_pub_hex)
            .and_then(|_| shadow.warm())
        {
            return ShadowReport {
                passed: false,
                scans_executed: 0,
                mismatches: 0,
                vetoes: 0,
                errors: 1,
                first_counterexample: Some(format!("shadow prepare/warm failed: {e}")),
            };
        }

        let mut priv_blocks = self.arena.blocks.clone();
        let mut temp_shield = ReactiveShield::from_profile(&self.profile);
        let mut vetoes = 0u64;
        let mut errors = 0u64;
        let mut first_ce = None;
        let eval_budget_ns = (self.profile.eval_deadline_ms as u64) * 1_000_000;

        for (idx, (t_ns, sigs)) in trace.iter().enumerate() {
            let req = EvalRequest {
                abi_version: ABI_VERSION,
                mode: WorkerMode::Shadow,
                boot_id: self.boot_id,
                lease_epoch: self.lease_epoch,
                generation: self.generation,
                scan_id: (idx as u64) + 1,
                logical_ns: *t_ns,
                input_seq: (idx as u64) + 1,
                state_revision: self.arena.state_revision,
                applied_output_mask: temp_shield.prev_applied_mask(),
                signals: sigs.clone(),
                blocks: priv_blocks.clone(),
            };
            match shadow.eval_sync(&req) {
                Ok(res) => {
                    if res.status != EvalStatus::Ok || (res.eval_duration_ns as u64) > eval_budget_ns {
                        errors += 1;
                        if first_ce.is_none() {
                            first_ce = Some(format!(
                                "scan {} status={:?} dur_ns={}",
                                idx + 1,
                                res.status,
                                res.eval_duration_ns
                            ));
                        }
                        break;
                    }
                    priv_blocks = res.blocks;
                    let mut no_lease = None;
                    let dec = temp_shield.evaluate(
                        *t_ns,
                        self.boot_id,
                        true,
                        false,
                        true,
                        true,
                        sigs,
                        Some(&candidate.rule_set),
                        &mut no_lease,
                        res.desired_mask,
                    );
                    if dec.guard_reason == GuardReason::InterlockVeto {
                        vetoes += 1;
                    }
                }
                Err(e) => {
                    errors += 1;
                    if first_ce.is_none() {
                        first_ce = Some(format!("scan {} err: {e}", idx + 1));
                    }
                    break;
                }
            }
        }

        ShadowReport {
            passed: errors == 0,
            scans_executed: trace.len() as u64,
            mismatches: 0,
            vetoes,
            errors,
            first_counterexample: first_ce,
        }
    }

    /// Executes exactly ONE scan cycle at the given release boundary (§5.2).
    pub fn step_once(&mut self) -> ScanTelemetryRow {
        let period_ns = (self.profile.scan_period_ms as u64) * 1_000_000;
        let eval_cutoff_rel_ns = (self.profile.eval_deadline_ms as u64) * 1_000_000;
        let publish_expiry_rel_ns = period_ns;

        let raw_release_ns = self.next_release_ns;
        let start_ns = self.clock.now_ns();
        let release_ns = if start_ns >= raw_release_ns + period_ns {
            let skipped = (start_ns - raw_release_ns) / period_ns;
            self.missed_releases += skipped;
            let aligned = raw_release_ns + skipped * period_ns;
            self.next_release_ns = aligned + period_ns;
            aligned
        } else {
            self.next_release_ns = raw_release_ns + period_ns;
            raw_release_ns
        };
        self.scan_id += 1;
        let scan_id = self.scan_id;

        // 1. Sample coherent input snapshot
        let (input_seq, signals) = self.driver.sample_inputs(start_ns);
        let io_healthy = self.driver.health().healthy;

        // 2. Execute authorized boundary hot-swap if pending
        let mut swapped_pkg_here = false;
        if let Some(pending) = self.pending_activation.take() {
            if self.control_armed && !self.deploy.fallback_latched && io_healthy {
                let mut can_swap = true;
                if let Some(prewarmed) = self.standby_engine.take() {
                    if let Some(mut old_eng) = self.active_engine.take() {
                        old_eng.revoke();
                    }
                    // Standby worker is already prepared & warmed off-scan -> O(1) boundary flip
                    self.active_engine = Some(prewarmed);
                    self.generation += 1;
                } else if let Some(ref mut eng) = self.active_engine {
                    if eng
                        .prepare(&pending.package, &self.deploy.trusted_pub_hex)
                        .and_then(|_| eng.warm())
                        .is_err()
                    {
                        can_swap = false;
                    }
                }
                if can_swap {
                    self.arena.apply_migration(&pending.migration, start_ns);
                    self.lease_epoch += 1;
                    let _ = self.deploy.commit_boundary_activation(&pending, scan_id);
                    swapped_pkg_here = true;
                } else {
                    self.deploy.fallback_latched = true;
                }
            }
        }

        let eval_start_ns = self.clock.now_ns();
        let active_rev = self.deploy.active_revision;
        let logic_hash = self
            .deploy
            .active_package
            .as_ref()
            .and_then(|p| p.wasm_digest.clone().or_else(|| Some(p.ir_digest.clone())))
            .unwrap_or_else(|| "none".into());

        // 3. Dispatch active evaluator & check cutoff deadline
        let mut worker_ok = false;
        let mut eval_result: Option<EvalResult> = None;
        if let Some(ref mut eng) = self.active_engine {
            let req = EvalRequest {
                abi_version: ABI_VERSION,
                mode: WorkerMode::Active,
                boot_id: self.boot_id,
                lease_epoch: self.lease_epoch,
                generation: self.generation,
                scan_id,
                logical_ns: start_ns,
                input_seq,
                state_revision: self.arena.state_revision,
                applied_output_mask: self.arena.last_applied_output_mask,
                signals: signals.clone(),
                blocks: self.arena.blocks.clone(),
            };
            if eng.dispatch(&req).is_ok() {
                if let Ok(res) = eng.read_result() {
                    let after_eval = self.clock.now_ns();
                    let within_cutoff =
                        after_eval <= release_ns.saturating_add(eval_cutoff_rel_ns);
                    if within_cutoff
                        && res.status == EvalStatus::Ok
                        && res.lease_epoch == self.lease_epoch
                        && res.generation == self.generation
                        && res.scan_id == scan_id
                    {
                        worker_ok = true;
                        eval_result = Some(res);
                    } else {
                        eng.revoke();
                    }
                }
            }
        } else {
            // No engine attached yet -> worker not ready
            worker_ok = false;
        }
        let eval_end_ns = self.clock.now_ns();
        let deadline_miss = eval_end_ns > release_ns.saturating_add(eval_cutoff_rel_ns);

        // 4. Offer non-blocking snapshot to Shadow (never touches output publisher!)
        if let Some(ref mut shadow) = self.shadow_engine {
            let sreq = EvalRequest {
                abi_version: ABI_VERSION,
                mode: WorkerMode::Shadow,
                boot_id: self.boot_id,
                lease_epoch: self.lease_epoch,
                generation: self.generation,
                scan_id,
                logical_ns: start_ns,
                input_seq,
                state_revision: self.arena.state_revision,
                applied_output_mask: self.arena.last_applied_output_mask,
                signals: signals.clone(),
                blocks: self.shadow_blocks.clone(),
            };
            if shadow.dispatch(&sreq).is_ok() {
                if let Ok(sres) = shadow.read_result() {
                    self.shadow_scans += 1;
                    self.shadow_blocks = sres.blocks;
                    if let Some(ref ares) = eval_result {
                        if ares.desired_mask != sres.desired_mask {
                            self.shadow_mismatches += 1;
                        }
                    }
                } else {
                    self.shadow_errors += 1;
                }
            } else {
                self.shadow_errors += 1;
            }
        }

        // 5. Reactive Shield & state confirmation / fallback reconciliation
        let proposed_mask = eval_result.as_ref().map(|r| r.desired_mask).unwrap_or(0);
        let active_rs = self.deploy.active_package.as_ref().map(|p| &p.rule_set);

        let decision = self.shield.evaluate(
            start_ns,
            self.boot_id,
            self.control_armed,
            self.deploy.fallback_latched,
            io_healthy,
            worker_ok,
            &signals,
            active_rs,
            &mut self.manual_lease,
            proposed_mask,
        );

        if decision.is_contingency_off || !worker_ok {
            self.arena.reconcile_fallback();
            if swapped_pkg_here && !worker_ok {
                // First scan of newly promoted v2 failed -> immediate rollback eligibility / fault flag
                self.deploy.fallback_latched = true;
            }
        } else if let Some(res) = eval_result {
            let valid_blocks = res.state_revision == self.arena.state_revision
                && res.blocks.len() <= control_abi::MAX_BLOCKS
                && res.blocks.len() == self.arena.blocks.len()
                && res
                    .blocks
                    .iter()
                    .zip(self.arena.blocks.iter())
                    .all(|(nb, ab)| {
                        nb.stable_id == ab.stable_id
                            && nb.block_type == ab.block_type
                            && nb.schema_version == ab.schema_version
                    });
            if valid_blocks || self.arena.blocks.is_empty() {
                self.arena.blocks = res.blocks;
            } else {
                self.arena.reconcile_fallback();
                self.deploy.fallback_latched = true;
            }
        }
        let decision_ns = self.clock.now_ns();

        // 6. Publish OutputFrame to driver with monotonic seq & expiration
        self.output_seq += 1;
        let out_frame = OutputFrame {
            boot_id: self.boot_id,
            scan_id,
            lease_epoch: self.lease_epoch,
            active_revision: active_rev,
            output_seq: self.output_seq,
            desired_mask: decision.desired_mask,
            applied_mask: decision.applied_mask,
            expiry_ns: release_ns.saturating_add(publish_expiry_rel_ns),
            guard_reason: decision.guard_reason,
        };
        let publish_ns = self.clock.now_ns();
        let obs = self.driver.apply_output(&out_frame, publish_ns);
        let ack_driver_ns = self.clock.now_ns();

        if obs.driver_status == DriverStatus::Applied {
            self.arena.last_applied_output_mask = decision.applied_mask;
        } else {
            self.deploy.fallback_latched = true;
            self.driver.force_safe_off(ack_driver_ns);
            self.arena.last_applied_output_mask = 0;
        }

        let persisted = self
            .deploy
            .active_package_hash
            .as_ref()
            .map(|_| !self.deploy.needs_operator_review)
            .unwrap_or(false);

        self.last_evidence = Some(EvidenceBundle {
            boot_id: self.boot_id,
            scan_id,
            active_revision: active_rev,
            output_seq: self.output_seq,
            selected_in_scan: worker_ok,
            guard_reason: decision.guard_reason,
            desired_mask: decision.desired_mask,
            applied_mask: decision.applied_mask,
            driver_applied: obs.driver_status,
            physical_observed_mask: obs.observed_mask,
            physical_observation_source: obs.source,
            active_persisted: persisted,
            observed_at_ns: obs.observed_at_ns,
            expires_at_ns: out_frame.expiry_ns,
        });

        let row = ScanTelemetryRow {
            boot_id: self.boot_id,
            scan_id,
            release_ns,
            start_ns,
            input_seq,
            eval_start_ns,
            eval_end_ns,
            decision_ns,
            publish_ns,
            ack_driver_ns,
            active_rev,
            logic_hash,
            lease_epoch: self.lease_epoch,
            deadline_miss,
            fallback_reason: decision.guard_reason,
            desired_mask: decision.desired_mask,
            applied_mask: decision.applied_mask,
            observed_mask: obs.observed_mask,
            signals,
        };
        if self.recent_scans.len() >= 256 {
            self.recent_scans.pop_front();
        }
        self.recent_scans.push_back(row.clone());
        row
    }

    pub fn last_temp_signal(&self) -> Option<InputSignal> {
        self.recent_scans
            .back()
            .and_then(|r| r.signals.iter().find(|s| s.id == SIGNAL_TEMP_PROCESS).copied())
    }
}
