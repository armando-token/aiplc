import {
  ABI_VERSION,
  BlockType,
  EvalRequest,
  EvalResult,
  EvalStatus,
  InputSignal,
  MAX_BLOCKS,
  MAX_SIGNALS,
  MigrationPolicy,
  RetentionClass,
  SignalQuality,
  StateBlock,
  WorkerMode,
} from "./types.js";

export const EVAL_REQUEST_HEADER_LEN = 72;
export const EVAL_RESULT_HEADER_LEN = 56;
export const INPUT_SIGNAL_WIRE_LEN = 16;
export const STATE_BLOCK_WIRE_LEN = 40;

const REQ_MAGIC = [0x50, 0x4c, 0x43, 0x51] as const; // "PLCQ"
const RES_MAGIC = [0x50, 0x4c, 0x43, 0x52] as const; // "PLCR"

export class WireError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "WireError";
  }
}

function assertU8(v: number, name: string): void {
  if (!Number.isInteger(v) || v < 0 || v > 0xff) {
    throw new WireError(`${name} out of u8 range: ${v}`);
  }
}
function assertU16(v: number, name: string): void {
  if (!Number.isInteger(v) || v < 0 || v > 0xffff) {
    throw new WireError(`${name} out of u16 range: ${v}`);
  }
}
function assertU32(v: number, name: string): void {
  if (!Number.isInteger(v) || v < 0 || v > 0xffffffff) {
    throw new WireError(`${name} out of u32 range: ${v}`);
  }
}
function assertI32(v: number, name: string): void {
  if (!Number.isInteger(v) || v < -0x80000000 || v > 0x7fffffff) {
    throw new WireError(`${name} out of i32 range: ${v}`);
  }
}
function assertU64(v: bigint, name: string): void {
  if (typeof v !== "bigint" || v < 0n || v > 0xffffffffffffffffn) {
    throw new WireError(`${name} out of u64 range`);
  }
}
function assertI64(v: bigint, name: string): void {
  if (typeof v !== "bigint" || v < -0x8000000000000000n || v > 0x7fffffffffffffffn) {
    throw new WireError(`${name} out of i64 range`);
  }
}

function checkEnum(val: number, maxInclusive: number, kind: string): void {
  if (!Number.isInteger(val) || val < 0 || val > maxInclusive) {
    throw new WireError(`invalid enum for ${kind}: ${val}`);
  }
}

export function encodeStateBlock(view: DataView, offset: number, blk: StateBlock): void {
  assertU32(blk.stableId, "stableId");
  checkEnum(blk.blockType, BlockType.Pid, "BlockType");
  assertU8(blk.schemaVersion, "schemaVersion");
  checkEnum(blk.retentionClass, RetentionClass.Diagnostic, "RetentionClass");
  checkEnum(blk.migrationPolicy, MigrationPolicy.RejectIncompatible, "MigrationPolicy");
  assertU8(blk.prevCond, "prevCond");
  assertU8(blk.phase, "phase");
  assertU8(blk.lastOutput, "lastOutput");
  assertU8(blk.flags, "flags");
  assertI32(blk.countOrParam, "countOrParam");
  assertU64(blk.startLogicalNs, "startLogicalNs");
  assertU64(blk.elapsedNs, "elapsedNs");
  assertI64(blk.auxI64, "auxI64");

  view.setUint32(offset, blk.stableId, true);
  view.setUint8(offset + 4, blk.blockType);
  view.setUint8(offset + 5, blk.schemaVersion);
  view.setUint8(offset + 6, blk.retentionClass);
  view.setUint8(offset + 7, blk.migrationPolicy);
  view.setUint8(offset + 8, blk.prevCond);
  view.setUint8(offset + 9, blk.phase);
  view.setUint8(offset + 10, blk.lastOutput);
  view.setUint8(offset + 11, blk.flags);
  view.setInt32(offset + 12, blk.countOrParam, true);
  view.setBigUint64(offset + 16, blk.startLogicalNs, true);
  view.setBigUint64(offset + 24, blk.elapsedNs, true);
  view.setBigInt64(offset + 32, blk.auxI64, true);
}

export function decodeStateBlock(view: DataView, offset: number): StateBlock {
  const blockType = view.getUint8(offset + 4);
  checkEnum(blockType, BlockType.Pid, "BlockType");
  const retentionClass = view.getUint8(offset + 6);
  checkEnum(retentionClass, RetentionClass.Diagnostic, "RetentionClass");
  const migrationPolicy = view.getUint8(offset + 7);
  checkEnum(migrationPolicy, MigrationPolicy.RejectIncompatible, "MigrationPolicy");

  return {
    stableId: view.getUint32(offset, true),
    blockType: blockType as BlockType,
    schemaVersion: view.getUint8(offset + 5),
    retentionClass: retentionClass as RetentionClass,
    migrationPolicy: migrationPolicy as MigrationPolicy,
    prevCond: view.getUint8(offset + 8),
    phase: view.getUint8(offset + 9),
    lastOutput: view.getUint8(offset + 10),
    flags: view.getUint8(offset + 11),
    countOrParam: view.getInt32(offset + 12, true),
    startLogicalNs: view.getBigUint64(offset + 16, true),
    elapsedNs: view.getBigUint64(offset + 24, true),
    auxI64: view.getBigInt64(offset + 32, true),
  };
}

export function encodeEvalRequest(req: EvalRequest): Uint8Array {
  assertU16(req.abiVersion, "abiVersion");
  if (req.abiVersion !== ABI_VERSION) {
    throw new WireError(`unsupported abiVersion ${req.abiVersion}`);
  }
  checkEnum(req.mode, WorkerMode.Shadow, "WorkerMode");
  if (req.signals.length > MAX_SIGNALS) {
    throw new WireError(`signals exceed max ${MAX_SIGNALS}`);
  }
  if (req.blocks.length > MAX_BLOCKS) {
    throw new WireError(`blocks exceed max ${MAX_BLOCKS}`);
  }
  assertU64(req.bootId, "bootId");
  assertU64(req.leaseEpoch, "leaseEpoch");
  assertU64(req.generation, "generation");
  assertU64(req.scanId, "scanId");
  assertU64(req.logicalNs, "logicalNs");
  assertU64(req.inputSeq, "inputSeq");
  assertU64(req.stateRevision, "stateRevision");
  assertU32(req.appliedOutputMask, "appliedOutputMask");

  const len =
    EVAL_REQUEST_HEADER_LEN +
    req.signals.length * INPUT_SIGNAL_WIRE_LEN +
    req.blocks.length * STATE_BLOCK_WIRE_LEN;
  const buf = new Uint8Array(len);
  const view = new DataView(buf.buffer);

  buf[0] = REQ_MAGIC[0];
  buf[1] = REQ_MAGIC[1];
  buf[2] = REQ_MAGIC[2];
  buf[3] = REQ_MAGIC[3];
  view.setUint16(4, req.abiVersion, true);
  view.setUint8(6, req.mode);
  view.setUint8(7, req.signals.length);
  view.setUint8(8, req.blocks.length);
  view.setBigUint64(12, req.bootId, true);
  view.setBigUint64(20, req.leaseEpoch, true);
  view.setBigUint64(28, req.generation, true);
  view.setBigUint64(36, req.scanId, true);
  view.setBigUint64(44, req.logicalNs, true);
  view.setBigUint64(52, req.inputSeq, true);
  view.setBigUint64(60, req.stateRevision, true);
  view.setUint32(68, req.appliedOutputMask, true);

  let off = EVAL_REQUEST_HEADER_LEN;
  for (const s of req.signals) {
    assertU32(s.id, "signal.id");
    assertI32(s.value, "signal.value");
    checkEnum(s.quality, SignalQuality.Unavailable, "SignalQuality");
    assertU32(s.sampleAgeMs, "signal.sampleAgeMs");
    view.setUint32(off, s.id, true);
    view.setInt32(off + 4, s.value, true);
    view.setUint8(off + 8, s.quality);
    view.setUint32(off + 12, s.sampleAgeMs, true);
    off += INPUT_SIGNAL_WIRE_LEN;
  }
  for (const blk of req.blocks) {
    encodeStateBlock(view, off, blk);
    off += STATE_BLOCK_WIRE_LEN;
  }
  return buf;
}

export function decodeEvalRequest(buf: Uint8Array): EvalRequest {
  if (buf.byteLength < EVAL_REQUEST_HEADER_LEN) {
    throw new WireError(`truncated EvalRequest: ${buf.byteLength}`);
  }
  if (
    buf[0] !== REQ_MAGIC[0] ||
    buf[1] !== REQ_MAGIC[1] ||
    buf[2] !== REQ_MAGIC[2] ||
    buf[3] !== REQ_MAGIC[3]
  ) {
    throw new WireError("invalid EvalRequest magic");
  }
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const abiVersion = view.getUint16(4, true);
  if (abiVersion !== ABI_VERSION) {
    throw new WireError(`unsupported abiVersion: ${abiVersion}`);
  }
  const mode = view.getUint8(6);
  checkEnum(mode, WorkerMode.Shadow, "WorkerMode");
  const sigCount = view.getUint8(7);
  const blkCount = view.getUint8(8);
  if (sigCount > MAX_SIGNALS || blkCount > MAX_BLOCKS) {
    throw new WireError("signal or block count exceeds limit");
  }
  const expected =
    EVAL_REQUEST_HEADER_LEN +
    sigCount * INPUT_SIGNAL_WIRE_LEN +
    blkCount * STATE_BLOCK_WIRE_LEN;
  if (buf.byteLength !== expected) {
    throw new WireError(`EvalRequest length mismatch: expected ${expected}, got ${buf.byteLength}`);
  }
  const signals: InputSignal[] = [];
  let off = EVAL_REQUEST_HEADER_LEN;
  for (let i = 0; i < sigCount; i++) {
    const q = view.getUint8(off + 8);
    checkEnum(q, SignalQuality.Unavailable, "SignalQuality");
    signals.push({
      id: view.getUint32(off, true),
      value: view.getInt32(off + 4, true),
      quality: q as SignalQuality,
      sampleAgeMs: view.getUint32(off + 12, true),
    });
    off += INPUT_SIGNAL_WIRE_LEN;
  }
  const blocks: StateBlock[] = [];
  for (let i = 0; i < blkCount; i++) {
    blocks.push(decodeStateBlock(view, off));
    off += STATE_BLOCK_WIRE_LEN;
  }
  return {
    abiVersion,
    mode: mode as WorkerMode,
    bootId: view.getBigUint64(12, true),
    leaseEpoch: view.getBigUint64(20, true),
    generation: view.getBigUint64(28, true),
    scanId: view.getBigUint64(36, true),
    logicalNs: view.getBigUint64(44, true),
    inputSeq: view.getBigUint64(52, true),
    stateRevision: view.getBigUint64(60, true),
    appliedOutputMask: view.getUint32(68, true),
    signals,
    blocks,
  };
}

export function encodeEvalResult(res: EvalResult): Uint8Array {
  if (res.abiVersion !== ABI_VERSION) {
    throw new WireError(`unsupported abiVersion ${res.abiVersion}`);
  }
  checkEnum(res.status, EvalStatus.StateMismatch, "EvalStatus");
  if (res.blocks.length > MAX_BLOCKS) {
    throw new WireError(`blocks exceed max ${MAX_BLOCKS}`);
  }
  assertU64(res.leaseEpoch, "leaseEpoch");
  assertU64(res.generation, "generation");
  assertU64(res.scanId, "scanId");
  assertU64(res.stateRevision, "stateRevision");
  assertU32(res.desiredMask, "desiredMask");
  assertU32(res.validMask, "validMask");
  assertU32(res.diagCode, "diagCode");
  assertU32(res.evalDurationNs, "evalDurationNs");

  const len = EVAL_RESULT_HEADER_LEN + res.blocks.length * STATE_BLOCK_WIRE_LEN;
  const buf = new Uint8Array(len);
  const view = new DataView(buf.buffer);
  buf[0] = RES_MAGIC[0];
  buf[1] = RES_MAGIC[1];
  buf[2] = RES_MAGIC[2];
  buf[3] = RES_MAGIC[3];
  view.setUint16(4, res.abiVersion, true);
  view.setUint8(6, res.status);
  view.setUint8(7, res.blocks.length);
  view.setBigUint64(8, res.leaseEpoch, true);
  view.setBigUint64(16, res.generation, true);
  view.setBigUint64(24, res.scanId, true);
  view.setBigUint64(32, res.stateRevision, true);
  view.setUint32(40, res.desiredMask, true);
  view.setUint32(44, res.validMask, true);
  view.setUint32(48, res.diagCode, true);
  view.setUint32(52, res.evalDurationNs, true);

  let off = EVAL_RESULT_HEADER_LEN;
  for (const blk of res.blocks) {
    encodeStateBlock(view, off, blk);
    off += STATE_BLOCK_WIRE_LEN;
  }
  return buf;
}

export function decodeEvalResult(buf: Uint8Array): EvalResult {
  if (buf.byteLength < EVAL_RESULT_HEADER_LEN) {
    throw new WireError(`truncated EvalResult: ${buf.byteLength}`);
  }
  if (
    buf[0] !== RES_MAGIC[0] ||
    buf[1] !== RES_MAGIC[1] ||
    buf[2] !== RES_MAGIC[2] ||
    buf[3] !== RES_MAGIC[3]
  ) {
    throw new WireError("invalid EvalResult magic");
  }
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const abiVersion = view.getUint16(4, true);
  if (abiVersion !== ABI_VERSION) {
    throw new WireError(`unsupported abiVersion: ${abiVersion}`);
  }
  const status = view.getUint8(6);
  checkEnum(status, EvalStatus.StateMismatch, "EvalStatus");
  const blkCount = view.getUint8(7);
  if (blkCount > MAX_BLOCKS) {
    throw new WireError("block count exceeds limit");
  }
  const expected = EVAL_RESULT_HEADER_LEN + blkCount * STATE_BLOCK_WIRE_LEN;
  if (buf.byteLength !== expected) {
    throw new WireError(`EvalResult length mismatch: expected ${expected}, got ${buf.byteLength}`);
  }
  const blocks: StateBlock[] = [];
  let off = EVAL_RESULT_HEADER_LEN;
  for (let i = 0; i < blkCount; i++) {
    blocks.push(decodeStateBlock(view, off));
    off += STATE_BLOCK_WIRE_LEN;
  }
  return {
    abiVersion,
    status: status as EvalStatus,
    leaseEpoch: view.getBigUint64(8, true),
    generation: view.getBigUint64(16, true),
    scanId: view.getBigUint64(24, true),
    stateRevision: view.getBigUint64(32, true),
    desiredMask: view.getUint32(40, true),
    validMask: view.getUint32(44, true),
    diagCode: view.getUint32(48, true),
    evalDurationNs: view.getUint32(52, true),
    blocks,
  };
}
