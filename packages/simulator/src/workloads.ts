import * as fs from "node:fs";
import * as path from "node:path";
import {
  profileSigningPayload,
  signBytesEd25519,
  type RuleOp,
  type RuleSetV1,
  type SiteProfileV1,
} from "@plc-auto/control-ir";
import { buildSignedModulePackage } from "@plc-auto/toolchain";

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

export function makeM2RuleSet(
  holdMs = 5_000,
  timingPolicy: "PRESERVE_ELAPSED" | "RESET" = "PRESERVE_ELAPSED"
): RuleSetV1 {
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
        timingStateOnChange: timingPolicy,
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

/**
 * W3 benchmark workload: 16 independent timers across virt.0..virt.15 & 32-signal profile capacity (§7.2.4).
 */
export function makeW3HeavyRuleSet(): RuleSetV1 {
  const changes: RuleOp[] = [];
  for (let i = 0; i < 16; i++) {
    changes.push({
      op: "upsertRule",
      ruleId: `w3.timer-${i}`,
      templateId: i % 4 === 0 ? "pulseTimer" : i % 4 === 1 ? "offDelay" : "thresholdHoldWithLatchedHysteresis",
      input: i === 0 ? "temp.process" : `sig.${i}`,
      unit: "degC_x10",
      triggerAtOrAbove: 440 + i,
      clearAtOrBelow: 430 + i,
      holdMs: 1000 + i * 200,
      requiredQuality: "GOOD",
      onInputFault: "COMMAND_OFF_AND_RESET",
      output: `virt.${i}`,
      timingStateOnChange: "PRESERVE_ELAPSED",
      stableBlockId: 3000 + i,
      blockType: i % 4 === 0 ? "TP" : i % 4 === 1 ? "TOF" : "TON",
    });
  }
  return {
    schema: "rule-set/v1",
    baseRevision: "1",
    changes,
  };
}

export function makeW3BenchProfile(base: SiteProfileV1, seedHex: string): SiteProfileV1 {
  const channels = [...base.channels];
  for (let i = 0; i < 16; i++) {
    channels.push({
      name: `virt.${i}`,
      maskBit: 3 + i,
      physical: false,
      gpioChip: null,
      gpioOffset: null,
      activeLow: false,
      dwellMs: 500,
      maxTransitionsPerWindow: 20,
      transitionWindowMs: 10_000,
    });
  }
  const sensors = [...base.sensors];
  for (let i = 1; i < 32; i++) {
    sensors.push({
      name: `sig.${i}`,
      signalId: 100 + i,
      unit: "degC_x10",
      minValid: -400,
      maxValid: 1500,
      abnormalSentinel: 32767,
      pollIntervalMs: 100,
      freshnessTtlMs: 1500,
      devicePath: "virtual",
      slaveAddr: 1,
      baudRate: 9600,
    });
  }
  const unsigned: SiteProfileV1 = {
    ...base,
    profileId: "demo-lyra-w3-bench",
    channels,
    sensors,
    signatureHex: null,
  };
  const payload = profileSigningPayload(unsigned);
  return {
    ...unsigned,
    signatureHex: signBytesEd25519(seedHex, payload),
  };
}

/** W4: Hot-swap migration from 10s TON to 5s PRESERVE_ELAPSED + prealarm pulse. */
export function makeW4MigrationRuleSet(): RuleSetV1 {
  return makeM2RuleSet(5_000, "PRESERVE_ELAPSED");
}

/** W5: Closed-loop PID threshold + counter workload for benchmark parity. */
export function makeW5PidRuleSet(): RuleSetV1 {
  return {
    schema: "rule-set/v1",
    baseRevision: "1",
    changes: [
      {
        op: "upsertRule",
        ruleId: "w5.pid-heat-permit",
        templateId: "thresholdHoldWithLatchedHysteresis",
        input: "temp.process",
        unit: "degC_x10",
        triggerAtOrAbove: 450,
        clearAtOrBelow: 440,
        holdMs: 2500,
        requiredQuality: "GOOD",
        onInputFault: "COMMAND_OFF_AND_RESET",
        output: "tower.red",
        timingStateOnChange: "PRESERVE_ELAPSED",
        stableBlockId: 5001,
        blockType: "TON",
      },
      {
        op: "upsertRule",
        ruleId: "w5.pid-cycle-count",
        templateId: "counterTrigger",
        input: "temp.process",
        unit: "degC_x10",
        triggerAtOrAbove: 420,
        holdMs: 2,
        requiredQuality: "GOOD",
        onInputFault: "COMMAND_OFF_AND_RESET",
        output: "tower.green",
        timingStateOnChange: "PRESERVE_ELAPSED",
        stableBlockId: 5002,
        blockType: "CTU",
      },
    ],
  };
}

export function emitW3ToW5Packages(demoDir: string): void {
  const profile = JSON.parse(
    fs.readFileSync(path.join(demoDir, "site-profile.json"), "utf8")
  ) as SiteProfileV1;
  const pubHex = fs.readFileSync(path.join(demoDir, "keys/test-bench.pub.hex"), "utf8").trim();
  const seedHex = fs.readFileSync(path.join(demoDir, "keys/test-bench.seed.hex"), "utf8").trim();
  const outDir = path.join(demoDir, "packages");
  fs.mkdirSync(outDir, { recursive: true });

  const w3Profile = makeW3BenchProfile(profile, seedHex);
  fs.writeFileSync(path.join(demoDir, "bench-w3-profile.json"), JSON.stringify(w3Profile, null, 2));

  const targets = ["darwin-arm64", "armv7-linux-gnueabihf", "aarch64-linux-gnu"] as const;
  const sets = [
    ["W3", makeW3HeavyRuleSet(), w3Profile],
    ["W4", makeW4MigrationRuleSet(), profile],
    ["W5", makeW5PidRuleSet(), profile],
  ] as const;

  for (const [wid, rs, prof] of sets) {
    for (const target of targets) {
      const pkg = buildSignedModulePackage({
        packageId: wid,
        baseRevision: "1",
        target,
        engine: "wamr-aot",
        profile: prof,
        ruleSet: rs,
        signingSeedHex: seedHex,
        publicKeyHex: pubHex,
        compileWasm: true,
        compileAot: true,
      });
      fs.writeFileSync(path.join(outDir, `${wid}.${target}.json`), JSON.stringify(pkg, null, 2));
    }
    const qpkg = buildSignedModulePackage({
      packageId: wid,
      baseRevision: "1",
      target: "armv7-linux-gnueabihf",
      engine: "quickjs",
      profile: prof,
      ruleSet: rs,
      signingSeedHex: seedHex,
      publicKeyHex: pubHex,
      compileWasm: false,
      compileAot: false,
    });
    fs.writeFileSync(path.join(outDir, `${wid}.quickjs.json`), JSON.stringify(qpkg, null, 2));
  }
}
