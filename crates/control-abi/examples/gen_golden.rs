use std::fs;
use std::path::PathBuf;

use control_abi::crypto::{
    module_package_signing_payload, profile_hash, profile_signing_payload, sha256_hex,
    sign_bytes_ed25519,
};
use control_abi::schema::{
    ChannelSpec, InterlockRule, MigrationPlanEntry, ModulePackageV1, RuleOp, RuleSetV1,
    SensorSpec, SiteProfileV1, TimingStateOnChange,
};
use control_abi::types::{
    BlockType, EvalStatus, InputSignal, MigrationPolicy, RetentionClass, SignalQuality,
    StateBlock, WorkerMode, ABI_VERSION, SIGNAL_PHYSICAL_INHIBIT, SIGNAL_TEMP_PROCESS,
};
use control_abi::wire::{encode_eval_request, encode_eval_result, EvalRequest, EvalResult};
use ed25519_dalek::SigningKey;
use serde_json::json;

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    // Deterministic bench test keypair (test domain only, §6.3)
    let sk_bytes: [u8; 32] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff, 0x01, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0, 0xb0, 0xc0,
        0xd0, 0xe0, 0xf0, 0x02,
    ];
    let sk = SigningKey::from_bytes(&sk_bytes);
    let sk_hex: String = sk_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let pub_hex: String = sk
        .verifying_key()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let keys_dir = root.join("profiles/demo-lyra/keys");
    fs::create_dir_all(&keys_dir).unwrap();
    fs::write(keys_dir.join("test-bench.pub.hex"), format!("{pub_hex}\n")).unwrap();
    fs::write(keys_dir.join("test-bench.seed.hex"), format!("{sk_hex}\n")).unwrap();

    // Build and sign profiles/demo-lyra/site-profile.json
    let mut profile = SiteProfileV1 {
        schema: "site-profile/v1".into(),
        device_id: "lyra-bench-01".into(),
        profile_id: "demo-lyra".into(),
        profile_version: "1.0.0".into(),
        scan_period_ms: 20,
        alt_scan_period_ms: 50,
        sample_deadline_ms: 2,
        eval_deadline_ms: 10,
        publish_deadline_ms: 13,
        allowed_physical_mask: 0x03,
        safe_fallback_mask: 0x00,
        channels: vec![
            ChannelSpec {
                name: "tower.red".into(),
                mask_bit: 0,
                physical: true,
                gpio_chip: Some("/dev/gpiochip0".into()),
                gpio_offset: Some(17),
                active_low: false,
                dwell_ms: 500,
                max_transitions_per_window: 6,
                transition_window_ms: 10_000,
            },
            ChannelSpec {
                name: "tower.green".into(),
                mask_bit: 1,
                physical: true,
                gpio_chip: Some("/dev/gpiochip0".into()),
                gpio_offset: Some(18),
                active_low: false,
                dwell_ms: 500,
                max_transitions_per_window: 6,
                transition_window_ms: 10_000,
            },
            ChannelSpec {
                name: "tower.blue".into(),
                mask_bit: 2,
                physical: false,
                gpio_chip: None,
                gpio_offset: None,
                active_low: false,
                dwell_ms: 0,
                max_transitions_per_window: 100,
                transition_window_ms: 10_000,
            },
        ],
        sensors: vec![SensorSpec {
            name: "temp.process".into(),
            signal_id: SIGNAL_TEMP_PROCESS,
            unit: "degC_x10".into(),
            min_valid: -400,
            max_valid: 1250,
            abnormal_sentinel: 0x7FFF,
            poll_interval_ms: 500,
            freshness_ttl_ms: 1500,
            device_path: "/dev/ttyS2".into(),
            slave_addr: 1,
            baud_rate: 9600,
        }],
        interlocks: vec![InterlockRule {
            name: "red_excludes_blue_prealarm".into(),
            forbidden_mask: 0b101,
            reason: "alarm_excludes_prealarm".into(),
        }],
        signature_hex: None,
        public_key_hex: Some(pub_hex.clone()),
    };
    let payload = profile_signing_payload(&profile).unwrap();
    profile.signature_hex = Some(sign_bytes_ed25519(&sk_bytes, &payload));
    fs::write(
        root.join("profiles/demo-lyra/site-profile.json"),
        serde_json::to_string_pretty(&profile).unwrap() + "\n",
    )
    .unwrap();

    // Generate Golden ABI fixtures for Rust <-> TypeScript cross-verification
    let golden_dir = root.join("profiles/demo-lyra/golden");
    fs::create_dir_all(&golden_dir).unwrap();

    let req = EvalRequest {
        abi_version: ABI_VERSION,
        mode: WorkerMode::Active,
        boot_id: 1_777_319_400_000_000_001,
        lease_epoch: 12,
        generation: 4,
        scan_id: 500,
        logical_ns: 7_000_000_000,
        input_seq: 14,
        state_revision: 41,
        applied_output_mask: 0x02,
        signals: vec![
            InputSignal {
                id: SIGNAL_TEMP_PROCESS,
                value: 451,
                quality: SignalQuality::Good,
                sample_age_ms: 100,
            },
            InputSignal {
                id: SIGNAL_PHYSICAL_INHIBIT,
                value: 0,
                quality: SignalQuality::Good,
                sample_age_ms: 5,
            },
        ],
        blocks: vec![StateBlock {
            stable_id: 1001,
            block_type: BlockType::Ton,
            schema_version: 1,
            retention_class: RetentionClass::Cyclic,
            migration_policy: MigrationPolicy::PreserveElapsed,
            prev_cond: 1,
            phase: 1,
            last_output: 0,
            flags: 0,
            count_or_param: 0,
            start_logical_ns: 4_500_000_000,
            elapsed_ns: 2_500_000_000,
            aux_i64: -10,
        }],
    };
    let mut req_bytes = Vec::new();
    encode_eval_request(&req, &mut req_bytes).unwrap();
    fs::write(golden_dir.join("eval_request.bin"), &req_bytes).unwrap();

    let res = EvalResult {
        abi_version: ABI_VERSION,
        status: EvalStatus::Ok,
        lease_epoch: 12,
        generation: 4,
        scan_id: 500,
        state_revision: 41,
        desired_mask: 0x01,
        valid_mask: 0x07,
        diag_code: 0,
        eval_duration_ns: 12_345,
        blocks: req.blocks.clone(),
    };
    let mut res_bytes = Vec::new();
    encode_eval_result(&res, &mut res_bytes).unwrap();
    fs::write(golden_dir.join("eval_result.bin"), &res_bytes).unwrap();

    let p_hash = profile_hash(&profile).unwrap();
    let rule_set = RuleSetV1 {
        schema: "rule-set/v1".into(),
        base_revision: "41".into(),
        changes: vec![RuleOp {
            op: "upsertRule".into(),
            rule_id: "temp.high-alarm".into(),
            template_id: "thresholdHoldWithLatchedHysteresis".into(),
            input: "temp.process".into(),
            unit: "degC_x10".into(),
            trigger_at_or_above: Some(450),
            clear_at_or_below: Some(440),
            band_low: None,
            band_high: None,
            pulse_period_ms: None,
            hold_ms: Some(10_000),
            required_quality: SignalQuality::Good,
            on_input_fault: "COMMAND_OFF_AND_RESET".into(),
            output: "tower.red".into(),
            timing_state_on_change: TimingStateOnChange::Reset,
            stable_block_id: Some(1001),
            block_type: Some(BlockType::Ton),
        }],
    };
    let mut pkg = ModulePackageV1 {
        schema: "module-package/v1".into(),
        package_id: "golden-m1".into(),
        base_revision: 41,
        target: "armv7-linux-gnueabihf".into(),
        engine: "native-ir".into(),
        abi_version: ABI_VERSION,
        state_schema: "arena/v1".into(),
        profile_hash: p_hash,
        ir_digest: sha256_hex(b"golden-ir"),
        canonical_source_digest: sha256_hex(b"golden-src"),
        wasm_digest: None,
        aot_digest: None,
        migration_plan_hash: sha256_hex(b"golden-mig"),
        policy_hash: sha256_hex(b"golden-pol"),
        max_state_bytes: 1280,
        max_linear_mem_bytes: 65536,
        requested_capabilities: vec![],
        required_blocks: vec![MigrationPlanEntry {
            stable_id: 1001,
            block_type: BlockType::Ton,
            schema_version: 1,
            state_size: 40,
            retention_class: RetentionClass::Cyclic,
            migration_policy: MigrationPolicy::Reset,
        }],
        rule_set,
        wasm_base64: None,
        aot_base64: None,
        js_source: None,
        signature_hex: None,
        public_key_hex: Some(pub_hex.clone()),
    };
    let pkg_payload = module_package_signing_payload(&pkg).unwrap();
    pkg.signature_hex = Some(sign_bytes_ed25519(&sk_bytes, &pkg_payload));
    fs::write(
        golden_dir.join("golden_package.json"),
        serde_json::to_string_pretty(&pkg).unwrap() + "\n",
    )
    .unwrap();

    fs::write(
        golden_dir.join("golden_digests.json"),
        serde_json::to_string_pretty(&json!({
            "reqSha256": sha256_hex(&req_bytes),
            "resSha256": sha256_hex(&res_bytes),
            "profileHash": pkg.profile_hash,
            "publicKeyHex": pub_hex,
        }))
        .unwrap()
            + "\n",
    )
    .unwrap();

    println!("Generated site-profile.json and golden fixtures successfully.");
}
