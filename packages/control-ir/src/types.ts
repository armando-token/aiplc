export const ABI_VERSION = 1 as const;
export const MAX_SIGNALS = 32;
export const MAX_BLOCKS = 32;
export const MAX_IPC_FRAME_BYTES = 16 * 1024;

export const SIGNAL_TEMP_PROCESS = 1;
export const SIGNAL_PHYSICAL_INHIBIT = 2;
export const SIGNAL_START_STOP_PERMIT = 3;
export const SIGNAL_TEST_AUX_1 = 4;

export const OUT_BIT_RED = 1 << 0;
export const OUT_BIT_GREEN = 1 << 1;
export const OUT_BIT_BLUE_VIRTUAL = 1 << 2;

export enum SignalQuality {
  Good = 0,
  Stale = 1,
  Invalid = 2,
  Unavailable = 3,
}

export type SignalQualityName = "GOOD" | "STALE" | "INVALID" | "UNAVAILABLE";

export const SIGNAL_QUALITY_FROM_NAME: Record<SignalQualityName, SignalQuality> = {
  GOOD: SignalQuality.Good,
  STALE: SignalQuality.Stale,
  INVALID: SignalQuality.Invalid,
  UNAVAILABLE: SignalQuality.Unavailable,
};

export const SIGNAL_QUALITY_TO_NAME: Record<SignalQuality, SignalQualityName> = {
  [SignalQuality.Good]: "GOOD",
  [SignalQuality.Stale]: "STALE",
  [SignalQuality.Invalid]: "INVALID",
  [SignalQuality.Unavailable]: "UNAVAILABLE",
};

export enum WorkerMode {
  Active = 0,
  Shadow = 1,
}

export enum EvalStatus {
  Ok = 0,
  Fault = 1,
  InvalidInput = 2,
  BudgetExceeded = 3,
  StateMismatch = 4,
}

export enum BlockType {
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

export type BlockTypeName =
  | "TON"
  | "TOF"
  | "TP"
  | "CTU"
  | "CTD"
  | "R_TRIG"
  | "F_TRIG"
  | "THRESHOLD_HYST"
  | "PULSE_GEN"
  | "PID";

export const BLOCK_TYPE_FROM_NAME: Record<BlockTypeName, BlockType> = {
  TON: BlockType.Ton,
  TOF: BlockType.Tof,
  TP: BlockType.Tp,
  CTU: BlockType.Ctu,
  CTD: BlockType.Ctd,
  R_TRIG: BlockType.RTrig,
  F_TRIG: BlockType.FTrig,
  THRESHOLD_HYST: BlockType.ThresholdHyst,
  PULSE_GEN: BlockType.PulseGen,
  PID: BlockType.Pid,
};

export const BLOCK_TYPE_TO_NAME: Record<BlockType, BlockTypeName> = {
  [BlockType.Ton]: "TON",
  [BlockType.Tof]: "TOF",
  [BlockType.Tp]: "TP",
  [BlockType.Ctu]: "CTU",
  [BlockType.Ctd]: "CTD",
  [BlockType.RTrig]: "R_TRIG",
  [BlockType.FTrig]: "F_TRIG",
  [BlockType.ThresholdHyst]: "THRESHOLD_HYST",
  [BlockType.PulseGen]: "PULSE_GEN",
  [BlockType.Pid]: "PID",
};

export enum RetentionClass {
  Cyclic = 0,
  Retain = 1,
  Diagnostic = 2,
}

export type RetentionClassName = "CYCLIC" | "RETAIN" | "DIAGNOSTIC";

export enum MigrationPolicy {
  PreserveElapsed = 0,
  Reset = 1,
  RejectIncompatible = 2,
}

export type MigrationPolicyName = "PRESERVE_ELAPSED" | "RESET" | "REJECT_INCOMPATIBLE";

export interface InputSignal {
  id: number;
  value: number;
  quality: SignalQuality;
  sampleAgeMs: number;
}

export interface StateBlock {
  stableId: number;
  blockType: BlockType;
  schemaVersion: number;
  retentionClass: RetentionClass;
  migrationPolicy: MigrationPolicy;
  prevCond: number;
  phase: number;
  lastOutput: number;
  flags: number;
  countOrParam: number;
  startLogicalNs: bigint;
  elapsedNs: bigint;
  auxI64: bigint;
}

export interface EvalRequest {
  abiVersion: number;
  mode: WorkerMode;
  bootId: bigint;
  leaseEpoch: bigint;
  generation: bigint;
  scanId: bigint;
  logicalNs: bigint;
  inputSeq: bigint;
  stateRevision: bigint;
  appliedOutputMask: number;
  signals: InputSignal[];
  blocks: StateBlock[];
}

export interface EvalResult {
  abiVersion: number;
  status: EvalStatus;
  leaseEpoch: bigint;
  generation: bigint;
  scanId: bigint;
  stateRevision: bigint;
  desiredMask: number;
  validMask: number;
  diagCode: number;
  evalDurationNs: number;
  blocks: StateBlock[];
}

export interface RuleOp {
  op: "upsertRule" | "removeRule";
  ruleId: string;
  templateId: string;
  input: string;
  unit: string;
  triggerAtOrAbove?: number | null;
  clearAtOrBelow?: number | null;
  bandLow?: number | null;
  bandHigh?: number | null;
  pulsePeriodMs?: number | null;
  holdMs?: number | null;
  requiredQuality: SignalQualityName;
  onInputFault: "COMMAND_OFF_AND_RESET" | "COMMAND_OFF_HOLD" | "SAFE_FALLBACK";
  output: string;
  timingStateOnChange: MigrationPolicyName;
  stableBlockId?: number | null;
  blockType?: BlockTypeName | null;
}

export interface RuleSetV1 {
  schema: "rule-set/v1";
  baseRevision: string;
  changes: RuleOp[];
}

export interface ChannelSpec {
  name: string;
  maskBit: number;
  physical: boolean;
  gpioChip: string | null;
  gpioOffset: number | null;
  activeLow: boolean;
  dwellMs: number;
  maxTransitionsPerWindow: number;
  transitionWindowMs: number;
}

export interface SensorSpec {
  name: string;
  signalId: number;
  unit: string;
  minValid: number;
  maxValid: number;
  abnormalSentinel: number;
  pollIntervalMs: number;
  freshnessTtlMs: number;
  devicePath: string;
  slaveAddr: number;
  baudRate: number;
}

export interface InterlockRule {
  name: string;
  forbiddenMask: number;
  reason: string;
}

export interface SiteProfileV1 {
  schema: "site-profile/v1";
  deviceId: string;
  profileId: string;
  profileVersion: string;
  scanPeriodMs: number;
  altScanPeriodMs: number;
  sampleDeadlineMs: number;
  evalDeadlineMs: number;
  publishDeadlineMs: number;
  allowedPhysicalMask: number;
  safeFallbackMask: number;
  channels: ChannelSpec[];
  sensors: SensorSpec[];
  interlocks: InterlockRule[];
  signatureHex?: string | null;
  publicKeyHex?: string | null;
}

export interface MigrationPlanEntry {
  stableId: number;
  blockType: BlockTypeName;
  schemaVersion: number;
  stateSize: number;
  retentionClass: RetentionClassName;
  migrationPolicy: MigrationPolicyName;
}

export interface ModulePackageV1 {
  schema: "module-package/v1";
  packageId: string;
  baseRevision: string;
  target: string;
  engine: string;
  abiVersion: number;
  stateSchema: string;
  profileHash: string;
  irDigest: string;
  canonicalSourceDigest: string;
  wasmDigest?: string | null;
  aotDigest?: string | null;
  migrationPlanHash: string;
  policyHash: string;
  maxStateBytes: number;
  maxLinearMemBytes: number;
  requestedCapabilities: string[];
  requiredBlocks: MigrationPlanEntry[];
  ruleSet: RuleSetV1;
  wasmBase64?: string | null;
  aotBase64?: string | null;
  jsSource?: string | null;
  signatureHex?: string | null;
  publicKeyHex?: string | null;
}
