import * as fs from "node:fs";
import * as path from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

import {
  ABI_VERSION,
  BLOCK_TYPE_TO_NAME,
  MigrationPlanEntry,
  ModulePackageV1,
  RuleSetV1,
  SiteProfileV1,
  canonicalJsonBytes,
  modulePackageSigningPayload,
  profileHash,
  sha256Hex,
  signBytesEd25519,
  validateRuleSetAgainstProfile,
} from "@plc-auto/control-ir";
import {
  blockTypeForRule,
  generateAssemblyScriptForRuleSet,
  stableIdForRule,
} from "./codegen_as.js";
import { generateQuickJsForRuleSet } from "./codegen_js.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "../../..");

export interface BuildPackageOptions {
  packageId: string;
  baseRevision: string;
  target: "armv7-linux-gnueabihf" | "aarch64-linux-gnu" | "darwin-arm64";
  engine: "native-ir" | "wamr-interp" | "wamr-aot" | "quickjs" | "wasmtime";
  profile: SiteProfileV1;
  ruleSet: RuleSetV1;
  signingSeedHex: string;
  publicKeyHex: string;
  compileWasm?: boolean;
  compileAot?: boolean;
  workDir?: string;
}

export function compileAssemblyScriptToWasm(asSource: string, workDir: string): Buffer {
  fs.mkdirSync(workDir, { recursive: true });
  const asPath = path.join(workDir, "module.ts");
  const wasmPath = path.join(workDir, "module.wasm");
  fs.writeFileSync(asPath, asSource, "utf8");

  const ascBin = path.join(repoRoot, "node_modules/.bin/asc");
  execFileSync(
    ascBin,
    [
      asPath,
      "--outFile",
      wasmPath,
      "--runtime",
      "stub",
      "--optimize",
      "--initialMemory",
      "2",
      "--maximumMemory",
      "2",
      "--noAssert",
    ],
    { stdio: "pipe" }
  );
  return fs.readFileSync(wasmPath);
}

export function compileWasmToAot(
  wasmBytes: Buffer,
  target: BuildPackageOptions["target"],
  workDir: string
): Buffer {
  fs.mkdirSync(workDir, { recursive: true });
  const wasmPath = path.join(workDir, "module.wasm");
  const aotPath = path.join(workDir, `module.${target}.aot`);
  fs.writeFileSync(wasmPath, wasmBytes);

  const wamrcBin = path.join(repoRoot, "third_party/wamrc-2.4.6/wamrc");
  const args = ["-o", aotPath];
  if (target === "armv7-linux-gnueabihf") {
    args.push(
      "--target=armv7",
      "--cpu=cortex-a7",
      "--cpu-features=+neon,+vfpv4",
      "--target-abi=gnueabihf"
    );
  } else if (target === "aarch64-linux-gnu" || target === "darwin-arm64") {
    args.push("--target=aarch64");
  }
  args.push(wasmPath);

  execFileSync(wamrcBin, args, { stdio: "pipe" });
  return fs.readFileSync(aotPath);
}

export function buildSignedModulePackage(opts: BuildPackageOptions): ModulePackageV1 {
  const validated = validateRuleSetAgainstProfile(opts.ruleSet, opts.profile);
  const pHash = profileHash(opts.profile);
  const irDigest = sha256Hex(canonicalJsonBytes(validated));

  const requiredBlocks: MigrationPlanEntry[] = validated.changes
    .filter((r) => r.op !== "removeRule")
    .map((r) => ({
      stableId: stableIdForRule(r),
      blockType: BLOCK_TYPE_TO_NAME[blockTypeForRule(r)],
      schemaVersion: 1,
      stateSize: 40,
      retentionClass: "CYCLIC",
      migrationPolicy: r.timingStateOnChange,
    }));

  const migrationPlanHash = sha256Hex(canonicalJsonBytes(requiredBlocks));
  const policyHash = sha256Hex(
    canonicalJsonBytes({
      interlocks: opts.profile.interlocks,
      allowedPhysicalMask: opts.profile.allowedPhysicalMask,
      safeFallbackMask: opts.profile.safeFallbackMask,
    })
  );

  const jsSource = generateQuickJsForRuleSet(validated);
  const asSource = generateAssemblyScriptForRuleSet(validated);

  const needWasm =
    opts.compileWasm ||
    opts.compileAot ||
    opts.engine === "wamr-interp" ||
    opts.engine === "wamr-aot" ||
    opts.engine === "wasmtime";

  let wasmBase64: string | null = null;
  let wasmDigest: string | null = null;
  let aotBase64: string | null = null;
  let aotDigest: string | null = null;

  const tmpDir =
    opts.workDir ??
    fs.mkdtempSync(path.join(repoRoot, "tmp-build-"));
  try {
    if (needWasm) {
      const wasmBytes = compileAssemblyScriptToWasm(asSource, tmpDir);
      wasmDigest = sha256Hex(wasmBytes);
      wasmBase64 = wasmBytes.toString("base64");

      if (opts.compileAot || opts.engine === "wamr-aot") {
        const aotBytes = compileWasmToAot(wasmBytes, opts.target, tmpDir);
        aotDigest = sha256Hex(aotBytes);
        aotBase64 = aotBytes.toString("base64");
      }
    }
  } finally {
    if (!opts.workDir) {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  }

  const canonicalSourceDigest =
    opts.engine === "quickjs"
      ? sha256Hex(jsSource)
      : sha256Hex(asSource);

  const pkg: ModulePackageV1 = {
    schema: "module-package/v1",
    packageId: opts.packageId,
    baseRevision: opts.baseRevision,
    target: opts.target,
    engine: opts.engine,
    abiVersion: ABI_VERSION,
    stateSchema: "arena/v1",
    profileHash: pHash,
    irDigest,
    canonicalSourceDigest,
    wasmDigest,
    aotDigest,
    migrationPlanHash,
    policyHash,
    maxStateBytes: 1280,
    maxLinearMemBytes: 131072,
    requestedCapabilities: [],
    requiredBlocks,
    ruleSet: validated,
    wasmBase64,
    aotBase64,
    jsSource,
    signatureHex: null,
    publicKeyHex: opts.publicKeyHex,
  };

  const payload = modulePackageSigningPayload(pkg);
  pkg.signatureHex = signBytesEd25519(opts.signingSeedHex, payload);
  return pkg;
}
