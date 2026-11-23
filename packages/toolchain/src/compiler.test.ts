import { describe, it } from "node:test";
import * as assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

import {
  type RuleSetV1,
  type SiteProfileV1,
  verifyModulePackageSignature,
} from "@plc-auto/control-ir";
import { buildSignedModulePackage } from "./index.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "../../..");
const demoDir = path.join(repoRoot, "profiles/demo-lyra");

export function makeM1RuleSet(holdMs = 10_000): RuleSetV1 {
  return {
    schema: "rule-set/v1",
    baseRevision: "0",
    changes: [
      {
        op: "upsertRule",
        ruleId: "temp.high-alarm",
        templateId: "thresholdHoldWithLatchedHysteresis",
        input: "temp.process",
        unit: "degC_x10",
        triggerAtOrAbove: 450,
        clearAtOrBelow: 440,
        holdMs,
        requiredQuality: "GOOD",
        onInputFault: "COMMAND_OFF_AND_RESET",
        output: "tower.red",
        timingStateOnChange: "RESET",
        stableBlockId: 1001,
        blockType: "TON",
      },
    ],
  };
}

export function makeM2RuleSet(holdMs = 5_000): RuleSetV1 {
  return {
    schema: "rule-set/v1",
    baseRevision: "1",
    changes: [
      {
        op: "upsertRule",
        ruleId: "temp.high-alarm",
        templateId: "thresholdHoldWithLatchedHysteresis",
        input: "temp.process",
        unit: "degC_x10",
        triggerAtOrAbove: 450,
        clearAtOrBelow: 440,
        holdMs,
        requiredQuality: "GOOD",
        onInputFault: "COMMAND_OFF_AND_RESET",
        output: "tower.red",
        timingStateOnChange: "PRESERVE_ELAPSED",
        stableBlockId: 1001,
        blockType: "TON",
      },
      {
        op: "upsertRule",
        ruleId: "temp.prealarm-pulse",
        templateId: "bandPrealarmPulse",
        input: "temp.process",
        unit: "degC_x10",
        bandLow: 400,
        bandHigh: 450,
        pulsePeriodMs: 2000,
        requiredQuality: "GOOD",
        onInputFault: "COMMAND_OFF_AND_RESET",
        output: "tower.blue",
        timingStateOnChange: "RESET",
        stableBlockId: 1002,
        blockType: "PULSE_GEN",
      },
    ],
  };
}

describe("P2/Phase1 Toolchain: AssemblyScript -> Wasm -> WAMR AOT & QuickJS-NG", () => {
  it("builds signed multi-target packages M1 & M2 for darwin-arm64, armv7, and aarch64", () => {
    const profile = JSON.parse(
      fs.readFileSync(path.join(demoDir, "site-profile.json"), "utf8")
    ) as SiteProfileV1;
    const pubHex = fs.readFileSync(path.join(demoDir, "keys/test-bench.pub.hex"), "utf8").trim();
    const seedHex = fs.readFileSync(path.join(demoDir, "keys/test-bench.seed.hex"), "utf8").trim();

    const outDir = path.join(demoDir, "packages");
    fs.mkdirSync(outDir, { recursive: true });

    const targets: Array<"darwin-arm64" | "armv7-linux-gnueabihf" | "aarch64-linux-gnu"> = [
      "darwin-arm64",
      "armv7-linux-gnueabihf",
      "aarch64-linux-gnu",
    ];

    for (const target of targets) {
      const m1 = buildSignedModulePackage({
        packageId: "M1",
        baseRevision: "0",
        target,
        engine: "wamr-aot",
        profile,
        ruleSet: makeM1RuleSet(10_000),
        signingSeedHex: seedHex,
        publicKeyHex: pubHex,
        compileWasm: true,
        compileAot: true,
      });
      assert.ok(m1.wasmBase64 && m1.wasmDigest);
      assert.ok(m1.aotBase64 && m1.aotDigest);
      assert.ok(m1.jsSource);
      assert.equal(verifyModulePackageSignature(m1, pubHex), true);
      fs.writeFileSync(path.join(outDir, `M1.${target}.json`), JSON.stringify(m1, null, 2));

      const m2 = buildSignedModulePackage({
        packageId: "M2",
        baseRevision: "1",
        target,
        engine: "wamr-aot",
        profile,
        ruleSet: makeM2RuleSet(5_000),
        signingSeedHex: seedHex,
        publicKeyHex: pubHex,
        compileWasm: true,
        compileAot: true,
      });
      assert.ok(m2.wasmBase64 && m2.wasmDigest);
      assert.ok(m2.aotBase64 && m2.aotDigest);
      assert.equal(verifyModulePackageSignature(m2, pubHex), true);
      fs.writeFileSync(path.join(outDir, `M2.${target}.json`), JSON.stringify(m2, null, 2));
    }

    // Also emit QuickJS-bound packages (canonicalSourceDigest = sha256(jsSource))
    for (const [id, rs] of [
      ["M1", makeM1RuleSet(10_000)],
      ["M2", makeM2RuleSet(5_000)],
    ] as const) {
      const qpkg = buildSignedModulePackage({
        packageId: id,
        baseRevision: rs.baseRevision,
        target: "armv7-linux-gnueabihf",
        engine: "quickjs",
        profile,
        ruleSet: rs,
        signingSeedHex: seedHex,
        publicKeyHex: pubHex,
        compileWasm: false,
        compileAot: false,
      });
      assert.equal(verifyModulePackageSignature(qpkg, pubHex), true);
      fs.writeFileSync(path.join(outDir, `${id}.quickjs.json`), JSON.stringify(qpkg, null, 2));
    }

    // Emit W0..W2 benchmark workloads using demo-lyra profile
    const w0: RuleSetV1 = {
      schema: "rule-set/v1",
      baseRevision: "1",
      changes: [
        {
          op: "upsertRule",
          ruleId: "w0.copy",
          templateId: "copyBoolean",
          input: "temp.process",
          unit: "degC_x10",
          requiredQuality: "GOOD",
          onInputFault: "COMMAND_OFF_AND_RESET",
          output: "tower.green",
          timingStateOnChange: "RESET",
          stableBlockId: 2000,
          blockType: "THRESHOLD_HYST",
        },
      ],
    };
    const w1 = makeM1RuleSet(10_000);
    const w2: RuleSetV1 = {
      schema: "rule-set/v1",
      baseRevision: "1",
      changes: [
        ...w1.changes,
        {
          op: "upsertRule",
          ruleId: "w2.counter",
          templateId: "counterTrigger",
          input: "temp.process",
          unit: "degC_x10",
          triggerAtOrAbove: 450,
          holdMs: 3,
          requiredQuality: "GOOD",
          onInputFault: "COMMAND_OFF_AND_RESET",
          output: "tower.green",
          timingStateOnChange: "PRESERVE_ELAPSED",
          stableBlockId: 2002,
          blockType: "CTU",
        },
      ],
    };

    for (const [wid, wrs] of [
      ["W0", w0],
      ["W1", w1],
      ["W2", w2],
    ] as const) {
      for (const target of targets) {
        const wpkg = buildSignedModulePackage({
          packageId: wid,
          baseRevision: "1",
          target,
          engine: "wamr-aot",
          profile,
          ruleSet: wrs,
          signingSeedHex: seedHex,
          publicKeyHex: pubHex,
          compileWasm: true,
          compileAot: true,
        });
        fs.writeFileSync(path.join(outDir, `${wid}.${target}.json`), JSON.stringify(wpkg, null, 2));
      }
      const wqjs = buildSignedModulePackage({
        packageId: wid,
        baseRevision: "1",
        target: "armv7-linux-gnueabihf",
        engine: "quickjs",
        profile,
        ruleSet: wrs,
        signingSeedHex: seedHex,
        publicKeyHex: pubHex,
        compileWasm: false,
        compileAot: false,
      });
      fs.writeFileSync(path.join(outDir, `${wid}.quickjs.json`), JSON.stringify(wqjs, null, 2));
    }
  });
});
