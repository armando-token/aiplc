import {
  BLOCK_TYPE_FROM_NAME,
  BlockTypeName,
  MigrationPolicyName,
  RuleOp,
  RuleSetV1,
  SIGNAL_QUALITY_FROM_NAME,
  SignalQualityName,
  SiteProfileV1,
} from "./types.js";

export class ValidationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ValidationError";
  }
}

const RULE_SET_KEYS = new Set(["schema", "baseRevision", "changes"]);
const RULE_OP_KEYS = new Set([
  "op",
  "ruleId",
  "templateId",
  "input",
  "unit",
  "triggerAtOrAbove",
  "clearAtOrBelow",
  "bandLow",
  "bandHigh",
  "pulsePeriodMs",
  "holdMs",
  "requiredQuality",
  "onInputFault",
  "output",
  "timingStateOnChange",
  "stableBlockId",
  "blockType",
]);

const VALID_TEMPLATES = new Set([
  "thresholdHoldWithLatchedHysteresis",
  "bandPrealarmPulse",
  "copyBoolean",
  "counterTrigger",
  "pulseTimer",
  "offDelay",
  "manualMask",
]);

const VALID_FAULT_ACTIONS = new Set([
  "COMMAND_OFF_AND_RESET",
  "COMMAND_OFF_HOLD",
  "SAFE_FALLBACK",
]);

const VALID_MIGRATIONS = new Set<MigrationPolicyName>([
  "PRESERVE_ELAPSED",
  "RESET",
  "REJECT_INCOMPATIBLE",
]);

function assertNoUnknownKeys(
  obj: Record<string, unknown>,
  allowed: Set<string>,
  ctx: string
): void {
  for (const key of Object.keys(obj)) {
    if (!allowed.has(key)) {
      throw new ValidationError(`${ctx}: unknown field '${key}'`);
    }
  }
}

function assertNonEmptyString(val: unknown, name: string): string {
  if (typeof val !== "string" || val.trim().length === 0) {
    throw new ValidationError(`${name} must be a non-empty string`);
  }
  return val;
}

function assertFiniteInt(val: unknown, name: string, min: number, max: number): number {
  if (typeof val !== "number" || !Number.isFinite(val) || !Number.isInteger(val)) {
    throw new ValidationError(`${name} must be a finite integer (no coercion)`);
  }
  if (val < min || val > max) {
    throw new ValidationError(`${name}=${val} out of range [${min}, ${max}]`);
  }
  return val;
}

function assertDecimalU64String(val: unknown, name: string): string {
  if (typeof val !== "string" || !/^[0-9]+$/.test(val)) {
    throw new ValidationError(`${name} must be a decimal u64 string`);
  }
  if (BigInt(val) > 18446744073709551615n) {
    throw new ValidationError(`${name} exceeds u64 max`);
  }
  return val;
}

export function validateRuleSetAgainstProfile(
  raw: unknown,
  profile: SiteProfileV1
): RuleSetV1 {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) {
    throw new ValidationError("ruleSet must be an object");
  }
  const obj = raw as Record<string, unknown>;
  assertNoUnknownKeys(obj, RULE_SET_KEYS, "ruleSet");

  if (obj["schema"] !== "rule-set/v1") {
    throw new ValidationError(`unsupported ruleSet schema: ${String(obj["schema"])}`);
  }
  const baseRevision = assertDecimalU64String(obj["baseRevision"], "baseRevision");

  if (!Array.isArray(obj["changes"]) || obj["changes"].length === 0) {
    throw new ValidationError("changes must be a non-empty array");
  }
  if (obj["changes"].length > 32) {
    throw new ValidationError("changes exceeds maximum 32 rules");
  }

  const sensorByName = new Map(profile.sensors.map((s) => [s.name, s]));
  const channelByName = new Map(profile.channels.map((c) => [c.name, c]));
  const outputOwners = new Map<string, string>();
  const seenRuleIds = new Set<string>();
  const seenBlockIds = new Set<number>();
  const validatedChanges: RuleOp[] = [];

  for (let i = 0; i < obj["changes"].length; i++) {
    const item = obj["changes"][i];
    if (item === null || typeof item !== "object" || Array.isArray(item)) {
      throw new ValidationError(`changes[${i}] must be an object`);
    }
    const r = item as Record<string, unknown>;
    assertNoUnknownKeys(r, RULE_OP_KEYS, `changes[${i}]`);

    const op = assertNonEmptyString(r["op"], `changes[${i}].op`);
    if (op !== "upsertRule" && op !== "removeRule") {
      throw new ValidationError(`changes[${i}].op invalid: ${op}`);
    }
    const ruleId = assertNonEmptyString(r["ruleId"], `changes[${i}].ruleId`);
    if (seenRuleIds.has(ruleId)) {
      throw new ValidationError(`duplicate ruleId '${ruleId}' in RuleSetV1`);
    }
    seenRuleIds.add(ruleId);
    const templateId = assertNonEmptyString(r["templateId"], `changes[${i}].templateId`);
    if (!VALID_TEMPLATES.has(templateId)) {
      throw new ValidationError(`changes[${i}].templateId unknown: ${templateId}`);
    }

    const input = assertNonEmptyString(r["input"], `changes[${i}].input`);
    const sensor = sensorByName.get(input);
    if (!sensor) {
      throw new ValidationError(`changes[${i}].input '${input}' not in site profile`);
    }

    const unit = assertNonEmptyString(r["unit"], `changes[${i}].unit`);
    if (unit !== sensor.unit) {
      throw new ValidationError(
        `changes[${i}].unit '${unit}' conflicts with sensor unit '${sensor.unit}'`
      );
    }

    const output = assertNonEmptyString(r["output"], `changes[${i}].output`);
    const channel = channelByName.get(output);
    if (!channel) {
      throw new ValidationError(
        `changes[${i}].output '${output}' not in site profile (free-text hardware bindings forbidden)`
      );
    }
    // Single logical writer rule per output channel (§6.1)
    if (op === "upsertRule") {
      const existingOwner = outputOwners.get(output);
      if (existingOwner && existingOwner !== ruleId) {
        throw new ValidationError(
          `multiple logical writers on output '${output}': '${existingOwner}' and '${ruleId}'`
        );
      }
      outputOwners.set(output, ruleId);
    }

    const requiredQuality = assertNonEmptyString(
      r["requiredQuality"],
      `changes[${i}].requiredQuality`
    ) as SignalQualityName;
    if (!(requiredQuality in SIGNAL_QUALITY_FROM_NAME)) {
      throw new ValidationError(`invalid requiredQuality: ${requiredQuality}`);
    }

    const onInputFault = assertNonEmptyString(
      r["onInputFault"],
      `changes[${i}].onInputFault`
    ) as RuleOp["onInputFault"];
    if (!VALID_FAULT_ACTIONS.has(onInputFault)) {
      throw new ValidationError(`invalid onInputFault: ${onInputFault}`);
    }

    const timingStateOnChange = assertNonEmptyString(
      r["timingStateOnChange"],
      `changes[${i}].timingStateOnChange`
    ) as MigrationPolicyName;
    if (!VALID_MIGRATIONS.has(timingStateOnChange)) {
      throw new ValidationError(`invalid timingStateOnChange: ${timingStateOnChange}`);
    }

    let triggerAtOrAbove: number | null | undefined = undefined;
    if (r["triggerAtOrAbove"] !== undefined && r["triggerAtOrAbove"] !== null) {
      triggerAtOrAbove = assertFiniteInt(
        r["triggerAtOrAbove"],
        `changes[${i}].triggerAtOrAbove`,
        sensor.minValid,
        sensor.maxValid
      );
    }

    let clearAtOrBelow: number | null | undefined = undefined;
    if (r["clearAtOrBelow"] !== undefined && r["clearAtOrBelow"] !== null) {
      clearAtOrBelow = assertFiniteInt(
        r["clearAtOrBelow"],
        `changes[${i}].clearAtOrBelow`,
        sensor.minValid,
        sensor.maxValid
      );
    }

    if (
      triggerAtOrAbove !== undefined &&
      triggerAtOrAbove !== null &&
      clearAtOrBelow !== undefined &&
      clearAtOrBelow !== null &&
      clearAtOrBelow > triggerAtOrAbove
    ) {
      throw new ValidationError(
        `changes[${i}]: clearAtOrBelow (${clearAtOrBelow}) cannot exceed triggerAtOrAbove (${triggerAtOrAbove})`
      );
    }

    let bandLow: number | null | undefined = undefined;
    if (r["bandLow"] !== undefined && r["bandLow"] !== null) {
      bandLow = assertFiniteInt(r["bandLow"], `changes[${i}].bandLow`, sensor.minValid, sensor.maxValid);
    }
    let bandHigh: number | null | undefined = undefined;
    if (r["bandHigh"] !== undefined && r["bandHigh"] !== null) {
      bandHigh = assertFiniteInt(
        r["bandHigh"],
        `changes[${i}].bandHigh`,
        sensor.minValid,
        sensor.maxValid
      );
    }
    if (
      bandLow !== undefined &&
      bandLow !== null &&
      bandHigh !== undefined &&
      bandHigh !== null &&
      bandLow >= bandHigh
    ) {
      throw new ValidationError(`changes[${i}]: bandLow must be strictly less than bandHigh`);
    }

    let holdMs: number | null | undefined = undefined;
    if (r["holdMs"] !== undefined && r["holdMs"] !== null) {
      holdMs = assertFiniteInt(r["holdMs"], `changes[${i}].holdMs`, 0, 3_600_000);
    }

    let pulsePeriodMs: number | null | undefined = undefined;
    if (r["pulsePeriodMs"] !== undefined && r["pulsePeriodMs"] !== null) {
      pulsePeriodMs = assertFiniteInt(
        r["pulsePeriodMs"],
        `changes[${i}].pulsePeriodMs`,
        20,
        600_000
      );
      if (channel.physical && pulsePeriodMs < channel.dwellMs * 2) {
        throw new ValidationError(
          `changes[${i}]: pulsePeriodMs (${pulsePeriodMs}) violates physical dwell_ms (${channel.dwellMs})`
        );
      }
    }

    let stableBlockId: number | null | undefined = undefined;
    if (r["stableBlockId"] !== undefined && r["stableBlockId"] !== null) {
      stableBlockId = assertFiniteInt(
        r["stableBlockId"],
        `changes[${i}].stableBlockId`,
        1,
        0x7fffffff
      );
      if (seenBlockIds.has(stableBlockId)) {
        throw new ValidationError(`duplicate stableBlockId ${stableBlockId} in RuleSetV1`);
      }
      seenBlockIds.add(stableBlockId);
    }

    let blockType: BlockTypeName | null | undefined = undefined;
    if (r["blockType"] !== undefined && r["blockType"] !== null) {
      const bt = assertNonEmptyString(r["blockType"], `changes[${i}].blockType`) as BlockTypeName;
      if (!(bt in BLOCK_TYPE_FROM_NAME)) {
        throw new ValidationError(`changes[${i}].blockType unknown: ${bt}`);
      }
      blockType = bt;
    }

    validatedChanges.push({
      op,
      ruleId,
      templateId,
      input,
      unit,
      triggerAtOrAbove: triggerAtOrAbove ?? null,
      clearAtOrBelow: clearAtOrBelow ?? null,
      bandLow: bandLow ?? null,
      bandHigh: bandHigh ?? null,
      pulsePeriodMs: pulsePeriodMs ?? null,
      holdMs: holdMs ?? null,
      requiredQuality,
      onInputFault,
      output,
      timingStateOnChange,
      stableBlockId: stableBlockId ?? null,
      blockType: blockType ?? null,
    });
  }

  return {
    schema: "rule-set/v1",
    baseRevision,
    changes: validatedChanges,
  };
}
