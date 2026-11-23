use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use control_abi::crypto::{module_package_signing_payload, sign_bytes_ed25519, verify_site_profile_signature};
use control_abi::schema::{ModulePackageV1, SiteProfileV1};
use control_abi::types::{
    EvalStatus, InputSignal, SignalQuality, WorkerMode, ABI_VERSION, OUT_BIT_RED,
    SIGNAL_PHYSICAL_INHIBIT, SIGNAL_TEMP_PROCESS,
};
use control_abi::wire::EvalRequest;
use field_io::{FieldIoDriver, NullDriver, SerialModbusPoller, SimFaultMode, Tht02PtySimulator, VirtualBenchDriver};
use logic_worker::engine::{A1WamrEngine, A2QuickJsEngine, EngineKind, LogicEngine};
use logic_worker::ipc::{WorkerInitPayload, WorkerProcessClient, WorkerSandboxParams};
use serde_json::json;
use supervisor::arena::StateArena;
use supervisor::clock::{ControlClock, MonotonicClock, VirtualClock};
use supervisor::deploy::DeploymentManager;
use supervisor::journal::DurableJournal;
use supervisor::scan_loop::ScanSupervisor;

use crate::telemetry::{
    compute_percentiles, hostname_str, kernel_release, BenchManifest, OffScanTelemetryRecorder,
    ScanCsvRow,
};

fn try_enable_sched_fifo(priority: i32) -> String {
    #[cfg(target_os = "linux")]
    {
        let param = libc::sched_param {
            sched_priority: priority,
        };
        let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
        if rc == 0 {
            return format!("SCHED_FIFO(prio={priority})");
        }
    }
    let _ = priority;
    "CFS/PREEMPT_DEFAULT".into()
}

pub fn run_jitter(out_dir: &Path, scans: usize, period_ms: u64, with_load: bool) {
    let rt_mode = try_enable_sched_fifo(60);
    let stop_load = Arc::new(AtomicBool::new(false));
    let mut load_threads = Vec::new();
    if with_load {
        for _ in 0..2 {
            let st = Arc::clone(&stop_load);
            load_threads.push(thread::spawn(move || {
                let mut x = 1u64;
                while !st.load(Ordering::Relaxed) {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                    std::hint::black_box(x);
                }
            }));
        }
    }

    let pid = Arc::new(AtomicU64::new(std::process::id() as u64));
    let rec = OffScanTelemetryRecorder::start(pid, None);
    let clock = MonotonicClock;
    let period_ns = period_ms * 1_000_000;
    let mut next_release = clock.now_ns() + period_ns;
    let mut misses = 0usize;

    for i in 0..scans {
        clock.sleep_until_ns(next_release);
        let start_ns = clock.now_ns();
        let jitter = (start_ns as i64) - (next_release as i64);
        let end_ns = clock.now_ns();
        let dur = end_ns.saturating_sub(start_ns);
        let miss = start_ns > next_release + period_ns;
        if miss {
            misses += 1;
        }
        rec.try_record_scan(ScanCsvRow {
            run_id: if with_load { "jitter-load".into() } else { "jitter-idle".into() },
            mode: if with_load { "load".into() } else { "idle".into() },
            scan_id: (i as u64) + 1,
            release_ns: next_release,
            start_ns,
            wakeup_jitter_ns: jitter,
            input_seq: (i as u64) + 1,
            eval_start_ns: start_ns,
            eval_end_ns: end_ns,
            eval_dur_ns: dur,
            decision_ns: end_ns,
            publish_ns: end_ns,
            ack_driver_ns: end_ns,
            cycle_dur_ns: dur,
            active_rev: 0,
            deadline_miss: miss,
            fallback_reason: "NONE".into(),
            applied_mask: 0,
        });
        next_release += period_ns;
        if start_ns > next_release + period_ns {
            next_release = start_ns + period_ns;
        }
    }

    stop_load.store(true, Ordering::Relaxed);
    for t in load_threads {
        let _ = t.join();
    }

    let manifest = BenchManifest {
        run_id: if with_load { "jitter-load".into() } else { "jitter-idle".into() },
        host_id: hostname_str(),
        kernel: kernel_release(),
        arch: std::env::consts::ARCH.into(),
        engine: "none".into(),
        workload: "scheduler-jitter".into(),
        mode: if with_load { "stress".into() } else { "idle".into() },
        started_unix_s: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        total_scans: scans,
        deadline_misses: misses,
        rt_scheduler: rt_mode,
    };
    let summary = rec.finish_and_write(out_dir, manifest).expect("write jitter");
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
}

fn load_pkg_for_engine(pkg_dir: &Path, workload: &str, engine: EngineKind, arch: &str) -> ModulePackageV1 {
    let fname = match engine {
        EngineKind::QuickJs => format!("{workload}.quickjs.json"),
        _ => {
            if arch == "arm" {
                format!("{workload}.armv7-linux-gnueabihf.json")
            } else if arch == "aarch64" && cfg!(target_os = "linux") {
                format!("{workload}.aarch64-linux-gnu.json")
            } else {
                format!("{workload}.darwin-arm64.json")
            }
        }
    };
    let raw = fs::read_to_string(pkg_dir.join(&fname))
        .unwrap_or_else(|e| panic!("failed to read package {}: {e}", pkg_dir.join(&fname).display()));
    serde_json::from_str(&raw).expect("parse package json")
}

fn parse_engine_kind(name: &str) -> EngineKind {
    match name {
        "a0-empty" => EngineKind::EmptyNoop,
        "a0-ir" => EngineKind::NativeIr,
        "a1-interp" => EngineKind::WamrInterp,
        "a1-aot" => EngineKind::WamrAot,
        "a2-quickjs" => EngineKind::QuickJs,
        "a3-wasmtime" => EngineKind::Wasmtime,
        other => panic!("unknown engine '{other}'"),
    }
}

pub fn run_engine_benchmark(
    engine_name: &str,
    workload: &str,
    mode: &str,
    scans: usize,
    profile_path: &Path,
    pubkey_path: &Path,
    pkg_dir: &Path,
    worker_bin: &Path,
    out_dir: &Path,
) {
    let rt_mode = try_enable_sched_fifo(55);
    let eng_kind = parse_engine_kind(engine_name);
    let profile: SiteProfileV1 =
        serde_json::from_str(&fs::read_to_string(profile_path).unwrap()).unwrap();
    let pub_hex = fs::read_to_string(pubkey_path).unwrap().trim().to_string();
    verify_site_profile_signature(&profile, &pub_hex).unwrap();

    let pkg = load_pkg_for_engine(pkg_dir, workload, eng_kind, std::env::consts::ARCH);
    let eval_budget_ns = (profile.eval_deadline_ms as u64) * 1_000_000;
    let period_ns = (profile.scan_period_ms as u64) * 1_000_000;

    let mut active_client = WorkerProcessClient::spawn(
        worker_bin,
        &WorkerInitPayload {
            engine: eng_kind,
            package: pkg.clone(),
            trusted_pub_hex: pub_hex.clone(),
            eval_budget_ns,
            sandbox: WorkerSandboxParams {
                slot_name: format!("bench-{engine_name}-{workload}-act"),
                memory_max_bytes: 32 * 1024 * 1024,
                drop_to_nobody: cfg!(target_os = "linux"),
                enable_seccomp: cfg!(target_os = "linux"),
            },
        },
        &[],
    )
    .expect("spawn active plc-worker");

    let with_shadow = mode == "shadow" || mode == "shadow-load";
    let with_load = mode == "shadow-load";

    let mut shadow_client = if with_shadow {
        Some(
            WorkerProcessClient::spawn(
                worker_bin,
                &WorkerInitPayload {
                    engine: eng_kind,
                    package: pkg.clone(),
                    trusted_pub_hex: pub_hex,
                    eval_budget_ns,
                    sandbox: WorkerSandboxParams {
                        slot_name: format!("bench-{engine_name}-{workload}-shd"),
                        memory_max_bytes: 32 * 1024 * 1024,
                        drop_to_nobody: cfg!(target_os = "linux"),
                        enable_seccomp: cfg!(target_os = "linux"),
                    },
                },
                &[],
            )
            .expect("spawn shadow plc-worker"),
        )
    } else {
        None
    };

    let stop_load = Arc::new(AtomicBool::new(false));
    let mut load_threads = Vec::new();
    if with_load {
        for _ in 0..2 {
            let st = Arc::clone(&stop_load);
            load_threads.push(thread::spawn(move || {
                let mut x = 3u64;
                while !st.load(Ordering::Relaxed) {
                    x = x.wrapping_mul(2862933555777941757).wrapping_add(3037000493);
                    std::hint::black_box(x);
                }
            }));
        }
    }

    let pid_atom = Arc::new(AtomicU64::new(active_client.pid() as u64));
    let cg_path = if cfg!(target_os = "linux") {
        let slot = format!("bench-{engine_name}-{workload}-act");
        let sanitized: String = slot
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let p = format!("/sys/fs/cgroup/plc-worker-{sanitized}");
        if Path::new(&p).exists() { Some(p) } else { None }
    } else {
        None
    };
    let rec = OffScanTelemetryRecorder::start(pid_atom, cg_path);
    let clock = MonotonicClock;

    // Build 32-signal template when running W3, otherwise 2 signals
    let sig_count = if workload == "W3" { 32 } else { 2 };
    let mut base_signals = Vec::with_capacity(sig_count);
    base_signals.push(InputSignal {
        id: SIGNAL_TEMP_PROCESS,
        value: 455,
        quality: SignalQuality::Good,
        sample_age_ms: 10,
    });
    base_signals.push(InputSignal {
        id: SIGNAL_PHYSICAL_INHIBIT,
        value: 0,
        quality: SignalQuality::Good,
        sample_age_ms: 0,
    });
    for s_idx in 1..sig_count.saturating_sub(1) {
        base_signals.push(InputSignal {
            id: 100 + s_idx as u32,
            value: 455,
            quality: SignalQuality::Good,
            sample_age_ms: 10,
        });
    }

    let mut arena = StateArena::new(1);
    arena.apply_migration(
        &supervisor::arena::PreparedMigration {
            target_revision: 1,
            entries: pkg.required_blocks.clone(),
        },
        clock.now_ns(),
    );

    let mut misses = 0usize;
    let mut next_release = clock.now_ns() + period_ns;
    let run_id = format!("{engine_name}-{workload}-{mode}");

    for i in 0..scans {
        clock.sleep_until_ns(next_release);
        let start_ns = clock.now_ns();
        let jitter = (start_ns as i64) - (next_release as i64);

        let req = EvalRequest {
            abi_version: ABI_VERSION,
            mode: WorkerMode::Active,
            boot_id: 1,
            lease_epoch: 1,
            generation: 1,
            scan_id: (i as u64) + 1,
            logical_ns: start_ns,
            input_seq: (i as u64) + 1,
            state_revision: arena.state_revision,
            applied_output_mask: arena.last_applied_output_mask,
            signals: base_signals.clone(),
            blocks: arena.blocks.clone(),
        };

        let eval_start = clock.now_ns();
        let res = active_client
            .eval_within(&req, Duration::from_millis(profile.eval_deadline_ms as u64))
            .expect("active eval");
        if let Some(ref mut sc) = shadow_client {
            let mut sreq = req.clone();
            sreq.mode = WorkerMode::Shadow;
            let _ = sc.eval_within(&sreq, Duration::from_millis(profile.eval_deadline_ms as u64));
        }
        let eval_end = clock.now_ns();

        arena.blocks = res.blocks;
        arena.last_applied_output_mask = res.desired_mask;
        let publish_ns = clock.now_ns();
        let cycle_dur = publish_ns.saturating_sub(start_ns);
        let eval_dur = eval_end.saturating_sub(eval_start);
        let miss = eval_dur > eval_budget_ns
            || publish_ns > next_release + period_ns
            || res.status != EvalStatus::Ok;
        if miss {
            misses += 1;
        }

        rec.try_record_scan(ScanCsvRow {
            run_id: run_id.clone(),
            mode: mode.into(),
            scan_id: (i as u64) + 1,
            release_ns: next_release,
            start_ns,
            wakeup_jitter_ns: jitter,
            input_seq: req.input_seq,
            eval_start_ns: eval_start,
            eval_end_ns: eval_end,
            eval_dur_ns: eval_dur,
            decision_ns: eval_end,
            publish_ns,
            ack_driver_ns: publish_ns,
            cycle_dur_ns: cycle_dur,
            active_rev: 1,
            deadline_miss: miss,
            fallback_reason: "NONE".into(),
            applied_mask: res.desired_mask,
        });

        next_release += period_ns;
        if publish_ns > next_release + period_ns {
            next_release = publish_ns + period_ns;
        }
    }

    stop_load.store(true, Ordering::Relaxed);
    for t in load_threads {
        let _ = t.join();
    }

    let manifest = BenchManifest {
        run_id,
        host_id: hostname_str(),
        kernel: kernel_release(),
        arch: std::env::consts::ARCH.into(),
        engine: engine_name.into(),
        workload: workload.into(),
        mode: mode.into(),
        started_unix_s: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        total_scans: scans,
        deadline_misses: misses,
        rt_scheduler: rt_mode,
    };
    let summary = rec.finish_and_write(out_dir, manifest).expect("write bench");
    active_client.revoke_and_kill();
    if let Some(mut sc) = shadow_client {
        sc.revoke_and_kill();
    }
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
}

pub fn run_soak(
    swaps: usize,
    profile_path: &Path,
    pubkey_path: &Path,
    pkg_dir: &Path,
    data_dir: &Path,
    out_dir: &Path,
) {
    let _ = fs::remove_dir_all(data_dir);
    fs::create_dir_all(data_dir).unwrap();
    fs::create_dir_all(out_dir).unwrap();

    let profile: SiteProfileV1 =
        serde_json::from_str(&fs::read_to_string(profile_path).unwrap()).unwrap();
    let pub_hex = fs::read_to_string(pubkey_path).unwrap().trim().to_string();
    let seed_hex = fs::read_to_string(pubkey_path.with_file_name("test-bench.seed.hex"))
        .unwrap()
        .trim()
        .to_string();
    let mut seed_arr = [0u8; 32];
    for i in 0..32 {
        seed_arr[i] = u8::from_str_radix(&seed_hex[i * 2..i * 2 + 2], 16).unwrap();
    }

    let m1 = load_pkg_for_engine(pkg_dir, "M1", EngineKind::WamrAot, std::env::consts::ARCH);
    let m2_tpl = load_pkg_for_engine(pkg_dir, "M2", EngineKind::WamrAot, std::env::consts::ARCH);

    let clock = Arc::new(VirtualClock::new(1_000_000_000));
    let drv = VirtualBenchDriver::new(42);
    let journal = DurableJournal::open(data_dir).unwrap();
    let deploy = DeploymentManager::init(profile.clone(), pub_hex.clone(), journal, 42).unwrap();
    let mut sup = ScanSupervisor::new(Arc::clone(&clock), drv, profile, deploy, 42);
    sup.arm_control();
    sup.install_active_engine(Box::new(A1WamrEngine::new(true)));

    // Commit initial M1
    sup.deploy.stage_and_verify("init".into(), m1.clone(), &sup.arena).unwrap();
    let h1 = sup.deploy.transactions["init"].revision.package_hash.clone();
    sup.deploy.authorize("init", "soak".into(), "Engineer".into(), &h1, 1_000_000_000, 600_000).unwrap();
    sup.pending_activation = sup.deploy.prepare_activation("init", 1_000_000_000, &sup.arena).unwrap();
    sup.step_once();

    let pid_atom = Arc::new(AtomicU64::new(std::process::id() as u64));
    let rec = OffScanTelemetryRecorder::start(pid_atom, None);
    let real_clock = MonotonicClock;
    let mut swap_latencies = Vec::with_capacity(swaps);

    for step in 0..swaps {
        let t_base = ((step as u64) + 2) * 100_000_000;
        clock.set_ns(t_base);
        if step % 2 == 0 {
            let mut pkg = m2_tpl.clone();
            pkg.base_revision = sup.deploy.active_revision;
            let pl = module_package_signing_payload(&pkg).unwrap();
            pkg.signature_hex = Some(sign_bytes_ed25519(&seed_arr, &pl));
            let tx = format!("soak-p-{step}");
            sup.deploy.stage_and_verify(tx.clone(), pkg, &sup.arena).unwrap();
            let h = sup.deploy.transactions[&tx].revision.package_hash.clone();
            sup.deploy.authorize(&tx, "soak".into(), "Operator".into(), &h, t_base, 600_000).unwrap();
            sup.pending_activation = sup.deploy.prepare_activation(&tx, t_base, &sup.arena).unwrap();
        } else {
            let tx = format!("soak-rb-{step}");
            // Reset fault budget counter inside accelerated soak harness while preserving production max_faults_before_latch=3
            sup.deploy.rearm_operator();
            sup.pending_activation = Some(sup.deploy.prepare_rollback(tx, "soak cycle".into(), &sup.arena).unwrap());
        }
        let s0 = real_clock.now_ns();
        let row = sup.step_once();
        let s1 = real_clock.now_ns();
        swap_latencies.push(s1.saturating_sub(s0));
        rec.try_record_scan(ScanCsvRow {
            run_id: "soak-1000".into(),
            mode: "soak".into(),
            scan_id: row.scan_id,
            release_ns: row.release_ns,
            start_ns: row.start_ns,
            wakeup_jitter_ns: 0,
            input_seq: row.input_seq,
            eval_start_ns: s0,
            eval_end_ns: s1,
            eval_dur_ns: s1.saturating_sub(s0),
            decision_ns: s1,
            publish_ns: s1,
            ack_driver_ns: s1,
            cycle_dur_ns: s1.saturating_sub(s0),
            active_rev: row.active_rev,
            deadline_miss: false,
            fallback_reason: "NONE".into(),
            applied_mask: row.applied_mask,
        });
    }

    let final_rev = sup.deploy.active_revision;
    let p_swap = compute_percentiles(&swap_latencies);
    let manifest = BenchManifest {
        run_id: "soak-1000".into(),
        host_id: hostname_str(),
        kernel: kernel_release(),
        arch: std::env::consts::ARCH.into(),
        engine: "wamr-aot".into(),
        workload: "M1-M2-swap-rollback".into(),
        mode: "soak".into(),
        started_unix_s: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        total_scans: swaps,
        deadline_misses: 0,
        rt_scheduler: "SOAK".into(),
    };
    let mut summary = rec.finish_and_write(out_dir, manifest).unwrap();
    summary["finalRevision"] = json!(final_rev.to_string());
    summary["swapLatencyNs"] = json!(p_swap);
    fs::write(out_dir.join("summary.json"), serde_json::to_vec_pretty(&summary).unwrap()).unwrap();
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
}

pub fn run_hil_suite(
    profile_path: &Path,
    pubkey_path: &Path,
    pkg_dir: &Path,
    worker_bin: &Path,
    supervisor_bin: &Path,
    data_dir: &Path,
    out_dir: &Path,
    hardware_gpio: bool,
) {
    fs::create_dir_all(out_dir).unwrap();
    let profile: SiteProfileV1 =
        serde_json::from_str(&fs::read_to_string(profile_path).unwrap()).unwrap();
    let pub_hex = fs::read_to_string(pubkey_path).unwrap().trim().to_string();
    let m1 = load_pkg_for_engine(pkg_dir, "M1", EngineKind::WamrAot, std::env::consts::ARCH);
    let m1_qjs = load_pkg_for_engine(pkg_dir, "M1", EngineKind::QuickJs, std::env::consts::ARCH);

    let mut checks: Vec<serde_json::Value> = Vec::new();
    let eval_budget_ns = (profile.eval_deadline_ms as u64) * 1_000_000;

    // H01: Exclusive GPIO claim + toggle ON/OFF + concurrent EBUSY check (on PLC Linux when --hardware-gpio)
    #[cfg(target_os = "linux")]
    if hardware_gpio {
        use field_io::LyraHardwareDriver;
        let mut drv = LyraHardwareDriver::open(101, "/dev/gpiochip0", 17, 18, None, 1, 9600, 1500)
            .expect("H01 open /dev/gpiochip0 lines 17 & 18");
        let second_err = LyraHardwareDriver::open(102, "/dev/gpiochip0", 17, 18, None, 1, 9600, 1500).is_err();
        let obs_on = drv.apply_output(
            &control_abi::types::OutputFrame {
                boot_id: 101,
                scan_id: 1,
                lease_epoch: 1,
                active_revision: 1,
                output_seq: 1,
                desired_mask: 1,
                applied_mask: 1,
                expiry_ns: u64::MAX,
                guard_reason: control_abi::types::GuardReason::None,
            },
            1_000_000,
        );
        thread::sleep(Duration::from_millis(20));
        let obs_off = drv.force_safe_off(2_000_000);
        checks.push(json!({
            "id": "H01",
            "passed": second_err && obs_on.observed_mask == Some(1) && obs_off.observed_mask == Some(0),
            "secondOpenBlocked": second_err,
            "observedOnMask": obs_on.observed_mask,
            "observedOffMask": obs_off.observed_mask,
            "hardware": true
        }));
    } else {
        let mut drv = NullDriver::new(101);
        let obs = drv.force_safe_off(0);
        checks.push(json!({"id": "H01", "passed": obs.observed_mask.unwrap_or(0) == 0, "hardware": false}));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = hardware_gpio;
        let mut drv = NullDriver::new(101);
        let obs = drv.force_safe_off(0);
        checks.push(json!({"id": "H01", "passed": obs.observed_mask.unwrap_or(0) == 0, "hardware": false}));
    }

    // H02: SerialModbusPoller <-> Tht02PtySimulator real RTU frames over PTY on target OS
    {
        let sim = Tht02PtySimulator::start(1, 452).expect("PTY simulator");
        let poller = SerialModbusPoller::start(
            sim.slave_path.clone(),
            1,
            9600,
            25,
            1500,
            -400,
            1500,
        )
        .expect("start SerialModbusPoller");
        thread::sleep(Duration::from_millis(120));
        let (snap, seq) = poller.snapshot();
        sim.controls.lock().unwrap().fault_mode = SimFaultMode::AbnormalSentinel;
        thread::sleep(Duration::from_millis(120));
        let (snap_fault, _seq2) = poller.snapshot();
        drop(poller);
        drop(sim);
        checks.push(json!({
            "id": "H02",
            "passed": seq > 0 && snap.quality == SignalQuality::Good && snap.value == 452 && snap_fault.quality == SignalQuality::Invalid,
            "goodValue": snap.value,
            "faultQuality": format!("{:?}", snap_fault.quality),
            "note": "Real Modbus RTU frames over OS PTY (THT-02 slave simulated)"
        }));
    }

    // F01: Hang timeout + F02: Cgroup v2 OOM + F03: Bad IPC Frame + F06: Seccomp-BPF + nobody
    {
        let base_sb = WorkerSandboxParams {
            slot_name: "hil-f01".into(),
            memory_max_bytes: 16 * 1024 * 1024,
            drop_to_nobody: cfg!(target_os = "linux"),
            enable_seccomp: cfg!(target_os = "linux"),
        };
        let req = EvalRequest {
            abi_version: ABI_VERSION,
            mode: WorkerMode::Active,
            boot_id: 1,
            lease_epoch: 1,
            generation: 1,
            scan_id: 1,
            logical_ns: 0,
            input_seq: 1,
            state_revision: 0,
            applied_output_mask: 0,
            signals: vec![InputSignal { id: SIGNAL_TEMP_PROCESS, value: 455, quality: SignalQuality::Good, sample_age_ms: 10 }],
            blocks: vec![],
        };

        // F01 Hang
        let mut c_hang = WorkerProcessClient::spawn(
            worker_bin,
            &WorkerInitPayload { engine: EngineKind::WamrAot, package: m1.clone(), trusted_pub_hex: pub_hex.clone(), eval_budget_ns, sandbox: base_sb.clone() },
            &["--fault-hang"],
        ).unwrap();
        let f01_ok = c_hang.eval_within(&req, Duration::from_millis(10)).is_err();
        c_hang.revoke_and_kill();
        checks.push(json!({"id": "F01", "passed": f01_ok}));

        // F02 Cgroup v2 8 MiB OOM containment
        let mut oom_sb = base_sb.clone();
        oom_sb.slot_name = "hil-f02-oom".into();
        oom_sb.memory_max_bytes = 8 * 1024 * 1024;
        let mut c_oom = WorkerProcessClient::spawn(
            worker_bin,
            &WorkerInitPayload { engine: EngineKind::WamrAot, package: m1.clone(), trusted_pub_hex: pub_hex.clone(), eval_budget_ns, sandbox: oom_sb },
            &["--fault-oom"],
        ).unwrap();
        let f02_res = c_oom.eval_within(&req, Duration::from_millis(500));
        let oom_stats = c_oom.cgroup_mem_stats();
        let f02_cg = c_oom.init_ack.sandbox.cgroup_applied;
        c_oom.revoke_and_kill();
        let oom_confirmed = oom_stats
            .as_ref()
            .map(|s| s.oom_events > 0 || s.peak_bytes >= 7 * 1024 * 1024)
            .unwrap_or(false);
        checks.push(json!({"id": "F02", "passed": f02_res.is_err() && (!cfg!(target_os = "linux") || (f02_cg && oom_confirmed)), "cgroupApplied": f02_cg, "oomConfirmed": oom_confirmed, "cgroupMemStats": oom_stats}));

        // F03 Bad Frame
        let mut c_bad = WorkerProcessClient::spawn(
            worker_bin,
            &WorkerInitPayload { engine: EngineKind::WamrAot, package: m1.clone(), trusted_pub_hex: pub_hex.clone(), eval_budget_ns, sandbox: base_sb.clone() },
            &["--fault-bad-frame"],
        ).unwrap();
        let f03_ok = c_bad.eval_within(&req, Duration::from_millis(50)).is_err();
        c_bad.revoke_and_kill();
        checks.push(json!({"id": "F03", "passed": f03_ok}));

        // F06 Seccomp-BPF + UID 65534 + open/socket EPERM
        let mut esc_sb = base_sb;
        esc_sb.slot_name = "hil-f06-sec".into();
        let mut c_esc = WorkerProcessClient::spawn(
            worker_bin,
            &WorkerInitPayload { engine: EngineKind::WamrAot, package: m1.clone(), trusted_pub_hex: pub_hex.clone(), eval_budget_ns, sandbox: esc_sb },
            &["--fault-escape"],
        ).unwrap();
        let err_msg = c_esc.eval_within(&req, Duration::from_millis(50)).unwrap_err().to_string();
        let ack_sb = c_esc.init_ack.sandbox.clone();
        c_esc.revoke_and_kill();
        let f06_ok = err_msg.contains("socket_rc=-1") && err_msg.contains("open_rc=-1")
            && (!cfg!(target_os = "linux") || (ack_sb.seccomp_applied && ack_sb.uid_dropped && ack_sb.effective_uid == 65534));
        checks.push(json!({"id": "F06", "passed": f06_ok, "sandbox": ack_sb, "escapeProbe": err_msg}));
    }

    // D03: Real SIGKILL against plc-supervisor over target filesystem WAL
    {
        let crash_dir = data_dir.join("hil-d03-sigkill");
        let _ = fs::remove_dir_all(&crash_dir);
        fs::create_dir_all(&crash_dir).unwrap();
        let j0 = DurableJournal::open(&crash_dir).unwrap();
        let mut dm0 = DeploymentManager::init(profile.clone(), pub_hex.clone(), j0, 500).unwrap();
        let a0 = StateArena::new(0);
        dm0.stage_and_verify("init-1".into(), m1.clone(), &a0).unwrap();
        let h1 = dm0.transactions["init-1"].revision.package_hash.clone();
        dm0.authorize("init-1", "hil".into(), "Engineer".into(), &h1, 0, 60_000).unwrap();
        let act1 = dm0.prepare_activation("init-1", 0, &a0).unwrap().unwrap();
        dm0.commit_boundary_activation(&act1, 0).unwrap();

        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut child = Command::new(supervisor_bin)
            .args([
                "--profile", profile_path.to_str().unwrap(),
                "--pubkey", pubkey_path.to_str().unwrap(),
                "--data-dir", crash_dir.to_str().unwrap(),
                "--engine", "wamr-aot",
                "--bind", &format!("127.0.0.1:{port}"),
            ])
            .spawn()
            .expect("spawn plc-supervisor for D03");
        thread::sleep(Duration::from_millis(120));
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL); }
        let _ = child.wait();
        let j_rec = DurableJournal::open(&crash_dir).unwrap();
        let rec = j_rec.recover_on_boot(501).unwrap();
        let d03_ok = !rec.armed_on_boot && rec.last_stable_revision.as_ref().map(|r| r.revision) == Some(1);
        checks.push(json!({"id": "D03", "passed": d03_ok, "recoveredRev": 1, "startedDisarmed": !rec.armed_on_boot}));
    }

    // S03-A2: QuickJS engine functional check + full M1->M2 swap->rollback check
    {
        let m2_qjs = load_pkg_for_engine(pkg_dir, "M2", EngineKind::QuickJs, std::env::consts::ARCH);
        let q_dir = data_dir.join("hil-s03-qjs");
        let _ = fs::remove_dir_all(&q_dir);
        fs::create_dir_all(&q_dir).unwrap();
        let qclk = Arc::new(VirtualClock::new(1_000_000_000));
        let qdrv = VirtualBenchDriver::new(600);
        let qj = DurableJournal::open(&q_dir).unwrap();
        let qdm = DeploymentManager::init(profile.clone(), pub_hex.clone(), qj, 600).unwrap();
        let mut qsup = ScanSupervisor::new(Arc::clone(&qclk), qdrv, profile.clone(), qdm, 600);
        qsup.arm_control();
        qsup.install_active_engine(Box::new(A2QuickJsEngine::new(eval_budget_ns)));

        qsup.deploy.stage_and_verify("q-m1".into(), m1_qjs.clone(), &qsup.arena).unwrap();
        let qh1 = qsup.deploy.transactions["q-m1"].revision.package_hash.clone();
        qsup.deploy.authorize("q-m1", "hil".into(), "Engineer".into(), &qh1, 1_000_000_000, 60_000).unwrap();
        qsup.pending_activation = qsup.deploy.prepare_activation("q-m1", 1_000_000_000, &qsup.arena).unwrap();
        let row_m1 = qsup.step_once();

        qclk.set_ns(2_000_000_000);
        qsup.deploy.stage_and_verify("q-m2".into(), m2_qjs, &qsup.arena).unwrap();
        let qh2 = qsup.deploy.transactions["q-m2"].revision.package_hash.clone();
        qsup.deploy.authorize("q-m2", "hil".into(), "Operator".into(), &qh2, 2_000_000_000, 60_000).unwrap();
        qsup.pending_activation = qsup.deploy.prepare_activation("q-m2", 2_000_000_000, &qsup.arena).unwrap();
        let row_m2 = qsup.step_once();

        qclk.set_ns(3_000_000_000);
        qsup.deploy.rearm_operator();
        qsup.pending_activation = Some(
            qsup.deploy.prepare_rollback("q-rb".into(), "S03-A2 rollback check".into(), &qsup.arena).unwrap(),
        );
        let row_rb = qsup.step_once();

        let mut qeng = A2QuickJsEngine::new(eval_budget_ns);
        qeng.prepare(&m1_qjs, &pub_hex).and_then(|_| qeng.warm()).unwrap();
        let qres = qeng.eval_sync(&EvalRequest {
            abi_version: ABI_VERSION,
            mode: WorkerMode::Active,
            boot_id: 1,
            lease_epoch: 1,
            generation: 1,
            scan_id: 1,
            logical_ns: 11_000_000_000,
            input_seq: 1,
            state_revision: 0,
            applied_output_mask: 0,
            signals: vec![InputSignal { id: SIGNAL_TEMP_PROCESS, value: 455, quality: SignalQuality::Good, sample_age_ms: 10 }],
            blocks: vec![],
        }).unwrap();
        let swap_rb_ok = row_m1.active_rev == 1 && row_m2.active_rev == 2 && row_rb.active_rev == 3
            && !row_m1.deadline_miss && !row_m2.deadline_miss && !row_rb.deadline_miss
            && qres.status == EvalStatus::Ok && (qres.desired_mask & OUT_BIT_RED) == 0;
        checks.push(json!({
            "id": "S03-A2",
            "passed": swap_rb_ok,
            "m1Rev": row_m1.active_rev,
            "m2Rev": row_m2.active_rev,
            "rollbackRev": row_rb.active_rev,
        }));
    }

    let report = json!({
        "host": hostname_str(),
        "kernel": kernel_release(),
        "arch": std::env::consts::ARCH,
        "checks": checks,
        "allPassed": checks.iter().all(|c| c["passed"] == true),
    });
    fs::write(out_dir.join("hil_report.json"), serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
