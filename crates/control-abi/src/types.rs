use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const ABI_VERSION: u16 = 1;
pub const MAX_SIGNALS: usize = 32;
pub const MAX_BLOCKS: usize = 32;
pub const MAX_IPC_FRAME_BYTES: usize = 16 * 1024;

pub const REQ_MAGIC: [u8; 4] = *b"PLCQ";
pub const RES_MAGIC: [u8; 4] = *b"PLCR";

/// Well-known signal IDs mapped by site profile.
pub const SIGNAL_TEMP_PROCESS: u32 = 1;
pub const SIGNAL_PHYSICAL_INHIBIT: u32 = 2;
pub const SIGNAL_START_STOP_PERMIT: u32 = 3;
pub const SIGNAL_TEST_AUX_1: u32 = 4;

/// Output mask bits mapped by site profile (`demo-lyra`).
pub const OUT_BIT_RED: u32 = 1 << 0;
pub const OUT_BIT_GREEN: u32 = 1 << 1;
pub const OUT_BIT_BLUE_VIRTUAL: u32 = 1 << 2;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AbiError {
    #[error("invalid magic header")]
    InvalidMagic,
    #[error("unsupported ABI version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid enum discriminant for {kind}: {value}")]
    InvalidEnum { kind: &'static str, value: u8 },
    #[error("count {count} exceeds max {max} for {kind}")]
    LimitExceeded {
        kind: &'static str,
        count: usize,
        max: usize,
    },
    #[error("buffer length mismatch: expected {expected}, got {actual}")]
    LengthMismatch { expected: usize, actual: usize },
    #[error("non-finite or out-of-range value: {0}")]
    InvalidValue(String),
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SignalQuality {
    Good = 0,
    Stale = 1,
    Invalid = 2,
    Unavailable = 3,
}

impl SignalQuality {
    pub fn from_u8(v: u8) -> Result<Self, AbiError> {
        match v {
            0 => Ok(Self::Good),
            1 => Ok(Self::Stale),
            2 => Ok(Self::Invalid),
            3 => Ok(Self::Unavailable),
            _ => Err(AbiError::InvalidEnum {
                kind: "SignalQuality",
                value: v,
            }),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkerMode {
    Active = 0,
    Shadow = 1,
}

impl WorkerMode {
    pub fn from_u8(v: u8) -> Result<Self, AbiError> {
        match v {
            0 => Ok(Self::Active),
            1 => Ok(Self::Shadow),
            _ => Err(AbiError::InvalidEnum {
                kind: "WorkerMode",
                value: v,
            }),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EvalStatus {
    Ok = 0,
    Fault = 1,
    InvalidInput = 2,
    BudgetExceeded = 3,
    StateMismatch = 4,
}

impl EvalStatus {
    pub fn from_u8(v: u8) -> Result<Self, AbiError> {
        match v {
            0 => Ok(Self::Ok),
            1 => Ok(Self::Fault),
            2 => Ok(Self::InvalidInput),
            3 => Ok(Self::BudgetExceeded),
            4 => Ok(Self::StateMismatch),
            _ => Err(AbiError::InvalidEnum {
                kind: "EvalStatus",
                value: v,
            }),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GuardReason {
    None = 0,
    PhysicalInhibit = 1,
    StartupDisarmed = 2,
    FallbackLatched = 3,
    IoFault = 4,
    InputQualityFault = 5,
    InterlockVeto = 6,
    ManualLeaseExpired = 7,
    RangeLimit = 8,
    DwellChatter = 9,
    WorkerDeadlineOrFault = 10,
    ShadowNoPublish = 11,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DriverStatus {
    Applied = 0,
    RejectedBusy = 1,
    Expired = 2,
    IoUnknown = 3,
    IoFault = 4,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BlockType {
    Ton = 0,
    Tof = 1,
    Tp = 2,
    Ctu = 3,
    Ctd = 4,
    RTrig = 5,
    FTrig = 6,
    ThresholdHyst = 7,
    PulseGen = 8,
    Pid = 9,
}

impl BlockType {
    pub fn from_u8(v: u8) -> Result<Self, AbiError> {
        match v {
            0 => Ok(Self::Ton),
            1 => Ok(Self::Tof),
            2 => Ok(Self::Tp),
            3 => Ok(Self::Ctu),
            4 => Ok(Self::Ctd),
            5 => Ok(Self::RTrig),
            6 => Ok(Self::FTrig),
            7 => Ok(Self::ThresholdHyst),
            8 => Ok(Self::PulseGen),
            9 => Ok(Self::Pid),
            _ => Err(AbiError::InvalidEnum {
                kind: "BlockType",
                value: v,
            }),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RetentionClass {
    Cyclic = 0,
    Retain = 1,
    Diagnostic = 2,
}

impl RetentionClass {
    pub fn from_u8(v: u8) -> Result<Self, AbiError> {
        match v {
            0 => Ok(Self::Cyclic),
            1 => Ok(Self::Retain),
            2 => Ok(Self::Diagnostic),
            _ => Err(AbiError::InvalidEnum {
                kind: "RetentionClass",
                value: v,
            }),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MigrationPolicy {
    PreserveElapsed = 0,
    Reset = 1,
    RejectIncompatible = 2,
}

impl MigrationPolicy {
    pub fn from_u8(v: u8) -> Result<Self, AbiError> {
        match v {
            0 => Ok(Self::PreserveElapsed),
            1 => Ok(Self::Reset),
            2 => Ok(Self::RejectIncompatible),
            _ => Err(AbiError::InvalidEnum {
                kind: "MigrationPolicy",
                value: v,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputSignal {
    pub id: u32,
    pub value: i32,
    pub quality: SignalQuality,
    pub sample_age_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateBlock {
    pub stable_id: u32,
    pub block_type: BlockType,
    pub schema_version: u8,
    pub retention_class: RetentionClass,
    pub migration_policy: MigrationPolicy,
    pub prev_cond: u8,
    pub phase: u8,
    pub last_output: u8,
    pub flags: u8,
    pub count_or_param: i32,
    #[serde(with = "u64_as_dec_string")]
    pub start_logical_ns: u64,
    #[serde(with = "u64_as_dec_string")]
    pub elapsed_ns: u64,
    #[serde(with = "i64_as_dec_string")]
    pub aux_i64: i64,
}

impl StateBlock {
    pub const fn empty(stable_id: u32, block_type: BlockType) -> Self {
        Self {
            stable_id,
            block_type,
            schema_version: 1,
            retention_class: RetentionClass::Cyclic,
            migration_policy: MigrationPolicy::PreserveElapsed,
            prev_cond: 0,
            phase: 0,
            last_output: 0,
            flags: 0,
            count_or_param: 0,
            start_logical_ns: 0,
            elapsed_ns: 0,
            aux_i64: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputFrame {
    #[serde(with = "u64_as_dec_string")]
    pub boot_id: u64,
    #[serde(with = "u64_as_dec_string")]
    pub scan_id: u64,
    #[serde(with = "u64_as_dec_string")]
    pub lease_epoch: u64,
    #[serde(with = "u64_as_dec_string")]
    pub active_revision: u64,
    #[serde(with = "u64_as_dec_string")]
    pub output_seq: u64,
    pub desired_mask: u32,
    pub applied_mask: u32,
    #[serde(with = "u64_as_dec_string")]
    pub expiry_ns: u64,
    pub guard_reason: GuardReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Observation {
    #[serde(with = "u64_as_dec_string")]
    pub output_seq: u64,
    pub driver_status: DriverStatus,
    pub observed_mask: Option<u32>,
    pub source: String,
    #[serde(with = "u64_as_dec_string")]
    pub observed_at_ns: u64,
    pub quality: SignalQuality,
}

/// Serde helpers ensuring all 64-bit integers serialize as canonical decimal strings in JSON (§3.2).
pub mod u64_as_dec_string {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(val: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&val.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let s = String::deserialize(d)?;
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(D::Error::custom(format!("non-canonical u64 string '{s}'")));
        }
        s.parse::<u64>()
            .map_err(|e| D::Error::custom(format!("invalid u64 string '{s}': {e}")))
    }
}

pub mod i64_as_dec_string {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(val: &i64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&val.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
        let s = String::deserialize(d)?;
        let digits = s.strip_prefix('-').unwrap_or(&s);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(D::Error::custom(format!("non-canonical i64 string '{s}'")));
        }
        s.parse::<i64>()
            .map_err(|e| D::Error::custom(format!("invalid i64 string '{s}': {e}")))
    }
}
