use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use control_abi::crypto::sha256_hex;
use control_abi::schema::{DeploymentRevision, DeploymentState, ModulePackageV1};
use crc32fast::Hasher;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("corrupt WAL record CRC at line {0}")]
    CorruptCrc(usize),
    #[error("package hash mismatch: expected {expected}, actual {actual}")]
    PackageHashMismatch { expected: String, actual: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalEnvelope {
    pub crc32: u32,
    pub record: DeploymentRevision,
}

#[derive(Debug, Clone)]
pub struct RecoveredState {
    pub boot_id: u64,
    pub armed_on_boot: bool,
    pub last_stable_package: Option<ModulePackageV1>,
    pub last_stable_revision: Option<DeploymentRevision>,
    pub highest_revision: u64,
    pub revisions: Vec<DeploymentRevision>,
    pub needs_operator_review: bool,
    pub recovery_note: String,
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    let f = File::open(dir)?;
    f.sync_all()
}

/// Atomic file write: write temp -> fsync(file) -> rename -> fsync(dir) (§5.5).
pub fn atomic_write_synced(target_path: &Path, data: &[u8]) -> Result<(), JournalError> {
    let parent = target_path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent"))?;
    fs::create_dir_all(parent)?;
    let tmp_path = parent.join(format!(
        ".tmp.{}.{}",
        target_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file"),
        std::process::id()
    ));
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp_path, target_path)?;
    sync_dir(parent)?;
    Ok(())
}

/// Optional deterministic crash-point hook for D03 power-cut / SIGKILL verification.
pub fn maybe_inject_crash(point_name: &str) {
    if let Ok(target) = std::env::var("PLC_CRASH_POINT") {
        if target == point_name {
            #[cfg(unix)]
            unsafe {
                libc::_exit(99);
            }
            #[cfg(not(unix))]
            std::process::abort();
        }
    }
}

pub struct DurableJournal {
    data_dir: PathBuf,
    packages_dir: PathBuf,
    wal_path: PathBuf,
}

impl DurableJournal {
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, JournalError> {
        let data_dir = data_dir.as_ref().to_path_buf();
        let packages_dir = data_dir.join("packages");
        let wal_path = data_dir.join("journal.wal");
        fs::create_dir_all(&packages_dir)?;
        if !wal_path.exists() {
            atomic_write_synced(&wal_path, b"")?;
        }
        Ok(Self {
            data_dir,
            packages_dir,
            wal_path,
        })
    }

    pub fn store_immutable_package(&self, pkg: &ModulePackageV1) -> Result<String, JournalError> {
        let bytes = serde_json::to_vec_pretty(pkg)?;
        let digest = sha256_hex(&bytes);
        let dst = self.packages_dir.join(format!("{digest}.json"));
        if !dst.exists() {
            atomic_write_synced(&dst, &bytes)?;
        }
        Ok(digest)
    }

    pub fn load_package(&self, package_hash: &str) -> Result<ModulePackageV1, JournalError> {
        let path = self.packages_dir.join(format!("{package_hash}.json"));
        let bytes = fs::read(&path)?;
        let actual = sha256_hex(&bytes);
        if actual != package_hash {
            return Err(JournalError::PackageHashMismatch {
                expected: package_hash.to_string(),
                actual,
            });
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn append_revision(&self, rev: &DeploymentRevision) -> Result<(), JournalError> {
        let payload = serde_json::to_vec(rev)?;
        let mut hasher = Hasher::new();
        hasher.update(&payload);
        let env = WalEnvelope {
            crc32: hasher.finalize(),
            record: rev.clone(),
        };
        let mut line = serde_json::to_vec(&env)?;
        line.push(b'\n');

        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.wal_path)?;
        f.write_all(&line)?;
        f.sync_all()?;
        sync_dir(&self.data_dir)?;
        Ok(())
    }

    /// Recovers state across reboots or crashes (§5.5):
    /// - Always starts disarmed (`armed_on_boot = false`)
    /// - Invalidates `Armed` transactions from previous boot -> `Superseded`
    /// - Marks inconclusive `ActiveVolatile` / `Recovering` -> `RecoveryRequired`
    /// - Loads last intact `Stable` package for recovery
    pub fn recover_on_boot(&self, new_boot_id: u64) -> Result<RecoveredState, JournalError> {
        let raw_bytes = fs::read(&self.wal_path)?;
        let mut records: Vec<DeploymentRevision> = Vec::new();
        let mut truncated_tail = false;
        let mut valid_byte_len = 0usize;
        let mut offset = 0usize;
        let mut line_idx = 0usize;

        while offset < raw_bytes.len() {
            let slice = &raw_bytes[offset..];
            let Some(nl_rel) = slice.iter().position(|&b| b == b'\n') else {
                // Incomplete trailing line without newline terminator -> truncate tail cleanly
                truncated_tail = true;
                break;
            };
            let line_end = offset + nl_rel + 1;
            let line_str = match std::str::from_utf8(&raw_bytes[offset..offset + nl_rel]) {
                Ok(s) => s,
                Err(_) => {
                    truncated_tail = true;
                    break;
                }
            };
            line_idx += 1;
            if line_str.trim().is_empty() {
                valid_byte_len = line_end;
                offset = line_end;
                continue;
            }
            let env: WalEnvelope = match serde_json::from_str(line_str) {
                Ok(v) => v,
                Err(_) => {
                    truncated_tail = true;
                    break;
                }
            };
            let payload = serde_json::to_vec(&env.record)?;
            let mut hasher = Hasher::new();
            hasher.update(&payload);
            if hasher.finalize() != env.crc32 {
                return Err(JournalError::CorruptCrc(line_idx));
            }
            records.push(env.record);
            valid_byte_len = line_end;
            offset = line_end;
        }

        if truncated_tail {
            let f = OpenOptions::new().write(true).open(&self.wal_path)?;
            f.set_len(valid_byte_len as u64)?;
            f.sync_all()?;
            sync_dir(&self.data_dir)?;
        }

        // Collapse latest state per tx_id in monotonic order
        let mut latest_by_tx: std::collections::BTreeMap<String, DeploymentRevision> =
            std::collections::BTreeMap::new();
        let mut highest_revision = 0u64;
        for rec in &records {
            highest_revision = highest_revision.max(rec.revision);
            latest_by_tx.insert(rec.tx_id.clone(), rec.clone());
        }

        let mut needs_review = truncated_tail;
        let mut notes = Vec::new();
        if truncated_tail {
            notes.push("truncated_wal_tail_ignored".to_string());
        }

        // Resolve incomplete transactions from prior boots
        for rec in latest_by_tx.values_mut() {
            if rec.boot_id != new_boot_id {
                match rec.state {
                    DeploymentState::Armed => {
                        rec.state = DeploymentState::Superseded;
                        rec.boot_id = new_boot_id;
                        self.append_revision(rec)?;
                        notes.push(format!("superseded_armed:{}", rec.tx_id));
                    }
                    DeploymentState::ActiveVolatile | DeploymentState::Recovering => {
                        rec.state = DeploymentState::RecoveryRequired;
                        rec.boot_id = new_boot_id;
                        self.append_revision(rec)?;
                        needs_review = true;
                        notes.push(format!("recovery_required:{}", rec.tx_id));
                    }
                    _ => {}
                }
            }
        }

        // Find last intact Stable (or Persisted if no Stable yet) in monotonic revision order
        let mut candidates: Vec<&DeploymentRevision> = latest_by_tx
            .values()
            .filter(|r| {
                r.state == DeploymentState::Stable
                    || r.state == DeploymentState::Persisted
                    || r.state == DeploymentState::RolledBack
            })
            .collect();
        candidates.sort_by_key(|r| r.revision);

        let mut last_stable_package = None;
        let mut last_stable_revision = None;
        for cand in candidates.into_iter().rev() {
            if let Ok(pkg) = self.load_package(&cand.package_hash) {
                last_stable_package = Some(pkg);
                last_stable_revision = Some(cand.clone());
                break;
            }
        }

        Ok(RecoveredState {
            boot_id: new_boot_id,
            armed_on_boot: false,
            last_stable_package,
            last_stable_revision,
            highest_revision,
            revisions: latest_by_tx.into_values().collect(),
            needs_operator_review: needs_review,
            recovery_note: notes.join(";"),
        })
    }
}
