use serde::{Deserialize, Serialize};

use crate::types::{
    u64_as_dec_string, BlockType, DriverStatus, GuardReason, MigrationPolicy, RetentionClass,
    SignalQuality,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TimingStateOnChange {
    PreserveElapsed,
    Reset,
    RejectIncompatible,
}

impl From<TimingStateOnChange> for MigrationPolicy {
    fn from(v: TimingStateOnChange) -> Self {
        match v {
            TimingStateOnChange::PreserveElapsed => MigrationPolicy::PreserveElapsed,
            TimingStateOnChange::Reset => MigrationPolicy::Reset,
            TimingStateOnChange::RejectIncompatible => MigrationPolicy::RejectIncompatible,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputChannelId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuleOp {
    pub op: String,
    pub rule_id: String,
    pub template_id: String,
    pub input: String,
    pub unit: String,
    #[serde(default)]
    pub trigger_at_or_above: Option<i32>,
    #[serde(default)]
    pub clear_at_or_below: Option<i32>,
    #[serde(default)]
    pub band_low: Option<i32>,
    #[serde(default)]
    pub band_high: Option<i32>,
    #[serde(default)]
    pub pulse_period_ms: Option<u32>,
    #[serde(default)]
    pub hold_ms: Option<u32>,
    pub required_quality: SignalQuality,
    pub on_input_fault: String,
    pub output: String,
    pub timing_state_on_change: TimingStateOnChange,
    #[serde(default)]
    pub stable_block_id: Option<u32>,
    #[serde(default)]
    pub block_type: Option<BlockType>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuleSetV1 {
    pub schema: String,
    pub base_revision: String,
    pub changes: Vec<RuleOp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChannelSpec {
    pub name: String,
    pub mask_bit: u8,
    pub physical: bool,
    pub gpio_chip: Option<String>,
    pub gpio_offset: Option<u32>,
    pub active_low: bool,
    pub dwell_ms: u32,
    pub max_transitions_per_window: u32,
    pub transition_window_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SensorSpec {
    pub name: String,
    pub signal_id: u32,
    pub unit: String,
    pub min_valid: i32,
    pub max_valid: i32,
    pub abnormal_sentinel: i32,
    pub poll_interval_ms: u32,
    pub freshness_ttl_ms: u32,
    pub device_path: String,
    pub slave_addr: u8,
    pub baud_rate: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InterlockRule {
    pub name: String,
    pub forbidden_mask: u32,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SiteProfileV1 {
    pub schema: String,
    pub device_id: String,
    pub profile_id: String,
    pub profile_version: String,
    pub scan_period_ms: u32,
    pub alt_scan_period_ms: u32,
    pub sample_deadline_ms: u32,
    pub eval_deadline_ms: u32,
    pub publish_deadline_ms: u32,
    pub allowed_physical_mask: u32,
    pub safe_fallback_mask: u32,
    pub channels: Vec<ChannelSpec>,
    pub sensors: Vec<SensorSpec>,
    pub interlocks: Vec<InterlockRule>,
    #[serde(default)]
    pub signature_hex: Option<String>,
    #[serde(default)]
    pub public_key_hex: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationPlanEntry {
    pub stable_id: u32,
    pub block_type: BlockType,
    pub schema_version: u8,
    pub state_size: u32,
    pub retention_class: RetentionClass,
    pub migration_policy: MigrationPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModulePackageV1 {
    pub schema: String,
    pub package_id: String,
    #[serde(with = "u64_as_dec_string")]
    pub base_revision: u64,
    pub target: String,
    pub engine: String,
    pub abi_version: u16,
    pub state_schema: String,
    pub profile_hash: String,
    pub ir_digest: String,
    pub canonical_source_digest: String,
    #[serde(default)]
    pub wasm_digest: Option<String>,
    #[serde(default)]
    pub aot_digest: Option<String>,
    pub migration_plan_hash: String,
    pub policy_hash: String,
    pub max_state_bytes: u32,
    pub max_linear_mem_bytes: u32,
    pub requested_capabilities: Vec<String>,
    pub required_blocks: Vec<MigrationPlanEntry>,
    pub rule_set: RuleSetV1,
    #[serde(default)]
    pub wasm_base64: Option<String>,
    #[serde(default)]
    pub aot_base64: Option<String>,
    #[serde(default)]
    pub js_source: Option<String>,
    #[serde(default)]
    pub signature_hex: Option<String>,
    #[serde(default)]
    pub public_key_hex: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DeploymentState {
    Staged,
    Verified,
    Shadow,
    Armed,
    ActiveVolatile,
    Persisted,
    Stable,
    Rejected,
    Recovering,
    RolledBack,
    FallbackLatched,
    RecoveryRequired,
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeploymentRevision {
    pub tx_id: String,
    pub device_id: String,
    #[serde(with = "u64_as_dec_string")]
    pub boot_id: u64,
    #[serde(with = "u64_as_dec_string")]
    pub revision: u64,
    #[serde(with = "u64_as_dec_string")]
    pub base_revision: u64,
    pub package_hash: String,
    pub logic_hash: String,
    pub rule_set_hash: String,
    pub profile_hash: String,
    pub state_schema: String,
    pub state: DeploymentState,
    #[serde(with = "u64_as_dec_string")]
    pub active_scan_id: u64,
}

/// Four-dimensional acknowledgement evidence required by §6.3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvidenceBundle {
    #[serde(with = "u64_as_dec_string")]
    pub boot_id: u64,
    #[serde(with = "u64_as_dec_string")]
    pub scan_id: u64,
    #[serde(with = "u64_as_dec_string")]
    pub active_revision: u64,
    #[serde(with = "u64_as_dec_string")]
    pub output_seq: u64,
    pub selected_in_scan: bool,
    pub guard_reason: GuardReason,
    pub desired_mask: u32,
    pub applied_mask: u32,
    pub driver_applied: DriverStatus,
    pub physical_observed_mask: Option<u32>,
    pub physical_observation_source: String,
    pub active_persisted: bool,
    #[serde(with = "u64_as_dec_string")]
    pub observed_at_ns: u64,
    #[serde(with = "u64_as_dec_string")]
    pub expires_at_ns: u64,
}
