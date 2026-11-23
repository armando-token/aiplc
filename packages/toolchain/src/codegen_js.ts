import { RetentionClass, RuleSetV1, SIGNAL_QUALITY_FROM_NAME } from "@plc-auto/control-ir";
import {
  blockTypeForRule,
  migrationPolicyNum,
  outputBitForName,
  signalIdForName,
  stableIdForRule,
} from "./codegen_as.js";

/**
 * Generates deterministic, allocation-bounded JS (`QuickJS-NG`) implementing the exact
 * `PLCQ` -> `PLCR` binary ABI and RuleSetV1 semantics with zero host/system APIs.
 */
export function generateQuickJsForRuleSet(ruleSet: RuleSetV1): string {
  const activeRules = ruleSet.changes.filter((r) => r.op !== "removeRule");
  const ruleChecks: string[] = [];

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
      if (lastOut === 1) {
        if (val <= ${clear}) {
          lastOut = 0; phase = 0; prevCond = 0; startNs = 0n; elapsedNs = 0n;
        } else {
          phase = 2; prevCond = val >= ${trig} ? 1 : 0;
        }
      } else {
        if (val >= ${trig}) {
          if (${holdNs}n === 0n) {
            phase = 2; lastOut = 1; prevCond = 1; startNs = logicalNs; elapsedNs = 0n;
          } else if (phase !== 1) {
            phase = 1; prevCond = 1;
            startNs = logicalNs >= elapsedNs ? (logicalNs - elapsedNs) : 0n;
            if (elapsedNs >= ${holdNs}n) { phase = 2; lastOut = 1; }
          } else {
            elapsedNs = logicalNs >= startNs ? (logicalNs - startNs) : 0n;
            prevCond = 1;
            if (elapsedNs >= ${holdNs}n) { phase = 2; lastOut = 1; }
          }
        } else {
          phase = 0; prevCond = 0; lastOut = 0; startNs = 0n; elapsedNs = 0n;
        }
      }`;
    } else if (r.templateId === "bandPrealarmPulse") {
      const low = r.bandLow ?? 400;
      const high = r.bandHigh ?? 450;
      const periodNs = BigInt(Math.max(40, r.pulsePeriodMs ?? 2000)) * 1_000_000n;
      const halfNs = periodNs / 2n;
      body = `
      if (val >= ${low} && val < ${high}) {
        if (phase === 0) {
          phase = 1; startNs = logicalNs; elapsedNs = 0n;
        } else {
          elapsedNs = logicalNs >= startNs ? (logicalNs - startNs) : 0n;
        }
        lastOut = (elapsedNs % ${periodNs}n) < ${halfNs}n ? 1 : 0;
        prevCond = 1;
      } else {
        phase = 0; prevCond = 0; lastOut = 0; startNs = 0n; elapsedNs = 0n;
      }`;
    } else if (r.templateId === "copyBoolean") {
      body = `const cond = val !== 0 ? 1 : 0; prevCond = cond; lastOut = cond; phase = cond;`;
    } else if (r.templateId === "manualMask") {
      const trig = r.triggerAtOrAbove ?? 1;
      body = `const cond = val >= ${trig} ? 1 : 0; prevCond = cond; lastOut = cond; phase = cond;`;
    } else if (r.templateId === "pulseTimer") {
      const trig = r.triggerAtOrAbove ?? 1;
      const holdNs = BigInt(r.holdMs ?? 1000) * 1_000_000n;
      body = `
      const cond = val >= ${trig};
      const rising = cond && prevCond === 0;
      prevCond = cond ? 1 : 0;
      if (phase === 1) {
        elapsedNs = logicalNs >= startNs ? (logicalNs - startNs) : 0n;
        if (elapsedNs >= ${holdNs}n) { phase = 0; lastOut = 0; } else { lastOut = 1; }
      } else if (rising && ${holdNs}n > 0n) {
        phase = 1; startNs = logicalNs; elapsedNs = 0n; lastOut = 1;
      } else {
        lastOut = 0;
      }`;
    } else if (r.templateId === "offDelay") {
      const trig = r.triggerAtOrAbove ?? 1;
      const holdNs = BigInt(r.holdMs ?? 1000) * 1_000_000n;
      body = `
      const cond = val >= ${trig};
      if (cond) {
        phase = 1; lastOut = 1; startNs = logicalNs; elapsedNs = 0n;
      } else if (phase === 1) {
        phase = 2; startNs = logicalNs; elapsedNs = 0n;
        lastOut = ${holdNs}n > 0n ? 1 : 0;
      } else if (phase === 2) {
        elapsedNs = logicalNs >= startNs ? (logicalNs - startNs) : 0n;
        if (elapsedNs >= ${holdNs}n) { phase = 0; lastOut = 0; } else { lastOut = 1; }
      } else {
        lastOut = 0;
      }
      prevCond = cond ? 1 : 0;`;
    } else if (r.templateId === "counterTrigger") {
      const trig = r.triggerAtOrAbove ?? 1;
      const preset = r.holdMs ?? 3;
      body = `
      const cond = val >= ${trig};
      if (cond && prevCond === 0 && countParam < 0x7fffffff) { countParam = (countParam + 1) | 0; }
      prevCond = cond ? 1 : 0;
      lastOut = countParam >= ${preset} ? 1 : 0;`;
    }

    const safeRuleComment = r.ruleId.replace(/[\r\n\u2028\u2029]/g, " ");
    const safeOutComment = r.output.replace(/[\r\n\u2028\u2029]/g, " ");
    ruleChecks.push(`
    // Rule: ${safeRuleComment} -> ${safeOutComment}
    {
      validMask = (validMask | ${outBit}) >>> 0;
      const sigOff = findSignal(rv, sigCount, ${sigId});
      const qualOk = sigOff >= 0 && reqBytes[sigOff + 8] === ${reqQual};
      let blkIdx = findBlock(outView, outBlkCount, ${sid});
      if (blkIdx < 0 && outBlkCount < 32) {
        if (56 + (outBlkCount + 1) * 40 > outBytes.byteLength) {
          throw new Error("response buffer overflow");
        }
        blkIdx = outBlkCount;
        const b = 56 + blkIdx * 40;
        outView.setUint32(b, ${sid}, true);
        outBytes[b + 4] = ${btype};
        outBytes[b + 5] = 1;
        outBytes[b + 6] = ${RetentionClass.Cyclic};
        outBytes[b + 7] = ${migPol};
        outBytes[b + 8] = 0;
        outBytes[b + 9] = 0;
        outBytes[b + 10] = 0;
        outBytes[b + 11] = 0;
        outView.setInt32(b + 12, 0, true);
        outView.setBigUint64(b + 16, 0n, true);
        outView.setBigUint64(b + 24, 0n, true);
        outView.setBigInt64(b + 32, 0n, true);
        outBlkCount += 1;
      }
      if (blkIdx >= 0) {
        const b = 56 + blkIdx * 40;
        let prevCond = outBytes[b + 8];
        let phase = outBytes[b + 9];
        let lastOut = outBytes[b + 10];
        let countParam = outView.getInt32(b + 12, true);
        let startNs = outView.getBigUint64(b + 16, true);
        let elapsedNs = outView.getBigUint64(b + 24, true);
        if (!qualOk) {
          if (${resetOnFault}) {
            prevCond = 0; phase = 0; lastOut = 0; startNs = 0n; elapsedNs = 0n;
          } else {
            lastOut = 0;
          }
        } else {
          const val = rv.getInt32(sigOff + 4, true);
          ${body}
        }
        outBytes[b + 8] = prevCond;
        outBytes[b + 9] = phase;
        outBytes[b + 10] = lastOut;
        outView.setInt32(b + 12, countParam, true);
        outView.setBigUint64(b + 16, startNs, true);
        outView.setBigUint64(b + 24, elapsedNs, true);
        if (lastOut !== 0) {
          desiredMask = (desiredMask | ${outBit}) >>> 0;
        }
      }
    }`);
  }

  return `"use strict";
const __RES_BACKING = new Uint8Array(4096);
const __RES_VIEW = new DataView(__RES_BACKING.buffer);

function findSignal(rv, sigCount, targetId) {
  let off = 72;
  for (let i = 0; i < sigCount; i++) {
    if (rv.getUint32(off, true) === targetId) return off;
    off += 16;
  }
  return -1;
}

function findBlock(ov, blkCount, stableId) {
  let off = 56;
  for (let i = 0; i < blkCount; i++) {
    if (ov.getUint32(off, true) === stableId) return i;
    off += 40;
  }
  return -1;
}

globalThis.evalScan = function(reqBytes) {
  if (!(reqBytes instanceof Uint8Array) || reqBytes.byteLength < 72) {
    throw new Error("bad EvalRequest");
  }
  const rv = new DataView(reqBytes.buffer, reqBytes.byteOffset, reqBytes.byteLength);
  if (rv.getUint32(0, true) !== 0x51434c50) throw new Error("bad magic");
  if (rv.getUint16(4, true) !== 1) throw new Error("bad abi");
  const sigCount = reqBytes[7];
  const inBlkCount = reqBytes[8];
  if (sigCount > 32 || inBlkCount > 32) throw new Error("limits exceeded");
  const inBlkStart = 72 + sigCount * 16;
  const inBlkBytes = inBlkCount * 40;
  if (reqBytes.byteLength < inBlkStart + inBlkBytes) {
    throw new Error("truncated EvalRequest");
  }
  const leaseEpoch = rv.getBigUint64(20, true);
  const generation = rv.getBigUint64(28, true);
  const scanId = rv.getBigUint64(36, true);
  const logicalNs = rv.getBigUint64(44, true);
  const stateRev = rv.getBigUint64(60, true);

  const outBytes = __RES_BACKING;
  const outView = __RES_VIEW;
  for (let i = 0; i < inBlkBytes; i++) {
    outBytes[56 + i] = reqBytes[inBlkStart + i];
  }

  let outBlkCount = inBlkCount;
  let desiredMask = 0;
  let validMask = 0;

  ${ruleChecks.join("\n")}

  outView.setUint32(0, 0x52434c50, true);
  outView.setUint16(4, 1, true);
  outBytes[6] = 0;
  outBytes[7] = outBlkCount;
  outView.setBigUint64(8, leaseEpoch, true);
  outView.setBigUint64(16, generation, true);
  outView.setBigUint64(24, scanId, true);
  outView.setBigUint64(32, stateRev, true);
  outView.setUint32(40, desiredMask >>> 0, true);
  outView.setUint32(44, validMask >>> 0, true);
  outView.setUint32(48, 0, true);
  outView.setUint32(52, 0, true);

  return outBytes.subarray(0, 56 + outBlkCount * 40);
};
`;
}
