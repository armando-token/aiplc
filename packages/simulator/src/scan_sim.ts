import {
  ABI_VERSION,
  BLOCK_TYPE_FROM_NAME,
  BlockType,
  EvalStatus,
  MigrationPolicy,
  OUT_BIT_BLUE_VIRTUAL,
  OUT_BIT_GREEN,
  OUT_BIT_RED,
  RetentionClass,
  SIGNAL_PHYSICAL_INHIBIT,
  SIGNAL_QUALITY_FROM_NAME,
  SIGNAL_START_STOP_PERMIT,
  SIGNAL_TEMP_PROCESS,
  SIGNAL_TEST_AUX_1,
  SignalQuality,
  WorkerMode,
  type EvalRequest,
  type EvalResult,
  type InputSignal,
  type MigrationPlanEntry,
  type RuleOp,
  type RuleSetV1,
  type SiteProfileV1,
  type StateBlock,
} from "@plc-auto/control-ir";

export type GuardReason =
  | "NONE"
  | "BOOT_DISARMED"
  | "WORKER_TIMEOUT_OR_FAULT"
  | "INPUT_QUALITY_FAULT"
  | "PHYSICAL_INHIBIT"
  | "INTERLOCK_VETO"
  | "DWELL_CHATTER"
  | "CHATTER_RATE"
  | "CHANNEL_NOT_PHYSICAL"
  | "MANUAL_OVERRIDE_ACTIVE"
  | "CONTROL_LATCHED";

export function outputBitForName(name: string): number {
  switch (name) {
    case "tower.red":
      return OUT_BIT_RED;
    case "tower.green":
      return OUT_BIT_GREEN;
    case "tower.blue":
      return OUT_BIT_BLUE_VIRTUAL;
    default: {
      if (name.startsWith("virt.")) {
        const idx = Number(name.slice(5));
        if (Number.isInteger(idx) && idx >= 0 && idx < 29) {
          return (1 << (3 + idx)) >>> 0;
        }
      }
      return 0;
    }
  }
}

export function signalIdForName(name: string): number {
  switch (name) {
    case "temp.process":
      return SIGNAL_TEMP_PROCESS;
    case "physical.inhibit":
      return SIGNAL_PHYSICAL_INHIBIT;
    case "start.stop":
      return SIGNAL_START_STOP_PERMIT;
    case "test.aux1":
      return SIGNAL_TEST_AUX_1;
    default: {
      if (name.startsWith("sig.")) {
        const idx = Number(name.slice(4));
        if (Number.isInteger(idx) && idx >= 0) {
          return 100 + idx;
        }
      }
      return 0;
    }
  }
}

export function stableIdForRule(rule: RuleOp): number {
  if (rule.stableBlockId !== undefined && rule.stableBlockId !== null) {
    return rule.stableBlockId;
  }
  let h = 0x811c9dc5 >>> 0;
  for (const b of Buffer.from(rule.ruleId, "utf8")) {
    h ^= b;
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  const masked = h & 0x7fffffff;
  return masked === 0 ? 1 : masked;
}

export function blockTypeForRule(rule: RuleOp): BlockType {
  if (rule.blockType) {
    return BLOCK_TYPE_FROM_NAME[rule.blockType];
  }
  switch (rule.templateId) {
    case "thresholdHoldWithLatchedHysteresis":
      return BlockType.Ton;
    case "bandPrealarmPulse":
      return BlockType.PulseGen;
    case "offDelay":
      return BlockType.Tof;
    case "pulseTimer":
      return BlockType.Tp;
    case "counterTrigger":
      return BlockType.Ctu;
    default:
      return BlockType.ThresholdHyst;
  }
}

function migrationPolicyNum(p: RuleOp["timingStateOnChange"]): MigrationPolicy {
  switch (p) {
    case "PRESERVE_ELAPSED":
      return MigrationPolicy.PreserveElapsed;
    case "RESET":
      return MigrationPolicy.Reset;
    case "REJECT_INCOMPATIBLE":
      return MigrationPolicy.RejectIncompatible;
  }
}

export function emptyBlock(
  stableId: number,
  blockType: BlockType,
  migrationPolicy: MigrationPolicy
): StateBlock {
  return {
    stableId,
    blockType,
    schemaVersion: 1,
    retentionClass: RetentionClass.Cyclic,
    migrationPolicy,
    prevCond: 0,
    phase: 0,
    lastOutput: 0,
    flags: 0,
    countOrParam: 0,
    startLogicalNs: 0n,
    elapsedNs: 0n,
    auxI64: 0n,
  };
}

function cloneBlock(b: StateBlock): StateBlock {
  return { ...b };
}

/**
 * Deterministic TS evaluator matching Rust `evaluate_ruleset_a0` (§5.2 / P3 parity).
 */
export function evaluateRuleSetTs(ruleSet: RuleSetV1, req: EvalRequest): EvalResult {
  const nextBlocks: StateBlock[] = req.blocks.map(cloneBlock);
  let desiredMask = 0;
  let validMask = 0;

  for (const rule of ruleSet.changes) {
    if (rule.op === "removeRule") {
      continue;
    }
    const outBit = outputBitForName(rule.output);
    if (outBit === 0) {
      continue;
    }
    validMask |= outBit;

    const sigId = signalIdForName(rule.input);
    const sig = req.signals.find((s) => s.id === sigId);
    const reqQual = SIGNAL_QUALITY_FROM_NAME[rule.requiredQuality];
    const qualityOk = sig !== undefined && sig.quality === reqQual;

    const sid = stableIdForRule(rule);
    const bt = blockTypeForRule(rule);
    let blk = nextBlocks.find((b) => b.stableId === sid);
    if (!blk) {
      blk = emptyBlock(sid, bt, migrationPolicyNum(rule.timingStateOnChange));
      nextBlocks.push(blk);
    }

    if (!qualityOk || !sig) {
      if (rule.onInputFault === "COMMAND_OFF_HOLD") {
        blk.lastOutput = 0;
      } else {
        blk.prevCond = 0;
        blk.phase = 0;
        blk.lastOutput = 0;
        blk.startLogicalNs = 0n;
        blk.elapsedNs = 0n;
      }
      continue;
    }

    const val = sig.value;

    switch (rule.templateId) {
      case "thresholdHoldWithLatchedHysteresis": {
        const trig = rule.triggerAtOrAbove ?? 450;
        const clear = rule.clearAtOrBelow ?? trig;
        const holdNs = BigInt(rule.holdMs ?? 0) * 1_000_000n;

        if (blk.lastOutput === 1) {
          if (val <= clear) {
            blk.lastOutput = 0;
            blk.phase = 0;
            blk.prevCond = 0;
            blk.startLogicalNs = 0n;
            blk.elapsedNs = 0n;
          } else {
            blk.phase = 2;
            blk.prevCond = val >= trig ? 1 : 0;
          }
        } else {
          if (val >= trig) {
            if (holdNs === 0n) {
              blk.phase = 2;
              blk.lastOutput = 1;
              blk.prevCond = 1;
              blk.startLogicalNs = req.logicalNs;
              blk.elapsedNs = 0n;
            } else if (blk.phase !== 1) {
              blk.phase = 1;
              blk.prevCond = 1;
              blk.startLogicalNs = req.logicalNs > blk.elapsedNs ? req.logicalNs - blk.elapsedNs : 0n;
              if (blk.elapsedNs >= holdNs) {
                blk.phase = 2;
                blk.lastOutput = 1;
              }
            } else {
              const elapsed =
                req.logicalNs > blk.startLogicalNs ? req.logicalNs - blk.startLogicalNs : 0n;
              blk.elapsedNs = elapsed;
              blk.prevCond = 1;
              if (elapsed >= holdNs) {
                blk.phase = 2;
                blk.lastOutput = 1;
              }
            }
          } else {
            blk.phase = 0;
            blk.prevCond = 0;
            blk.lastOutput = 0;
            blk.startLogicalNs = 0n;
            blk.elapsedNs = 0n;
          }
        }
        break;
      }

      case "bandPrealarmPulse": {
        const low = rule.bandLow ?? 400;
        const high = rule.bandHigh ?? 450;
        const periodMs = Math.max(40, rule.pulsePeriodMs ?? 2000);
        const periodNs = BigInt(periodMs) * 1_000_000n;
        const halfNs = periodNs / 2n;

        if (val >= low && val < high) {
          if (blk.phase === 0) {
            blk.phase = 1;
            blk.startLogicalNs = req.logicalNs;
            blk.elapsedNs = 0n;
          } else {
            blk.elapsedNs =
              req.logicalNs > blk.startLogicalNs ? req.logicalNs - blk.startLogicalNs : 0n;
          }
          const rem = blk.elapsedNs % periodNs;
          blk.lastOutput = rem < halfNs ? 1 : 0;
          blk.prevCond = 1;
        } else {
          blk.phase = 0;
          blk.prevCond = 0;
          blk.lastOutput = 0;
          blk.startLogicalNs = 0n;
          blk.elapsedNs = 0n;
        }
        break;
      }

      case "copyBoolean": {
        const cond = val !== 0 ? 1 : 0;
        blk.prevCond = cond;
        blk.lastOutput = cond;
        blk.phase = cond;
        break;
      }

      case "manualMask": {
        const trig = rule.triggerAtOrAbove ?? 1;
        const on = val >= trig ? 1 : 0;
        blk.prevCond = on;
        blk.lastOutput = on;
        blk.phase = on;
        break;
      }

      case "pulseTimer": {
        const trig = rule.triggerAtOrAbove ?? 1;
        const holdNs = BigInt(rule.holdMs ?? 1000) * 1_000_000n;
        const cond = val >= trig;
        const rising = cond && blk.prevCond === 0;
        blk.prevCond = cond ? 1 : 0;
        if (blk.phase === 1) {
          const elapsed =
            req.logicalNs > blk.startLogicalNs ? req.logicalNs - blk.startLogicalNs : 0n;
          blk.elapsedNs = elapsed;
          if (elapsed >= holdNs) {
            blk.phase = 0;
            blk.lastOutput = 0;
          } else {
            blk.lastOutput = 1;
          }
        } else if (rising && holdNs > 0n) {
          blk.phase = 1;
          blk.startLogicalNs = req.logicalNs;
          blk.elapsedNs = 0n;
          blk.lastOutput = 1;
        } else {
          blk.lastOutput = 0;
        }
        break;
      }

      case "offDelay": {
        const trig = rule.triggerAtOrAbove ?? 1;
        const holdNs = BigInt(rule.holdMs ?? 1000) * 1_000_000n;
        const cond = val >= trig;
        if (cond) {
          blk.phase = 1;
          blk.lastOutput = 1;
          blk.startLogicalNs = req.logicalNs;
          blk.elapsedNs = 0n;
        } else if (blk.phase === 1) {
          blk.phase = 2;
          blk.startLogicalNs = req.logicalNs;
          blk.elapsedNs = 0n;
          blk.lastOutput = holdNs > 0n ? 1 : 0;
        } else if (blk.phase === 2) {
          const elapsed =
            req.logicalNs > blk.startLogicalNs ? req.logicalNs - blk.startLogicalNs : 0n;
          blk.elapsedNs = elapsed;
          if (elapsed >= holdNs) {
            blk.phase = 0;
            blk.lastOutput = 0;
          } else {
            blk.lastOutput = 1;
          }
        } else {
          blk.lastOutput = 0;
        }
        blk.prevCond = cond ? 1 : 0;
        break;
      }

      case "counterTrigger": {
        const trig = rule.triggerAtOrAbove ?? 1;
        const preset = rule.holdMs ?? 3;
        const cond = val >= trig;
        if (cond && blk.prevCond === 0) {
          blk.countOrParam += 1;
        }
        blk.prevCond = cond ? 1 : 0;
        blk.lastOutput = blk.countOrParam >= preset ? 1 : 0;
        break;
      }

      default:
        blk.lastOutput = 0;
        break;
    }

    if (blk.lastOutput !== 0) {
      desiredMask |= outBit;
    }
  }

  return {
    abiVersion: ABI_VERSION,
    status: EvalStatus.Ok,
    leaseEpoch: req.leaseEpoch,
    generation: req.generation,
    scanId: req.scanId,
    stateRevision: req.stateRevision,
    desiredMask,
    validMask,
    diagCode: 0,
    evalDurationNs: 0,
    blocks: nextBlocks,
  };
}

export function requiredBlocksFromRuleSet(ruleSet: RuleSetV1): MigrationPlanEntry[] {
  const out: MigrationPlanEntry[] = [];
  for (const r of ruleSet.changes) {
    if (r.op === "removeRule") continue;
    const sid = stableIdForRule(r);
    const bt = blockTypeForRule(r);
    const btName =
      r.blockType ??
      (bt === BlockType.Ton
        ? "TON"
        : bt === BlockType.PulseGen
          ? "PULSE_GEN"
          : bt === BlockType.Tof
            ? "TOF"
            : bt === BlockType.Tp
              ? "TP"
              : bt === BlockType.Ctu
                ? "CTU"
                : "THRESHOLD_HYST");
    out.push({
      stableId: sid,
      blockType: btName,
      schemaVersion: 1,
      stateSize: 40,
      retentionClass: "CYCLIC",
      migrationPolicy: r.timingStateOnChange,
    });
  }
  return out;
}

export class StateArenaTs {
  stateRevision = 0n;
  blocks: StateBlock[] = [];

  applyMigration(entries: MigrationPlanEntry[], boundaryLogicalNs: bigint): void {
    const seen = new Set<number>();
    const next: StateBlock[] = [];
    for (const entry of entries) {
      if (seen.has(entry.stableId)) {
        throw new Error(`Duplicate stable block ID ${entry.stableId}`);
      }
      seen.add(entry.stableId);
      const bt = BLOCK_TYPE_FROM_NAME[entry.blockType];
      const pol = migrationPolicyNum(entry.migrationPolicy);
      const existing = this.blocks.find((b) => b.stableId === entry.stableId);
      if (existing) {
        if (existing.blockType !== bt || existing.schemaVersion !== entry.schemaVersion) {
          throw new Error(`Incompatible block change for stableId=${entry.stableId}`);
        }
        if (pol === MigrationPolicy.PreserveElapsed) {
          const preserved = cloneBlock(existing);
          preserved.migrationPolicy = pol;
          if (preserved.phase === 1) {
            if (preserved.startLogicalNs > 0n && preserved.startLogicalNs <= boundaryLogicalNs) {
              preserved.elapsedNs = boundaryLogicalNs - preserved.startLogicalNs;
            } else {
              preserved.startLogicalNs =
                boundaryLogicalNs > preserved.elapsedNs
                  ? boundaryLogicalNs - preserved.elapsedNs
                  : 0n;
            }
          }
          next.push(preserved);
        } else if (pol === MigrationPolicy.Reset) {
          next.push(emptyBlock(entry.stableId, bt, pol));
        } else {
          throw new Error(`REJECT_INCOMPATIBLE policy triggered for stableId=${entry.stableId}`);
        }
      } else {
        next.push(emptyBlock(entry.stableId, bt, pol));
      }
    }
    this.blocks = next;
    this.stateRevision += 1n;
  }
}

export class ShieldTs {
  private prevAppliedMask = 0;
  private lastTransitionNs: bigint[];
  private hasTransitioned: boolean[];
  private freshnessTtlMs: number;

  constructor(private readonly profile: SiteProfileV1) {
    this.freshnessTtlMs = profile.sensors[0]?.freshnessTtlMs ?? 1500;
    this.lastTransitionNs = Array(this.profile.channels.length).fill(0n);
    this.hasTransitioned = Array(this.profile.channels.length).fill(false);
  }

  evaluate(
    nowNs: bigint,
    armed: boolean,
    workerOk: boolean,
    signals: InputSignal[],
    desiredMask: number
  ): { appliedMask: number; guardReason: GuardReason } {
    if (!armed) {
      this.prevAppliedMask = 0;
      return { appliedMask: 0, guardReason: "BOOT_DISARMED" };
    }
    const inhibit = signals.find((s) => s.id === SIGNAL_PHYSICAL_INHIBIT);
    if (inhibit && inhibit.value !== 0) {
      this.prevAppliedMask = 0;
      return { appliedMask: 0, guardReason: "PHYSICAL_INHIBIT" };
    }
    const temp = signals.find((s) => s.id === SIGNAL_TEMP_PROCESS);
    if (!temp || temp.quality !== SignalQuality.Good || temp.sampleAgeMs >= this.freshnessTtlMs) {
      this.prevAppliedMask = 0;
      return { appliedMask: 0, guardReason: "INPUT_QUALITY_FAULT" };
    }
    if (!workerOk) {
      this.prevAppliedMask = 0;
      return { appliedMask: 0, guardReason: "WORKER_TIMEOUT_OR_FAULT" };
    }

    let candidate = desiredMask;
    let guardReason: GuardReason = "NONE";

    // Mutual exclusion interlocks (e.g. red > blue)
    for (const ilk of this.profile.interlocks) {
      if ((candidate & ilk.forbiddenMask) === ilk.forbiddenMask) {
        candidate &= ~OUT_BIT_BLUE_VIRTUAL;
        if ((candidate & ilk.forbiddenMask) === ilk.forbiddenMask) {
          candidate &= ~ilk.forbiddenMask;
        }
        guardReason = "INTERLOCK_VETO";
      }
    }

    // Min dwell chatter filter on channels unless contingency OFF
    let applied = candidate;
    for (let chIdx = 0; chIdx < this.profile.channels.length; chIdx++) {
      const ch = this.profile.channels[chIdx]!;
      if (ch.maskBit >= 32) continue;
      const bit = (1 << ch.maskBit) >>> 0;
      const prevOn = (this.prevAppliedMask & bit) !== 0;
      const nextOn = (applied & bit) !== 0;
      if (prevOn !== nextOn) {
        const dwellNs = BigInt(ch.dwellMs) * 1_000_000n;
        const prevNs = this.lastTransitionNs[chIdx] ?? 0n;
        const since = nowNs > prevNs ? nowNs - prevNs : 0n;
        if (this.hasTransitioned[chIdx] && since < dwellNs) {
          applied = prevOn ? applied | bit : applied & ~bit;
          guardReason = "DWELL_CHATTER";
        } else {
          this.lastTransitionNs[chIdx] = nowNs;
          this.hasTransitioned[chIdx] = true;
        }
      }
    }

    // Post-dwell interlock recheck matching Rust shield.rs step 9
    for (const ilk of this.profile.interlocks) {
      if ((applied & ilk.forbiddenMask) === ilk.forbiddenMask) {
        applied &= ~OUT_BIT_BLUE_VIRTUAL;
        if ((applied & ilk.forbiddenMask) === ilk.forbiddenMask) {
          applied &= ~ilk.forbiddenMask;
        }
        guardReason = "INTERLOCK_VETO";
      }
    }

    this.prevAppliedMask = applied;
    return { appliedMask: applied, guardReason };
  }
}

export interface SimStepInput {
  logicalNs: bigint;
  signals: InputSignal[];
  swapToRuleSet?: RuleSetV1;
}

export interface SimStepOutput {
  scanId: number;
  logicalNs: bigint;
  desiredMask: number;
  appliedMask: number;
  guardReason: GuardReason;
  blocks: StateBlock[];
}

export function simulateRuleSetTrace(
  profile: SiteProfileV1,
  initialRuleSet: RuleSetV1,
  steps: SimStepInput[]
): SimStepOutput[] {
  const arena = new StateArenaTs();
  const shield = new ShieldTs(profile);
  let active = initialRuleSet;
  arena.applyMigration(requiredBlocksFromRuleSet(active), 0n);

  const out: SimStepOutput[] = [];
  for (let idx = 0; idx < steps.length; idx++) {
    const step = steps[idx]!;
    if (step.swapToRuleSet) {
      const reqBlocks = requiredBlocksFromRuleSet(step.swapToRuleSet);
      arena.applyMigration(reqBlocks, step.logicalNs);
      active = step.swapToRuleSet;
    }

    const req: EvalRequest = {
      abiVersion: ABI_VERSION,
      mode: WorkerMode.Shadow,
      bootId: 1n,
      leaseEpoch: 1n,
      generation: 1n,
      scanId: BigInt(idx + 1),
      logicalNs: step.logicalNs,
      inputSeq: BigInt(idx + 1),
      stateRevision: arena.stateRevision,
      appliedOutputMask: 0,
      signals: step.signals,
      blocks: arena.blocks,
    };

    const res = evaluateRuleSetTs(active, req);
    arena.blocks = res.blocks;
    const { appliedMask, guardReason } = shield.evaluate(
      step.logicalNs,
      true,
      res.status === EvalStatus.Ok,
      step.signals,
      res.desiredMask
    );

    if (guardReason === "INPUT_QUALITY_FAULT") {
      for (const r of active.changes) {
        if (r.op === "removeRule" || r.onInputFault === "COMMAND_OFF_HOLD") continue;
        const sid = stableIdForRule(r);
        const blk = arena.blocks.find((b) => b.stableId === sid);
        if (blk) {
          blk.prevCond = 0;
          blk.phase = 0;
          blk.lastOutput = 0;
          blk.startLogicalNs = 0n;
          blk.elapsedNs = 0n;
        }
      }
    }

    out.push({
      scanId: idx + 1,
      logicalNs: step.logicalNs,
      desiredMask: res.desiredMask,
      appliedMask,
      guardReason,
      blocks: arena.blocks.map(cloneBlock),
    });
  }
  return out;
}
