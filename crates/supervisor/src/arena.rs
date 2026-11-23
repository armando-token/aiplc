use std::collections::HashSet;

use control_abi::schema::{MigrationPlanEntry, ModulePackageV1};
use control_abi::types::{MigrationPolicy, StateBlock, MAX_BLOCKS};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("duplicate stableId {0} in candidate package")]
    DuplicateStableId(u32),
    #[error("block count {0} exceeds MAX_BLOCKS ({MAX_BLOCKS})")]
    TooManyBlocks(usize),
    #[error("incompatible block type for stableId {stable_id}: active={active:?}, candidate={candidate:?}")]
    IncompatibleBlockType {
        stable_id: u32,
        active: control_abi::BlockType,
        candidate: control_abi::BlockType,
    },
    #[error("incompatible schemaVersion for stableId {stable_id}: active={active}, candidate={candidate}")]
    IncompatibleSchemaVersion {
        stable_id: u32,
        active: u8,
        candidate: u8,
    },
    #[error("migration policy REJECT_INCOMPATIBLE for stableId {0}")]
    RejectedByPolicy(u32),
}

#[derive(Debug, Clone)]
pub struct PreparedMigration {
    pub target_revision: u64,
    pub entries: Vec<MigrationPlanEntry>,
}

/// Canonical state arena owned exclusively by the supervisor (§5.3).
#[derive(Debug, Clone)]
pub struct StateArena {
    pub state_revision: u64,
    pub blocks: Vec<StateBlock>,
    pub last_applied_output_mask: u32,
}

impl StateArena {
    pub fn new(initial_revision: u64) -> Self {
        Self {
            state_revision: initial_revision,
            blocks: Vec::with_capacity(MAX_BLOCKS),
            last_applied_output_mask: 0,
        }
    }

    /// Validates compatibility and constructs migration metadata OUTSIDE the scan cycle.
    pub fn plan_migration(
        &self,
        candidate: &ModulePackageV1,
        target_revision: u64,
    ) -> Result<PreparedMigration, MigrationError> {
        if candidate.required_blocks.len() > MAX_BLOCKS {
            return Err(MigrationError::TooManyBlocks(candidate.required_blocks.len()));
        }
        let mut seen = HashSet::new();
        for entry in &candidate.required_blocks {
            if entry.state_size != 40 {
                return Err(MigrationError::IncompatibleSchemaVersion {
                    stable_id: entry.stable_id,
                    active: 40,
                    candidate: entry.state_size as u8,
                });
            }
            if !seen.insert(entry.stable_id) {
                return Err(MigrationError::DuplicateStableId(entry.stable_id));
            }
            if let Some(existing) = self
                .blocks
                .iter()
                .find(|b| b.stable_id == entry.stable_id)
            {
                if existing.block_type != entry.block_type {
                    return Err(MigrationError::IncompatibleBlockType {
                        stable_id: entry.stable_id,
                        active: existing.block_type,
                        candidate: entry.block_type,
                    });
                }
                if existing.schema_version != entry.schema_version {
                    return Err(MigrationError::IncompatibleSchemaVersion {
                        stable_id: entry.stable_id,
                        active: existing.schema_version,
                        candidate: entry.schema_version,
                    });
                }
                if entry.migration_policy == MigrationPolicy::RejectIncompatible {
                    return Err(MigrationError::RejectedByPolicy(entry.stable_id));
                }
            }
        }

        Ok(PreparedMigration {
            target_revision,
            entries: candidate.required_blocks.clone(),
        })
    }

    /// Applies prepared state migration deterministically at the scan boundary.
    pub fn apply_migration(&mut self, plan: &PreparedMigration, boundary_logical_ns: u64) {
        let mut next = Vec::with_capacity(plan.entries.len());
        for entry in &plan.entries {
            if let Some(existing) = self
                .blocks
                .iter()
                .find(|b| b.stable_id == entry.stable_id)
            {
                match entry.migration_policy {
                    MigrationPolicy::PreserveElapsed => {
                        let mut preserved = *existing;
                        preserved.migration_policy = entry.migration_policy;
                        preserved.retention_class = entry.retention_class;
                        if preserved.phase == 1 {
                            if preserved.start_logical_ns <= boundary_logical_ns {
                                preserved.elapsed_ns =
                                    boundary_logical_ns - preserved.start_logical_ns;
                            } else {
                                preserved.start_logical_ns =
                                    boundary_logical_ns.saturating_sub(preserved.elapsed_ns);
                            }
                        }
                        next.push(preserved);
                    }
                    MigrationPolicy::Reset | MigrationPolicy::RejectIncompatible => {
                        let mut reset_blk = StateBlock::empty(entry.stable_id, entry.block_type);
                        reset_blk.schema_version = entry.schema_version;
                        reset_blk.retention_class = entry.retention_class;
                        reset_blk.migration_policy = entry.migration_policy;
                        next.push(reset_blk);
                    }
                }
            } else {
                let mut fresh = StateBlock::empty(entry.stable_id, entry.block_type);
                fresh.schema_version = entry.schema_version;
                fresh.retention_class = entry.retention_class;
                fresh.migration_policy = entry.migration_policy;
                next.push(fresh);
            }
        }
        self.blocks = next;
        self.state_revision = plan.target_revision;
    }

    /// Reconciles canonical state on contingency fallback (resets cyclic timers/latches).
    pub fn reconcile_fallback(&mut self) {
        for blk in &mut self.blocks {
            blk.prev_cond = 0;
            blk.phase = 0;
            blk.last_output = 0;
            blk.start_logical_ns = 0;
            blk.elapsed_ns = 0;
        }
    }
}
