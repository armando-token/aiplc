use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::schema::{ModulePackageV1, SiteProfileV1};
use crate::types::AbiError;

/// Produces deterministic canonical JSON bytes by recursively sorting object keys.
pub fn canonical_json_bytes(value: &Value) -> Result<Vec<u8>, AbiError> {
    fn sort_value(v: &Value) -> Value {
        match v {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut sorted = serde_json::Map::new();
                for k in keys {
                    if let Some(child) = map.get(k) {
                        sorted.insert(k.clone(), sort_value(child));
                    }
                }
                Value::Object(sorted)
            }
            Value::Array(arr) => Value::Array(arr.iter().map(sort_value).collect()),
            other => other.clone(),
        }
    }
    let normalized = sort_value(value);
    serde_json::to_vec(&normalized).map_err(|e| AbiError::InvalidValue(e.to_string()))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

pub fn hex_decode(s: &str) -> Result<Vec<u8>, AbiError> {
    if s.len() % 2 != 0 {
        return Err(AbiError::InvalidValue("odd hex length".into()));
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let hi = from_hex_nibble(chunk[0])?;
        let lo = from_hex_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn from_hex_nibble(b: u8) -> Result<u8, AbiError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(AbiError::InvalidValue("invalid hex digit".into())),
    }
}

pub fn sign_bytes_ed25519(secret_key_bytes: &[u8; 32], msg: &[u8]) -> String {
    let sk = SigningKey::from_bytes(secret_key_bytes);
    let sig: Signature = sk.sign(msg);
    hex_encode(&sig.to_bytes())
}

pub fn verify_bytes_ed25519(
    public_key_bytes: &[u8; 32],
    msg: &[u8],
    sig_hex: &str,
) -> Result<(), AbiError> {
    let vk = VerifyingKey::from_bytes(public_key_bytes)
        .map_err(|e| AbiError::InvalidValue(format!("bad public key: {e}")))?;
    let sig_raw = hex_decode(sig_hex)?;
    let sig_arr: [u8; 64] = sig_raw
        .try_into()
        .map_err(|_| AbiError::InvalidValue("signature must be 64 bytes".into()))?;
    let sig = Signature::from_bytes(&sig_arr);
    vk.verify_strict(msg, &sig)
        .map_err(|e| AbiError::InvalidValue(format!("signature verify failed: {e}")))
}

pub fn profile_signing_payload(profile: &SiteProfileV1) -> Result<Vec<u8>, AbiError> {
    let mut unsigned = profile.clone();
    unsigned.signature_hex = None;
    unsigned.public_key_hex = None;
    let val = serde_json::to_value(&unsigned).map_err(|e| AbiError::InvalidValue(e.to_string()))?;
    canonical_json_bytes(&val)
}

pub fn profile_hash(profile: &SiteProfileV1) -> Result<String, AbiError> {
    Ok(sha256_hex(&profile_signing_payload(profile)?))
}

pub fn verify_site_profile_signature(
    profile: &SiteProfileV1,
    trusted_pub_hex: &str,
) -> Result<(), AbiError> {
    let sig_hex = profile
        .signature_hex
        .as_deref()
        .ok_or_else(|| AbiError::InvalidValue("missing profile signatureHex".into()))?;
    let pub_raw = hex_decode(trusted_pub_hex)?;
    let pub_arr: [u8; 32] = pub_raw
        .try_into()
        .map_err(|_| AbiError::InvalidValue("trusted public key must be 32 bytes".into()))?;
    if let Some(embedded) = profile.public_key_hex.as_deref() {
        if !embedded.eq_ignore_ascii_case(trusted_pub_hex) {
            return Err(AbiError::InvalidValue(
                "profile publicKeyHex does not match trusted key".into(),
            ));
        }
    }
    let payload = profile_signing_payload(profile)?;
    verify_bytes_ed25519(&pub_arr, &payload, sig_hex)
}

pub fn module_package_signing_payload(pkg: &ModulePackageV1) -> Result<Vec<u8>, AbiError> {
    let mut unsigned = pkg.clone();
    unsigned.signature_hex = None;
    unsigned.public_key_hex = None;
    let val = serde_json::to_value(&unsigned).map_err(|e| AbiError::InvalidValue(e.to_string()))?;
    canonical_json_bytes(&val)
}

pub fn verify_module_package_signature(
    pkg: &ModulePackageV1,
    trusted_pub_hex: &str,
) -> Result<(), AbiError> {
    let sig_hex = pkg
        .signature_hex
        .as_deref()
        .ok_or_else(|| AbiError::InvalidValue("missing package signatureHex".into()))?;
    let pub_raw = hex_decode(trusted_pub_hex)?;
    let pub_arr: [u8; 32] = pub_raw
        .try_into()
        .map_err(|_| AbiError::InvalidValue("trusted public key must be 32 bytes".into()))?;
    if let Some(embedded) = pkg.public_key_hex.as_deref() {
        if !embedded.eq_ignore_ascii_case(trusted_pub_hex) {
            return Err(AbiError::InvalidValue(
                "package publicKeyHex does not match trusted key".into(),
            ));
        }
    }
    let payload = module_package_signing_payload(pkg)?;
    verify_bytes_ed25519(&pub_arr, &payload, sig_hex)
}
