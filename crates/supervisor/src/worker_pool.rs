use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use control_abi::schema::ModulePackageV1;
use control_abi::types::{EvalStatus, StateBlock, WorkerMode, ABI_VERSION};
use control_abi::wire::{EvalRequest, EvalResult};
use logic_worker::engine::{
    A0NativeIrEngine, A1WamrEngine, A2QuickJsEngine, EngineCapabilities, EngineKind, LogicEngine,
    WorkerError,
};
use logic_worker::ipc::{WorkerInitPayload, WorkerProcessClient, WorkerSandboxParams};
use logic_worker::ir_eval::initial_blocks_for_ruleset;
use logic_worker::sandbox::SandboxReport;

static SLOT_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_slot_name(base: &str, gen: u64) -> String {
    let seq = SLOT_SEQ.fetch_add(1, Ordering::Relaxed);
    let sanitized: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let prefix = if sanitized.is_empty() { "slot" } else { &sanitized };
    format!("{prefix}-g{gen}-{seq}")
}

pub fn engine_kind_from_str(s: &str) -> Result<EngineKind, String> {
    match s {
        "empty-noop" => Ok(EngineKind::EmptyNoop),
        "native-ir" => Ok(EngineKind::NativeIr),
        "wamr-interp" => Ok(EngineKind::WamrInterp),
        "wamr-aot" => Ok(EngineKind::WamrAot),
        "quickjs" | "quick-js" => Ok(EngineKind::QuickJs),
        "wasmtime" => Ok(EngineKind::Wasmtime),
        other => Err(format!("unknown engine kind '{other}'")),
    }
}

/// Validates that all mandatory Linux OS sandbox primitives are enforced on a worker process.
pub fn validate_linux_sandbox_policy(report: &SandboxReport) -> Result<(), String> {
    let mut missing = Vec::new();
    if !report.cgroup_applied {
        missing.push("cgroup2");
    }
    if !report.seccomp_applied {
        missing.push("seccomp_bpf");
    }
    if !report.uid_dropped || report.effective_uid != 65534 {
        missing.push("uid_nobody_65534");
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "strict sandbox violation: missing [{}]; note='{}'",
            missing.join(", "),
            report.note
        ))
    }
}

/// Validates OS sandbox posture according to target OS (`Linux` strictly verifies cgroup+seccomp+nobody).
pub fn strict_sandbox_satisfied(report: &SandboxReport) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        validate_linux_sandbox_policy(report)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = report;
        Ok(())
    }
}

/// Resolves the `plc-worker` executable path from CLI override, env var, sibling exe, or `$PATH`.
pub fn find_worker_binary(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    if let Ok(env_p) = env::var("PLC_WORKER_BIN") {
        let p = PathBuf::from(env_p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(cur) = env::current_exe() {
        for anc in cur.ancestors().skip(1).take(6) {
            let candidate = anc.join("plc-worker");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            let candidate = dir.join("plc-worker");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct WorkerSpawnSpec {
    pub engine_kind: EngineKind,
    pub worker_bin: Option<PathBuf>,
    pub in_process: bool,
    pub force_worker: bool,
    pub strict_sandbox: bool,
    pub eval_budget_ns: u64,
    pub eval_deadline_ms: u64,
    pub memory_max_bytes: u64,
    pub rt_fifo_active_worker: bool,
    pub extra_args: Vec<String>,
}

impl Default for WorkerSpawnSpec {
    fn default() -> Self {
        #[cfg(target_os = "linux")]
        let default_strict = unsafe { libc::geteuid() == 0 };
        #[cfg(not(target_os = "linux"))]
        let default_strict = false;

        Self {
            engine_kind: EngineKind::WamrAot,
            worker_bin: None,
            in_process: false,
            force_worker: false,
            strict_sandbox: default_strict,
            eval_budget_ns: 5_000_000,
            eval_deadline_ms: 5,
            memory_max_bytes: 16 * 1024 * 1024,
            rt_fifo_active_worker: false,
            extra_args: Vec::new(),
        }
    }
}

impl WorkerSpawnSpec {
    pub fn should_isolate_active(&self) -> bool {
        if self.in_process {
            return false;
        }
        if self.force_worker {
            return true;
        }
        !matches!(self.engine_kind, EngineKind::NativeIr | EngineKind::EmptyNoop)
    }
}

enum ReapCommand {
    Kill(WorkerProcessClient),
    Shutdown,
}

#[derive(Clone)]
pub struct WorkerReaperHandle {
    tx: mpsc::SyncSender<ReapCommand>,
}

impl WorkerReaperHandle {
    /// Moves a revoked/stale worker to the background reaper without waiting or blocking the RT loop.
    pub fn submit(&self, mut client: WorkerProcessClient) {
        if let Err(err) = self.tx.try_send(ReapCommand::Kill(client)) {
            client = match err {
                mpsc::TrySendError::Full(ReapCommand::Kill(c))
                | mpsc::TrySendError::Disconnected(ReapCommand::Kill(c)) => c,
                _ => return,
            };
            // Fallback off-scan cleanup thread only when bounded channel is full/closed
            let _ = thread::Builder::new()
                .name("worker-reap-fallback".into())
                .spawn(move || {
                    client.revoke_and_kill();
                });
        }
    }
}

/// Off-scan process reaper that owns termination (`SIGKILL` + `waitpid` + cgroup rmdir).
pub struct WorkerReaper {
    tx: mpsc::SyncSender<ReapCommand>,
    handle: Option<JoinHandle<()>>,
}

impl WorkerReaper {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::sync_channel::<ReapCommand>(128);
        let handle = thread::Builder::new()
            .name("worker-reaper".into())
            .spawn(move || {
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        ReapCommand::Kill(mut client) => {
                            client.revoke_and_kill();
                            drop(client);
                        }
                        ReapCommand::Shutdown => break,
                    }
                }
                while let Ok(cmd) = rx.try_recv() {
                    if let ReapCommand::Kill(mut client) = cmd {
                        client.revoke_and_kill();
                        drop(client);
                    }
                }
            })
            .expect("spawn worker-reaper thread");
        Self {
            tx,
            handle: Some(handle),
        }
    }

    pub fn handle(&self) -> WorkerReaperHandle {
        WorkerReaperHandle {
            tx: self.tx.clone(),
        }
    }

    pub fn shutdown(&mut self) {
        let _ = self.tx.try_send(ReapCommand::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for WorkerReaper {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Out-of-process `plc-worker` implementation of `LogicEngine`.
/// Never spawns, kills, or calls `waitpid` on the scan thread: all cleanup is handed off to `WorkerReaper`.
pub struct IsolatedWorkerEngine {
    kind: EngineKind,
    worker_bin: PathBuf,
    slot_base: String,
    generation: u64,
    eval_budget_ns: u64,
    default_deadline: Duration,
    memory_max_bytes: u64,
    strict_sandbox: bool,
    drop_to_nobody: bool,
    enable_seccomp: bool,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    rt_fifo_active_worker: bool,
    extra_args: Vec<String>,
    reaper: WorkerReaperHandle,
    client: Option<WorkerProcessClient>,
    prepared_pkg: Option<(ModulePackageV1, String)>,
    pending_sync_req: Option<EvalRequest>,
}

impl IsolatedWorkerEngine {
    pub fn spawn_warmed(
        spec: &WorkerSpawnSpec,
        slot_base: &str,
        generation: u64,
        pkg: &ModulePackageV1,
        trusted_pub_hex: &str,
        reaper: WorkerReaperHandle,
    ) -> Result<Self, WorkerError> {
        let worker_bin = find_worker_binary(spec.worker_bin.as_deref()).ok_or_else(|| {
            WorkerError::Runtime(
                "plc-worker binary not found (set --worker-bin or PLC_WORKER_BIN)".into(),
            )
        })?;
        let mut eng = Self::new_unspawned(spec, worker_bin, slot_base, generation, reaper);
        eng.prepare(pkg, trusted_pub_hex)?;
        eng.warm()?;
        Ok(eng)
    }

    pub fn new_unspawned(
        spec: &WorkerSpawnSpec,
        worker_bin: PathBuf,
        slot_base: &str,
        generation: u64,
        reaper: WorkerReaperHandle,
    ) -> Self {
        Self {
            kind: spec.engine_kind,
            worker_bin,
            slot_base: slot_base.to_string(),
            generation,
            eval_budget_ns: spec.eval_budget_ns,
            default_deadline: Duration::from_millis(spec.eval_deadline_ms.max(1)),
            memory_max_bytes: spec.memory_max_bytes,
            strict_sandbox: spec.strict_sandbox,
            #[cfg(target_os = "linux")]
            drop_to_nobody: spec.strict_sandbox || unsafe { libc::geteuid() == 0 },
            #[cfg(not(target_os = "linux"))]
            drop_to_nobody: false,
            #[cfg(target_os = "linux")]
            enable_seccomp: true,
            #[cfg(not(target_os = "linux"))]
            enable_seccomp: false,
            rt_fifo_active_worker: spec.rt_fifo_active_worker,
            extra_args: spec.extra_args.clone(),
            reaper,
            client: None,
            prepared_pkg: None,
            pending_sync_req: None,
        }
    }

    fn retire_client_off_scan(&mut self) {
        if let Some(c) = self.client.take() {
            self.reaper.submit(c);
        }
    }
}

impl LogicEngine for IsolatedWorkerEngine {
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            kind: self.kind,
            uses_aot: matches!(self.kind, EngineKind::WamrAot | EngineKind::Wasmtime),
            supports_fuel_or_interrupt: matches!(
                self.kind,
                EngineKind::QuickJs | EngineKind::Wasmtime
            ),
            max_linear_mem_bytes: self.memory_max_bytes.min(u32::MAX as u64) as u32,
        }
    }

    fn prepare(&mut self, pkg: &ModulePackageV1, trusted_pub_hex: &str) -> Result<(), WorkerError> {
        self.retire_client_off_scan();
        self.prepared_pkg = Some((pkg.clone(), trusted_pub_hex.to_string()));
        Ok(())
    }

    fn warm(&mut self) -> Result<(), WorkerError> {
        let (pkg, pub_hex) = self.prepared_pkg.clone().ok_or(WorkerError::NotReady)?;
        self.retire_client_off_scan();

        let slot = next_slot_name(&self.slot_base, self.generation);
        let init = WorkerInitPayload {
            engine: self.kind,
            package: pkg,
            trusted_pub_hex: pub_hex,
            eval_budget_ns: self.eval_budget_ns,
            sandbox: WorkerSandboxParams {
                slot_name: slot,
                memory_max_bytes: self.memory_max_bytes,
                drop_to_nobody: self.drop_to_nobody,
                enable_seccomp: self.enable_seccomp,
            },
        };
        let extra_refs: Vec<&str> = self.extra_args.iter().map(|s| s.as_str()).collect();
        let client = WorkerProcessClient::spawn(&self.worker_bin, &init, &extra_refs)?;
        if self.strict_sandbox {
            if let Err(msg) = strict_sandbox_satisfied(client.sandbox_report()) {
                self.reaper.submit(client);
                return Err(WorkerError::Verification(msg));
            }
        }
        #[cfg(target_os = "linux")]
        if self.rt_fifo_active_worker {
            unsafe {
                let param = libc::sched_param { sched_priority: 49 };
                let _ = libc::sched_setscheduler(
                    client.pid() as libc::pid_t,
                    libc::SCHED_FIFO,
                    &param,
                );
            }
        }
        self.client = Some(client);
        Ok(())
    }

    fn dispatch(&mut self, req: &EvalRequest) -> Result<(), WorkerError> {
        if self.client.is_none() {
            return Err(WorkerError::NotReady);
        }
        self.pending_sync_req = Some(req.clone());
        Ok(())
    }

    fn read_result(&mut self) -> Result<EvalResult, WorkerError> {
        let req = self.pending_sync_req.take().ok_or(WorkerError::NotReady)?;
        let dl = Instant::now() + self.default_deadline;
        self.eval_until(&req, dl)
    }

    fn eval_until(
        &mut self,
        req: &EvalRequest,
        deadline: Instant,
    ) -> Result<EvalResult, WorkerError> {
        let Some(client) = self.client.as_mut() else {
            return Err(WorkerError::NotReady);
        };
        if client.is_tainted() {
            self.retire_client_off_scan();
            return Err(WorkerError::Ipc("tainted worker discarded".into()));
        }
        match client.eval_until(req, deadline) {
            Ok(res) => Ok(res),
            Err(e) => {
                // Immediately detach tainted worker to background reaper without blocking scan thread
                self.retire_client_off_scan();
                Err(e)
            }
        }
    }

    fn revoke(&mut self) {
        self.pending_sync_req = None;
        self.retire_client_off_scan();
    }

    fn worker_pid(&self) -> Option<u32> {
        self.client.as_ref().map(|c| c.pid())
    }

    fn sandbox_report(&self) -> Option<SandboxReport> {
        self.client.as_ref().map(|c| c.sandbox_report().clone())
    }

    fn is_tainted(&self) -> bool {
        self.client.as_ref().map(|c| c.is_tainted()).unwrap_or(true)
    }
}

impl Drop for IsolatedWorkerEngine {
    fn drop(&mut self) {
        self.retire_client_off_scan();
    }
}

/// Spawns and warms either an isolated `plc-worker` child process or an in-process engine (off-scan).
pub fn create_warmed_engine_off_scan(
    spec: &WorkerSpawnSpec,
    slot_base: &str,
    generation: u64,
    pkg: &ModulePackageV1,
    trusted_pub_hex: &str,
    reaper: &WorkerReaperHandle,
) -> Result<Box<dyn LogicEngine>, WorkerError> {
    if spec.should_isolate_active() {
        let eng = IsolatedWorkerEngine::spawn_warmed(
            spec,
            slot_base,
            generation,
            pkg,
            trusted_pub_hex,
            reaper.clone(),
        )?;
        return Ok(Box::new(eng));
    }

    let mut eng: Box<dyn LogicEngine> = match spec.engine_kind {
        EngineKind::EmptyNoop => Box::new(A0NativeIrEngine::new_empty_noop()),
        EngineKind::NativeIr => Box::new(A0NativeIrEngine::new_ir()),
        EngineKind::WamrInterp => Box::new(A1WamrEngine::new(false)),
        EngineKind::WamrAot => Box::new(A1WamrEngine::new(true)),
        EngineKind::QuickJs => Box::new(A2QuickJsEngine::new(spec.eval_budget_ns)),
        EngineKind::Wasmtime => {
            return Err(WorkerError::Runtime(
                "wasmtime must use isolated worker or explicit feature build".into(),
            ));
        }
    };
    eng.prepare(pkg, trusted_pub_hex)?;
    eng.warm()?;
    Ok(eng)
}

#[derive(Debug, Clone)]
pub struct ShadowEvalSample {
    pub boot_id: u64,
    pub lease_epoch: u64,
    pub generation: u64,
    pub scan_id: u64,
    pub logical_ns: u64,
    pub input_seq: u64,
    pub state_revision: u64,
    pub applied_output_mask: u32,
    pub signals: Vec<control_abi::InputSignal>,
    pub active_desired_mask: Option<u32>,
    pub active_worker_ok: bool,
    pub eval_deadline_ms: u64,
}

#[derive(Debug, Default)]
pub struct ShadowMetrics {
    pub scans_executed: AtomicU64,
    pub mismatches: AtomicU64,
    pub errors: AtomicU64,
    pub dropped: AtomicU64,
    pub worker_pid: AtomicU32,
    pub installed_revision: AtomicU64,
    pub active_running: AtomicBool,
    pub first_counterexample: Mutex<Option<String>>,
    pub last_error: Mutex<Option<String>>,
}

impl ShadowMetrics {
    pub fn snapshot(&self) -> ShadowMetricsSnapshot {
        ShadowMetricsSnapshot {
            scans_executed: self.scans_executed.load(Ordering::Acquire),
            mismatches: self.mismatches.load(Ordering::Acquire),
            errors: self.errors.load(Ordering::Acquire),
            dropped: self.dropped.load(Ordering::Acquire),
            worker_pid: self.worker_pid.load(Ordering::Acquire),
            installed_revision: self.installed_revision.load(Ordering::Acquire),
            active_running: self.active_running.load(Ordering::Acquire),
            first_counterexample: self.first_counterexample.lock().unwrap().clone(),
            last_error: self.last_error.lock().unwrap().clone(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShadowMetricsSnapshot {
    pub scans_executed: u64,
    pub mismatches: u64,
    pub errors: u64,
    pub dropped: u64,
    pub worker_pid: u32,
    pub installed_revision: u64,
    pub active_running: bool,
    pub first_counterexample: Option<String>,
    pub last_error: Option<String>,
}

enum ShadowCtrlCmd {
    Install {
        pkg: ModulePackageV1,
        trusted_pub_hex: String,
        revision: u64,
        spec: WorkerSpawnSpec,
    },
    Clear,
    Shutdown,
}

/// Asynchronous non-blocking Shadow runner executing candidate rulesets strictly off the RT scan loop.
pub struct AsyncShadowRunner {
    eval_tx: mpsc::SyncSender<ShadowEvalSample>,
    cmd_tx: mpsc::Sender<ShadowCtrlCmd>,
    pub metrics: Arc<ShadowMetrics>,
    handle: Option<JoinHandle<()>>,
}

impl AsyncShadowRunner {
    pub fn start(reaper: WorkerReaperHandle) -> Self {
        // Depth 1: if Shadow worker is slow/wedged, RT thread drops offer cleanly and increments `dropped`.
        let (eval_tx, eval_rx) = mpsc::sync_channel::<ShadowEvalSample>(1);
        let (cmd_tx, cmd_rx) = mpsc::channel::<ShadowCtrlCmd>();
        let metrics = Arc::new(ShadowMetrics::default());
        let metrics_bg = Arc::clone(&metrics);

        let handle = thread::Builder::new()
            .name("shadow-runner".into())
            .spawn(move || {
                let mut shadow_eng: Option<Box<dyn LogicEngine>> = None;
                let mut shadow_blocks: Vec<StateBlock> = Vec::new();
                let mut shadow_gen: u64 = 1;
                let mut shadow_deadline_ms: u64 = 5;

                'outer: loop {
                    // Drain any control commands first
                    while let Ok(cmd) = cmd_rx.try_recv() {
                        match cmd {
                            ShadowCtrlCmd::Install {
                                pkg,
                                trusted_pub_hex,
                                revision,
                                spec,
                            } => {
                                shadow_eng = None;
                                metrics_bg.active_running.store(false, Ordering::Release);
                                metrics_bg.worker_pid.store(0, Ordering::Release);
                                metrics_bg.dropped.store(0, Ordering::Release);
                                metrics_bg.errors.store(0, Ordering::Release);
                                metrics_bg.mismatches.store(0, Ordering::Release);
                                shadow_gen += 1;
                                shadow_deadline_ms = spec.eval_deadline_ms.max(1);
                                shadow_blocks = initial_blocks_for_ruleset(&pkg.rule_set);
                                // Clear any stale counterexamples for this fresh installation
                                *metrics_bg.first_counterexample.lock().unwrap() = None;
                                *metrics_bg.last_error.lock().unwrap() = None;

                                match create_warmed_engine_off_scan(
                                    &spec,
                                    "shadow",
                                    shadow_gen,
                                    &pkg,
                                    &trusted_pub_hex,
                                    &reaper,
                                ) {
                                    Ok(eng) => {
                                        metrics_bg
                                            .worker_pid
                                            .store(eng.worker_pid().unwrap_or(0), Ordering::Release);
                                        metrics_bg
                                            .installed_revision
                                            .store(revision, Ordering::Release);
                                        metrics_bg.active_running.store(true, Ordering::Release);
                                        shadow_eng = Some(eng);
                                    }
                                    Err(e) => {
                                        metrics_bg.errors.fetch_add(1, Ordering::Relaxed);
                                        *metrics_bg.last_error.lock().unwrap() = Some(e.to_string());
                                    }
                                }
                            }
                            ShadowCtrlCmd::Clear => {
                                shadow_eng = None;
                                shadow_blocks.clear();
                                metrics_bg.active_running.store(false, Ordering::Release);
                                metrics_bg.worker_pid.store(0, Ordering::Release);
                                metrics_bg.installed_revision.store(0, Ordering::Release);
                            }
                            ShadowCtrlCmd::Shutdown => {
                                let _ = shadow_eng.take();
                                break 'outer;
                            }
                        }
                    }

                    match eval_rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(sample) => {
                            let Some(eng) = shadow_eng.as_mut() else {
                                continue;
                            };
                            let req = EvalRequest {
                                abi_version: ABI_VERSION,
                                mode: WorkerMode::Shadow,
                                boot_id: sample.boot_id,
                                lease_epoch: sample.lease_epoch,
                                generation: sample.generation,
                                scan_id: sample.scan_id,
                                logical_ns: sample.logical_ns,
                                input_seq: sample.input_seq,
                                state_revision: sample.state_revision,
                                applied_output_mask: sample.applied_output_mask,
                                signals: sample.signals,
                                blocks: shadow_blocks.clone(),
                            };
                            let dl_ms = shadow_deadline_ms.max(sample.eval_deadline_ms).max(1);
                            let dl = Instant::now() + Duration::from_millis(dl_ms);
                            match eng.eval_until(&req, dl) {
                                Ok(res) => {
                                    if res.status == EvalStatus::Ok {
                                        shadow_blocks = res.blocks;
                                        metrics_bg.scans_executed.fetch_add(1, Ordering::Relaxed);
                                        if sample.active_worker_ok {
                                            if let Some(active_mask) = sample.active_desired_mask {
                                                if res.desired_mask != active_mask {
                                                    metrics_bg
                                                        .mismatches
                                                        .fetch_add(1, Ordering::Relaxed);
                                                    let mut ce = metrics_bg
                                                        .first_counterexample
                                                        .lock()
                                                        .unwrap();
                                                    if ce.is_none() {
                                                        *ce = Some(format!(
                                                            "scan_id={} active_mask={} shadow_mask={}",
                                                            sample.scan_id, active_mask, res.desired_mask
                                                        ));
                                                    }
                                                }
                                            }
                                        }
                                    } else {
                                        metrics_bg.errors.fetch_add(1, Ordering::Relaxed);
                                        let mut le = metrics_bg.last_error.lock().unwrap();
                                        *le = Some(format!("shadow eval status {:?}", res.status));
                                    }
                                }
                                Err(e) => {
                                    metrics_bg.errors.fetch_add(1, Ordering::Relaxed);
                                    *metrics_bg.last_error.lock().unwrap() = Some(e.to_string());
                                    let mut ce = metrics_bg.first_counterexample.lock().unwrap();
                                    if ce.is_none() {
                                        *ce = Some(format!(
                                            "scan_id={} shadow_error={}",
                                            sample.scan_id, e
                                        ));
                                    }
                                    // Tear down wedged/tainted shadow worker cleanly without affecting active loop
                                    shadow_eng = None;
                                    metrics_bg.active_running.store(false, Ordering::Release);
                                    metrics_bg.worker_pid.store(0, Ordering::Release);
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break 'outer,
                    }
                }
            })
            .expect("spawn shadow-runner thread");

        Self {
            eval_tx,
            cmd_tx,
            metrics,
            handle: Some(handle),
        }
    }

    /// Offers a scan snapshot cleanly without EVER blocking the RT scan thread.
    pub fn offer_nonblocking(&self, sample: ShadowEvalSample) {
        if !self.metrics.active_running.load(Ordering::Acquire) {
            return;
        }
        if self.eval_tx.try_send(sample).is_err() {
            self.metrics.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn install_candidate(
        &self,
        pkg: ModulePackageV1,
        trusted_pub_hex: String,
        revision: u64,
        spec: WorkerSpawnSpec,
    ) {
        let _ = self.cmd_tx.send(ShadowCtrlCmd::Install {
            pkg,
            trusted_pub_hex,
            revision,
            spec,
        });
    }

    pub fn clear(&self) {
        let _ = self.cmd_tx.send(ShadowCtrlCmd::Clear);
    }

    pub fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(ShadowCtrlCmd::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for AsyncShadowRunner {
    fn drop(&mut self) {
        self.shutdown();
    }
}
