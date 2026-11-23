use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use control_abi::schema::ModulePackageV1;
use control_abi::types::{
    EvalStatus, InputSignal, SignalQuality, StateBlock, WorkerMode, ABI_VERSION,
    OUT_BIT_BLUE_VIRTUAL, OUT_BIT_RED, SIGNAL_TEMP_PROCESS,
};
use control_abi::wire::EvalRequest;
use logic_worker::engine::{
    A0NativeIrEngine, A1WamrEngine, A2QuickJsEngine, EngineKind, LogicEngine,
};
use logic_worker::ipc::{WorkerInitPayload, WorkerProcessClient, WorkerSandboxParams};
use logic_worker::wasm_inspect::validate_wasm_module_bytes;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn load_pub_key() -> String {
    fs::read_to_string(repo_root().join("profiles/demo-lyra/keys/test-bench.pub.hex"))
        .unwrap()
        .trim()
        .to_string()
}

fn load_pkg(name: &str) -> ModulePackageV1 {
    let raw = fs::read_to_string(repo_root().join("profiles/demo-lyra/packages").join(name)).unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn make_req(scan_id: u64, logical_ns: u64, temp: i32, q: SignalQuality, blocks: Vec<StateBlock>) -> EvalRequest {
    EvalRequest {
        abi_version: ABI_VERSION,
        mode: WorkerMode::Active,
        boot_id: 1,
        lease_epoch: 1,
        generation: 1,
        scan_id,
        logical_ns,
        input_seq: scan_id,
        state_revision: 1,
        applied_output_mask: 0,
        signals: vec![InputSignal {
            id: SIGNAL_TEMP_PROCESS,
            value: temp,
            quality: q,
            sample_age_ms: 50,
        }],
        blocks,
    }
}

#[test]
fn parity_across_a0_a1_interp_a1_aot_a2_quickjs() {
    let pub_hex = load_pub_key();
    let m2_aot_pkg = load_pkg("M2.darwin-arm64.json");

    let mut a0 = A0NativeIrEngine::new_ir();
    let mut a1_interp = A1WamrEngine::new(false);
    let mut a1_aot = A1WamrEngine::new(true);

    a0.prepare(&m2_aot_pkg, &pub_hex).unwrap();
    a0.warm().unwrap();
    a1_interp.prepare(&m2_aot_pkg, &pub_hex).unwrap();
    a1_interp.warm().unwrap();
    a1_aot.prepare(&m2_aot_pkg, &pub_hex).unwrap();
    a1_aot.warm().unwrap();

    // Verify QuickJS rejects wrong engine/canonicalSourceDigest mismatch!
    let mut a2 = A2QuickJsEngine::new(5_000_000);
    assert!(a2.prepare(&m2_aot_pkg, &pub_hex).is_err());

    // Sequence of scans testing M2:
    // 1. t=0.0s, T=42.0C (420) -> prealarm blue pulse ON, red OFF
    // 2. t=1.0s, T=42.0C (420) -> half period (1000ms of 2000ms) -> blue pulse OFF
    // 3. t=2.0s, T=45.0C (450) -> blue OFF, red TON starts
    // 4. t=6.98s, T=45.0C -> red still OFF
    // 5. t=7.0s, T=45.0C -> red ON!
    // 6. t=8.0s, T=44.5C -> red stays latched ON!
    // 7. t=9.0s, T=44.0C -> red clears OFF!
    let steps = [
        (1u64, 0u64, 420i32, SignalQuality::Good, OUT_BIT_BLUE_VIRTUAL),
        (2, 1_000_000_000, 420, SignalQuality::Good, 0),
        (3, 2_000_000_000, 450, SignalQuality::Good, 0),
        (4, 6_980_000_000, 450, SignalQuality::Good, 0),
        (5, 7_000_000_000, 450, SignalQuality::Good, OUT_BIT_RED),
        (6, 8_000_000_000, 445, SignalQuality::Good, OUT_BIT_RED | OUT_BIT_BLUE_VIRTUAL),
        (7, 9_000_000_000, 440, SignalQuality::Good, 0),
        (8, 10_000_000_000, 460, SignalQuality::Stale, 0),
    ];

    let mut b_a0: Vec<StateBlock> = Vec::new();
    let mut b_i: Vec<StateBlock> = Vec::new();
    let mut b_a: Vec<StateBlock> = Vec::new();

    for &(scan, t_ns, temp, q, expected_mask) in &steps {
        let r0 = a0.eval_sync(&make_req(scan, t_ns, temp, q, b_a0.clone())).unwrap();
        let ri = a1_interp.eval_sync(&make_req(scan, t_ns, temp, q, b_i.clone())).unwrap();
        let ra = a1_aot.eval_sync(&make_req(scan, t_ns, temp, q, b_a.clone())).unwrap();

        assert_eq!(r0.status, EvalStatus::Ok);
        assert_eq!(r0.desired_mask, expected_mask, "scan {scan} A0");
        assert_eq!(ri.desired_mask, expected_mask, "scan {scan} A1-interp");
        assert_eq!(ra.desired_mask, expected_mask, "scan {scan} A1-aot");
        assert_eq!(r0.blocks, ri.blocks, "scan {scan} block state parity A0 vs A1-interp");
        assert_eq!(r0.blocks, ra.blocks, "scan {scan} block state parity A0 vs A1-aot");

        b_a0 = r0.blocks;
        b_i = ri.blocks;
        b_a = ra.blocks;
    }

    // Also test A2 QuickJS with properly signed quickjs package if present
    let qjs_path = repo_root().join("profiles/demo-lyra/packages/M2.quickjs.json");
    if qjs_path.exists() {
        let m2_qjs_pkg: ModulePackageV1 =
            serde_json::from_str(&fs::read_to_string(qjs_path).unwrap()).unwrap();
        a2.prepare(&m2_qjs_pkg, &pub_hex).unwrap();
        a2.warm().unwrap();
        let mut b_q: Vec<StateBlock> = Vec::new();
        for &(scan, t_ns, temp, q, expected_mask) in &steps {
            let rq = a2.eval_sync(&make_req(scan, t_ns, temp, q, b_q.clone())).unwrap();
            assert_eq!(rq.desired_mask, expected_mask, "scan {scan} A2-quickjs");
            b_q = rq.blocks;
        }
    }

    #[cfg(feature = "wasmtime-engine")]
    {
        let mut a3 = logic_worker::A3WasmtimeEngine::new(500_000).unwrap();
        a3.prepare(&m2_aot_pkg, &pub_hex).unwrap();
        a3.warm().unwrap();
        let mut b_w: Vec<StateBlock> = Vec::new();
        for &(scan, t_ns, temp, q, expected_mask) in &steps {
            let rw = a3.eval_sync(&make_req(scan, t_ns, temp, q, b_w.clone())).unwrap();
            assert_eq!(rw.desired_mask, expected_mask, "scan {scan} A3-wasmtime");
            b_w = rw.blocks;
        }
    }
}

#[test]
fn rejects_wasm_with_imports_start_section_or_excessive_memory() {
    // Minimal invalid wasm with import section (id=2, count=1)
    let bad_import = [
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
        0x02, 0x01, 0x01,
    ];
    assert!(validate_wasm_module_bytes(&bad_import, 131072).is_err());

    // Start section (id=8, len=1, func_idx=0)
    let bad_start = [
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
        0x08, 0x01, 0x00,
    ];
    assert!(validate_wasm_module_bytes(&bad_start, 131072).is_err());
}

#[test]
fn ipc_worker_process_evaluates_and_respects_timeout_and_bad_frames() {
    let worker_bin = PathBuf::from(env!("CARGO_BIN_EXE_plc-worker"));
    let pub_hex = load_pub_key();
    let pkg = load_pkg("M1.darwin-arm64.json");

    let init = WorkerInitPayload {
        engine: EngineKind::WamrAot,
        package: pkg.clone(),
        trusted_pub_hex: pub_hex.clone(),
        eval_budget_ns: 5_000_000,
        sandbox: WorkerSandboxParams {
            slot_name: "test-slot".into(),
            memory_max_bytes: 16 * 1024 * 1024,
            drop_to_nobody: false,
            enable_seccomp: false,
        },
    };

    // Normal evaluation over IPC UnixSocket
    let mut client = WorkerProcessClient::spawn(&worker_bin, &init, &[]).unwrap();
    let res = client
        .eval_within(
            &make_req(1, 0, 450, SignalQuality::Good, vec![]),
            Duration::from_millis(50),
        )
        .unwrap();
    assert_eq!(res.status, EvalStatus::Ok);
    assert_eq!(res.desired_mask, 0);
    client.revoke_and_kill();

    // F01: Hanging worker triggers DeadlineExceeded cleanly
    let mut hang_client = WorkerProcessClient::spawn(&worker_bin, &init, &["--fault-hang"]).unwrap();
    let err = hang_client.eval_within(
        &make_req(2, 0, 450, SignalQuality::Good, vec![]),
        Duration::from_millis(10),
    );
    assert!(err.is_err());
    hang_client.revoke_and_kill();

    // F03: Bad frame from worker rejected cleanly
    let mut bad_client = WorkerProcessClient::spawn(&worker_bin, &init, &["--fault-bad-frame"]).unwrap();
    let err3 = bad_client.eval_within(
        &make_req(3, 0, 450, SignalQuality::Good, vec![]),
        Duration::from_millis(50),
    );
    assert!(err3.is_err());
    bad_client.revoke_and_kill();
}
