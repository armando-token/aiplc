import { describe, it } from "node:test";
import * as assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

import {
  SIGNAL_PHYSICAL_INHIBIT,
  SIGNAL_TEMP_PROCESS,
  SignalQuality,
  type SiteProfileV1,
} from "@plc-auto/control-ir";
import {
  makeM1RuleSet,
  simulateRuleSetTrace,
  type SimStepInput,
} from "@plc-auto/simulator";
import {
  PlcOpenImportError,
  StDiagnosticError,
  importPlcOpenLdXml,
  importStructuredText,
  renderLadderSvg,
  ruleSetToLadderModel,
} from "./index.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "../../..");
const demoDir = path.join(repoRoot, "profiles/demo-lyra");

function sig(tempDecic: number) {
  return [
    { id: SIGNAL_TEMP_PROCESS, value: tempDecic, quality: SignalQuality.Good, sampleAgeMs: 10 },
    { id: SIGNAL_PHYSICAL_INHIBIT, value: 0, quality: SignalQuality.Good, sampleAgeMs: 0 },
  ];
}

describe("P2/Phase 2 IEC 61131-3 ST + PLCopen XML LD Import (I01 & I02)", () => {
  it("I01: imports equivalent ST and PLCopen LD with identical trace behavior and renders offline SVG", () => {
    const profile = JSON.parse(
      fs.readFileSync(path.join(demoDir, "site-profile.json"), "utf8")
    ) as SiteProfileV1;

    const stSrc = `
PROGRAM HighTempAlarm
VAR
  tAlarm : TON;
END_VAR
tAlarm(IN := temp_process >= 450, PT := T#10s);
tower_red := tAlarm.Q;
END_PROGRAM
`;
    const stRes = importStructuredText(stSrc, "0");

    const ldXml = `<?xml version="1.0" encoding="utf-8"?>
<project xmlns="http://www.plcopen.org/xml/tc6_0201">
  <types><pous>
    <pou name="HighTempAlarmLD" pouType="program">
      <body><LD>
        <contact localId="1" negated="false"><variable>temp.process</variable></contact>
        <block localId="2" typeName="TON" holdMs="10000" triggerAtOrAbove="450">
          <connectionPointIn><connection refLocalId="1"/></connectionPointIn>
        </block>
        <coil localId="3" triggerAtOrAbove="450">
          <variable>tower.red</variable>
          <connectionPointIn><connection refLocalId="2"/></connectionPointIn>
        </coil>
      </LD></body>
    </pou>
  </pous></types>
</project>`;
    const ldRes = importPlcOpenLdXml(ldXml, "0");

    const steps: SimStepInput[] = [
      { logicalNs: 0n, signals: sig(449) },
      { logicalNs: 1_000_000_000n, signals: sig(450) },
      { logicalNs: 5_000_000_000n, signals: sig(451) },
      { logicalNs: 10_980_000_000n, signals: sig(451) },
      { logicalNs: 11_000_000_000n, signals: sig(451) },
    ];

    // Baseline M1 with exact threshold (trigger=450, clear=450, 10s TON)
    const canonical = makeM1RuleSet(10_000);
    canonical.changes[0]!.clearAtOrBelow = 450;

    const outCan = simulateRuleSetTrace(profile, canonical, steps).map((s) => s.appliedMask);
    const outSt = simulateRuleSetTrace(profile, stRes.ruleSet, steps).map((s) => s.appliedMask);
    const outLd = simulateRuleSetTrace(profile, ldRes.ruleSet, steps).map((s) => s.appliedMask);

    assert.deepEqual(outSt, outCan);
    assert.deepEqual(outLd, outCan);

    // Offline SVG rendering check (includes Ladder rungs & VIRTUAL marker if blue)
    const svg = renderLadderSvg(ruleSetToLadderModel(ldRes.ruleSet));
    assert.ok(svg.startsWith("<svg "));
    assert.ok(svg.includes("TON"));
    assert.ok(svg.includes("tower.red"));
  });

  it("I02: rejects direct %I/%Q addresses, WHILE loops, unknown FBs, XXE XML, and duplicate coils", () => {
    // 1. Direct %QX0.0 rejected with line:col
    assert.throws(
      () =>
        importStructuredText(`
PROGRAM BadIO
%QX0.0 := TRUE;
END_PROGRAM`),
      (e: unknown) => e instanceof StDiagnosticError && e.line === 3
    );

    // 2. Unbounded WHILE loop rejected
    assert.throws(
      () =>
        importStructuredText(`
PROGRAM BadLoop
WHILE TRUE DO
END_PROGRAM`),
      (e: unknown) => e instanceof StDiagnosticError && /WHILE/i.test(e.message)
    );

    // 3. External/unknown call rejected
    assert.throws(
      () =>
        importStructuredText(`
PROGRAM BadExt
SYS_EXEC(1);
END_PROGRAM`),
      (e: unknown) => e instanceof StDiagnosticError && /SYS_EXEC/.test(e.message)
    );

    // 4. Duplicate coil write in ST rejected
    assert.throws(
      () =>
        importStructuredText(`
PROGRAM DupSt
VAR t1 : TON; t2 : TON; END_VAR
t1(IN := temp_process >= 450, PT := T#5s);
tower_red := t1.Q;
t2(IN := temp_process >= 460, PT := T#2s);
tower_red := t2.Q;
END_PROGRAM`),
      (e: unknown) => e instanceof StDiagnosticError && /Duplicate coil/i.test(e.message)
    );

    // 5. XXE <!DOCTYPE> <!ENTITY> rejected cleanly in PLCopen XML
    assert.throws(
      () =>
        importPlcOpenLdXml(`<?xml version="1.0"?>
<!DOCTYPE foo [<!ENTITY xxe SYSTEM "file:///etc/passwd">]>
<project><pou name="X"><body><LD></LD></body></pou></project>`),
      (e: unknown) => e instanceof PlcOpenImportError && /XXE/i.test(e.message)
    );

    // 6. Duplicate coil write in PLCopen XML rejected
    assert.throws(
      () =>
        importPlcOpenLdXml(`
<project><pou name="Dup"><body><LD>
  <coil localId="1"><variable>tower.red</variable></coil>
  <coil localId="2"><variable>tower.red</variable></coil>
</LD></body></pou></project>`),
      (e: unknown) => e instanceof PlcOpenImportError && /Duplicate coil/i.test(e.message)
    );
  });
});
