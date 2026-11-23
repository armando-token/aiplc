import * as crypto from "node:crypto";
import { ModulePackageV1, SiteProfileV1 } from "./types.js";

function sortJsonValue(v: unknown): unknown {
  if (Array.isArray(v)) {
    return v.map(sortJsonValue);
  }
  if (v !== null && typeof v === "object") {
    const obj = v as Record<string, unknown>;
    const sorted: Record<string, unknown> = {};
    for (const k of Object.keys(obj).sort()) {
      sorted[k] = sortJsonValue(obj[k]);
    }
    return sorted;
  }
  return v;
}

export function canonicalJsonBytes(value: unknown): Buffer {
  return Buffer.from(JSON.stringify(sortJsonValue(value)), "utf8");
}

export function sha256Hex(bytes: Uint8Array | string): string {
  return crypto.createHash("sha256").update(bytes).digest("hex");
}

function rawSeedToPrivateKeyObject(seed32: Uint8Array): crypto.KeyObject {
  // PKCS#8 DER prefix for raw Ed25519 32-byte seed: 302e020100300506032b657004220420
  const prefix = Buffer.from("302e020100300506032b657004220420", "hex");
  if (seed32.length !== 32) {
    throw new Error("Ed25519 private seed must be 32 bytes");
  }
  return crypto.createPrivateKey({
    key: Buffer.concat([prefix, Buffer.from(seed32)]),
    format: "der",
    type: "pkcs8",
  });
}

function rawPubToPublicKeyObject(pub32: Uint8Array): crypto.KeyObject {
  // SPKI DER prefix for raw Ed25519 32-byte public key: 302a300506032b6570032100
  const prefix = Buffer.from("302a300506032b6570032100", "hex");
  if (pub32.length !== 32) {
    throw new Error("Ed25519 public key must be 32 bytes");
  }
  return crypto.createPublicKey({
    key: Buffer.concat([prefix, Buffer.from(pub32)]),
    format: "der",
    type: "spki",
  });
}

const HEX64_RE = /^[0-9a-fA-F]{64}$/;
const HEX128_RE = /^[0-9a-fA-F]{128}$/;

export function signBytesEd25519(seedHex: string, message: Uint8Array): string {
  if (!HEX64_RE.test(seedHex)) {
    throw new Error("Ed25519 private seed hex must be exactly 64 hex chars");
  }
  const keyObj = rawSeedToPrivateKeyObject(Buffer.from(seedHex, "hex"));
  return crypto.sign(null, Buffer.from(message), keyObj).toString("hex");
}

export function verifyBytesEd25519(
  pubHex: string,
  message: Uint8Array,
  sigHex: string
): boolean {
  if (!HEX64_RE.test(pubHex) || !HEX128_RE.test(sigHex)) {
    return false;
  }
  const keyObj = rawPubToPublicKeyObject(Buffer.from(pubHex, "hex"));
  return crypto.verify(null, Buffer.from(message), keyObj, Buffer.from(sigHex, "hex"));
}

export function profileSigningPayload(profile: SiteProfileV1): Buffer {
  const copy: SiteProfileV1 = {
    ...profile,
    signatureHex: null,
    publicKeyHex: null,
  };
  return canonicalJsonBytes(copy);
}

export function profileHash(profile: SiteProfileV1): string {
  return sha256Hex(profileSigningPayload(profile));
}

export function verifySiteProfileSignature(
  profile: SiteProfileV1,
  trustedPubHex: string
): boolean {
  if (!profile.signatureHex) return false;
  if (
    profile.publicKeyHex &&
    profile.publicKeyHex.toLowerCase() !== trustedPubHex.toLowerCase()
  ) {
    return false;
  }
  return verifyBytesEd25519(
    trustedPubHex,
    profileSigningPayload(profile),
    profile.signatureHex
  );
}

export function modulePackageSigningPayload(pkg: ModulePackageV1): Buffer {
  const copy: ModulePackageV1 = {
    ...pkg,
    signatureHex: null,
    publicKeyHex: null,
  };
  return canonicalJsonBytes(copy);
}

export function verifyModulePackageSignature(
  pkg: ModulePackageV1,
  trustedPubHex: string
): boolean {
  if (!pkg.signatureHex) return false;
  if (
    pkg.publicKeyHex &&
    pkg.publicKeyHex.toLowerCase() !== trustedPubHex.toLowerCase()
  ) {
    return false;
  }
  return verifyBytesEd25519(
    trustedPubHex,
    modulePackageSigningPayload(pkg),
    pkg.signatureHex
  );
}
