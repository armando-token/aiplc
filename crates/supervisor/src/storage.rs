use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use control_abi::schema::{DeploymentRevision, DeploymentState, ModulePackageV1};
use serde::Serialize;

use crate::journal::{maybe_inject_crash, DurableJournal};

/// Atomic boundary epoch updated synchronously by the RT scan thread on swap/rollback.
/// Allows the asynchronous `StorageWriter` to verify that a delayed WAL commit has not been
/// superseded by a newer boundary before marking `Persisted` / `Stable` / `RolledBack`.
#[derive(Debug, Default)]
pub struct BoundaryEpochTracker {
    pub boot_id: AtomicU64,
    pub revision: AtomicU64,
    pub lease_epoch: AtomicU64,
}

impl BoundaryEpochTracker {
    pub fn new(boot_id: u64, revision: u64, lease_epoch: u64) -> Self {
        Self {
            boot_id: AtomicU64::new(boot_id),
            revision: AtomicU64::new(revision),
            lease_epoch: AtomicU64::new(lease_epoch),
        }
    }

    pub fn store(&self, boot_id: u64, revision: u64, lease_epoch: u64) {
        self.boot_id.store(boot_id, Ordering::Release);
        self.revision.store(revision, Ordering::Release);
        self.lease_epoch.store(lease_epoch, Ordering::Release);
    }

    pub fn matches(&self, boot_id: u64, revision: u64, lease_epoch: u64) -> bool {
        self.boot_id.load(Ordering::Acquire) == boot_id
            && self.revision.load(Ordering::Acquire) == revision
            && self.lease_epoch.load(Ordering::Acquire) == lease_epoch
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStatusSnapshot {
    pub degraded: bool,
    pub active_persisted: bool,
    pub persisted_revision: u64,
    pub last_error: Option<String>,
    pub commits_completed: u64,
    pub stale_commits_superseded: u64,
    pub last_journal_persist_ns: u64,
    pub max_journal_persist_ns: u64,
    pub queue_drops: u64,
}

#[derive(Debug, Default)]
pub struct StorageHealthState {
    pub degraded: AtomicBool,
    pub active_persisted: AtomicBool,
    pub persisted_revision: AtomicU64,
    pub commits_completed: AtomicU64,
    pub stale_commits_superseded: AtomicU64,
    pub last_journal_persist_ns: AtomicU64,
    pub max_journal_persist_ns: AtomicU64,
    pub queue_drops: AtomicU64,
    pub delay_ms: AtomicU64,
    pub inject_write_error: AtomicBool,
    pub last_error: Mutex<Option<String>>,
    pub recent_events: Mutex<VecDeque<StorageEvent>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageEvent {
    pub kind: String,
    pub revision: u64,
    pub lease_epoch: u64,
    pub persist_ns: u64,
    pub detail: String,
}

impl StorageHealthState {
    pub fn new(initial_rev: u64, initially_persisted: bool, delay_ms: u64) -> Self {
        Self {
            degraded: AtomicBool::new(false),
            active_persisted: AtomicBool::new(initially_persisted),
            persisted_revision: AtomicU64::new(initial_rev),
            delay_ms: AtomicU64::new(delay_ms),
            ..Default::default()
        }
    }

    pub fn mark_degraded(&self, err: String) {
        self.degraded.store(true, Ordering::Release);
        self.active_persisted.store(false, Ordering::Release);
        *self.last_error.lock().unwrap() = Some(err.clone());
        self.push_event(StorageEvent {
            kind: "storage_error".into(),
            revision: self.persisted_revision.load(Ordering::Acquire),
            lease_epoch: 0,
            persist_ns: 0,
            detail: err,
        });
    }

    fn push_event(&self, ev: StorageEvent) {
        let mut q = self.recent_events.lock().unwrap();
        if q.len() >= 128 {
            q.pop_front();
        }
        q.push_back(ev);
    }

    pub fn snapshot(&self) -> StorageStatusSnapshot {
        StorageStatusSnapshot {
            degraded: self.degraded.load(Ordering::Acquire),
            active_persisted: self.active_persisted.load(Ordering::Acquire),
            persisted_revision: self.persisted_revision.load(Ordering::Acquire),
            last_error: self.last_error.lock().unwrap().clone(),
            commits_completed: self.commits_completed.load(Ordering::Acquire),
            stale_commits_superseded: self.stale_commits_superseded.load(Ordering::Acquire),
            last_journal_persist_ns: self.last_journal_persist_ns.load(Ordering::Acquire),
            max_journal_persist_ns: self.max_journal_persist_ns.load(Ordering::Acquire),
            queue_drops: self.queue_drops.load(Ordering::Acquire),
        }
    }
}

pub enum StorageCmd {
    StorePackageAndAppend {
        pkg: Box<ModulePackageV1>,
        rev_staged: Box<DeploymentRevision>,
        rev_verified: Box<DeploymentRevision>,
        reply: mpsc::SyncSender<Result<String, String>>,
    },
    AppendRevision {
        rev: Box<DeploymentRevision>,
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    CommitBoundary {
        rev: Box<DeploymentRevision>,
        lease_epoch: u64,
        is_rollback: bool,
        enqueue_instant: Instant,
    },
    Flush {
        reply: mpsc::SyncSender<()>,
    },
    Shutdown,
}

#[derive(Clone)]
pub struct StorageWriterHandle {
    tx: mpsc::SyncSender<StorageCmd>,
    pub health: Arc<StorageHealthState>,
}

impl StorageWriterHandle {
    pub fn has_capacity_and_healthy(&self) -> Result<(), String> {
        if self.health.degraded.load(Ordering::Acquire) {
            return Err(self
                .health
                .last_error
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "storage degraded".into()));
        }
        Ok(())
    }

    pub fn store_package_and_append_revisions(
        &self,
        pkg: ModulePackageV1,
        rev_staged: DeploymentRevision,
        rev_verified: DeploymentRevision,
    ) -> Result<String, String> {
        self.has_capacity_and_healthy()?;
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx
            .try_send(StorageCmd::StorePackageAndAppend {
                pkg: Box::new(pkg),
                rev_staged: Box::new(rev_staged),
                rev_verified: Box::new(rev_verified),
                reply: tx,
            })
            .map_err(|_| "storage writer queue congested".to_string())?;
        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|e| format!("storage writer timeout/closed: {e}"))?
    }

    pub fn append_revision_sync(&self, rev: DeploymentRevision) -> Result<(), String> {
        self.has_capacity_and_healthy()?;
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx
            .try_send(StorageCmd::AppendRevision {
                rev: Box::new(rev),
                reply: tx,
            })
            .map_err(|_| "storage writer queue congested".to_string())?;
        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|e| format!("storage writer timeout/closed: {e}"))?
    }

    /// Non-blocking handoff called by the RT scan thread at boundary swap. NEVER blocks!
    pub fn commit_boundary_nonblocking(
        &self,
        rev: DeploymentRevision,
        lease_epoch: u64,
        is_rollback: bool,
    ) -> Result<(), &'static str> {
        self.health.active_persisted.store(false, Ordering::Release);
        match self.tx.try_send(StorageCmd::CommitBoundary {
            rev: Box::new(rev),
            lease_epoch,
            is_rollback,
            enqueue_instant: Instant::now(),
        }) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.health.queue_drops.fetch_add(1, Ordering::Relaxed);
                self.health
                    .mark_degraded("storage writer channel full/closed during boundary commit".into());
                Err("storage queue full")
            }
        }
    }

    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::sync_channel(1);
        if self.tx.try_send(StorageCmd::Flush { reply: tx }).is_err() {
            return false;
        }
        rx.recv_timeout(timeout).is_ok()
    }
}

/// Dedicated off-scan WAL / package persistence owner (`storage-writer` thread).
/// Guarantees `DurableJournal` is never touched from the RT scan thread or concurrently by HTTP threads.
pub struct StorageWriter {
    tx: mpsc::SyncSender<StorageCmd>,
    pub health: Arc<StorageHealthState>,
    handle: Option<JoinHandle<()>>,
}

impl StorageWriter {
    pub fn start(
        journal: DurableJournal,
        tracker: Arc<BoundaryEpochTracker>,
        initial_rev: u64,
        initially_persisted: bool,
        delay_ms: u64,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel::<StorageCmd>(128);
        let health = Arc::new(StorageHealthState::new(
            initial_rev,
            initially_persisted,
            delay_ms,
        ));
        let health_bg = Arc::clone(&health);

        let handle = thread::Builder::new()
            .name("storage-writer".into())
            .spawn(move || {
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        StorageCmd::StorePackageAndAppend {
                            pkg,
                            mut rev_staged,
                            mut rev_verified,
                            reply,
                        } => {
                            let res = (|| -> Result<String, String> {
                                if health_bg.inject_write_error.load(Ordering::Acquire) {
                                    return Err("simulated WAL/package IO error".into());
                                }
                                let hash = journal
                                    .store_immutable_package(&pkg)
                                    .map_err(|e| e.to_string())?;
                                rev_staged.package_hash = hash.clone();
                                rev_verified.package_hash = hash.clone();
                                journal
                                    .append_revision(&rev_staged)
                                    .map_err(|e| e.to_string())?;
                                journal
                                    .append_revision(&rev_verified)
                                    .map_err(|e| e.to_string())?;
                                Ok(hash)
                            })();
                            if let Err(ref err) = res {
                                health_bg.mark_degraded(err.clone());
                            }
                            let _ = reply.send(res);
                        }
                        StorageCmd::AppendRevision { rev, reply } => {
                            let res = if health_bg.inject_write_error.load(Ordering::Acquire) {
                                Err("simulated WAL append IO error".into())
                            } else {
                                journal.append_revision(&rev).map_err(|e| e.to_string())
                            };
                            if let Err(ref err) = res {
                                health_bg.mark_degraded(err.clone());
                            }
                            let _ = reply.send(res);
                        }
                        StorageCmd::CommitBoundary {
                            mut rev,
                            lease_epoch,
                            is_rollback,
                            enqueue_instant,
                        } => {
                            if health_bg.inject_write_error.load(Ordering::Acquire) {
                                health_bg.mark_degraded(
                                    "simulated WAL boundary write IO error (EIO)".into(),
                                );
                                continue;
                            }

                            let boot_id = rev.boot_id;
                            let target_rev = rev.revision;
                            rev.state = if is_rollback {
                                DeploymentState::Recovering
                            } else {
                                DeploymentState::ActiveVolatile
                            };
                            if let Err(e) = journal.append_revision(&rev) {
                                health_bg.mark_degraded(format!(
                                    "WAL append ActiveVolatile failed: {e}"
                                ));
                                continue;
                            }

                            maybe_inject_crash("AFTER_VOLATILE_BEFORE_PERSISTED");

                            let d_ms = health_bg.delay_ms.load(Ordering::Acquire);
                            if d_ms > 0 {
                                thread::sleep(Duration::from_millis(d_ms));
                            }

                            if !tracker.matches(boot_id, target_rev, lease_epoch) {
                                health_bg
                                    .stale_commits_superseded
                                    .fetch_add(1, Ordering::Relaxed);
                                rev.state = DeploymentState::Superseded;
                                let _ = journal.append_revision(&rev);
                                health_bg.push_event(StorageEvent {
                                    kind: "boundary_superseded_before_persist".into(),
                                    revision: target_rev,
                                    lease_epoch,
                                    persist_ns: enqueue_instant.elapsed().as_nanos() as u64,
                                    detail: "newer boundary took effect before WAL commit finished"
                                        .into(),
                                });
                                continue;
                            }

                            if health_bg.inject_write_error.load(Ordering::Acquire) {
                                health_bg.mark_degraded(
                                    "simulated WAL failure before Persisted".into(),
                                );
                                continue;
                            }

                            rev.state = DeploymentState::Persisted;
                            if let Err(e) = journal.append_revision(&rev) {
                                health_bg.mark_degraded(format!(
                                    "WAL append Persisted failed: {e}"
                                ));
                                continue;
                            }

                            maybe_inject_crash("AFTER_PERSISTED_BEFORE_STABLE");

                            if !tracker.matches(boot_id, target_rev, lease_epoch) {
                                health_bg
                                    .stale_commits_superseded
                                    .fetch_add(1, Ordering::Relaxed);
                                rev.state = DeploymentState::Superseded;
                                let _ = journal.append_revision(&rev);
                                health_bg.push_event(StorageEvent {
                                    kind: "boundary_superseded_before_stable".into(),
                                    revision: target_rev,
                                    lease_epoch,
                                    persist_ns: enqueue_instant.elapsed().as_nanos() as u64,
                                    detail: "newer boundary took effect before Stable commit"
                                        .into(),
                                });
                                continue;
                            }

                            rev.state = if is_rollback {
                                DeploymentState::RolledBack
                            } else {
                                DeploymentState::Stable
                            };
                            if let Err(e) = journal.append_revision(&rev) {
                                health_bg.mark_degraded(format!(
                                    "WAL append Stable/RolledBack failed: {e}"
                                ));
                                continue;
                            }

                            let elapsed_ns = enqueue_instant.elapsed().as_nanos() as u64;
                            health_bg
                                .last_journal_persist_ns
                                .store(elapsed_ns, Ordering::Release);
                            let mut cur_max =
                                health_bg.max_journal_persist_ns.load(Ordering::Acquire);
                            while elapsed_ns > cur_max {
                                match health_bg.max_journal_persist_ns.compare_exchange_weak(
                                    cur_max,
                                    elapsed_ns,
                                    Ordering::AcqRel,
                                    Ordering::Acquire,
                                ) {
                                    Ok(_) => break,
                                    Err(v) => cur_max = v,
                                }
                            }
                            health_bg.persisted_revision.store(target_rev, Ordering::Release);
                            health_bg.active_persisted.store(true, Ordering::Release);
                            health_bg.degraded.store(false, Ordering::Release);
                            health_bg.commits_completed.fetch_add(1, Ordering::Relaxed);
                            *health_bg.last_error.lock().unwrap() = None;
                            health_bg.push_event(StorageEvent {
                                kind: if is_rollback {
                                    "boundary_rolled_back_persisted".into()
                                } else {
                                    "boundary_stable_persisted".into()
                                },
                                revision: target_rev,
                                lease_epoch,
                                persist_ns: elapsed_ns,
                                detail: format!("tx={}", rev.tx_id),
                            });
                        }
                        StorageCmd::Flush { reply } => {
                            let _ = reply.send(());
                        }
                        StorageCmd::Shutdown => break,
                    }
                }
            })
            .expect("spawn storage-writer thread");

        Self {
            tx,
            health,
            handle: Some(handle),
        }
    }

    pub fn handle(&self) -> StorageWriterHandle {
        StorageWriterHandle {
            tx: self.tx.clone(),
            health: Arc::clone(&self.health),
        }
    }

    pub fn shutdown(&mut self) {
        let _ = self.tx.send(StorageCmd::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for StorageWriter {
    fn drop(&mut self) {
        self.shutdown();
    }
}
