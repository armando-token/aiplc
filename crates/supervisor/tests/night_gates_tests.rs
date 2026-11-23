use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use control_abi::crypto::verify_site_profile_signature;
use control_abi::schema::{
    DeploymentRevision, DeploymentState, ModulePackageV1, SiteProfileV1,
};
use field_io::VirtualBenchDriver;
use logic_worker::sandbox::SandboxReport;
use serde_json::Value;
use supervisor::api::AuthTokens;
use supervisor::arena::StateArena;
use supervisor::clock::MonotonicClock;
use supervisor::deploy::DeploymentManager;
use supervisor::journal::DurableJournal;
use supervisor::runtime::{serve_decoupled_http, DecoupledRuntime, DecoupledRuntimeConfig};
use supervisor::storage::{BoundaryEpochTracker, StorageWriter};
use supervisor::telemetry_out::NightRunMetadata;
use supervisor::worker_pool::{
    engine_kind_from_str, validate_linux_sandbox_policy, WorkerSpawnSpec,
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn pkg_arch_suffix() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "darwin-arm64"
    }
    #[cfg(all(target_os = "linux", target_arch = "arm"))]
    {
        "armv7-linux-gnueabihf"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "aarch64-linux-gnu"
    }
    #[cfg(all(
        target_os = "linux",
        not(any(target_arch = "arm", target_arch = "aarch64"))
    ))]
    {
        "darwin-arm64"
    }
}

fn load_profile_and_key() -> (SiteProfileV1, String, [u8; 32]) {
    let dir = repo_root().join("profiles/demo-lyra");
    let profile: SiteProfileV1 =
        serde_json::from_str(&fs::read_to_string(dir.join("site-profile.json")).unwrap()).unwrap();
    let pub_hex = fs::read_to_string(dir.join("keys/test-bench.pub.hex"))
        .unwrap()
        .trim()
        .to_string();
    let seed_hex = fs::read_to_string(dir.join("keys/test-bench.seed.hex"))
        .unwrap()
        .trim()
        .to_string();
    let mut seed = [0u8; 32];
    for i in 0..32 {
        seed[i] = u8::from_str_radix(&seed_hex[i * 2..i * 2 + 2], 16).unwrap();
    }
    verify_site_profile_signature(&profile, &pub_hex).unwrap();
    (profile, pub_hex, seed)
}

fn load_arch_pkg(stem: &str) -> ModulePackageV1 {
    let name = format!("{stem}.{}.json", pkg_arch_suffix());
    serde_json::from_str(
        &fs::read_to_string(repo_root().join("profiles/demo-lyra/packages").join(name)).unwrap(),
    )
    .unwrap()
}

fn sign_pkg(mut pkg: ModulePackageV1, base_rev: u64, seed: &[u8; 32]) -> ModulePackageV1 {
    pkg.base_revision = base_rev;
    let payload = control_abi::crypto::module_package_signing_payload(&pkg).unwrap();
    pkg.signature_hex = Some(control_abi::sign_bytes_ed25519(seed, &payload));
    pkg
}

fn temp_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "plc-night-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

fn free_loopback_addr() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    format!("127.0.0.1:{}", l.local_addr().unwrap().port())
}

fn http_req(addr: &str, method: &str, path: &str, token: Option<&str>, body: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let body_bytes = body.unwrap_or("").as_bytes();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if body.is_some() {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n", body_bytes.len()));
    s.write_all(req.as_bytes()).unwrap();
    if !body_bytes.is_empty() {
        s.write_all(body_bytes).unwrap();
    }
    let mut resp = String::new();
    let _ = s.read_to_string(&mut resp);
    let code = resp
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let json_body = resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (code, json_body)
}

#[test]
fn strict_sandbox_policy_rejects_missing_linux_controls() {
    let ok = SandboxReport {
        cgroup_applied: true,
        cgroup_path: Some("/sys/fs/cgroup/plc-worker-a".into()),
        seccomp_applied: true,
        uid_dropped: true,
        effective_uid: 65534,
        network_namespace: false,
        memory_max_bytes: 16 * 1024 * 1024,
        note: String::new(),
    };
    assert!(validate_linux_sandbox_policy(&ok).is_ok());

    let missing_seccomp = SandboxReport {
        seccomp_applied: false,
        ..ok.clone()
    };
    assert!(validate_linux_sandbox_policy(&missing_seccomp)
        .unwrap_err()
        .contains("seccomp_bpf"));

    let missing_uid = SandboxReport {
        uid_dropped: false,
        effective_uid: 0,
        ..ok.clone()
    };
    assert!(validate_linux_sandbox_policy(&missing_uid)
        .unwrap_err()
        .contains("uid_nobody_65534"));

    let missing_cg = SandboxReport {
        cgroup_applied: false,
        ..ok
    };
    assert!(validate_linux_sandbox_policy(&missing_cg)
        .unwrap_err()
        .contains("cgroup2"));
}

#[test]
fn n1_1_n1_2_n1_4_isolated_active_wamr_deadline_and_partial_eof_isolation() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_plc-supervisor"));
    let root = repo_root();
    let dir_slow = temp_dir("n1-slow");
    let addr_slow = free_loopback_addr();

    // Ensure plc-worker sibling exists next to plc-supervisor in test target dir or PATH
    let pkg_path = root
        .join("profiles/demo-lyra/packages")
        .join(format!("M1.{}.json", pkg_arch_suffix()));

    // Spawn plc-supervisor with isolated wamr-aot active worker that turns slow after 8 evals
    let mut child = Command::new(&bin)
        .args([
            "--profile",
            root.join("profiles/demo-lyra/site-profile.json").to_str().unwrap(),
            "--pubkey",
            root.join("profiles/demo-lyra/keys/test-bench.pub.hex").to_str().unwrap(),
            "--data-dir",
            dir_slow.to_str().unwrap(),
            "--initial-pkg",
            pkg_path.to_str().unwrap(),
            "--engine",
            "wamr-aot",
            "--no-strict-sandbox",
            "--silent-night",
            "--auto-arm",
            "--bind",
            &addr_slow,
            "--worker-fault-arg",
            "--fault-slow-ms",
            "--worker-fault-arg",
            "80",
            "--worker-fault-arg",
            "--fault-after-evals",
            "--worker-fault-arg",
            "8",
        ])
        .spawn()
        .expect("spawn plc-supervisor");

    // Wait for startup & verify silent-night metadata
    let mut state: Value = Value::Null;
    for _ in 0..80 {
        if let Ok(s) = TcpStream::connect(&addr_slow) {
            drop(s);
            let (code, body) = http_req(
                &addr_slow,
                "GET",
                "/v1/devices/lyra-bench-01/state",
                Some("plc-reader-token"),
                None,
            );
            if code == 200 {
                state = serde_json::from_str(&body).unwrap();
                if state["fallbackLatched"].as_bool() == Some(true) {
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    }

    assert_eq!(state["ioMode"].as_str(), Some("virtual"));
    assert_eq!(state["serialBackend"].as_str(), Some("none"));
    assert_eq!(state["lcdEnabled"].as_bool(), Some(false));
    assert_eq!(state["isolatedWorker"].as_bool(), Some(true));
    assert_eq!(
        state["fallbackLatched"].as_bool(),
        Some(true),
        "slow isolated worker must trigger safe fallback latch without crashing supervisor: {state}"
    );
    assert_eq!(state["appliedMask"].as_u64(), Some(0));
    assert!(
        state["activeWorkerPid"].is_null(),
        "faulted/tainted worker must be retired immediately: {state}"
    );

    // Verify supervisor itself stayed alive and responsive after worker failure!
    let (h_code, _) = http_req(&addr_slow, "GET", "/health", None, None);
    assert_eq!(h_code, 200);
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir_slow);

    // Case N1.2: Partial frame / unexpected EOF inside active worker after 4 evals
    let dir_eof = temp_dir("n1-eof");
    let addr_eof = free_loopback_addr();
    let mut child2 = Command::new(&bin)
        .args([
            "--profile",
            root.join("profiles/demo-lyra/site-profile.json").to_str().unwrap(),
            "--pubkey",
            root.join("profiles/demo-lyra/keys/test-bench.pub.hex").to_str().unwrap(),
            "--data-dir",
            dir_eof.to_str().unwrap(),
            "--initial-pkg",
            pkg_path.to_str().unwrap(),
            "--engine",
            "wamr-aot",
            "--no-strict-sandbox",
            "--silent-night",
            "--auto-arm",
            "--bind",
            &addr_eof,
            "--worker-fault-arg",
            "--fault-partial-eof",
            "--worker-fault-arg",
            "--fault-after-evals",
            "--worker-fault-arg",
            "4",
        ])
        .spawn()
        .expect("spawn plc-supervisor partial-eof");

    let mut state2 = Value::Null;
    for _ in 0..80 {
        if TcpStream::connect(&addr_eof).is_ok() {
            let (code, body) = http_req(
                &addr_eof,
                "GET",
                "/v1/devices/lyra-bench-01/state",
                Some("plc-reader-token"),
                None,
            );
            if code == 200 {
                state2 = serde_json::from_str(&body).unwrap();
                if state2["fallbackLatched"].as_bool() == Some(true) {
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(state2["fallbackLatched"].as_bool(), Some(true));
    assert_eq!(state2["appliedMask"].as_u64(), Some(0));
    let _ = child2.kill();
    let _ = child2.wait();
    let _ = fs::remove_dir_all(&dir_eof);
}

#[test]
fn n1_3_n1_5_async_shadow_divergent_and_wedged_non_blocking() {
    let (profile, pub_hex, seed) = load_profile_and_key();
    let m1 = load_arch_pkg("M1");
    let m2 = sign_pkg(load_arch_pkg("M2"), 1, &seed);
    let data_dir = temp_dir("shadow-async");
    let metrics_dir = temp_dir("shadow-metrics");

    let mut dm = DeploymentManager::init(
        profile.clone(),
        pub_hex.clone(),
        DurableJournal::open(&data_dir).unwrap(),
        101,
    )
    .unwrap();
    let empty_arena = StateArena::new(0);
    dm.stage_and_verify("init".into(), m1, &empty_arena).unwrap();
    let h1 = dm.transactions["init"].revision.package_hash.clone();
    dm.authorize("init", "op".into(), "Operator".into(), &h1, 0, 60_000)
        .unwrap();
    let act1 = dm.prepare_activation("init", 0, &empty_arena).unwrap().unwrap();
    dm.commit_boundary_activation(&act1, 0).unwrap();
    drop(dm);

    let journal = DurableJournal::open(&data_dir).unwrap();
    let recovered = journal.recover_on_boot(102).unwrap();
    let tracker = Arc::new(BoundaryEpochTracker::new(102, 1, 1));
    let storage = StorageWriter::start(journal, Arc::clone(&tracker), 1, true, 0);
    let vb = VirtualBenchDriver::new(102);
    let vb_shared = Arc::clone(&vb.shared);

    let mut spec = WorkerSpawnSpec::default();
    spec.engine_kind = engine_kind_from_str("wamr-aot").unwrap();
    spec.strict_sandbox = false;
    spec.eval_deadline_ms = 5;

    let rt = DecoupledRuntime::start(DecoupledRuntimeConfig {
        profile: profile.clone(),
        trusted_pub_hex: pub_hex.clone(),
        boot_id: 102,
        recovered,
        storage,
        boundary_epoch: tracker,
        driver: Box::new(vb),
        virtual_bench_shared: Some(vb_shared),
        clock: Arc::new(MonotonicClock),
        worker_spec: spec.clone(),
        io_mode: "virtual".into(),
        serial_backend: "none".into(),
        lcd_enabled: false,
        auto_arm: true,
        rt_fifo: false,
        metrics_meta: NightRunMetadata {
            run_id: "shadow-test".into(),
            device_id: profile.device_id.clone(),
            boot_id: 102,
            io_mode: "virtual".into(),
            serial_backend: "none".into(),
            lcd_enabled: false,
            isolated_worker: true,
            strict_sandbox: false,
            scan_period_ms: 20,
            eval_deadline_ms: 5,
            metrics_dir: Some(metrics_dir.clone()),
            max_raw_scan_rows: 10_000,
        },
        console_dir: None,
        tokens: AuthTokens::default(),
        stop_flag: None,
    })
    .unwrap();

    // Case A (N1.3): Divergent Shadow detects mismatches without touching active outputs or latching fallback
    let mut div_spec = spec.clone();
    div_spec.extra_args = vec!["--fault-diverge".into()];
    rt.shadow_runner
        .install_candidate(m2.clone(), pub_hex.clone(), 2, div_spec);
    thread::sleep(Duration::from_millis(260));
    let div_snap = rt.shadow_runner.metrics.snapshot();
    assert!(div_snap.scans_executed >= 4, "shadow scans: {div_snap:?}");
    assert!(div_snap.mismatches > 0, "expected shadow mismatches: {div_snap:?}");
    assert!(div_snap.first_counterexample.is_some());
    assert!(
        !rt.scalars
            .fallback_latched
            .load(std::sync::atomic::Ordering::Acquire),
        "divergent shadow must never latch active fallback"
    );

    // Case B (N1.5): Hanging Shadow (200ms sleep per eval, 80ms shadow timeout > 20ms scan period)
    // NEVER stalls active scan loop; shadow_dropped > 0 AND errors > 0 cleanly!
    let mut hang_spec = spec;
    hang_spec.eval_deadline_ms = 80;
    hang_spec.extra_args = vec!["--fault-hang".into()];
    rt.shadow_runner
        .install_candidate(m2, pub_hex, 3, hang_spec);
    let scan_before = rt.scalars.scan_id.load(std::sync::atomic::Ordering::Acquire);
    thread::sleep(Duration::from_millis(320));
    let scan_after = rt.scalars.scan_id.load(std::sync::atomic::Ordering::Acquire);
    let hang_snap = rt.shadow_runner.metrics.snapshot();

    assert!(
        scan_after - scan_before >= 10,
        "active scan loop stalled while shadow hung: delta={}",
        scan_after - scan_before
    );
    assert!(
        hang_snap.dropped > 0,
        "expected shadow_dropped > 0 when shadow hangs: {hang_snap:?}"
    );
    assert_eq!(
        rt.scalars
            .missed_releases
            .load(std::sync::atomic::Ordering::Acquire),
        0,
        "active loop must have zero missed releases while shadow is wedged"
    );

    rt.request_stop();
    if let Ok(mut owned) = Arc::try_unwrap(rt) {
        owned.join_threads();
    }
    let _ = fs::remove_dir_all(&data_dir);
    let _ = fs::remove_dir_all(&metrics_dir);
}

#[test]
fn gate2_decoupled_concurrent_api_swap_rollback_and_metrics() {
    let (profile, pub_hex, seed) = load_profile_and_key();
    let m1 = load_arch_pkg("M1");
    let m2 = sign_pkg(load_arch_pkg("M2"), 1, &seed);
    let data_dir = temp_dir("gate2-data");
    let metrics_dir = temp_dir("gate2-metrics");

    {
        let mut dm = DeploymentManager::init(
            profile.clone(),
            pub_hex.clone(),
            DurableJournal::open(&data_dir).unwrap(),
            201,
        )
        .unwrap();
        let arena = StateArena::new(0);
        dm.stage_and_verify("init".into(), m1, &arena).unwrap();
        let h1 = dm.transactions["init"].revision.package_hash.clone();
        dm.authorize("init", "op".into(), "Operator".into(), &h1, 0, 60_000)
            .unwrap();
        let act1 = dm.prepare_activation("init", 0, &arena).unwrap().unwrap();
        dm.commit_boundary_activation(&act1, 0).unwrap();
    }

    let journal = DurableJournal::open(&data_dir).unwrap();
    let recovered = journal.recover_on_boot(202).unwrap();
    let tracker = Arc::new(BoundaryEpochTracker::new(202, 1, 1));
    let storage = StorageWriter::start(journal, Arc::clone(&tracker), 1, true, 0);
    let vb = VirtualBenchDriver::new(202);
    let vb_shared = Arc::clone(&vb.shared);
    let stop = Arc::new(AtomicBool::new(false));

    let mut spec = WorkerSpawnSpec::default();
    spec.engine_kind = engine_kind_from_str("wamr-aot").unwrap();
    spec.strict_sandbox = false;
    spec.eval_deadline_ms = 5;

    let rt = DecoupledRuntime::start(DecoupledRuntimeConfig {
        profile: profile.clone(),
        trusted_pub_hex: pub_hex,
        boot_id: 202,
        recovered,
        storage,
        boundary_epoch: tracker,
        driver: Box::new(vb),
        virtual_bench_shared: Some(vb_shared),
        clock: Arc::new(MonotonicClock),
        worker_spec: spec,
        io_mode: "virtual".into(),
        serial_backend: "none".into(),
        lcd_enabled: false,
        auto_arm: true,
        rt_fifo: false,
        metrics_meta: NightRunMetadata {
            run_id: "gate2-host".into(),
            device_id: profile.device_id.clone(),
            boot_id: 202,
            io_mode: "virtual".into(),
            serial_backend: "none".into(),
            lcd_enabled: false,
            isolated_worker: true,
            strict_sandbox: false,
            scan_period_ms: 20,
            eval_deadline_ms: 5,
            metrics_dir: Some(metrics_dir.clone()),
            max_raw_scan_rows: 10_000,
        },
        console_dir: None,
        tokens: AuthTokens::default(),
        stop_flag: Some(Arc::clone(&stop)),
    })
    .unwrap();

    let addr = free_loopback_addr();
    let listener = TcpListener::bind(&addr).unwrap();
    let rt_http = Arc::clone(&rt);
    let stop_http = Arc::clone(&stop);
    let http_h = thread::spawn(move || serve_decoupled_http(listener, rt_http, stop_http));

    // Concurrent HTTP reader/virtual-input load while hot-swapping M1 -> M2 -> Rollback M1
    let load_stop = Arc::new(AtomicBool::new(false));
    let mut clients = Vec::new();
    for t in 0..4 {
        let a = addr.clone();
        let ls = Arc::clone(&load_stop);
        clients.push(thread::spawn(move || {
            let mut ok_count = 0usize;
            while !ls.load(std::sync::atomic::Ordering::Acquire) {
                let (c, _) = if t % 2 == 0 {
                    http_req(&a, "GET", "/v1/devices/lyra-bench-01/state", Some("plc-reader-token"), None)
                } else {
                    http_req(&a, "GET", "/v1/devices/lyra-bench-01/scans", Some("plc-reader-token"), None)
                };
                if c == 200 || c == 429 {
                    ok_count += 1;
                }
                thread::sleep(Duration::from_millis(5));
            }
            ok_count
        }));
    }

    // Propose M2 -> Shadow -> Authorize -> Activate
    let prop_body = serde_json::json!({"txId": "tx-g2-m2", "package": m2}).to_string();
    let (c_p, r_p) = http_req(
        &addr,
        "POST",
        "/v1/devices/lyra-bench-01/proposals",
        Some("plc-author-token"),
        Some(&prop_body),
    );
    assert_eq!(c_p, 201, "propose: {r_p}");
    let p_val: Value = serde_json::from_str(&r_p).unwrap();
    let h2 = p_val["revision"]["packageHash"].as_str().unwrap();

    let (c_sh, r_sh) = http_req(
        &addr,
        "POST",
        "/v1/proposals/tx-g2-m2/shadow",
        Some("plc-operator-token"),
        Some("{\"waitMs\":80}"),
    );
    assert_eq!(c_sh, 200, "shadow: {r_sh}");

    let auth_body = serde_json::json!({
        "operatorId": "night-op",
        "expectedPackageHash": h2,
        "ttlMs": 60000
    })
    .to_string();
    let (c_au, _) = http_req(
        &addr,
        "POST",
        "/v1/proposals/tx-g2-m2/authorizations",
        Some("plc-operator-token"),
        Some(&auth_body),
    );
    assert_eq!(c_au, 200);

    let (c_ac, r_ac) = http_req(
        &addr,
        "POST",
        "/v1/proposals/tx-g2-m2/activate",
        Some("plc-operator-token"),
        Some("{}"),
    );
    assert_eq!(c_ac, 202, "activate: {r_ac}");

    // Wait for boundary promotion to rev=2 & activePersisted=true
    let mut promoted = false;
    for _ in 0..40 {
        let (_, b) = http_req(
            &addr,
            "GET",
            "/v1/devices/lyra-bench-01/state",
            Some("plc-reader-token"),
            None,
        );
        let st: Value = serde_json::from_str(&b).unwrap();
        if st["activeRevision"].as_u64() == Some(2) && st["activePersisted"].as_bool() == Some(true) {
            promoted = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(promoted, "expected rev 2 active & persisted");

    // Warm rollback back to M1 (creates monotonic rev 3)
    let (c_rb, r_rb) = http_req(
        &addr,
        "POST",
        "/v1/devices/lyra-bench-01/rollback",
        Some("plc-operator-token"),
        Some("{\"reason\":\"gate2_warm_rb\"}"),
    );
    assert_eq!(c_rb, 202, "rollback: {r_rb}");

    let mut rolled_back = false;
    for _ in 0..40 {
        let (_, b) = http_req(
            &addr,
            "GET",
            "/v1/devices/lyra-bench-01/state",
            Some("plc-reader-token"),
            None,
        );
        let st: Value = serde_json::from_str(&b).unwrap();
        if st["activeRevision"].as_u64() == Some(3)
            && st["activePersisted"].as_bool() == Some(true)
            && st["warmRollbackCount"].as_u64() == Some(1)
        {
            rolled_back = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(rolled_back, "expected warm rollback rev 3 persisted");

    load_stop.store(true, std::sync::atomic::Ordering::Release);
    for c in clients {
        assert!(c.join().unwrap() > 0);
    }

    let (_, nm_body) = http_req(
        &addr,
        "GET",
        "/v1/devices/lyra-bench-01/night-metrics",
        Some("plc-reader-token"),
        None,
    );
    let nm: Value = serde_json::from_str(&nm_body).unwrap();
    assert_eq!(nm["summary"]["telemetryDropped"].as_u64(), Some(0));

    // Verify storage error propagates cleanly as HTTP 503 STORAGE_DEGRADED
    let m2_next = sign_pkg(load_arch_pkg("M2"), 3, &seed);
    rt.storage_handle
        .health
        .inject_write_error
        .store(true, std::sync::atomic::Ordering::Release);
    rt.storage_handle
        .health
        .mark_degraded("simulated disk EIO".into());
    let prop_deg = serde_json::json!({"txId": "tx-g2-deg", "package": m2_next}).to_string();
    let (c_deg, r_deg) = http_req(
        &addr,
        "POST",
        "/v1/devices/lyra-bench-01/proposals",
        Some("plc-author-token"),
        Some(&prop_deg),
    );
    assert_eq!(c_deg, 503, "expected 503 STORAGE_DEGRADED, got {c_deg}: {r_deg}");

    stop.store(true, std::sync::atomic::Ordering::Release);
    let _ = http_h.join();
    if let Ok(mut owned) = Arc::try_unwrap(rt) {
        owned.join_threads();
    }

    let run_dir = metrics_dir.join("gate2-host");
    for f in [
        "manifest.json",
        "scans.csv",
        "samples.csv",
        "events.jsonl",
        "histograms.json",
        "summary.json",
    ] {
        assert!(run_dir.join(f).is_file(), "missing artifact {f}");
    }

    let _ = fs::remove_dir_all(&data_dir);
    let _ = fs::remove_dir_all(&metrics_dir);
}

#[test]
fn gate2_delayed_wal_supersede_and_error_propagation() {
    let data_dir = temp_dir("wal-delay-err");
    let journal = DurableJournal::open(&data_dir).unwrap();
    let tracker = Arc::new(BoundaryEpochTracker::new(300, 1, 1));
    // Inject 80ms delay between ActiveVolatile and Persisted
    let mut storage = StorageWriter::start(journal, Arc::clone(&tracker), 1, true, 80);
    let handle = storage.handle();

    let sample_schema = load_arch_pkg("M1").state_schema;
    let mk_rev = |rev: u64, tx: &str| DeploymentRevision {
        tx_id: tx.into(),
        device_id: "lyra-bench-01".into(),
        boot_id: 300,
        revision: rev,
        base_revision: rev - 1,
        package_hash: format!("hash-{rev}"),
        logic_hash: format!("logic-{rev}"),
        rule_set_hash: format!("ir-{rev}"),
        profile_hash: "prof".into(),
        state_schema: sample_schema.clone(),
        state: DeploymentState::Armed,
        active_scan_id: rev * 10,
    };

    // Enqueue rev=2 (lease=2) then supersede in RAM with rev=3 (lease=3) BEFORE delayed WAL write finishes!
    tracker.store(300, 2, 2);
    handle
        .commit_boundary_nonblocking(mk_rev(2, "tx-2"), 2, false)
        .unwrap();

    // Wait briefly so writer is sleeping inside delay window on rev=2, then simulate urgent rollback to rev=3!
    thread::sleep(Duration::from_millis(15));
    tracker.store(300, 3, 3);
    handle
        .commit_boundary_nonblocking(mk_rev(3, "tx-3"), 3, true)
        .unwrap();

    assert!(handle.flush(Duration::from_secs(5)));
    let snap = handle.health.snapshot();
    assert_eq!(
        snap.stale_commits_superseded, 1,
        "delayed rev=2 must abort instead of overwriting rev=3: {snap:?}"
    );
    assert_eq!(snap.persisted_revision, 3);
    assert!(snap.active_persisted);
    assert!(!snap.degraded);

    // Now test WAL write error propagation (NEVER swallowed!)
    handle
        .health
        .inject_write_error
        .store(true, std::sync::atomic::Ordering::Release);
    tracker.store(300, 4, 4);
    handle
        .commit_boundary_nonblocking(mk_rev(4, "tx-4-eio"), 4, false)
        .unwrap();
    assert!(handle.flush(Duration::from_secs(5)));
    let degraded_snap = handle.health.snapshot();
    assert!(degraded_snap.degraded, "WAL write error must mark storage degraded");
    assert!(!degraded_snap.active_persisted);
    assert!(degraded_snap.last_error.is_some());
    assert!(handle.has_capacity_and_healthy().is_err());

    storage.shutdown();
    let _ = fs::remove_dir_all(&data_dir);
}

#[test]
fn gate2_silent_night_forbids_physical_io_and_crash_recovers_disarmed() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_plc-supervisor"));
    let root = repo_root();
    let dir = temp_dir("silent-flag");

    // Verify --silent-night rejects --hardware-gpio immediately
    let out = Command::new(&bin)
        .args([
            "--profile",
            root.join("profiles/demo-lyra/site-profile.json").to_str().unwrap(),
            "--pubkey",
            root.join("profiles/demo-lyra/keys/test-bench.pub.hex").to_str().unwrap(),
            "--data-dir",
            dir.to_str().unwrap(),
            "--silent-night",
            "--hardware-gpio",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "--silent-night must forbid --hardware-gpio");

    // Verify real Boot #1 crash AFTER_VOLATILE_BEFORE_PERSISTED -> real Boot #2 recovers
    // M1 rev=1 with armed=false AND needsOperatorReview=true (single recover_on_boot pass)
    let (_, _, seed) = load_profile_and_key();
    let pkg_m1_path = root
        .join("profiles/demo-lyra/packages")
        .join(format!("M1.{}.json", pkg_arch_suffix()));
    let m2_signed = sign_pkg(load_arch_pkg("M2"), 1, &seed);
    let addr0 = free_loopback_addr();

    // Seed M1 rev=1 cleanly without crash env
    let mut seed_proc = Command::new(&bin)
        .args([
            "--profile",
            root.join("profiles/demo-lyra/site-profile.json").to_str().unwrap(),
            "--pubkey",
            root.join("profiles/demo-lyra/keys/test-bench.pub.hex").to_str().unwrap(),
            "--data-dir",
            dir.to_str().unwrap(),
            "--initial-pkg",
            pkg_m1_path.to_str().unwrap(),
            "--engine",
            "wamr-aot",
            "--no-strict-sandbox",
            "--silent-night",
            "--bind",
            &addr0,
        ])
        .spawn()
        .unwrap();
    for _ in 0..80 {
        if TcpStream::connect(&addr0).is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = seed_proc.kill();
    let _ = seed_proc.wait();

    // Boot #1 with PLC_CRASH_POINT=AFTER_VOLATILE_BEFORE_PERSISTED
    let addr1 = free_loopback_addr();
    let mut crash_proc = Command::new(&bin)
        .args([
            "--profile",
            root.join("profiles/demo-lyra/site-profile.json").to_str().unwrap(),
            "--pubkey",
            root.join("profiles/demo-lyra/keys/test-bench.pub.hex").to_str().unwrap(),
            "--data-dir",
            dir.to_str().unwrap(),
            "--engine",
            "wamr-aot",
            "--no-strict-sandbox",
            "--silent-night",
            "--bind",
            &addr1,
        ])
        .env("PLC_CRASH_POINT", "AFTER_VOLATILE_BEFORE_PERSISTED")
        .spawn()
        .unwrap();
    for _ in 0..80 {
        if TcpStream::connect(&addr1).is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        http_req(
            &addr1,
            "POST",
            "/v1/devices/lyra-bench-01/rearm",
            Some("plc-operator-token"),
            Some("{}"),
        )
        .0,
        200
    );
    let prop_body = serde_json::json!({"txId": "tx-rec-crash", "package": m2_signed}).to_string();
    let (cp, rp) = http_req(
        &addr1,
        "POST",
        "/v1/devices/lyra-bench-01/proposals",
        Some("plc-author-token"),
        Some(&prop_body),
    );
    assert_eq!(cp, 201, "{rp}");
    let pv: Value = serde_json::from_str(&rp).unwrap();
    let h2 = pv["revision"]["packageHash"].as_str().unwrap();
    let auth_body = serde_json::json!({
        "operatorId": "op-rec",
        "expectedPackageHash": h2,
        "ttlMs": 60000
    })
    .to_string();
    assert_eq!(
        http_req(
            &addr1,
            "POST",
            "/v1/proposals/tx-rec-crash/authorizations",
            Some("plc-operator-token"),
            Some(&auth_body),
        )
        .0,
        200
    );
    assert_eq!(
        http_req(
            &addr1,
            "POST",
            "/v1/proposals/tx-rec-crash/activate",
            Some("plc-operator-token"),
            Some("{}"),
        )
        .0,
        202
    );
    let exit_st = crash_proc.wait().unwrap();
    assert_eq!(exit_st.code(), Some(99), "expected exit 99 on injected crash");

    // Boot #2 real process MUST recover M1 rev=1 with armed=false AND needsOperatorReview=true
    let addr2 = free_loopback_addr();
    let mut rec_proc = Command::new(&bin)
        .args([
            "--profile",
            root.join("profiles/demo-lyra/site-profile.json").to_str().unwrap(),
            "--pubkey",
            root.join("profiles/demo-lyra/keys/test-bench.pub.hex").to_str().unwrap(),
            "--data-dir",
            dir.to_str().unwrap(),
            "--engine",
            "wamr-aot",
            "--no-strict-sandbox",
            "--silent-night",
            "--bind",
            &addr2,
        ])
        .spawn()
        .unwrap();
    let mut st2 = Value::Null;
    for _ in 0..80 {
        if TcpStream::connect(&addr2).is_ok() {
            let (cs, bs) = http_req(
                &addr2,
                "GET",
                "/v1/devices/lyra-bench-01/state",
                Some("plc-reader-token"),
                None,
            );
            if cs == 200 {
                st2 = serde_json::from_str(&bs).unwrap();
                break;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(st2["activeRevision"].as_u64(), Some(1), "boot2 state: {st2}");
    assert_eq!(st2["armed"].as_bool(), Some(false), "boot2 state: {st2}");
    assert_eq!(
        st2["needsOperatorReview"].as_bool(),
        Some(true),
        "boot2 must retain needsOperatorReview=true: {st2}"
    );
    assert_eq!(st2["fallbackLatched"].as_bool(), Some(true), "boot2 state: {st2}");
    let _ = rec_proc.kill();
    let _ = rec_proc.wait();

    let _ = fs::remove_dir_all(&dir);
}
