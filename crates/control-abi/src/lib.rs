//! Shared binary ABI, domain types, canonical JSON schemas, and Ed25519 verification
//! for PLC Automático.

pub mod crypto;
pub mod schema;
pub mod types;
pub mod wire;

pub use crypto::{
    canonical_json_bytes, sha256_hex, sign_bytes_ed25519, verify_bytes_ed25519,
    verify_module_package_signature, verify_site_profile_signature,
};
pub use schema::{
    ChannelSpec, DeploymentRevision, EvidenceBundle, InterlockRule, MigrationPlanEntry,
    ModulePackageV1, OutputChannelId, RuleOp, RuleSetV1, SensorSpec, SiteProfileV1,
    TimingStateOnChange,
};
pub use types::{
    BlockType, DriverStatus, EvalStatus, GuardReason, InputSignal, MigrationPolicy, Observation,
    OutputFrame, RetentionClass, SignalQuality, StateBlock, WorkerMode, ABI_VERSION,
    MAX_BLOCKS, MAX_IPC_FRAME_BYTES, MAX_SIGNALS, OUT_BIT_BLUE_VIRTUAL, OUT_BIT_GREEN,
    OUT_BIT_RED, SIGNAL_PHYSICAL_INHIBIT, SIGNAL_START_STOP_PERMIT, SIGNAL_TEMP_PROCESS,
    SIGNAL_TEST_AUX_1,
};
pub use wire::{
    decode_eval_request, decode_eval_result, encode_eval_request, encode_eval_result,
    EvalRequest, EvalResult, IpcMessageType,
};
