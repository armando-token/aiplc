import { describe, it } from "node:test";
import * as assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

import { validateRuleSetAgainstProfile, type SiteProfileV1 } from "@plc-auto/control-ir";
import { parseSpanishIntent, type IntentOutcome } from "./index.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const corpusPath = path.resolve(__dirname, "../corpus/corpus_120.json");
const profilePath = path.resolve(__dirname, "../../../profiles/demo-lyra/site-profile.json");

interface CorpusCase {
  id: number;
  split: "dev" | "eval";
  noisyAsr: boolean;
  text: string;
  expected: IntentOutcome;
  intentKind?: string;
}

describe("P2/Phase 2 Spanish Intent Engine (§6.4 / V01)", () => {
  it("achieves 0 false accepts and 0 false activations across all 120 corpus cases (dev + eval + 40+ ASR)", () => {
    const cases = JSON.parse(fs.readFileSync(corpusPath, "utf8")) as CorpusCase[];
    const siteProfile = JSON.parse(fs.readFileSync(profilePath, "utf8")) as SiteProfileV1;
    assert.equal(cases.length, 120, "corpus must contain exactly 120 cases");

    const uniqueTexts = new Set(cases.map((c) => c.text.trim().toLowerCase()));
    assert.equal(uniqueTexts.size, 120, "all 120 corpus inputs must be distinct");

    const noisyCount = cases.filter((c) => c.noisyAsr).length;
    assert.ok(noisyCount >= 40, `expected >= 40 noisy ASR variants, got ${noisyCount}`);

    let falseAccepts = 0;
    const mismatches: string[] = [];

    for (const c of cases) {
      const res = parseSpanishIntent(c.text, { baseRevision: "1", profile: siteProfile });
      if (c.expected !== "ACCEPT" && res.outcome === "ACCEPT") {
        falseAccepts++;
        mismatches.push(`FALSE ACCEPT #${c.id} "${c.text}" -> ${res.intentKind}`);
      } else if (res.outcome !== c.expected) {
        mismatches.push(`#${c.id} expected=${c.expected} actual=${res.outcome} "${c.text}"`);
      } else if (c.intentKind && res.intentKind !== c.intentKind) {
        mismatches.push(
          `#${c.id} kind expected=${c.intentKind} actual=${res.intentKind} "${c.text}"`
        );
      }
      if (res.outcome === "ACCEPT") {
        assert.equal(res.requiresConfirmation, true, "never auto-commit");
        assert.ok(res.ruleSet && res.ruleSet.changes.length >= 1);
        validateRuleSetAgainstProfile(res.ruleSet, siteProfile);
      }
    }

    assert.equal(falseAccepts, 0, `false accepts encountered:\n${mismatches.join("\n")}`);
    assert.deepEqual(mismatches, [], `mismatches:\n${mismatches.join("\n")}`);
  });

  it("flags virtual channel warning on tower.blue and rejects grados fahrenheit", () => {
    const res = parseSpanishIntent(
      "Si la temperatura esta entre 40 C y 45 C haz parpadear la luz azul cada 2 segundos"
    );
    assert.equal(res.outcome, "ACCEPT");
    assert.equal(res.virtualChannelWarning, true);
    assert.equal(res.requiresConfirmation, true);

    const badUnit = parseSpanishIntent("Enciende la luz roja si supera 100 grados fahrenheit");
    assert.equal(badUnit.outcome, "REJECT");
  });
});
