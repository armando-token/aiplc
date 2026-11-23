use control_abi::crypto::{
    module_package_signing_payload, profile_hash, sha256_hex, sign_bytes_ed25519,
    verify_module_package_signature, verify_site_profile_signature,
};
use control_abi::schema::{
    ChannelSpec, InterlockRule, MigrationPlanEntry, ModulePackageV1, RuleOp, RuleSetV1,
    SensorSpec, SiteProfileV1, TimingStateOnChange,
};
use control_abi::types::{
    BlockType, EvalStatus, InputSignal, MigrationPolicy, RetentionClass, SignalQuality,
    StateBlock, WorkerMode, ABI_VERSION, SIGNAL_TEMP_PROCESS,
};
use control_abi::wire::{
    decode_eval_request, decode_eval_result, encode_eval_request, encode_eval_result, EvalRequest,
    EvalResult,
};
use ed25519_dalek::SigningKey;

#[test]
fn wire_roundtrip_request_and_result() {
    let req = EvalRequest {
        abi_version: ABI_VERSION,
        mode: WorkerMode::Active,
        boot_id: 987654321012345,
        lease_epoch: 7,
        generation: 3,
        scan_id: 42,
        logical_ns: 1_000_000_000,
        input_seq: 11,
        state_revision: 5,
        applied_output_mask: 0x01,
        signals: vec![InputSignal {
            id: SIGNAL_TEMP_PROCESS,
            value: 451,
            quality: SignalQuality::Good,
            sample_age_ms: 120,
        }],
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
            start_logical_ns: 500_000_000,
            elapsed_ns: 500_000_000,
            aux_i64: -42,
        }],
    };
    let mut buf = Vec::new();
    encode_eval_request(&req, &mut buf).unwrap();
    let decoded = decode_eval_request(&buf).unwrap();
    assert_eq!(req, decoded);

    let res = EvalResult {
        abi_version: ABI_VERSION,
        status: EvalStatus::Ok,
        lease_epoch: 7,
        generation: 3,
        scan_id: 42,
        state_revision: 5,
        desired_mask: 0x01,
        valid_mask: 0x03,
        diag_code: 0,
        eval_duration_ns: 45_000,
        blocks: decoded.blocks,
    };
    let mut rbuf = Vec::new();
    encode_eval_result(&res, &mut rbuf).unwrap();
    let rdec = decode_eval_result(&rbuf).unwrap();
    assert_eq!(res, rdec);
}

#[test]
fn rejects_corrupted_magic_version_and_enums() {
    let req = EvalRequest {
        abi_version: ABI_VERSION,
        mode: WorkerMode::Shadow,
        boot_id: 1,
        lease_epoch: 1,
        generation: 1,
        scan_id: 1,
        logical_ns: 1,
        input_seq: 1,
        state_revision: 1,
        applied_output_mask: 0,
        signals: vec![],
        blocks: vec![],
    };
    let mut buf = Vec::new();
    encode_eval_request(&req, &mut buf).unwrap();
    let mut bad_magic = buf.clone();
    bad_magic[0] = b'X';
    assert!(decode_eval_request(&bad_magic).is_err());

    let mut bad_enum = buf.clone();
    bad_enum[6] = 9;
    assert!(decode_eval_request(&bad_enum).is_err());

    let mut truncated = buf[..10].to_vec();
    truncated[0] = b'P';
    assert!(decode_eval_request(&truncated).is_err());
}

#[test]
fn signs_and_verifies_profile_and_package_and_64bit_json_strings() {
    let sk_bytes = [0x42u8; 32];
    let sk = SigningKey::from_bytes(&sk_bytes);
    let pub_hex: String = sk
        .verifying_key()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

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
        channels: vec![ChannelSpec {
            name: "tower.red".into(),
            mask_bit: 0,
            physical: true,
            gpio_chip: Some("/dev/gpiochip0".into()),
            gpio_offset: Some(17),
            active_low: false,
            dwell_ms: 500,
            max_transitions_per_window: 6,
            transition_window_ms: 10_000,
        }],
        sensors: vec![SensorSpec {
            name: "temp.process".into(),
            signal_id: 1,
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
            name: "red_excludes_blue".into(),
            forbidden_mask: 0b101,
            reason: "alarm_excludes_prealarm".into(),
        }],
        signature_hex: None,
        public_key_hex: Some(pub_hex.clone()),
    };
    let payload = control_abi::crypto::profile_signing_payload(&profile).unwrap();
    profile.signature_hex = Some(sign_bytes_ed25519(&sk_bytes, &payload));
    verify_site_profile_signature(&profile, &pub_hex).unwrap();

    let p_hash = profile_hash(&profile).unwrap();
    let rule_set = RuleSetV1 {
        schema: "rule-set/v1".into(),
        base_revision: "1".into(),
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
        package_id: "M1".into(),
        base_revision: 9007199254740993, // > MAX_SAFE_INTEGER to verify string serialization
        target: "armv7-linux-gnueabihf".into(),
        engine: "native-ir".into(),
        abi_version: ABI_VERSION,
        state_schema: "arena/v1".into(),
        profile_hash: p_hash,
        ir_digest: sha256_hex(b"ir"),
        canonical_source_digest: sha256_hex(b"src"),
        wasm_digest: None,
        aot_digest: None,
        migration_plan_hash: sha256_hex(b"mig"),
        policy_hash: sha256_hex(b"pol"),
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
    let pkg_json = serde_json::to_string(&pkg).unwrap();
    assert!(pkg_json.contains("\"9007199254740993\""));

    let pkg_payload = module_package_signing_payload(&pkg).unwrap();
    pkg.signature_hex = Some(sign_bytes_ed25519(&sk_bytes, &pkg_payload));
    verify_module_package_signature(&pkg, &pub_hex).unwrap();

    // Tampering invalidates signature
    let mut tampered = pkg.clone();
    tampered.base_revision += 1;
    assert!(verify_module_package_signature(&tampered, &pub_hex).is_err());
}
