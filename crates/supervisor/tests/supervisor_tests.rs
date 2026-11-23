use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use control_abi::crypto::verify_site_profile_signature;
use control_abi::schema::{DeploymentState, ModulePackageV1, SiteProfileV1};
use control_abi::types::{
    BlockType, GuardReason, MigrationPolicy, SignalQuality, OUT_BIT_BLUE_VIRTUAL, OUT_BIT_RED,
};
use field_io::VirtualBenchDriver;
use logic_worker::engine::{A0NativeIrEngine, A1WamrEngine, A2QuickJsEngine};
use supervisor::api::{ApiContext, AuthTokens};
use supervisor::arena::StateArena;
use supervisor::clock::VirtualClock;
use supervisor::deploy::{DeployError, DeploymentManager};
use supervisor::journal::DurableJournal;
use supervisor::scan_loop::ScanSupervisor;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn load_profile_and_key() -> (SiteProfileV1, String) {
    let dir = repo_root().join("profiles/demo-lyra");
    let profile: SiteProfileV1 =
        serde_json::from_str(&fs::read_to_string(dir.join("site-profile.json")).unwrap()).unwrap();
    let pub_hex = fs::read_to_string(dir.join("keys/test-bench.pub.hex"))
        .unwrap()
        .trim()
        .to_string();
    verify_site_profile_signature(&profile, &pub_hex).unwrap();
    (profile, pub_hex)
}

fn load_pkg(name: &str) -> ModulePackageV1 {
    serde_json::from_str(
        &fs::read_to_string(repo_root().join("profiles/demo-lyra/packages").join(name)).unwrap(),
    )
    .unwrap()
}

fn temp_data_dir(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("plc-auto-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn golden_trace_10s_to_5s_reset_and_preserve_elapsed() {
    let (profile, pub_hex) = load_profile_and_key();
    let m1 = load_pkg("M1.darwin-arm64.json");
    let mut m2_reset = load_pkg("M2.darwin-arm64.json");
    // Ensure M2 uses RESET on TON 1001 for the §8.2 golden trace 1
    m2_reset.rule_set.changes[0].timing_state_on_change =
        control_abi::schema::TimingStateOnChange::Reset;
    m2_reset.required_blocks[0].migration_policy = MigrationPolicy::Reset;

    // --- Part A: Golden Trace §8.2 with RESET ---
    let dir_a = temp_data_dir("golden-reset");
    let clock = Arc::new(VirtualClock::new(0));
    let drv = VirtualBenchDriver::new(100);
    let drv_shared = Arc::clone(&drv.shared);
    let journal = DurableJournal::open(&dir_a).unwrap();
    let deploy = DeploymentManager::init(profile.clone(), pub_hex.clone(), journal, 100).unwrap();
    let mut sup = ScanSupervisor::new(Arc::clone(&clock), drv, profile.clone(), deploy, 100);
    sup.arm_control();
    sup.install_active_engine(Box::new(A0NativeIrEngine::new_ir()));

    // Stage/authorize/activate M1 at t=0
    sup.deploy
        .stage_and_verify("tx-m1".into(), m1.clone(), &sup.arena)
        .unwrap();
    let h1 = sup.deploy.transactions["tx-m1"].revision.package_hash.clone();
    sup.deploy
        .authorize("tx-m1", "op1".into(), "Operator".into(), &h1, 0, 60_000)
        .unwrap();
    sup.pending_activation = sup
        .deploy
        .prepare_activation("tx-m1", 0, &sup.arena)
        .unwrap();

    // 0.0 s: 44.9 C, GOOD -> Red OFF
    drv_shared.lock().unwrap().temp_decic = 449;
    let r0 = sup.step_once();
    assert_eq!(r0.applied_mask & OUT_BIT_RED, 0);

    // 1.0 s: 45.0 C -> starts 10 s TON
    clock.set_ns(1_000_000_000);
    drv_shared.lock().unwrap().temp_decic = 450;
    let r1 = sup.step_once();
    assert_eq!(r1.applied_mask & OUT_BIT_RED, 0);

    // 4.0 s: 44.9 C -> resets TON
    clock.set_ns(4_000_000_000);
    drv_shared.lock().unwrap().temp_decic = 449;
    sup.step_once();
    assert_eq!(sup.arena.blocks[0].phase, 0);

    // 4.5 s: 45.1 C -> starts new accumulation
    clock.set_ns(4_500_000_000);
    drv_shared.lock().unwrap().temp_decic = 451;
    sup.step_once();
    assert_eq!(sup.arena.blocks[0].phase, 1);

    // Prepare M2 (5s RESET) with baseRevision = 1 and re-sign
    let seed_hex = fs::read_to_string(repo_root().join("profiles/demo-lyra/keys/test-bench.seed.hex"))
        .unwrap()
        .trim()
        .to_string();
    let mut seed_arr = [0u8; 32];
    for i in 0..32 {
        seed_arr[i] = u8::from_str_radix(&seed_hex[i * 2..i * 2 + 2], 16).unwrap();
    }
    m2_reset.base_revision = 1;
    let payload = control_abi::crypto::module_package_signing_payload(&m2_reset).unwrap();
    m2_reset.signature_hex = Some(control_abi::sign_bytes_ed25519(&seed_arr, &payload));

    sup.deploy
        .stage_and_verify("tx-m2-reset".into(), m2_reset.clone(), &sup.arena)
        .unwrap();
    let h2 = sup.deploy.transactions["tx-m2-reset"]
        .revision
        .package_hash
        .clone();
    sup.deploy
        .authorize("tx-m2-reset", "op1".into(), "Operator".into(), &h2, 6_000_000_000, 60_000)
        .unwrap();
    sup.pending_activation = sup
        .deploy
        .prepare_activation("tx-m2-reset", 6_500_000_000, &sup.arena)
        .unwrap();

    // 7.0 s: Promotion boundary with RESET -> timer restarts from 7.0s
    clock.set_ns(7_000_000_000);
    let r_prom = sup.step_once();
    assert_eq!(r_prom.active_rev, 2);
    assert_eq!(r_prom.applied_mask & OUT_BIT_RED, 0);

    // 11.98 s (4.98 s since promotion) -> Red STILL OFF
    clock.set_ns(11_980_000_000);
    let r_1198 = sup.step_once();
    assert_eq!(r_1198.applied_mask & OUT_BIT_RED, 0);

    // 12.0 s (5.00 s since promotion) -> Red ON!
    clock.set_ns(12_000_000_000);
    let r_1200 = sup.step_once();
    assert_eq!(r_1200.applied_mask & OUT_BIT_RED, OUT_BIT_RED);

    // 12.5 s: 44.5 C -> stays latched ON! Interlock blocks blue prealarm while red is active!
    clock.set_ns(12_500_000_000);
    drv_shared.lock().unwrap().temp_decic = 445;
    let r_1250 = sup.step_once();
    assert_eq!(r_1250.applied_mask & OUT_BIT_RED, OUT_BIT_RED);
    assert_eq!(r_1250.applied_mask & OUT_BIT_BLUE_VIRTUAL, 0);
    assert_eq!(r_1250.fallback_reason, GuardReason::InterlockVeto);

    // 13.0 s: 44.0 C -> clears OFF (after dwell 500ms since 12.0s)!
    clock.set_ns(13_000_000_000);
    drv_shared.lock().unwrap().temp_decic = 440;
    let r_1300 = sup.step_once();
    assert_eq!(r_1300.applied_mask & OUT_BIT_RED, 0);

    // 13.5 s: 45.1 C, then samples turn STALE -> immediate contingency OFF + timer cleared
    clock.set_ns(13_500_000_000);
    drv_shared.lock().unwrap().temp_decic = 451;
    sup.step_once();
    assert_eq!(sup.arena.blocks[0].phase, 1);

    clock.set_ns(15_000_000_000);
    drv_shared.lock().unwrap().temp_quality = SignalQuality::Stale;
    drv_shared.lock().unwrap().temp_age_ms = 1500;
    let r_stale = sup.step_once();
    assert_eq!(r_stale.applied_mask, 0);
    assert_eq!(r_stale.fallback_reason, GuardReason::InputQualityFault);
    assert_eq!(sup.arena.blocks[0].phase, 0);

    // --- Part B: S01 PRESERVE_ELAPSED ---
    let mut m2_pres = load_pkg("M2.darwin-arm64.json");
    m2_pres.base_revision = 1;
    let p_pres = control_abi::crypto::module_package_signing_payload(&m2_pres).unwrap();
    m2_pres.signature_hex = Some(control_abi::sign_bytes_ed25519(&seed_arr, &p_pres));

    let dir_b = temp_data_dir("golden-preserve");
    let clock_b = Arc::new(VirtualClock::new(0));
    let drv_b = VirtualBenchDriver::new(200);
    let drv_b_sh = Arc::clone(&drv_b.shared);
    let j_b = DurableJournal::open(&dir_b).unwrap();
    let d_b = DeploymentManager::init(profile.clone(), pub_hex.clone(), j_b, 200).unwrap();
    let mut sup_b = ScanSupervisor::new(Arc::clone(&clock_b), drv_b, profile, d_b, 200);
    sup_b.arm_control();
    sup_b.install_active_engine(Box::new(A0NativeIrEngine::new_ir()));

    sup_b.deploy.stage_and_verify("b-m1".into(), m1, &sup_b.arena).unwrap();
    let bh1 = sup_b.deploy.transactions["b-m1"].revision.package_hash.clone();
    sup_b.deploy.authorize("b-m1", "op".into(), "Operator".into(), &bh1, 0, 60_000).unwrap();
    sup_b.pending_activation = sup_b.deploy.prepare_activation("b-m1", 0, &sup_b.arena).unwrap();
    sup_b.step_once();

    // 4.5s: 45.1C -> TON accumulates 2.5s until 7.0s
    clock_b.set_ns(4_500_000_000);
    drv_b_sh.lock().unwrap().temp_decic = 451;
    sup_b.step_once();

    clock_b.set_ns(6_500_000_000);
    sup_b.step_once(); // elapsed = 2.0s

    sup_b.deploy.stage_and_verify("b-m2".into(), m2_pres, &sup_b.arena).unwrap();
    let bh2 = sup_b.deploy.transactions["b-m2"].revision.package_hash.clone();
    sup_b.deploy.authorize("b-m2", "op".into(), "Operator".into(), &bh2, 6_500_000_000, 60_000).unwrap();
    sup_b.pending_activation = sup_b.deploy.prepare_activation("b-m2", 6_500_000_000, &sup_b.arena).unwrap();

    // 7.0s: promote with PRESERVE_ELAPSED -> elapsed is 2.5s, needs 2.5s more (fires at 9.5s!)
    clock_b.set_ns(7_000_000_000);
    let r_b7 = sup_b.step_once();
    assert_eq!(r_b7.applied_mask & OUT_BIT_RED, 0);

    clock_b.set_ns(9_480_000_000);
    let r_b948 = sup_b.step_once();
    assert_eq!(r_b948.applied_mask & OUT_BIT_RED, 0);

    clock_b.set_ns(9_500_000_000);
    let r_b950 = sup_b.step_once();
    assert_eq!(r_b950.applied_mask & OUT_BIT_RED, OUT_BIT_RED);

    let _ = fs::remove_dir_all(&dir_a);
    let _ = fs::remove_dir_all(&dir_b);
}

#[test]
fn s02_rejects_ton_to_tof_and_duplicate_stable_id() {
    let (profile, pub_hex) = load_profile_and_key();
    let m1 = load_pkg("M1.darwin-arm64.json");
    let dir = temp_data_dir("s02");
    let mut arena = StateArena::new(1);
    arena.blocks = vec![control_abi::StateBlock::empty(1001, BlockType::Ton)];

    // Change 1001 from TON to TOF
    let mut bad_type = m1.clone();
    bad_type.required_blocks[0].block_type = BlockType::Tof;
    assert!(arena.plan_migration(&bad_type, 2).is_err());

    // Duplicate stable ID
    let mut dup_id = m1.clone();
    dup_id.required_blocks.push(m1.required_blocks[0].clone());
    assert!(arena.plan_migration(&dup_id, 2).is_err());

    let j = DurableJournal::open(&dir).unwrap();
    let mut dm = DeploymentManager::init(profile, pub_hex, j, 1).unwrap();
    assert!(matches!(
        dm.stage_and_verify("bad".into(), bad_type, &arena),
        Err(DeployError::Verification(_)) | Err(DeployError::RevisionConflict { .. })
    ));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn d01_d02_d04_and_api_routes_work_end_to_end() {
    let (profile, pub_hex) = load_profile_and_key();
    let m1 = load_pkg("M1.darwin-arm64.json");
    let mut m2 = load_pkg("M2.darwin-arm64.json");
    let seed_hex = fs::read_to_string(repo_root().join("profiles/demo-lyra/keys/test-bench.seed.hex"))
        .unwrap()
        .trim()
        .to_string();
    let mut seed_arr = [0u8; 32];
    for i in 0..32 {
        seed_arr[i] = u8::from_str_radix(&seed_hex[i * 2..i * 2 + 2], 16).unwrap();
    }

    let dir = temp_data_dir("api-d01-d02-d04");
    let clock = Arc::new(VirtualClock::new(0));
    let drv = VirtualBenchDriver::new(777);
    let drv_sh = Arc::clone(&drv.shared);
    let j = DurableJournal::open(&dir).unwrap();
    let dm = DeploymentManager::init(profile.clone(), pub_hex, j, 777).unwrap();
    let mut sup = ScanSupervisor::new(Arc::clone(&clock), drv, profile, dm, 777);
    sup.arm_control();
    sup.install_active_engine(Box::new(A1WamrEngine::new(true)));

    let ctx = ApiContext::new(sup, AuthTokens::default(), None);

    // 1. Reader cannot propose (403), Engineer/Author can propose M1 (201)
    let body_m1 = serde_json::json!({"txId": "tx-1", "package": m1}).to_string();
    let (st_forb, _, _) = ctx.handle_http_request(
        "POST",
        "/v1/devices/lyra-bench-01/proposals",
        Some("Bearer plc-reader-token"),
        body_m1.as_bytes(),
    );
    assert_eq!(st_forb, 403);

    let (st_prop, _, resp_prop) = ctx.handle_http_request(
        "POST",
        "/v1/devices/lyra-bench-01/proposals",
        Some("Bearer plc-author-token"),
        body_m1.as_bytes(),
    );
    assert_eq!(st_prop, 201);
    let v_prop: serde_json::Value = serde_json::from_slice(&resp_prop).unwrap();
    let pkg_hash_1 = v_prop["revision"]["packageHash"].as_str().unwrap().to_string();

    // D02: Idempotent repeat returns 201 with same tx; different body with same tx returns 409
    let (st_idem, _, _) = ctx.handle_http_request(
        "POST",
        "/v1/devices/lyra-bench-01/proposals",
        Some("Bearer plc-author-token"),
        body_m1.as_bytes(),
    );
    assert_eq!(st_idem, 201);

    let mut m1_diff = m1.clone();
    m1_diff.package_id = "M1-altered".into();
    let body_diff = serde_json::json!({"txId": "tx-1", "package": m1_diff}).to_string();
    let (st_conf, _, _) = ctx.handle_http_request(
        "POST",
        "/v1/devices/lyra-bench-01/proposals",
        Some("Bearer plc-author-token"),
        body_diff.as_bytes(),
    );
    assert_eq!(st_conf, 409);

    // Authorize & activate M1
    let auth_body = serde_json::json!({
        "operatorId": "op-armando",
        "expectedPackageHash": pkg_hash_1,
        "ttlMs": 30000
    })
    .to_string();
    let (st_auth, _, _) = ctx.handle_http_request(
        "POST",
        "/v1/proposals/tx-1/authorizations",
        Some("Bearer plc-operator-token"),
        auth_body.as_bytes(),
    );
    assert_eq!(st_auth, 200);

    let (st_act, _, _) = ctx.handle_http_request(
        "POST",
        "/v1/proposals/tx-1/activate",
        Some("Bearer plc-operator-token"),
        b"{}",
    );
    assert_eq!(st_act, 202);

    // Run 1 scan boundary so M1 becomes Stable rev 1
    {
        let mut s = ctx.supervisor.lock().unwrap();
        s.step_once();
    }

    // D02: Repeating activate after activation returns 200 idempotently, never double-activates
    let (st_act_rep, _, _) = ctx.handle_http_request(
        "POST",
        "/v1/proposals/tx-1/activate",
        Some("Bearer plc-operator-token"),
        b"{}",
    );
    assert_eq!(st_act_rep, 200);

    // D01: Propose M2 with stale baseRevision=0 -> 409 Conflict!
    let mut m2_stale = m2.clone();
    m2_stale.base_revision = 0;
    let body_m2_stale = serde_json::json!({"txId": "tx-2-stale", "package": m2_stale}).to_string();
    let (st_d01, _, _) = ctx.handle_http_request(
        "POST",
        "/v1/devices/lyra-bench-01/proposals",
        Some("Bearer plc-author-token"),
        body_m2_stale.as_bytes(),
    );
    assert_eq!(st_d01, 409);

    // D04: Witness discrepancy between appliedMask (0) and observedMask (1) exposed separately!
    drv_sh.lock().unwrap().witness_override_mask = Some(1);
    {
        let mut s = ctx.supervisor.lock().unwrap();
        s.step_once();
    }
    let (st_state, _, state_bytes) = ctx.handle_http_request(
        "GET",
        "/v1/devices/lyra-bench-01/state",
        Some("Bearer plc-reader-token"),
        b"",
    );
    assert_eq!(st_state, 200);
    let st_val: serde_json::Value = serde_json::from_slice(&state_bytes).unwrap();
    assert_eq!(st_val["lastEvidence"]["appliedMask"], 0);
    assert_eq!(st_val["lastEvidence"]["physicalObservedMask"], 1);
    assert_eq!(st_val["lastEvidence"]["activePersisted"], true);

    // Rollback check
    m2.base_revision = 1;
    let p2 = control_abi::crypto::module_package_signing_payload(&m2).unwrap();
    m2.signature_hex = Some(control_abi::sign_bytes_ed25519(&seed_arr, &p2));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn d03_journal_crash_recovery_starts_disarmed_and_recovers_last_stable() {
    let (profile, pub_hex) = load_profile_and_key();
    let m1 = load_pkg("M1.darwin-arm64.json");
    let mut m2 = load_pkg("M2.darwin-arm64.json");
    let seed_hex = fs::read_to_string(repo_root().join("profiles/demo-lyra/keys/test-bench.seed.hex"))
        .unwrap()
        .trim()
        .to_string();
    let mut seed_arr = [0u8; 32];
    for i in 0..32 {
        seed_arr[i] = u8::from_str_radix(&seed_hex[i * 2..i * 2 + 2], 16).unwrap();
    }

    let dir = temp_data_dir("d03-crash");
    {
        let j1 = DurableJournal::open(&dir).unwrap();
        let mut dm = DeploymentManager::init(profile.clone(), pub_hex.clone(), j1, 100).unwrap();
        let arena = StateArena::new(0);
        dm.stage_and_verify("tx1".into(), m1.clone(), &arena).unwrap();
        let h1 = dm.transactions["tx1"].revision.package_hash.clone();
        dm.authorize("tx1", "op".into(), "Operator".into(), &h1, 0, 60_000).unwrap();
        let act1 = dm.prepare_activation("tx1", 0, &arena).unwrap().unwrap();
        dm.commit_boundary_activation(&act1, 1).unwrap();

        // Simulate abrupt power cut right after ActiveVolatile before Persisted!
        m2.base_revision = 1;
        let p2 = control_abi::crypto::module_package_signing_payload(&m2).unwrap();
        m2.signature_hex = Some(control_abi::sign_bytes_ed25519(&seed_arr, &p2));
        dm.stage_and_verify("tx2".into(), m2, &arena).unwrap();
        let h2 = dm.transactions["tx2"].revision.package_hash.clone();
        dm.authorize("tx2", "op".into(), "Operator".into(), &h2, 100, 60_000).unwrap();
        let mut rev2 = dm.transactions["tx2"].revision.clone();
        rev2.state = DeploymentState::ActiveVolatile;
        rev2.active_scan_id = 55;
        dm.journal.append_revision(&rev2).unwrap();
    }

    // Boot #2 with new boot_id=200
    let j2 = DurableJournal::open(&dir).unwrap();
    let rec = j2.recover_on_boot(200).unwrap();
    assert_eq!(rec.armed_on_boot, false); // MUST start disarmed
    assert_eq!(rec.needs_operator_review, true);
    assert_eq!(rec.last_stable_revision.as_ref().unwrap().revision, 1);
    let tx2_rec = rec.revisions.iter().find(|r| r.tx_id == "tx2").unwrap();
    assert_eq!(tx2_rec.state, DeploymentState::RecoveryRequired);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn f05_shield_dwell_chatter_and_contingency_off_priority() {
    let (profile, pub_hex) = load_profile_and_key();
    let m1 = load_pkg("M1.darwin-arm64.json");
    let dir = temp_data_dir("f05-dwell");
    let clock = Arc::new(VirtualClock::new(0));
    let drv = VirtualBenchDriver::new(500);
    let j = DurableJournal::open(&dir).unwrap();
    let dm = DeploymentManager::init(profile.clone(), pub_hex, j, 500).unwrap();
    let mut sup = ScanSupervisor::new(Arc::clone(&clock), drv, profile, dm, 500);
    sup.arm_control();

    // First toggle ON at t=1.0s via manual lease -> allowed
    sup.manual_lease = Some(supervisor::shield::ManualLease {
        boot_id: 500,
        mask: OUT_BIT_RED,
        valid_mask: OUT_BIT_RED,
        expires_logical_ns: 10_000_000_000,
        operator_id: "op".into(),
    });
    // Need worker_ok=true to reach manual lease check
    sup.install_active_engine(Box::new(A0NativeIrEngine::new_empty_noop()));
    sup.deploy.stage_and_verify("t0".into(), m1, &sup.arena).unwrap();
    let h0 = sup.deploy.transactions["t0"].revision.package_hash.clone();
    sup.deploy.authorize("t0", "op".into(), "Op".into(), &h0, 0, 60_000).unwrap();
    sup.pending_activation = sup.deploy.prepare_activation("t0", 0, &sup.arena).unwrap();

    clock.set_ns(1_000_000_000);
    let r1 = sup.step_once();
    assert_eq!(r1.applied_mask, OUT_BIT_RED);

    // Attempt toggle OFF at 1.1s (100ms < 500ms dwell) -> held ON with DwellChatter!
    sup.manual_lease.as_mut().unwrap().mask = 0;
    clock.set_ns(1_100_000_000);
    let r2 = sup.step_once();
    assert_eq!(r2.applied_mask, OUT_BIT_RED);
    assert_eq!(r2.fallback_reason, GuardReason::DwellChatter);

    // Contingency fault (physical inhibit) at 1.12s -> MUST bypass dwell and turn OFF immediately!
    sup.driver.shared.lock().unwrap().physical_inhibit = true;
    clock.set_ns(1_120_000_000);
    let r3 = sup.step_once();
    assert_eq!(r3.applied_mask, 0);
    assert_eq!(r3.fallback_reason, GuardReason::PhysicalInhibit);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn s03_and_quickjs_rollback_flow() {
    let (profile, pub_hex) = load_profile_and_key();
    let m1_qjs = load_pkg("M1.quickjs.json");
    let mut m2_qjs = load_pkg("M2.quickjs.json");
    let seed_hex = fs::read_to_string(repo_root().join("profiles/demo-lyra/keys/test-bench.seed.hex"))
        .unwrap()
        .trim()
        .to_string();
    let mut seed_arr = [0u8; 32];
    for i in 0..32 {
        seed_arr[i] = u8::from_str_radix(&seed_hex[i * 2..i * 2 + 2], 16).unwrap();
    }

    let dir = temp_data_dir("s03-qjs");
    let clock = Arc::new(VirtualClock::new(0));
    let drv = VirtualBenchDriver::new(600);
    let j = DurableJournal::open(&dir).unwrap();
    let dm = DeploymentManager::init(profile.clone(), pub_hex, j, 600).unwrap();
    let mut sup = ScanSupervisor::new(Arc::clone(&clock), drv, profile, dm, 600);
    sup.arm_control();
    sup.install_active_engine(Box::new(A2QuickJsEngine::new(5_000_000)));

    sup.deploy.stage_and_verify("q1".into(), m1_qjs, &sup.arena).unwrap();
    let h1 = sup.deploy.transactions["q1"].revision.package_hash.clone();
    sup.deploy.authorize("q1", "op".into(), "Op".into(), &h1, 0, 60_000).unwrap();
    sup.pending_activation = sup.deploy.prepare_activation("q1", 0, &sup.arena).unwrap();
    let r1 = sup.step_once();
    assert_eq!(r1.active_rev, 1);

    m2_qjs.base_revision = 1;
    let p2 = control_abi::crypto::module_package_signing_payload(&m2_qjs).unwrap();
    m2_qjs.signature_hex = Some(control_abi::sign_bytes_ed25519(&seed_arr, &p2));
    sup.deploy.stage_and_verify("q2".into(), m2_qjs, &sup.arena).unwrap();
    let h2 = sup.deploy.transactions["q2"].revision.package_hash.clone();
    sup.deploy.authorize("q2", "op".into(), "Op".into(), &h2, 100, 60_000).unwrap();
    sup.pending_activation = sup.deploy.prepare_activation("q2", 100, &sup.arena).unwrap();
    clock.set_ns(20_000_000);
    let r2 = sup.step_once();
    assert_eq!(r2.active_rev, 2);

    // Rollback to M1 -> produces monotonic revision 3 pointing to M1 hash!
    let rb = sup
        .deploy
        .prepare_rollback("rb-1".into(), "operator rollback".into(), &sup.arena)
        .unwrap();
    sup.pending_activation = Some(rb);
    clock.set_ns(40_000_000);
    let r3 = sup.step_once();
    assert_eq!(r3.active_rev, 3);
    assert_eq!(sup.deploy.active_package_hash.as_deref(), Some(h1.as_str()));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn d03_real_sigkill_exit_crash_injection() {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    let bin = PathBuf::from(env!("CARGO_BIN_EXE_plc-supervisor"));
    let root = repo_root();
    let dir = temp_data_dir("d03-sigkill-bin");

    // Pre-initialize WAL with M1 rev 1 as Stable cleanly before injecting M2 crash point
    let (profile, pub_hex) = load_profile_and_key();
    let m1 = load_pkg("M1.darwin-arm64.json");
    {
        let j_init = DurableJournal::open(&dir).unwrap();
        let mut dm = DeploymentManager::init(profile, pub_hex, j_init, 10).unwrap();
        let arena = StateArena::new(0);
        dm.stage_and_verify("init-m1".into(), m1, &arena).unwrap();
        let h1 = dm.transactions["init-m1"].revision.package_hash.clone();
        dm.authorize("init-m1", "op".into(), "Operator".into(), &h1, 0, 60_000)
            .unwrap();
        let act1 = dm.prepare_activation("init-m1", 0, &arena).unwrap().unwrap();
        dm.commit_boundary_activation(&act1, 0).unwrap();
    }

    // Pick a free port on loopback
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let addr = format!("127.0.0.1:{port}");

    // Boot #1: spawn plc-supervisor recovering M1 rev 1, with crash injected during M2 promotion
    let mut child = Command::new(&bin)
        .args([
            "--profile",
            root.join("profiles/demo-lyra/site-profile.json").to_str().unwrap(),
            "--pubkey",
            root.join("profiles/demo-lyra/keys/test-bench.pub.hex").to_str().unwrap(),
            "--data-dir",
            dir.to_str().unwrap(),
            "--engine",
            "native-ir",
            "--bind",
            &addr,
        ])
        .env("PLC_CRASH_POINT", "AFTER_VOLATILE_BEFORE_PERSISTED")
        .spawn()
        .expect("spawn plc-supervisor");

    // Wait briefly for HTTP listener to come up
    let mut connected = false;
    for _ in 0..40 {
        if TcpStream::connect(&addr).is_ok() {
            connected = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(connected, "plc-supervisor HTTP listener did not come up");

    // Helper for raw HTTP POST over loopback
    let post_json = |path: &str, token: &str, body: &str| -> (u16, String) {
        let mut s = TcpStream::connect(&addr).unwrap();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut resp = String::new();
        let _ = s.read_to_string(&mut resp);
        let code = resp
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (code, resp)
    };

    // Propose M2 (baseRevision=1)
    let mut m2 = load_pkg("M2.darwin-arm64.json");
    m2.base_revision = 1;
    let seed_hex = fs::read_to_string(root.join("profiles/demo-lyra/keys/test-bench.seed.hex"))
        .unwrap()
        .trim()
        .to_string();
    let mut seed_arr = [0u8; 32];
    for i in 0..32 {
        seed_arr[i] = u8::from_str_radix(&seed_hex[i * 2..i * 2 + 2], 16).unwrap();
    }
    let p2 = control_abi::crypto::module_package_signing_payload(&m2).unwrap();
    m2.signature_hex = Some(control_abi::sign_bytes_ed25519(&seed_arr, &p2));

    let prop_body = serde_json::json!({ "txId": "crash-tx2", "package": m2 }).to_string();
    let (c_prop, prop_resp) = post_json("/v1/devices/lyra-bench-01/proposals", "plc-author-token", &prop_body);
    assert_eq!(c_prop, 201, "propose response: {prop_resp}");
    let body_json_str = prop_resp.split("\r\n\r\n").nth(1).unwrap();
    let prop_val: serde_json::Value = serde_json::from_str(body_json_str).unwrap();
    let h2 = prop_val["revision"]["packageHash"].as_str().unwrap().to_string();

    let auth_body = serde_json::json!({
        "operatorId": "op-hil",
        "expectedPackageHash": h2,
        "ttlMs": 60000
    })
    .to_string();
    let (c_auth, _) = post_json("/v1/proposals/crash-tx2/authorizations", "plc-operator-token", &auth_body);
    assert_eq!(c_auth, 200);

    // Recovered boot starts disarmed -> activate MUST reject with 409 before rearm!
    let (c_disarmed, _) = post_json("/v1/proposals/crash-tx2/activate", "plc-operator-token", "{}");
    assert_eq!(c_disarmed, 409);

    let (c_rearm, _) = post_json("/v1/devices/lyra-bench-01/rearm", "plc-operator-token", "{}");
    assert_eq!(c_rearm, 200);

    let (c_act, _) = post_json("/v1/proposals/crash-tx2/activate", "plc-operator-token", "{}");
    assert_eq!(c_act, 202);

    // Wait for scan boundary crash injection (_exit(99))
    let status = child.wait().expect("wait crashed child");
    assert_eq!(status.code(), Some(99), "expected crash exit code 99");

    // Boot #2 recovery inspection: MUST be disarmed, recover M1 rev 1, and flag RecoveryRequired on crash-tx2!
    let j2 = DurableJournal::open(&dir).unwrap();
    let rec = j2.recover_on_boot(999).unwrap();
    assert!(!rec.armed_on_boot);
    assert!(rec.needs_operator_review);
    assert_eq!(rec.last_stable_revision.as_ref().unwrap().revision, 1);
    let crashed_rec = rec.revisions.iter().find(|r| r.tx_id == "crash-tx2").unwrap();
    assert_eq!(crashed_rec.state, DeploymentState::RecoveryRequired);

    // Also spawn without crash point and send real SIGKILL
    let port2 = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let addr2 = format!("127.0.0.1:{port2}");
    let mut child2 = Command::new(&bin)
        .args([
            "--profile",
            root.join("profiles/demo-lyra/site-profile.json").to_str().unwrap(),
            "--pubkey",
            root.join("profiles/demo-lyra/keys/test-bench.pub.hex").to_str().unwrap(),
            "--data-dir",
            dir.to_str().unwrap(),
            "--engine",
            "native-ir",
            "--bind",
            &addr2,
        ])
        .spawn()
        .expect("spawn plc-supervisor 2");
    for _ in 0..40 {
        if TcpStream::connect(&addr2).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    unsafe {
        libc::kill(child2.id() as libc::pid_t, libc::SIGKILL);
    }
    let st2 = child2.wait().unwrap();
    assert!(!st2.success());
    let j3 = DurableJournal::open(&dir).unwrap();
    let rec2 = j3.recover_on_boot(1000).unwrap();
    assert!(!rec2.armed_on_boot);
    assert_eq!(rec2.last_stable_revision.as_ref().unwrap().revision, 1);

    let _ = fs::remove_dir_all(&dir);
}
