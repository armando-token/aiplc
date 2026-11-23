use crate::types::{
    AbiError, BlockType, EvalStatus, InputSignal, MigrationPolicy, RetentionClass, SignalQuality,
    StateBlock, WorkerMode, ABI_VERSION, MAX_BLOCKS, MAX_SIGNALS, REQ_MAGIC, RES_MAGIC,
};

pub const EVAL_REQUEST_HEADER_LEN: usize = 72;
pub const EVAL_RESULT_HEADER_LEN: usize = 56;
pub const INPUT_SIGNAL_WIRE_LEN: usize = 16;
pub const STATE_BLOCK_WIRE_LEN: usize = 40;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcMessageType {
    InitModule = 1,
    InitAck = 2,
    EvalReq = 3,
    EvalRes = 4,
    Revoke = 5,
    Ping = 6,
    Pong = 7,
    Error = 255,
}

impl IpcMessageType {
    pub fn from_u8(v: u8) -> Result<Self, AbiError> {
        match v {
            1 => Ok(Self::InitModule),
            2 => Ok(Self::InitAck),
            3 => Ok(Self::EvalReq),
            4 => Ok(Self::EvalRes),
            5 => Ok(Self::Revoke),
            6 => Ok(Self::Ping),
            7 => Ok(Self::Pong),
            255 => Ok(Self::Error),
            _ => Err(AbiError::InvalidEnum {
                kind: "IpcMessageType",
                value: v,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalRequest {
    pub abi_version: u16,
    pub mode: WorkerMode,
    pub boot_id: u64,
    pub lease_epoch: u64,
    pub generation: u64,
    pub scan_id: u64,
    pub logical_ns: u64,
    pub input_seq: u64,
    pub state_revision: u64,
    pub applied_output_mask: u32,
    pub signals: Vec<InputSignal>,
    pub blocks: Vec<StateBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalResult {
    pub abi_version: u16,
    pub status: EvalStatus,
    pub lease_epoch: u64,
    pub generation: u64,
    pub scan_id: u64,
    pub state_revision: u64,
    pub desired_mask: u32,
    pub valid_mask: u32,
    pub diag_code: u32,
    pub eval_duration_ns: u32,
    pub blocks: Vec<StateBlock>,
}

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_i64(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn read_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn read_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn read_i32(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn read_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}
fn read_i64(b: &[u8], off: usize) -> i64 {
    i64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

fn encode_signal(out: &mut Vec<u8>, s: &InputSignal) {
    push_u32(out, s.id);
    push_i32(out, s.value);
    out.push(s.quality as u8);
    out.extend_from_slice(&[0u8; 3]);
    push_u32(out, s.sample_age_ms);
}

fn decode_signal(b: &[u8], off: usize) -> Result<InputSignal, AbiError> {
    Ok(InputSignal {
        id: read_u32(b, off),
        value: read_i32(b, off + 4),
        quality: SignalQuality::from_u8(b[off + 8])?,
        sample_age_ms: read_u32(b, off + 12),
    })
}

pub fn encode_state_block(out: &mut Vec<u8>, blk: &StateBlock) {
    push_u32(out, blk.stable_id);
    out.push(blk.block_type as u8);
    out.push(blk.schema_version);
    out.push(blk.retention_class as u8);
    out.push(blk.migration_policy as u8);
    out.push(blk.prev_cond);
    out.push(blk.phase);
    out.push(blk.last_output);
    out.push(blk.flags);
    push_i32(out, blk.count_or_param);
    push_u64(out, blk.start_logical_ns);
    push_u64(out, blk.elapsed_ns);
    push_i64(out, blk.aux_i64);
}

pub fn decode_state_block(b: &[u8], off: usize) -> Result<StateBlock, AbiError> {
    Ok(StateBlock {
        stable_id: read_u32(b, off),
        block_type: BlockType::from_u8(b[off + 4])?,
        schema_version: b[off + 5],
        retention_class: RetentionClass::from_u8(b[off + 6])?,
        migration_policy: MigrationPolicy::from_u8(b[off + 7])?,
        prev_cond: b[off + 8],
        phase: b[off + 9],
        last_output: b[off + 10],
        flags: b[off + 11],
        count_or_param: read_i32(b, off + 12),
        start_logical_ns: read_u64(b, off + 16),
        elapsed_ns: read_u64(b, off + 24),
        aux_i64: read_i64(b, off + 32),
    })
}

pub fn encode_eval_request(req: &EvalRequest, out: &mut Vec<u8>) -> Result<(), AbiError> {
    if req.abi_version != ABI_VERSION {
        return Err(AbiError::UnsupportedVersion(req.abi_version));
    }
    if req.signals.len() > MAX_SIGNALS {
        return Err(AbiError::LimitExceeded {
            kind: "signals",
            count: req.signals.len(),
            max: MAX_SIGNALS,
        });
    }
    if req.blocks.len() > MAX_BLOCKS {
        return Err(AbiError::LimitExceeded {
            kind: "blocks",
            count: req.blocks.len(),
            max: MAX_BLOCKS,
        });
    }
    out.clear();
    out.reserve(
        EVAL_REQUEST_HEADER_LEN
            + req.signals.len() * INPUT_SIGNAL_WIRE_LEN
            + req.blocks.len() * STATE_BLOCK_WIRE_LEN,
    );
    out.extend_from_slice(&REQ_MAGIC);
    push_u16(out, req.abi_version);
    out.push(req.mode as u8);
    out.push(req.signals.len() as u8);
    out.push(req.blocks.len() as u8);
    out.extend_from_slice(&[0u8; 3]);
    push_u64(out, req.boot_id);
    push_u64(out, req.lease_epoch);
    push_u64(out, req.generation);
    push_u64(out, req.scan_id);
    push_u64(out, req.logical_ns);
    push_u64(out, req.input_seq);
    push_u64(out, req.state_revision);
    push_u32(out, req.applied_output_mask);
    debug_assert_eq!(out.len(), EVAL_REQUEST_HEADER_LEN);
    for s in &req.signals {
        encode_signal(out, s);
    }
    for blk in &req.blocks {
        encode_state_block(out, blk);
    }
    Ok(())
}

pub fn decode_eval_request(b: &[u8]) -> Result<EvalRequest, AbiError> {
    if b.len() < EVAL_REQUEST_HEADER_LEN {
        return Err(AbiError::LengthMismatch {
            expected: EVAL_REQUEST_HEADER_LEN,
            actual: b.len(),
        });
    }
    if b[0..4] != REQ_MAGIC {
        return Err(AbiError::InvalidMagic);
    }
    let abi_version = read_u16(b, 4);
    if abi_version != ABI_VERSION {
        return Err(AbiError::UnsupportedVersion(abi_version));
    }
    let mode = WorkerMode::from_u8(b[6])?;
    let sig_count = b[7] as usize;
    let blk_count = b[8] as usize;
    if sig_count > MAX_SIGNALS {
        return Err(AbiError::LimitExceeded {
            kind: "signals",
            count: sig_count,
            max: MAX_SIGNALS,
        });
    }
    if blk_count > MAX_BLOCKS {
        return Err(AbiError::LimitExceeded {
            kind: "blocks",
            count: blk_count,
            max: MAX_BLOCKS,
        });
    }
    let expected = EVAL_REQUEST_HEADER_LEN
        + sig_count * INPUT_SIGNAL_WIRE_LEN
        + blk_count * STATE_BLOCK_WIRE_LEN;
    if b.len() != expected {
        return Err(AbiError::LengthMismatch {
            expected,
            actual: b.len(),
        });
    }
    let mut signals = Vec::with_capacity(sig_count);
    let mut off = EVAL_REQUEST_HEADER_LEN;
    for _ in 0..sig_count {
        signals.push(decode_signal(b, off)?);
        off += INPUT_SIGNAL_WIRE_LEN;
    }
    let mut blocks = Vec::with_capacity(blk_count);
    for _ in 0..blk_count {
        blocks.push(decode_state_block(b, off)?);
        off += STATE_BLOCK_WIRE_LEN;
    }
    Ok(EvalRequest {
        abi_version,
        mode,
        boot_id: read_u64(b, 12),
        lease_epoch: read_u64(b, 20),
        generation: read_u64(b, 28),
        scan_id: read_u64(b, 36),
        logical_ns: read_u64(b, 44),
        input_seq: read_u64(b, 52),
        state_revision: read_u64(b, 60),
        applied_output_mask: read_u32(b, 68),
        signals,
        blocks,
    })
}

pub fn encode_eval_result(res: &EvalResult, out: &mut Vec<u8>) -> Result<(), AbiError> {
    if res.abi_version != ABI_VERSION {
        return Err(AbiError::UnsupportedVersion(res.abi_version));
    }
    if res.blocks.len() > MAX_BLOCKS {
        return Err(AbiError::LimitExceeded {
            kind: "blocks",
            count: res.blocks.len(),
            max: MAX_BLOCKS,
        });
    }
    out.clear();
    out.reserve(EVAL_RESULT_HEADER_LEN + res.blocks.len() * STATE_BLOCK_WIRE_LEN);
    out.extend_from_slice(&RES_MAGIC);
    push_u16(out, res.abi_version);
    out.push(res.status as u8);
    out.push(res.blocks.len() as u8);
    push_u64(out, res.lease_epoch);
    push_u64(out, res.generation);
    push_u64(out, res.scan_id);
    push_u64(out, res.state_revision);
    push_u32(out, res.desired_mask);
    push_u32(out, res.valid_mask);
    push_u32(out, res.diag_code);
    push_u32(out, res.eval_duration_ns);
    debug_assert_eq!(out.len(), EVAL_RESULT_HEADER_LEN);
    for blk in &res.blocks {
        encode_state_block(out, blk);
    }
    Ok(())
}

pub fn decode_eval_result(b: &[u8]) -> Result<EvalResult, AbiError> {
    if b.len() < EVAL_RESULT_HEADER_LEN {
        return Err(AbiError::LengthMismatch {
            expected: EVAL_RESULT_HEADER_LEN,
            actual: b.len(),
        });
    }
    if b[0..4] != RES_MAGIC {
        return Err(AbiError::InvalidMagic);
    }
    let abi_version = read_u16(b, 4);
    if abi_version != ABI_VERSION {
        return Err(AbiError::UnsupportedVersion(abi_version));
    }
    let status = EvalStatus::from_u8(b[6])?;
    let blk_count = b[7] as usize;
    if blk_count > MAX_BLOCKS {
        return Err(AbiError::LimitExceeded {
            kind: "blocks",
            count: blk_count,
            max: MAX_BLOCKS,
        });
    }
    let expected = EVAL_RESULT_HEADER_LEN + blk_count * STATE_BLOCK_WIRE_LEN;
    if b.len() != expected {
        return Err(AbiError::LengthMismatch {
            expected,
            actual: b.len(),
        });
    }
    let mut blocks = Vec::with_capacity(blk_count);
    let mut off = EVAL_RESULT_HEADER_LEN;
    for _ in 0..blk_count {
        blocks.push(decode_state_block(b, off)?);
        off += STATE_BLOCK_WIRE_LEN;
    }
    Ok(EvalResult {
        abi_version,
        status,
        lease_epoch: read_u64(b, 8),
        generation: read_u64(b, 16),
        scan_id: read_u64(b, 24),
        state_revision: read_u64(b, 32),
        desired_mask: read_u32(b, 40),
        valid_mask: read_u32(b, 44),
        diag_code: read_u32(b, 48),
        eval_duration_ns: read_u32(b, 52),
        blocks,
    })
}
