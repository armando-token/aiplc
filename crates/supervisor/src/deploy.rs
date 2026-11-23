use std::collections::BTreeMap;

use control_abi::crypto::{
    canonical_json_bytes, profile_hash, sha256_hex, verify_module_package_signature,
};
use control_abi::schema::{DeploymentRevision, DeploymentState, ModulePackageV1, SiteProfileV1};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::arena::{PreparedMigration, StateArena};
use crate::journal::{maybe_inject_crash, DurableJournal, JournalError};

#[derive(Debug, Error)]
pub enum DeployError {
    #[error("journal error: {0}")]
    Journal(#[from] JournalError),
    #[error("signature or verification error: {0}")]
    Verification(String),
    #[error("conflict on baseRevision: expected {expected}, got {got}")]
    RevisionConflict { expected: u64, got: u64 },
    #[error("idempotency conflict for txId '{0}': body hash differs")]
    IdempotencyMismatch(String),
    #[error("invalid state transition from {from:?} for tx '{tx_id}'")]
    InvalidTransition { tx_id: String, from: DeploymentState },
    #[error("another transaction '{0}' is already armed")]
    AlreadyArmed(String),
    #[error("authorization expired or invalid")]
    AuthorizationInvalid,
    #[error("unknown transaction '{0}'")]
    UnknownTx(String),
    #[error("incompatible migration: {0}")]
    IncompatibleMigration(String),
    #[error("rollback error: {0}")]
    RollbackFailed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShadowReport {
    pub passed: bool,
    pub scans_executed: u64,
    pub mismatches: u64,
    pub vetoes: u64,
    pub errors: u64,
    pub first_counterexample: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationRecord {
    pub operator_id: String,
    pub role: String,
    pub package_hash: String,
    pub diff_hash: String,
    pub evidence_digest: String,
    pub expires_logical_ns: u64,
    pub authorized_at_ns: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionRecord {
    pub tx_id: String,
    pub revision: DeploymentRevision,
    pub request_digest: String,
    pub package: ModulePackageV1,
    pub shadow_report: Option<ShadowReport>,
    pub authorization: Option<AuthorizationRecord>,
    pub reason: String,
}

pub struct PendingActivation {
    pub tx_id: String,
    pub revision_num: u64,
    pub package: ModulePackageV1,
    pub package_hash: String,
    pub migration: PreparedMigration,
    pub is_rollback: bool,
}

/// Manages deployment FSM, idempotent transactions, monotonic revisions, and anti-flapping (§5.5).
pub struct DeploymentManager {
    pub device_id: String,
    pub boot_id: u64,
    pub profile: SiteProfileV1,
    pub profile_hash_str: String,
    pub trusted_pub_hex: String,
    pub journal: DurableJournal,

    pub active_revision: u64,
    pub active_package: Option<ModulePackageV1>,
    pub active_package_hash: Option<String>,
    pub prev_stable_package: Option<ModulePackageV1>,
    pub prev_stable_hash: Option<String>,

    pub highest_revision: u64,
    pub armed_tx_id: Option<String>,
    pub transactions: BTreeMap<String, TransactionRecord>,

    /// Anti-flapping budget: max consecutive rollbacks/faults before latching fallback.
    pub recent_fault_count: u32,
    pub max_faults_before_latch: u32,
    pub fallback_latched: bool,
    pub needs_operator_review: bool,
}

impl DeploymentManager {
    pub fn init(
        profile: SiteProfileV1,
        trusted_pub_hex: String,
        journal: DurableJournal,
        boot_id: u64,
    ) -> Result<Self, DeployError> {
        let p_hash = profile_hash(&profile).map_err(|e| DeployError::Verification(e.to_string()))?;
        let recovered = journal.recover_on_boot(boot_id)?;

        let mut txs = BTreeMap::new();
        for rev in &recovered.revisions {
            if let Ok(pkg) = journal.load_package(&rev.package_hash) {
                txs.insert(
                    rev.tx_id.clone(),
                    TransactionRecord {
                        tx_id: rev.tx_id.clone(),
                        revision: rev.clone(),
                        request_digest: rev.package_hash.clone(),
                        package: pkg,
                        shadow_report: None,
                        authorization: None,
                        reason: recovered.recovery_note.clone(),
                    },
                );
            }
        }

        let active_rev = recovered
            .last_stable_revision
            .as_ref()
            .map(|r| r.revision)
            .unwrap_or(0);
        let active_hash = recovered
            .last_stable_revision
            .as_ref()
            .map(|r| r.package_hash.clone());

        Ok(Self {
            device_id: profile.device_id.clone(),
            boot_id,
            profile,
            profile_hash_str: p_hash,
            trusted_pub_hex,
            journal,
            active_revision: active_rev,
            active_package: recovered.last_stable_package,
            active_package_hash: active_hash,
            prev_stable_package: None,
            prev_stable_hash: None,
            highest_revision: recovered.highest_revision,
            armed_tx_id: None,
            transactions: txs,
            recent_fault_count: 0,
            max_faults_before_latch: 3,
            fallback_latched: recovered.needs_operator_review,
            needs_operator_review: recovered.needs_operator_review,
        })
    }

    /// 1. Staged -> Verified: validates signature, digests, profileHash, ABI, and migration compatibility.
    pub fn stage_and_verify(
        &mut self,
        tx_id: String,
        pkg: ModulePackageV1,
        arena: &StateArena,
    ) -> Result<&TransactionRecord, DeployError> {
        let pkg_val = serde_json::to_value(&pkg)
            .map_err(|e| DeployError::Verification(e.to_string()))?;
        let req_digest = sha256_hex(
            &canonical_json_bytes(&pkg_val).map_err(|e| DeployError::Verification(e.to_string()))?,
        );

        // Idempotency check (§5.5 & D02)
        if let Some(existing) = self.transactions.get(&tx_id) {
            if existing.request_digest == req_digest {
                return Ok(self.transactions.get(&tx_id).unwrap());
            } else {
                return Err(DeployError::IdempotencyMismatch(tx_id));
            }
        }

        // Base revision check (D01)
        if pkg.base_revision != self.active_revision {
            return Err(DeployError::RevisionConflict {
                expected: self.active_revision,
                got: pkg.base_revision,
            });
        }

        // Profile hash, capabilities & Ed25519 signature check (I02)
        if pkg.profile_hash != self.profile_hash_str {
            return Err(DeployError::Verification(format!(
                "profileHash mismatch: expected {}, got {}",
                self.profile_hash_str, pkg.profile_hash
            )));
        }
        if !pkg.requested_capabilities.is_empty() {
            return Err(DeployError::Verification(
                "module requested forbidden capabilities".into(),
            ));
        }
        verify_module_package_signature(&pkg, &self.trusted_pub_hex)
            .map_err(|e| DeployError::Verification(e.to_string()))?;

        // Check static migration compatibility against active canonical arena (S02)
        let next_rev = self.highest_revision + 1;
        arena
            .plan_migration(&pkg, next_rev)
            .map_err(|e| DeployError::IncompatibleMigration(e.to_string()))?;

        let pkg_hash = self.journal.store_immutable_package(&pkg)?;
        self.highest_revision = next_rev;

        let mut rev = DeploymentRevision {
            tx_id: tx_id.clone(),
            device_id: self.device_id.clone(),
            boot_id: self.boot_id,
            revision: next_rev,
            base_revision: pkg.base_revision,
            package_hash: pkg_hash.clone(),
            logic_hash: pkg.wasm_digest.clone().unwrap_or_else(|| pkg.ir_digest.clone()),
            rule_set_hash: pkg.ir_digest.clone(),
            profile_hash: pkg.profile_hash.clone(),
            state_schema: pkg.state_schema.clone(),
            state: DeploymentState::Staged,
            active_scan_id: 0,
        };
        self.journal.append_revision(&rev)?;

        rev.state = DeploymentState::Verified;
        self.journal.append_revision(&rev)?;

        self.transactions.insert(
            tx_id.clone(),
            TransactionRecord {
                tx_id: tx_id.clone(),
                revision: rev,
                request_digest: req_digest,
                package: pkg,
                shadow_report: None,
                authorization: None,
                reason: "verified".into(),
            },
        );
        Ok(self.transactions.get(&tx_id).unwrap())
    }

    /// 2. Records Shadow verification result (F04: rejects candidate if shadow failed/diverged).
    pub fn record_shadow_result(
        &mut self,
        tx_id: &str,
        report: ShadowReport,
    ) -> Result<&TransactionRecord, DeployError> {
        let tx = self
            .transactions
            .get_mut(tx_id)
            .ok_or_else(|| DeployError::UnknownTx(tx_id.to_string()))?;
        if tx.revision.state != DeploymentState::Verified
            && tx.revision.state != DeploymentState::Shadow
        {
            return Err(DeployError::InvalidTransition {
                tx_id: tx_id.to_string(),
                from: tx.revision.state,
            });
        }
        tx.revision.state = if report.passed {
            DeploymentState::Shadow
        } else {
            DeploymentState::Rejected
        };
        tx.shadow_report = Some(report);
        self.journal.append_revision(&tx.revision)?;
        Ok(self.transactions.get(tx_id).unwrap())
    }

    /// 3. Authorization (§6.3): binds operator, role, package hash, evidence, and monotonic TTL.
    pub fn authorize(
        &mut self,
        tx_id: &str,
        operator_id: String,
        role: String,
        expected_package_hash: &str,
        now_ns: u64,
        ttl_ms: u64,
    ) -> Result<&TransactionRecord, DeployError> {
        let tx = self
            .transactions
            .get_mut(tx_id)
            .ok_or_else(|| DeployError::UnknownTx(tx_id.to_string()))?;

        if tx.revision.package_hash != expected_package_hash {
            return Err(DeployError::Verification(
                "authorization packageHash mismatch".into(),
            ));
        }
        if matches!(
            tx.revision.state,
            DeploymentState::Rejected
                | DeploymentState::Superseded
                | DeploymentState::RecoveryRequired
                | DeploymentState::ActiveVolatile
                | DeploymentState::Stable
                | DeploymentState::Persisted
                | DeploymentState::Recovering
        ) {
            return Err(DeployError::InvalidTransition {
                tx_id: tx_id.to_string(),
                from: tx.revision.state,
            });
        }
        if tx.revision.base_revision != self.active_revision {
            return Err(DeployError::RevisionConflict {
                expected: self.active_revision,
                got: tx.revision.base_revision,
            });
        }
        if let Some(ref shadow) = tx.shadow_report {
            if !shadow.passed {
                return Err(DeployError::AuthorizationInvalid);
            }
        }
        // Only one armed tx per device at a time (§5.5)
        if let Some(ref armed) = self.armed_tx_id {
            if armed != tx_id {
                return Err(DeployError::AlreadyArmed(armed.clone()));
            }
        }

        let evidence_digest = tx
            .shadow_report
            .as_ref()
            .and_then(|s| serde_json::to_vec(s).ok())
            .map(|b| sha256_hex(&b))
            .unwrap_or_else(|| sha256_hex(b"no-shadow"));

        tx.authorization = Some(AuthorizationRecord {
            operator_id,
            role,
            package_hash: tx.revision.package_hash.clone(),
            diff_hash: tx.revision.rule_set_hash.clone(),
            evidence_digest,
            expires_logical_ns: now_ns.saturating_add(ttl_ms.saturating_mul(1_000_000)),
            authorized_at_ns: now_ns,
        });
        tx.revision.state = DeploymentState::Armed;
        self.armed_tx_id = Some(tx_id.to_string());
        self.journal.append_revision(&tx.revision)?;
        Ok(self.transactions.get(tx_id).unwrap())
    }

    /// 4. Prepares activation for the next scan boundary (idempotent if already active/activated).
    pub fn prepare_activation(
        &mut self,
        tx_id: &str,
        now_ns: u64,
        arena: &StateArena,
    ) -> Result<Option<PendingActivation>, DeployError> {
        let tx = self
            .transactions
            .get(tx_id)
            .ok_or_else(|| DeployError::UnknownTx(tx_id.to_string()))?;

        // Idempotent repeat of activate returns None (already activated) without re-triggering (D02)
        if matches!(
            tx.revision.state,
            DeploymentState::ActiveVolatile | DeploymentState::Persisted | DeploymentState::Stable
        ) {
            return Ok(None);
        }
        if tx.revision.state != DeploymentState::Armed {
            return Err(DeployError::InvalidTransition {
                tx_id: tx_id.to_string(),
                from: tx.revision.state,
            });
        }
        if tx.revision.base_revision != self.active_revision {
            return Err(DeployError::RevisionConflict {
                expected: self.active_revision,
                got: tx.revision.base_revision,
            });
        }
        let auth = tx
            .authorization
            .as_ref()
            .ok_or(DeployError::AuthorizationInvalid)?;
        if now_ns >= auth.expires_logical_ns || auth.package_hash != tx.revision.package_hash {
            return Err(DeployError::AuthorizationInvalid);
        }

        let migration = arena
            .plan_migration(&tx.package, tx.revision.revision)
            .map_err(|e| DeployError::IncompatibleMigration(e.to_string()))?;

        Ok(Some(PendingActivation {
            tx_id: tx_id.to_string(),
            revision_num: tx.revision.revision,
            package: tx.package.clone(),
            package_hash: tx.revision.package_hash.clone(),
            migration,
            is_rollback: false,
        }))
    }

    pub fn cancel_armed(&mut self, tx_id: &str) -> Result<&TransactionRecord, DeployError> {
        let tx = self
            .transactions
            .get_mut(tx_id)
            .ok_or_else(|| DeployError::UnknownTx(tx_id.to_string()))?;
        if tx.revision.state == DeploymentState::Armed {
            tx.revision.state = DeploymentState::Rejected;
            tx.reason = "cancelled_by_operator".into();
            if self.armed_tx_id.as_deref() == Some(tx_id) {
                self.armed_tx_id = None;
            }
            self.journal.append_revision(&tx.revision)?;
        }
        Ok(self.transactions.get(tx_id).unwrap())
    }

    /// Called after boundary swap succeeds in RAM (ActiveVolatile -> Persisted -> Stable).
    pub fn commit_boundary_activation(
        &mut self,
        act: &PendingActivation,
        active_scan_id: u64,
    ) -> Result<(), DeployError> {
        maybe_inject_crash("BEFORE_VOLATILE");

        if let Some(cur_pkg) = self.active_package.clone() {
            self.prev_stable_package = Some(cur_pkg);
            self.prev_stable_hash = self.active_package_hash.clone();
        }
        self.active_revision = act.revision_num;
        self.active_package = Some(act.package.clone());
        self.active_package_hash = Some(act.package_hash.clone());
        if self.armed_tx_id.as_deref() == Some(&act.tx_id) {
            self.armed_tx_id = None;
        }

        if let Some(tx) = self.transactions.get_mut(&act.tx_id) {
            tx.revision.active_scan_id = active_scan_id;
            tx.revision.state = DeploymentState::ActiveVolatile;
            self.journal.append_revision(&tx.revision)?;
            maybe_inject_crash("AFTER_VOLATILE_BEFORE_PERSISTED");

            tx.revision.state = DeploymentState::Persisted;
            self.journal.append_revision(&tx.revision)?;
            maybe_inject_crash("AFTER_PERSISTED");

            tx.revision.state = if act.is_rollback {
                DeploymentState::RolledBack
            } else {
                DeploymentState::Stable
            };
            self.journal.append_revision(&tx.revision)?;
        }
        Ok(())
    }

    /// Creates a new monotonic rollback revision pointing to the previous known stable package (§5.5).
    pub fn prepare_rollback(
        &mut self,
        tx_id: String,
        reason: String,
        arena: &StateArena,
    ) -> Result<PendingActivation, DeployError> {
        maybe_inject_crash("DURING_ROLLBACK");
        self.recent_fault_count += 1;
        if self.recent_fault_count > self.max_faults_before_latch {
            self.fallback_latched = true;
            return Err(DeployError::RollbackFailed(
                "rollback fault budget exceeded; latched in safe fallback".into(),
            ));
        }

        let prev_pkg = self
            .prev_stable_package
            .clone()
            .ok_or_else(|| DeployError::RollbackFailed("no previous stable package available".into()))?;
        let prev_hash = self
            .prev_stable_hash
            .clone()
            .ok_or_else(|| DeployError::RollbackFailed("no previous stable hash".into()))?;

        let next_rev = self.highest_revision + 1;
        self.highest_revision = next_rev;
        let migration = arena
            .plan_migration(&prev_pkg, next_rev)
            .map_err(|e| DeployError::IncompatibleMigration(e.to_string()))?;

        let rev = DeploymentRevision {
            tx_id: tx_id.clone(),
            device_id: self.device_id.clone(),
            boot_id: self.boot_id,
            revision: next_rev,
            base_revision: self.active_revision,
            package_hash: prev_hash.clone(),
            logic_hash: prev_pkg
                .wasm_digest
                .clone()
                .unwrap_or_else(|| prev_pkg.ir_digest.clone()),
            rule_set_hash: prev_pkg.ir_digest.clone(),
            profile_hash: prev_pkg.profile_hash.clone(),
            state_schema: prev_pkg.state_schema.clone(),
            state: DeploymentState::Recovering,
            active_scan_id: 0,
        };
        self.journal.append_revision(&rev)?;

        self.transactions.insert(
            tx_id.clone(),
            TransactionRecord {
                tx_id: tx_id.clone(),
                revision: rev,
                request_digest: prev_hash.clone(),
                package: prev_pkg.clone(),
                shadow_report: None,
                authorization: None,
                reason,
            },
        );

        Ok(PendingActivation {
            tx_id,
            revision_num: next_rev,
            package: prev_pkg,
            package_hash: prev_hash,
            migration,
            is_rollback: true,
        })
    }

    pub fn rearm_operator(&mut self) {
        self.fallback_latched = false;
        self.needs_operator_review = false;
        self.recent_fault_count = 0;
    }
}
