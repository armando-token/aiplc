use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use control_abi::types::{GuardReason, InputSignal};
use serde::Serialize;

use crate::scan_loop::ScanTelemetryRow;
use crate::storage::StorageHealthState;
use crate::worker_pool::ShadowMetrics;

#[derive(Debug, Clone)]
pub struct NightScanSample {
    pub row: ScanTelemetryRow,
    pub wakeup_jitter_ns: u64,
    pub eval_ipc_ns: u64,
    pub publish_ns: u64,
    pub cycle_dur_ns: u64,
    pub ram_swap_ns: u64,
    pub worker_ok: bool,
    pub active_worker_pid: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NightEvent {
    pub ts_unix_ms: u128,
    pub scan_id: u64,
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JitterCounterexample {
    pub scan_id: u64,
    pub wakeup_jitter_ns: u64,
    pub eval_ipc_ns: u64,
    pub publish_ns: u64,
    pub cycle_dur_ns: u64,
    pub ram_swap_ns: u64,
    pub deadline_miss: bool,
    pub active_worker_pid: u32,
    pub mem_available_kb: u64,
    pub supervisor_rss_kb: u64,
    pub worker_rss_kb: u64,
    pub temp_soc_mc: i64,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LatencySummary {
    pub count: u64,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub p999_ns: u64,
    pub max_ns: u64,
}

/// Fixed-memory 2us-resolution sub-ms histogram + coarser ms tail buckets (zero heap growth over 24h).
pub struct LatencyHistogram {
    // [0..500): 0..1000us step 2us (500 buckets)
    // [500..860): 1ms..10ms step 25us (360 buckets)
    // [860..950): 10ms..100ms step 1ms (90 buckets)
    // [950]: >= 100ms
    buckets: Vec<u64>,
    count: u64,
    max_ns: u64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyHistogram {
    pub fn new() -> Self {
        Self {
            buckets: vec![0u64; 951],
            count: 0,
            max_ns: 0,
        }
    }

    pub fn record(&mut self, ns: u64) {
        self.count += 1;
        if ns > self.max_ns {
            self.max_ns = ns;
        }
        let us = ns / 1_000;
        let idx = if us < 1_000 {
            (us / 2) as usize
        } else if us < 10_000 {
            500 + ((us - 1_000) / 25) as usize
        } else if us < 100_000 {
            860 + ((us - 10_000) / 1_000) as usize
        } else {
            950
        };
        self.buckets[idx.min(950)] += 1;
    }

    fn bucket_upper_ns(idx: usize) -> u64 {
        if idx < 500 {
            ((idx as u64) + 1) * 2_000
        } else if idx < 860 {
            1_000_000 + ((idx as u64 - 500) + 1) * 25_000
        } else if idx < 950 {
            10_000_000 + ((idx as u64 - 860) + 1) * 1_000_000
        } else {
            100_000_000
        }
    }

    pub fn quantile_ns(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = ((self.count as f64) * q).ceil().max(1.0) as u64;
        let mut cum = 0u64;
        for (i, &c) in self.buckets.iter().enumerate() {
            cum += c;
            if cum >= target {
                return Self::bucket_upper_ns(i).min(self.max_ns);
            }
        }
        self.max_ns
    }

    pub fn summary(&self) -> LatencySummary {
        LatencySummary {
            count: self.count,
            p50_ns: self.quantile_ns(0.50),
            p95_ns: self.quantile_ns(0.95),
            p99_ns: self.quantile_ns(0.99),
            p999_ns: self.quantile_ns(0.999),
            max_ns: self.max_ns,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NightSummarySnapshot {
    pub run_id: String,
    pub device_id: String,
    pub boot_id: u64,
    pub io_mode: String,
    pub serial_backend: String,
    pub lcd_enabled: bool,
    pub scans_total: u64,
    pub deadline_misses: u64,
    pub missed_releases: u64,
    pub telemetry_dropped: u64,
    pub shadow_dropped: u64,
    pub warm_rollback_count: u64,
    pub fallback_latch_count: u64,
    pub wakeup_jitter: LatencySummary,
    pub eval_ipc: LatencySummary,
    pub publish: LatencySummary,
    pub cycle_dur: LatencySummary,
    pub ram_swap: LatencySummary,
    pub journal_persist_last_ns: u64,
    pub journal_persist_max_ns: u64,
    pub latest_sample: OsSample,
    pub first_jitter_counterexample: Option<JitterCounterexample>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OsSample {
    pub ts_unix_ms: u128,
    pub scan_id: u64,
    pub supervisor_rss_kb: u64,
    pub supervisor_pss_kb: u64,
    pub worker_rss_kb: u64,
    pub worker_pss_kb: u64,
    pub mem_available_kb: u64,
    pub temp_soc_mc: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanRowView {
    pub boot_id: u64,
    pub scan_id: u64,
    pub release_ns: u64,
    pub start_ns: u64,
    pub input_seq: u64,
    pub eval_start_ns: u64,
    pub eval_end_ns: u64,
    pub decision_ns: u64,
    pub publish_ns: u64,
    pub ack_driver_ns: u64,
    pub active_rev: u64,
    pub logic_hash: String,
    pub lease_epoch: u64,
    pub deadline_miss: bool,
    pub fallback_reason: GuardReason,
    pub desired_mask: u32,
    pub applied_mask: u32,
    pub observed_mask: Option<u32>,
    pub signals: Vec<InputSignal>,
}

impl From<&ScanTelemetryRow> for ScanRowView {
    fn from(r: &ScanTelemetryRow) -> Self {
        Self {
            boot_id: r.boot_id,
            scan_id: r.scan_id,
            release_ns: r.release_ns,
            start_ns: r.start_ns,
            input_seq: r.input_seq,
            eval_start_ns: r.eval_start_ns,
            eval_end_ns: r.eval_end_ns,
            decision_ns: r.decision_ns,
            publish_ns: r.publish_ns,
            ack_driver_ns: r.ack_driver_ns,
            active_rev: r.active_rev,
            logic_hash: r.logic_hash.clone(),
            lease_epoch: r.lease_epoch,
            deadline_miss: r.deadline_miss,
            fallback_reason: r.fallback_reason,
            desired_mask: r.desired_mask,
            applied_mask: r.applied_mask,
            observed_mask: r.observed_mask,
            signals: r.signals.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedSnapshot {
    pub device_id: String,
    pub boot_id: u64,
    pub io_mode: String,
    pub serial_backend: String,
    pub lcd_enabled: bool,
    pub isolated_worker: bool,
    pub scan_id: u64,
    pub active_revision: u64,
    pub lease_epoch: u64,
    pub generation: u64,
    pub applied_mask: u32,
    pub guard_reason: GuardReason,
    pub missed_releases: u64,
    pub active_worker_pid: u32,
    pub recent_scans: VecDeque<ScanRowView>,
    pub latest_row: Option<ScanRowView>,
}

/// Shared lock-free scalar counters updated synchronously by RT scan thread (`Release`/`Acquire`).
#[derive(Debug, Default)]
pub struct SharedRtScalars {
    pub scan_id: AtomicU64,
    pub missed_releases: AtomicU64,
    pub telemetry_dropped: AtomicU64,
    pub active_revision: AtomicU64,
    pub lease_epoch: AtomicU64,
    pub generation: AtomicU64,
    pub armed: AtomicBool,
    pub fallback_latched: AtomicBool,
    pub needs_operator_review: AtomicBool,
    pub applied_mask: AtomicU32,
    pub active_worker_pid: AtomicU32,
    pub fallback_latch_count: AtomicU64,
    pub warm_rollback_count: AtomicU64,
}

#[derive(Debug, Clone)]
pub struct NightRunMetadata {
    pub run_id: String,
    pub device_id: String,
    pub boot_id: u64,
    pub io_mode: String,
    pub serial_backend: String,
    pub lcd_enabled: bool,
    pub isolated_worker: bool,
    pub strict_sandbox: bool,
    pub scan_period_ms: u32,
    pub eval_deadline_ms: u32,
    pub metrics_dir: Option<PathBuf>,
    pub max_raw_scan_rows: u64,
}

enum TelemetryMsg {
    Scan(NightScanSample),
    Event(NightEvent),
    Flush(mpsc::SyncSender<()>),
    Shutdown,
}

#[derive(Clone)]
pub struct TelemetryHandle {
    tx: mpsc::SyncSender<TelemetryMsg>,
    pub scalars: Arc<SharedRtScalars>,
    pub snapshot: Arc<RwLock<PublishedSnapshot>>,
    pub summary: Arc<RwLock<NightSummarySnapshot>>,
    pub recent_events: Arc<RwLock<VecDeque<NightEvent>>>,
}

impl TelemetryHandle {
    /// Enqueues a scan telemetry sample from the RT scan thread WITHOUT blocking.
    pub fn offer_scan_nonblocking(&self, sample: NightScanSample) {
        if self.tx.try_send(TelemetryMsg::Scan(sample)).is_err() {
            self.scalars
                .telemetry_dropped
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn emit_event_nonblocking(&self, scan_id: u64, kind: impl Into<String>, detail: impl Into<String>) {
        let _ = self.tx.try_send(TelemetryMsg::Event(NightEvent {
            ts_unix_ms: now_unix_ms(),
            scan_id,
            kind: kind.into(),
            detail: detail.into(),
        }));
    }

    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::sync_channel(1);
        if self.tx.try_send(TelemetryMsg::Flush(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(timeout).is_ok()
    }
}

pub struct NightTelemetryAggregator {
    tx: mpsc::SyncSender<TelemetryMsg>,
    pub handle: TelemetryHandle,
    join: Option<JoinHandle<()>>,
}

impl NightTelemetryAggregator {
    pub fn start(
        meta: NightRunMetadata,
        storage_health: Arc<StorageHealthState>,
        shadow_metrics: Arc<ShadowMetrics>,
        scalars: Arc<SharedRtScalars>,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel::<TelemetryMsg>(2048);

        let initial_pub = PublishedSnapshot {
            device_id: meta.device_id.clone(),
            boot_id: meta.boot_id,
            io_mode: meta.io_mode.clone(),
            serial_backend: meta.serial_backend.clone(),
            lcd_enabled: meta.lcd_enabled,
            isolated_worker: meta.isolated_worker,
            scan_id: 0,
            active_revision: scalars.active_revision.load(Ordering::Acquire),
            lease_epoch: scalars.lease_epoch.load(Ordering::Acquire),
            generation: scalars.generation.load(Ordering::Acquire),
            applied_mask: 0,
            guard_reason: GuardReason::StartupDisarmed,
            missed_releases: 0,
            active_worker_pid: 0,
            recent_scans: VecDeque::with_capacity(256),
            latest_row: None,
        };
        let initial_sum = NightSummarySnapshot {
            run_id: meta.run_id.clone(),
            device_id: meta.device_id.clone(),
            boot_id: meta.boot_id,
            io_mode: meta.io_mode.clone(),
            serial_backend: meta.serial_backend.clone(),
            lcd_enabled: meta.lcd_enabled,
            scans_total: 0,
            deadline_misses: 0,
            missed_releases: 0,
            telemetry_dropped: 0,
            shadow_dropped: 0,
            warm_rollback_count: 0,
            fallback_latch_count: 0,
            wakeup_jitter: LatencySummary::default(),
            eval_ipc: LatencySummary::default(),
            publish: LatencySummary::default(),
            cycle_dur: LatencySummary::default(),
            ram_swap: LatencySummary::default(),
            journal_persist_last_ns: 0,
            journal_persist_max_ns: 0,
            latest_sample: OsSample::default(),
            first_jitter_counterexample: None,
        };

        let snapshot = Arc::new(RwLock::new(initial_pub));
        let summary = Arc::new(RwLock::new(initial_sum));
        let recent_events = Arc::new(RwLock::new(VecDeque::with_capacity(256)));

        let handle = TelemetryHandle {
            tx: tx.clone(),
            scalars: Arc::clone(&scalars),
            snapshot: Arc::clone(&snapshot),
            summary: Arc::clone(&summary),
            recent_events: Arc::clone(&recent_events),
        };

        let meta_bg = meta.clone();
        let snap_bg = Arc::clone(&snapshot);
        let sum_bg = Arc::clone(&summary);
        let events_bg = Arc::clone(&recent_events);

        let join = thread::Builder::new()
            .name("telemetry-agg".into())
            .spawn(move || {
                let mut writers = open_writers(&meta_bg);
                let mut h_jitter = LatencyHistogram::new();
                let mut h_eval = LatencyHistogram::new();
                let mut h_pub = LatencyHistogram::new();
                let mut h_cycle = LatencyHistogram::new();
                let mut h_swap = LatencyHistogram::new();
                let mut deadline_misses = 0u64;
                let mut scans_total = 0u64;
                let mut first_ce: Option<JitterCounterexample> = None;
                let mut latest_os = sample_os_metrics(0, 0);
                let mut unsaved_scans = 0u64;

                while let Ok(msg) = rx.recv() {
                    match msg {
                        TelemetryMsg::Scan(s) => {
                            scans_total += 1;
                            unsaved_scans += 1;
                            if s.row.deadline_miss {
                                deadline_misses += 1;
                            }
                            h_jitter.record(s.wakeup_jitter_ns);
                            h_eval.record(s.eval_ipc_ns);
                            h_pub.record(s.publish_ns);
                            h_cycle.record(s.cycle_dur_ns);
                            if s.ram_swap_ns > 0 {
                                h_swap.record(s.ram_swap_ns);
                            }

                            if scans_total == 1 || scans_total % 100 == 0 {
                                latest_os = sample_os_metrics(s.row.scan_id, s.active_worker_pid);
                                if let Some(ref mut w) = writers {
                                    w.write_os_sample(&latest_os);
                                }
                            }

                            // Capture first counterexample if wakeup_jitter or cycle_dur exceeds 0.5 ms
                            if first_ce.is_none()
                                && (s.wakeup_jitter_ns >= 500_000 || s.cycle_dur_ns >= 500_000)
                            {
                                let os_now = sample_os_metrics(s.row.scan_id, s.active_worker_pid);
                                let ce = JitterCounterexample {
                                    scan_id: s.row.scan_id,
                                    wakeup_jitter_ns: s.wakeup_jitter_ns,
                                    eval_ipc_ns: s.eval_ipc_ns,
                                    publish_ns: s.publish_ns,
                                    cycle_dur_ns: s.cycle_dur_ns,
                                    ram_swap_ns: s.ram_swap_ns,
                                    deadline_miss: s.row.deadline_miss,
                                    active_worker_pid: s.active_worker_pid,
                                    mem_available_kb: os_now.mem_available_kb,
                                    supervisor_rss_kb: os_now.supervisor_rss_kb,
                                    worker_rss_kb: os_now.worker_rss_kb,
                                    temp_soc_mc: os_now.temp_soc_mc,
                                };
                                first_ce = Some(ce.clone());
                                if let Some(ref mut w) = writers {
                                    w.write_event(&NightEvent {
                                        ts_unix_ms: now_unix_ms(),
                                        scan_id: s.row.scan_id,
                                        kind: "jitter_threshold_exceeded".into(),
                                        detail: serde_json::to_string(&ce).unwrap_or_default(),
                                    });
                                }
                            }

                            if let Some(ref mut w) = writers {
                                w.write_scan_row(&s, meta_bg.max_raw_scan_rows);
                            }

                            {
                                let mut pub_w = snap_bg.write().unwrap();
                                pub_w.scan_id = s.row.scan_id;
                                pub_w.active_revision = s.row.active_rev;
                                pub_w.lease_epoch = s.row.lease_epoch;
                                pub_w.generation = scalars.generation.load(Ordering::Acquire);
                                pub_w.applied_mask = s.row.applied_mask;
                                pub_w.guard_reason = s.row.fallback_reason;
                                pub_w.missed_releases =
                                    scalars.missed_releases.load(Ordering::Acquire);
                                pub_w.active_worker_pid = s.active_worker_pid;
                                let view = ScanRowView::from(&s.row);
                                if pub_w.recent_scans.len() >= 256 {
                                    pub_w.recent_scans.pop_front();
                                }
                                pub_w.recent_scans.push_back(view.clone());
                                pub_w.latest_row = Some(view);
                            }

                            if unsaved_scans >= 50 || s.ram_swap_ns > 0 {
                                unsaved_scans = 0;
                                let sum_snapshot = build_summary_snapshot(
                                    &meta_bg,
                                    &scalars,
                                    &storage_health,
                                    &shadow_metrics,
                                    scans_total,
                                    deadline_misses,
                                    &h_jitter,
                                    &h_eval,
                                    &h_pub,
                                    &h_cycle,
                                    &h_swap,
                                    &latest_os,
                                    &first_ce,
                                );
                                *sum_bg.write().unwrap() = sum_snapshot.clone();
                                if let Some(ref mut w) = writers {
                                    w.flush_streams();
                                    w.write_summary_and_hist(
                                        &sum_snapshot,
                                        &h_jitter,
                                        &h_eval,
                                        &h_pub,
                                        &h_cycle,
                                        &h_swap,
                                    );
                                }
                            }
                        }
                        TelemetryMsg::Event(ev) => {
                            {
                                let mut ev_w = events_bg.write().unwrap();
                                if ev_w.len() >= 256 {
                                    ev_w.pop_front();
                                }
                                ev_w.push_back(ev.clone());
                            }
                            if let Some(ref mut w) = writers {
                                w.write_event(&ev);
                            }
                        }
                        TelemetryMsg::Flush(reply) => {
                            let sum_snapshot = build_summary_snapshot(
                                &meta_bg,
                                &scalars,
                                &storage_health,
                                &shadow_metrics,
                                scans_total,
                                deadline_misses,
                                &h_jitter,
                                &h_eval,
                                &h_pub,
                                &h_cycle,
                                &h_swap,
                                &latest_os,
                                &first_ce,
                            );
                            *sum_bg.write().unwrap() = sum_snapshot.clone();
                            if let Some(ref mut w) = writers {
                                w.flush_streams();
                                w.write_summary_and_hist(
                                    &sum_snapshot,
                                    &h_jitter,
                                    &h_eval,
                                    &h_pub,
                                    &h_cycle,
                                    &h_swap,
                                );
                            }
                            let _ = reply.send(());
                        }
                        TelemetryMsg::Shutdown => break,
                    }
                }

                // Final flush on shutdown
                let final_sum = build_summary_snapshot(
                    &meta_bg,
                    &scalars,
                    &storage_health,
                    &shadow_metrics,
                    scans_total,
                    deadline_misses,
                    &h_jitter,
                    &h_eval,
                    &h_pub,
                    &h_cycle,
                    &h_swap,
                    &latest_os,
                    &first_ce,
                );
                *sum_bg.write().unwrap() = final_sum.clone();
                if let Some(ref mut w) = writers {
                    w.flush_streams();
                    w.write_summary_and_hist(
                        &final_sum,
                        &h_jitter,
                        &h_eval,
                        &h_pub,
                        &h_cycle,
                        &h_swap,
                    );
                }
            })
            .expect("spawn telemetry-agg thread");

        Self {
            tx,
            handle,
            join: Some(join),
        }
    }

    pub fn shutdown(&mut self) {
        let _ = self.tx.send(TelemetryMsg::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for NightTelemetryAggregator {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[allow(clippy::too_many_arguments)]
fn build_summary_snapshot(
    meta: &NightRunMetadata,
    scalars: &SharedRtScalars,
    storage: &StorageHealthState,
    shadow: &ShadowMetrics,
    scans_total: u64,
    deadline_misses: u64,
    h_jitter: &LatencyHistogram,
    h_eval: &LatencyHistogram,
    h_pub: &LatencyHistogram,
    h_cycle: &LatencyHistogram,
    h_swap: &LatencyHistogram,
    latest_os: &OsSample,
    first_ce: &Option<JitterCounterexample>,
) -> NightSummarySnapshot {
    NightSummarySnapshot {
        run_id: meta.run_id.clone(),
        device_id: meta.device_id.clone(),
        boot_id: meta.boot_id,
        io_mode: meta.io_mode.clone(),
        serial_backend: meta.serial_backend.clone(),
        lcd_enabled: meta.lcd_enabled,
        scans_total,
        deadline_misses,
        missed_releases: scalars.missed_releases.load(Ordering::Acquire),
        telemetry_dropped: scalars.telemetry_dropped.load(Ordering::Acquire),
        shadow_dropped: shadow.dropped.load(Ordering::Acquire),
        warm_rollback_count: scalars.warm_rollback_count.load(Ordering::Acquire),
        fallback_latch_count: scalars.fallback_latch_count.load(Ordering::Acquire),
        wakeup_jitter: h_jitter.summary(),
        eval_ipc: h_eval.summary(),
        publish: h_pub.summary(),
        cycle_dur: h_cycle.summary(),
        ram_swap: h_swap.summary(),
        journal_persist_last_ns: storage.last_journal_persist_ns.load(Ordering::Acquire),
        journal_persist_max_ns: storage.max_journal_persist_ns.load(Ordering::Acquire),
        latest_sample: latest_os.clone(),
        first_jitter_counterexample: first_ce.clone(),
    }
}

struct DiskTelemetryWriters {
    run_dir: PathBuf,
    scans_csv: BufWriter<File>,
    anomalies_csv: BufWriter<File>,
    samples_csv: BufWriter<File>,
    events_jsonl: BufWriter<File>,
    raw_scan_rows_written: u64,
}

impl DiskTelemetryWriters {
    fn write_scan_row(&mut self, s: &NightScanSample, max_raw_rows: u64) {
        let obs_s = s
            .row
            .observed_mask
            .map(|m| m.to_string())
            .unwrap_or_else(|| "-".into());
        if self.raw_scan_rows_written < max_raw_rows {
            self.raw_scan_rows_written += 1;
            let _ = writeln!(
                self.scans_csv,
                "{},{},{},{},{},{},{},{},{},{},{},{:?},{},{},{}",
                s.row.scan_id,
                s.row.release_ns,
                s.row.start_ns,
                s.wakeup_jitter_ns,
                s.eval_ipc_ns,
                s.publish_ns,
                s.cycle_dur_ns,
                s.ram_swap_ns,
                s.row.active_rev,
                s.row.lease_epoch,
                u8::from(s.row.deadline_miss),
                s.row.fallback_reason,
                s.row.desired_mask,
                s.row.applied_mask,
                obs_s
            );
        }
        if s.wakeup_jitter_ns >= 300_000
            || s.cycle_dur_ns >= 300_000
            || s.row.deadline_miss
            || s.ram_swap_ns > 0
        {
            let _ = writeln!(
                self.anomalies_csv,
                "{},{},{},{},{},{},{},{},{:?}",
                s.row.scan_id,
                s.wakeup_jitter_ns,
                s.eval_ipc_ns,
                s.publish_ns,
                s.cycle_dur_ns,
                s.ram_swap_ns,
                s.row.active_rev,
                u8::from(s.row.deadline_miss),
                s.row.fallback_reason
            );
        }
    }

    fn write_os_sample(&mut self, os: &OsSample) {
        let _ = writeln!(
            self.samples_csv,
            "{},{},{},{},{},{},{},{}",
            os.ts_unix_ms,
            os.scan_id,
            os.supervisor_rss_kb,
            os.supervisor_pss_kb,
            os.worker_rss_kb,
            os.worker_pss_kb,
            os.mem_available_kb,
            os.temp_soc_mc
        );
    }

    fn write_event(&mut self, ev: &NightEvent) {
        if let Ok(line) = serde_json::to_string(ev) {
            let _ = writeln!(self.events_jsonl, "{line}");
            let _ = self.events_jsonl.flush();
        }
    }

    fn flush_streams(&mut self) {
        let _ = self.scans_csv.flush();
        let _ = self.anomalies_csv.flush();
        let _ = self.samples_csv.flush();
        let _ = self.events_jsonl.flush();
    }

    fn write_summary_and_hist(
        &self,
        sum: &NightSummarySnapshot,
        h_jitter: &LatencyHistogram,
        h_eval: &LatencyHistogram,
        h_pub: &LatencyHistogram,
        h_cycle: &LatencyHistogram,
        h_swap: &LatencyHistogram,
    ) {
        if let Ok(bytes) = serde_json::to_vec_pretty(sum) {
            let tmp = self.run_dir.join("summary.json.tmp");
            if fs::write(&tmp, &bytes).is_ok() {
                let _ = fs::rename(&tmp, self.run_dir.join("summary.json"));
            }
        }
        let hist_val = serde_json::json!({
            "runId": sum.run_id,
            "scansTotal": sum.scans_total,
            "wakeupJitterNs": { "summary": sum.wakeup_jitter, "buckets": h_jitter.buckets },
            "evalIpcNs": { "summary": sum.eval_ipc, "buckets": h_eval.buckets },
            "publishNs": { "summary": sum.publish, "buckets": h_pub.buckets },
            "cycleDurNs": { "summary": sum.cycle_dur, "buckets": h_cycle.buckets },
            "ramSwapNs": { "summary": sum.ram_swap, "buckets": h_swap.buckets },
        });
        if let Ok(bytes) = serde_json::to_vec_pretty(&hist_val) {
            let tmp = self.run_dir.join("histograms.json.tmp");
            if fs::write(&tmp, &bytes).is_ok() {
                let _ = fs::rename(&tmp, self.run_dir.join("histograms.json"));
            }
        }
    }
}

fn open_writers(meta: &NightRunMetadata) -> Option<DiskTelemetryWriters> {
    let base = meta.metrics_dir.as_ref()?;
    let run_dir = base.join(&meta.run_id);
    if fs::create_dir_all(&run_dir).is_err() {
        return None;
    }

    let manifest = serde_json::json!({
        "runId": meta.run_id,
        "deviceId": meta.device_id,
        "bootId": meta.boot_id,
        "ioMode": meta.io_mode,
        "serialBackend": meta.serial_backend,
        "lcdEnabled": meta.lcd_enabled,
        "isolatedWorker": meta.isolated_worker,
        "strictSandbox": meta.strict_sandbox,
        "scanPeriodMs": meta.scan_period_ms,
        "evalDeadlineMs": meta.eval_deadline_ms,
        "startedUnixMs": now_unix_ms(),
        "pid": std::process::id(),
    });
    if let Ok(b) = serde_json::to_vec_pretty(&manifest) {
        let _ = fs::write(run_dir.join("manifest.json"), b);
    }

    let open_append_csv = |name: &str, header: &str| -> Option<BufWriter<File>> {
        let path = run_dir.join(name);
        let existed = path.exists() && fs::metadata(&path).map(|m| m.len() > 0).unwrap_or(false);
        let mut f = OpenOptions::new().create(true).append(true).open(&path).ok()?;
        if !existed {
            let _ = writeln!(f, "{header}");
        }
        Some(BufWriter::with_capacity(16 * 1024, f))
    };

    let scans_csv = open_append_csv(
        "scans.csv",
        "scan_id,release_ns,start_ns,wakeup_jitter_ns,eval_ipc_ns,publish_ns,cycle_dur_ns,ram_swap_ns,active_rev,lease_epoch,deadline_miss,guard_reason,desired_mask,applied_mask,observed_mask",
    )?;
    let anomalies_csv = open_append_csv(
        "anomalies.csv",
        "scan_id,wakeup_jitter_ns,eval_ipc_ns,publish_ns,cycle_dur_ns,ram_swap_ns,active_rev,deadline_miss,guard_reason",
    )?;
    let samples_csv = open_append_csv(
        "samples.csv",
        "ts_unix_ms,scan_id,supervisor_rss_kb,supervisor_pss_kb,worker_rss_kb,worker_pss_kb,mem_available_kb,temp_soc_mc",
    )?;
    let events_jsonl = BufWriter::with_capacity(
        4096,
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("events.jsonl"))
            .ok()?,
    );

    Some(DiskTelemetryWriters {
        run_dir,
        scans_csv,
        anomalies_csv,
        samples_csv,
        events_jsonl,
        raw_scan_rows_written: 0,
    })
}

pub fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn sample_os_metrics(scan_id: u64, worker_pid: u32) -> OsSample {
    let (sup_rss, sup_pss) = read_proc_rss_pss(Path::new("/proc/self"));
    let (w_rss, w_pss) = if worker_pid > 0 {
        read_proc_rss_pss(&PathBuf::from(format!("/proc/{worker_pid}")))
    } else {
        (0, 0)
    };
    let mem_avail = read_mem_available_kb();
    let temp = read_thermal_mc();
    OsSample {
        ts_unix_ms: now_unix_ms(),
        scan_id,
        supervisor_rss_kb: sup_rss,
        supervisor_pss_kb: sup_pss,
        worker_rss_kb: w_rss,
        worker_pss_kb: w_pss,
        mem_available_kb: mem_avail,
        temp_soc_mc: temp,
    }
}

fn read_proc_rss_pss(proc_dir: &Path) -> (u64, u64) {
    let mut rss = 0u64;
    let mut pss = 0u64;
    if let Ok(rollup) = fs::read_to_string(proc_dir.join("smaps_rollup")) {
        for line in rollup.lines() {
            if let Some(rest) = line.strip_prefix("Rss:") {
                rss = parse_kb_field(rest);
            } else if let Some(rest) = line.strip_prefix("Pss:") {
                pss = parse_kb_field(rest);
            }
        }
        if rss > 0 || pss > 0 {
            return (rss, pss);
        }
    }
    if let Ok(status) = fs::read_to_string(proc_dir.join("status")) {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                rss = parse_kb_field(rest);
                return (rss, rss);
            }
        }
    }
    (0, 0)
}

fn read_mem_available_kb() -> u64 {
    if let Ok(info) = fs::read_to_string("/proc/meminfo") {
        for line in info.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                return parse_kb_field(rest);
            }
        }
    }
    0
}

fn read_thermal_mc() -> i64 {
    if let Ok(s) = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
        if let Ok(v) = s.trim().parse::<i64>() {
            return v;
        }
    }
    0
}

fn parse_kb_field(rest: &str) -> u64 {
    rest.split_whitespace()
        .next()
        .and_then(|tok| tok.parse::<u64>().ok())
        .unwrap_or(0)
}
