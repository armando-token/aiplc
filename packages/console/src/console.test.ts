import { describe, it } from "node:test";
import * as assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

import { PlcConsoleClient, writeOfflineWebBundle } from "./index.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const distWebDir = path.resolve(__dirname, "../dist-web");

describe("P3 Offline Console Web UI & Client Recovery (D02 / D04)", () => {
  it("generates zero-CDN offline dist-web bundle with virtual badges & 4 evidence badges", () => {
    const written = writeOfflineWebBundle(distWebDir);
    assert.equal(written.length, 3);

    const html = fs.readFileSync(path.join(distWebDir, "index.html"), "utf8");
    const css = fs.readFileSync(path.join(distWebDir, "styles.css"), "utf8");
    const js = fs.readFileSync(path.join(distWebDir, "app.js"), "utf8");

    // Verify 0 external CDN URLs anywhere (exclude XML namespace URI inside SVG generator)
    const cleanJs = js.replace(/http:\/\/www\.w3\.org\/2000\/svg/g, "");
    for (const content of [html, css, cleanJs]) {
      assert.equal(/https?:\/\//i.test(content), false, "external CDN reference forbidden");
    }

    assert.ok(html.includes("SELECTED_IN_SCAN"));
    assert.ok(html.includes("DRIVER_APPLIED"));
    assert.ok(html.includes("PHYSICAL_OBSERVED"));
    assert.ok(html.includes("ACTIVE_PERSISTED"));
    assert.ok(html.includes("VIRTUAL"));
  });

  it("reconciles pending txId after network drop without re-creating new txId (D02)", async () => {
    let calls = 0;
    const mockFetch: typeof fetch = async (input) => {
      calls++;
      const url = String(input);
      if (url.includes("/v1/proposals/tx-lost-1")) {
        return new Response(
          JSON.stringify({
            transaction: { revision: { state: "ACTIVE_VOLATILE" } },
            activeRevision: "2",
          }),
          { status: 200 }
        );
      }
      return new Response("{}", { status: 404 });
    };

    const client = new PlcConsoleClient("http://127.0.0.1:8443", "plc-operator-token", mockFetch);
    const res = await client.reconcilePendingTx("lyra-bench-01", {
      txId: "tx-lost-1",
      action: "ACTIVATE",
      createdAtMs: 1000,
    });
    assert.equal(calls, 1);
    assert.equal(res.phase, "SETTLED");
    assert.ok(res.detail.includes("ACTIVE_VOLATILE"));
  });
});
