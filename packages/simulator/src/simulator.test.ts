import { describe, it } from "node:test";
import * as assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

import {
  OUT_BIT_BLUE_VIRTUAL,
  OUT_BIT_RED,
  SIGNAL_PHYSICAL_INHIBIT,
  SIGNAL_TEMP_PROCESS,
  SignalQuality,
  type SiteProfileV1,
} from "@plc-auto/control-ir";
import {
  emitW3ToW5Packages,
  makeM1RuleSet,
  makeM2RuleSet,
  runPidAndCbfClosedLoopSimulation,
  simulateRuleSetTrace,
  type SimStepInput,
} from "./index.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "../../..");
const demoDir = path.join(repoRoot, "profiles/demo-lyra");

function sig(tempDecic: number, quality = SignalQuality.Good, sampleAgeMs = 10, inhibit = 0) {
  return [
    { id: SIGNAL_TEMP_PROCESS, value: tempDecic, quality, sampleAgeMs },
    { id: SIGNAL_PHYSICAL_INHIBIT, value: inhibit, quality: SignalQuality.Good, sampleAgeMs: 0 },
  ];
}

describe("Phase 2 Simulator: §8.2 Golden Traces, PID Bumpless, Experimental CBF & W3-W5", () => {
  it("matches §8.2 golden trace with RESET and PRESERVE_ELAPSED exact ms boundaries", () => {
    const profile = JSON.parse(
      fs.readFileSync(path.join(demoDir, "site-profile.json"), "utf8")
    ) as SiteProfileV1;

    const m1 = makeM1RuleSet(10_000);
    const m2Reset = makeM2RuleSet(5_000, "RESET");
    const m2Preserve = makeM2RuleSet(5_000, "PRESERVE_ELAPSED");

    const resetSteps: SimStepInput[] = [
      { logicalNs: 0n, signals: sig(449) },
      { logicalNs: 1_000_000_000n, signals: sig(450) },
      { logicalNs: 4_000_000_000n, signals: sig(449) },
      { logicalNs: 4_500_000_000n, signals: sig(451) },
      { logicalNs: 7_000_000_000n, signals: sig(451), swapToRuleSet: m2Reset },
      { logicalNs: 11_980_000_000n, signals: sig(451) },
      { logicalNs: 12_000_000_000n, signals: sig(451) },
      { logicalNs: 12_500_000_000n, signals: sig(445) },
      { logicalNs: 13_000_000_000n, signals: sig(440) },
      { logicalNs: 13_500_000_000n, signals: sig(451) },
      { logicalNs: 15_000_000_000n, signals: sig(451, SignalQuality.Stale, 1500) },
    ];

    const rOut = simulateRuleSetTrace(profile, m1, resetSteps);
    assert.equal((rOut[5]!.appliedMask & OUT_BIT_RED), 0); // 11.98s OFF
    assert.equal((rOut[6]!.appliedMask & OUT_BIT_RED), OUT_BIT_RED); // 12.0s ON
    assert.equal((rOut[7]!.appliedMask & OUT_BIT_RED), OUT_BIT_RED); // 12.5s latched ON
    assert.equal((rOut[7]!.appliedMask & OUT_BIT_BLUE_VIRTUAL), 0); // interlock veto
    assert.equal(rOut[7]!.guardReason, "INTERLOCK_VETO");
    assert.equal((rOut[8]!.appliedMask & OUT_BIT_RED), 0); // 13.0s 44.0C clear
    assert.equal(rOut[10]!.appliedMask, 0);
    assert.equal(rOut[10]!.guardReason, "INPUT_QUALITY_FAULT");

    const presSteps: SimStepInput[] = [
      { logicalNs: 0n, signals: sig(449) },
      { logicalNs: 4_500_000_000n, signals: sig(451) },
      { logicalNs: 6_500_000_000n, signals: sig(451) },
      { logicalNs: 7_000_000_000n, signals: sig(451), swapToRuleSet: m2Preserve },
      { logicalNs: 9_480_000_000n, signals: sig(451) },
      { logicalNs: 9_500_000_000n, signals: sig(451) },
    ];
    const pOut = simulateRuleSetTrace(profile, m1, presSteps);
    assert.equal((pOut[4]!.appliedMask & OUT_BIT_RED), 0); // 9.48s OFF
    assert.equal((pOut[5]!.appliedMask & OUT_BIT_RED), OUT_BIT_RED); // 9.50s ON!
  });

  it("verifies PID anti-windup + bumpless <1% jump and Experimental CBF safety invariance", () => {
    const res = runPidAndCbfClosedLoopSimulation();
    assert.ok(res.pidBumplessMaxJumpPct < 1.0, `jump ${res.pidBumplessMaxJumpPct}% >= 1%`);
    assert.ok(res.pidSettledErrorC < 0.25, `PID error ${res.pidSettledErrorC}C too large`);
    assert.ok(res.cbfMinBarrierH >= 0, `CBF invariant violated: h=${res.cbfMinBarrierH}`);
    assert.ok(res.cbfInterventions > 0);
    assert.ok(res.cbfMaxElapsedUs < 500);
  });

  it("emits signed W3, W4, W5 benchmark packages cleanly", () => {
    emitW3ToW5Packages(demoDir);
    for (const f of ["W3.armv7-linux-gnueabihf.json", "W4.quickjs.json", "W5.darwin-arm64.json"]) {
      assert.ok(fs.existsSync(path.join(demoDir, "packages", f)), `missing ${f}`);
    }
  });
});
