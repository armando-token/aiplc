import {
  BLOCK_TYPE_FROM_NAME,
  BlockType,
  MigrationPolicy,
  RetentionClass,
  RuleOp,
  RuleSetV1,
  SIGNAL_QUALITY_FROM_NAME,
  SIGNAL_PHYSICAL_INHIBIT,
  SIGNAL_START_STOP_PERMIT,
  SIGNAL_TEMP_PROCESS,
  SIGNAL_TEST_AUX_1,
  OUT_BIT_BLUE_VIRTUAL,
  OUT_BIT_GREEN,
  OUT_BIT_RED,
} from "@plc-auto/control-ir";

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
  const bytes = Buffer.from(rule.ruleId, "utf8");
  for (const b of bytes) {
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

export function migrationPolicyNum(policy: RuleOp["timingStateOnChange"]): number {
  switch (policy) {
    case "PRESERVE_ELAPSED":
      return MigrationPolicy.PreserveElapsed;
    case "RESET":
      return MigrationPolicy.Reset;
    case "REJECT_INCOMPATIBLE":
      return MigrationPolicy.RejectIncompatible;
  }
}

/**
 * Generates deterministic, zero-allocation AssemblyScript (`--runtime stub`) source
 * implementing the exact `PLCQ` -> `PLCR` binary ABI and RuleSetV1 semantics.
 */
export function generateAssemblyScriptForRuleSet(ruleSet: RuleSetV1): string {
  const activeRules = ruleSet.changes.filter((r) => r.op !== "removeRule");

  const ruleBlocks: string[] = [];
  for (const r of activeRules) {
    const outBit = outputBitForName(r.output);
    const sigId = signalIdForName(r.input);
    const reqQual = SIGNAL_QUALITY_FROM_NAME[r.requiredQuality];
    const sid = stableIdForRule(r);
    const btype = blockTypeForRule(r);
    const migPol = migrationPolicyNum(r.timingStateOnChange);
    const resetOnFault = r.onInputFault !== "COMMAND_OFF_HOLD";

    let body = "";
    if (r.templateId === "thresholdHoldWithLatchedHysteresis") {
      const trig = r.triggerAtOrAbove ?? 450;
      const clear = r.clearAtOrBelow ?? trig;
      const holdNs = BigInt(r.holdMs ?? 0) * 1_000_000n;
      body = `
      if (lastOut == 1) {
        if (val <= ${clear}) {
          lastOut = 0; phase = 0; prevCond = 0; startNs = 0; elapsedNs = 0;
        } else {
          phase = 2; prevCond = val >= ${trig} ? 1 : 0;
        }
      } else {
        if (val >= ${trig}) {
          if (${holdNs} == 0) {
            phase = 2; lastOut = 1; prevCond = 1; startNs = logicalNs; elapsedNs = 0;
          } else if (phase != 1) {
            phase = 1; prevCond = 1;
            startNs = logicalNs >= elapsedNs ? (logicalNs - elapsedNs) : 0;
            if (elapsedNs >= ${holdNs}) { phase = 2; lastOut = 1; }
          } else {
            elapsedNs = logicalNs >= startNs ? (logicalNs - startNs) : 0;
            prevCond = 1;
            if (elapsedNs >= ${holdNs}) { phase = 2; lastOut = 1; }
          }
        } else {
          phase = 0; prevCond = 0; lastOut = 0; startNs = 0; elapsedNs = 0;
        }
      }`;
    } else if (r.templateId === "bandPrealarmPulse") {
      const low = r.bandLow ?? 400;
      const high = r.bandHigh ?? 450;
      const periodNs = BigInt(Math.max(40, r.pulsePeriodMs ?? 2000)) * 1_000_000n;
      const halfNs = periodNs / 2n;
      body = `
      if (val >= ${low} && val < ${high}) {
        if (phase == 0) {
          phase = 1; startNs = logicalNs; elapsedNs = 0;
        } else {
          elapsedNs = logicalNs >= startNs ? (logicalNs - startNs) : 0;
        }
        const rem: u64 = elapsedNs % ${periodNs};
        lastOut = rem < ${halfNs} ? 1 : 0;
        prevCond = 1;
      } else {
        phase = 0; prevCond = 0; lastOut = 0; startNs = 0; elapsedNs = 0;
      }`;
    } else if (r.templateId === "copyBoolean") {
      body = `
      const cond: u8 = val != 0 ? 1 : 0;
      prevCond = cond; lastOut = cond; phase = cond;`;
    } else if (r.templateId === "manualMask") {
      const trig = r.triggerAtOrAbove ?? 1;
      body = `
      const cond: u8 = val >= ${trig} ? 1 : 0;
      prevCond = cond; lastOut = cond; phase = cond;`;
    } else if (r.templateId === "pulseTimer") {
      const trig = r.triggerAtOrAbove ?? 1;
      const holdNs = BigInt(r.holdMs ?? 1000) * 1_000_000n;
      body = `
      const cond: bool = val >= ${trig};
      const rising: bool = cond && prevCond == 0;
      prevCond = cond ? 1 : 0;
      if (phase == 1) {
        elapsedNs = logicalNs >= startNs ? (logicalNs - startNs) : 0;
        if (elapsedNs >= ${holdNs}) { phase = 0; lastOut = 0; } else { lastOut = 1; }
      } else if (rising && ${holdNs} > 0) {
        phase = 1; startNs = logicalNs; elapsedNs = 0; lastOut = 1;
      } else {
        lastOut = 0;
      }`;
    } else if (r.templateId === "offDelay") {
      const trig = r.triggerAtOrAbove ?? 1;
      const holdNs = BigInt(r.holdMs ?? 1000) * 1_000_000n;
      body = `
      const cond: bool = val >= ${trig};
      if (cond) {
        phase = 1; lastOut = 1; startNs = logicalNs; elapsedNs = 0;
      } else if (phase == 1) {
        phase = 2; startNs = logicalNs; elapsedNs = 0;
        lastOut = ${holdNs} > 0 ? 1 : 0;
      } else if (phase == 2) {
        elapsedNs = logicalNs >= startNs ? (logicalNs - startNs) : 0;
        if (elapsedNs >= ${holdNs}) { phase = 0; lastOut = 0; } else { lastOut = 1; }
      } else {
        lastOut = 0;
      }
      prevCond = cond ? 1 : 0;`;
    } else if (r.templateId === "counterTrigger") {
      const trig = r.triggerAtOrAbove ?? 1;
      const preset = r.holdMs ?? 3;
      body = `
      const cond: bool = val >= ${trig};
      if (cond && prevCond == 0 && countParam < 0x7fffffff) { countParam += 1; }
      prevCond = cond ? 1 : 0;
      lastOut = countParam >= ${preset} ? 1 : 0;`;
    }

    const safeRuleComment = r.ruleId.replace(/[\r\n\u2028\u2029]/g, " ");
    const safeOutComment = r.output.replace(/[\r\n\u2028\u2029]/g, " ");
    ruleBlocks.push(`
  // Rule: ${safeRuleComment} -> ${safeOutComment}
  {
    validMask |= ${outBit};
    const sigOff = findSignal(reqPtr, sigCount, ${sigId});
    const qualOk: bool = sigOff >= 0 && load<u8>(sigOff + 8) == ${reqQual};
    let blkIdx = findBlock(resBlocksBase, outBlkCount, ${sid});
    if (blkIdx < 0 && outBlkCount < 32) {
      if (<i32>(56 + (outBlkCount + 1) * 40) > resCap) return -5;
      blkIdx = <i32>outBlkCount;
      const b = resBlocksBase + (<usize>blkIdx * 40);
      store<u32>(b, ${sid});
      store<u8>(b + 4, ${btype});
      store<u8>(b + 5, 1);
      store<u8>(b + 6, ${RetentionClass.Cyclic});
      store<u8>(b + 7, ${migPol});
      store<u8>(b + 8, 0);
      store<u8>(b + 9, 0);
      store<u8>(b + 10, 0);
      store<u8>(b + 11, 0);
      store<i32>(b + 12, 0);
      store<u64>(b + 16, 0);
      store<u64>(b + 24, 0);
      store<i64>(b + 32, 0);
      outBlkCount += 1;
    }
    if (blkIdx >= 0) {
      const b = resBlocksBase + (<usize>blkIdx * 40);
      let prevCond = load<u8>(b + 8);
      let phase = load<u8>(b + 9);
      let lastOut = load<u8>(b + 10);
      let countParam = load<i32>(b + 12);
      let startNs = load<u64>(b + 16);
      let elapsedNs = load<u64>(b + 24);
      if (!qualOk) {
        if (${resetOnFault ? "true" : "false"}) {
          prevCond = 0; phase = 0; lastOut = 0; startNs = 0; elapsedNs = 0;
        } else {
          lastOut = 0;
        }
      } else {
        const val = load<i32>(sigOff + 4);
        ${body}
      }
      store<u8>(b + 8, prevCond);
      store<u8>(b + 9, phase);
      store<u8>(b + 10, lastOut);
      store<i32>(b + 12, countParam);
      store<u64>(b + 16, startNs);
      store<u64>(b + 24, elapsedNs);
      if (lastOut != 0) {
        desiredMask |= ${outBit};
      }
    }
  }`);
  }

  return `// Auto-generated deterministic PLC logic module (AssemblyScript stub runtime)
const REQ_BUF: usize = memory.data(4096);
const RES_BUF: usize = memory.data(4096);

export function get_req_buf(): i32 {
  return <i32>REQ_BUF;
}

export function get_res_buf(): i32 {
  return <i32>RES_BUF;
}

function findSignal(reqPtr: usize, sigCount: u32, targetId: u32): isize {
  const base = reqPtr + 72;
  for (let i: u32 = 0; i < sigCount; i++) {
    const off = base + (<usize>i * 16);
    if (load<u32>(off) == targetId) {
      return <isize>off;
    }
  }
  return -1;
}

function findBlock(blocksBase: usize, blkCount: u32, stableId: u32): isize {
  for (let i: u32 = 0; i < blkCount; i++) {
    const off = blocksBase + (<usize>i * 40);
    if (load<u32>(off) == stableId) {
      return <isize>i;
    }
  }
  return -1;
}

export function eval_scan(reqPtrArg: i32, reqLen: i32, resPtrArg: i32, resCap: i32): i32 {
  const reqPtr = <usize>reqPtrArg;
  const resPtr = <usize>resPtrArg;
  if (reqLen < 72 || resCap < 56) return -1;
  // Magic "PLCQ" = 0x51434c50 LE
  if (load<u32>(reqPtr) != 0x51434c50) return -2;
  const abiVer = load<u16>(reqPtr + 4);
  if (abiVer != 1) return -3;

  const sigCount = <u32>load<u8>(reqPtr + 7);
  const inBlkCount = <u32>load<u8>(reqPtr + 8);
  if (sigCount > 32 || inBlkCount > 32) return -4;
  if (reqLen < <i32>(72 + sigCount * 16 + inBlkCount * 40)) return -6;

  const leaseEpoch = load<u64>(reqPtr + 20);
  const generation = load<u64>(reqPtr + 28);
  const scanId = load<u64>(reqPtr + 36);
  const logicalNs = load<u64>(reqPtr + 44);
  const stateRev = load<u64>(reqPtr + 60);

  const inBlocksBase = reqPtr + 72 + (<usize>sigCount * 16);
  const resBlocksBase = resPtr + 56;
  let outBlkCount = inBlkCount;
  if (<i32>(56 + outBlkCount * 40) > resCap) return -5;
  memory.copy(resBlocksBase, inBlocksBase, <usize>inBlkCount * 40);

  let desiredMask: u32 = 0;
  let validMask: u32 = 0;

  ${ruleBlocks.join("\n")}

  // Header "PLCR" = 0x52434c50 LE
  store<u32>(resPtr, 0x52434c50);
  store<u16>(resPtr + 4, 1);
  store<u8>(resPtr + 6, 0); // EvalStatus::Ok
  store<u8>(resPtr + 7, <u8>outBlkCount);
  store<u64>(resPtr + 8, leaseEpoch);
  store<u64>(resPtr + 16, generation);
  store<u64>(resPtr + 24, scanId);
  store<u64>(resPtr + 32, stateRev);
  store<u32>(resPtr + 40, desiredMask);
  store<u32>(resPtr + 44, validMask);
  store<u32>(resPtr + 48, 0);
  store<u32>(resPtr + 52, 0);

  return <i32>(56 + outBlkCount * 40);
}
`;
}
