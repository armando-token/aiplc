use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Percentiles {
    pub count: usize,
    pub min_ns: u64,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub p999_ns: u64,
    pub max_ns: u64,
    pub mean_ns: u64,
}

pub fn compute_percentiles(samples: &[u64]) -> Percentiles {
    if samples.is_empty() {
        return Percentiles {
            count: 0,
            min_ns: 0,
            p50_ns: 0,
            p95_ns: 0,
            p99_ns: 0,
            p999_ns: 0,
            max_ns: 0,
            mean_ns: 0,
        };
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let at = |q: f64| -> u64 {
        let idx = ((n as f64 - 1.0) * q).round() as usize;
        sorted[idx.min(n - 1)]
    };
    let sum: u128 = sorted.iter().map(|&x| x as u128).sum();
    Percentiles {
        count: n,
        min_ns: sorted[0],
        p50_ns: at(0.50),
        p95_ns: at(0.95),
        p99_ns: at(0.99),
        p999_ns: at(0.999),
        max_ns: sorted[n - 1],
        mean_ns: (sum / (n as u128)) as u64,
    }
}

#[derive(Debug, Clone)]
pub struct ScanCsvRow {
    pub run_id: String,
    pub mode: String,
    pub scan_id: u64,
    pub release_ns: u64,
    pub start_ns: u64,
    pub wakeup_jitter_ns: i64,
    pub input_seq: u64,
    pub eval_start_ns: u64,
    pub eval_end_ns: u64,
    pub eval_dur_ns: u64,
    pub decision_ns: u64,
    pub publish_ns: u64,
    pub ack_driver_ns: u64,
    pub cycle_dur_ns: u64,
    pub active_rev: u64,
    pub deadline_miss: bool,
    pub fallback_reason: String,
    pub applied_mask: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSample {
    pub ts_ns: u64,
    pub rss_kb: u64,
    pub pss_kb: u64,
    pub vmhwm_kb: u64,
    pub cgroup_current_bytes: u64,
    pub cgroup_peak_bytes: u64,
    pub oom_kills: u64,
    pub mem_available_kb: u64,
    pub temp_soc_mc: i64,
    pub dropped_samples: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchManifest {
    pub run_id: String,
    pub host_id: String,
    pub kernel: String,
    pub arch: String,
    pub engine: String,
    pub workload: String,
    pub mode: String,
    pub started_unix_s: u64,
    pub total_scans: usize,
    pub deadline_misses: usize,
    pub rt_scheduler: String,
}

pub struct OffScanTelemetryRecorder {
    scan_tx: mpsc::SyncSender<ScanCsvRow>,
    dropped: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<(Vec<ScanCsvRow>, Vec<ResourceSample>, Vec<String>)>>,
    event_tx: mpsc::SyncSender<String>,
}

impl OffScanTelemetryRecorder {
    pub fn start(target_pid: Arc<AtomicU64>, cgroup_dir: Option<String>) -> Self {
        let (scan_tx, scan_rx) = mpsc::sync_channel::<ScanCsvRow>(8192);
        let (event_tx, event_rx) = mpsc::sync_channel::<String>(512);
        let dropped = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_c = Arc::clone(&stop);
        let dropped_c = Arc::clone(&dropped);

        let handle = thread::Builder::new()
            .name("bench-telemetry".into())
            .spawn(move || {
                let mut scans = Vec::new();
                let mut samples = Vec::new();
                let mut events = Vec::new();
                let mut tick = 0u64;

                while !stop_c.load(Ordering::Acquire) {
                    while let Ok(r) = scan_rx.try_recv() {
                        scans.push(r);
                    }
                    while let Ok(ev) = event_rx.try_recv() {
                        events.push(ev);
                    }
                    if tick % 5 == 0 {
                        let pid = target_pid.load(Ordering::Acquire);
                        let pid_u32 = if pid == 0 { std::process::id() } else { pid as u32 };
                        samples.push(read_proc_sample(
                            pid_u32,
                            cgroup_dir.as_deref(),
                            dropped_c.load(Ordering::Relaxed),
                        ));
                    }
                    tick += 1;
                    thread::sleep(Duration::from_millis(10));
                }
                while let Ok(r) = scan_rx.try_recv() {
                    scans.push(r);
                }
                while let Ok(ev) = event_rx.try_recv() {
                    events.push(ev);
                }
                let pid = target_pid.load(Ordering::Acquire);
                let pid_u32 = if pid == 0 { std::process::id() } else { pid as u32 };
                samples.push(read_proc_sample(
                    pid_u32,
                    cgroup_dir.as_deref(),
                    dropped_c.load(Ordering::Relaxed),
                ));
                (scans, samples, events)
            })
            .expect("spawn telemetry thread");

        Self {
            scan_tx,
            dropped,
            stop,
            handle: Some(handle),
            event_tx,
        }
    }

    pub fn try_record_scan(&self, row: ScanCsvRow) {
        if self.scan_tx.try_send(row).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_event(&self, json_line: String) {
        let _ = self.event_tx.try_send(json_line);
    }

    pub fn finish_and_write(
        mut self,
        out_dir: &Path,
        manifest: BenchManifest,
    ) -> std::io::Result<serde_json::Value> {
        self.stop.store(true, Ordering::Release);
        let (scans, samples, events) = self.handle.take().unwrap().join().unwrap();
        fs::create_dir_all(out_dir)?;

        // 1. scans.csv (18 exact columns)
        {
            let f = File::create(out_dir.join("scans.csv"))?;
            let mut w = BufWriter::new(f);
            writeln!(
                w,
                "run_id,mode,scan_id,release_ns,start_ns,wakeup_jitter_ns,input_seq,eval_start_ns,eval_end_ns,eval_dur_ns,decision_ns,publish_ns,ack_driver_ns,cycle_dur_ns,active_rev,deadline_miss,fallback_reason,applied_mask"
            )?;
            for s in &scans {
                writeln!(
                    w,
                    "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                    s.run_id,
                    s.mode,
                    s.scan_id,
                    s.release_ns,
                    s.start_ns,
                    s.wakeup_jitter_ns,
                    s.input_seq,
                    s.eval_start_ns,
                    s.eval_end_ns,
                    s.eval_dur_ns,
                    s.decision_ns,
                    s.publish_ns,
                    s.ack_driver_ns,
                    s.cycle_dur_ns,
                    s.active_rev,
                    s.deadline_miss,
                    s.fallback_reason,
                    s.applied_mask
                )?;
            }
            w.flush()?;
        }

        // 2. events.jsonl
        {
            let mut f = File::create(out_dir.join("events.jsonl"))?;
            for ev in &events {
                writeln!(f, "{ev}")?;
            }
        }

        // 3. samples.csv
        {
            let f = File::create(out_dir.join("samples.csv"))?;
            let mut w = BufWriter::new(f);
            writeln!(
                w,
                "ts_ns,rss_kb,pss_kb,vmhwm_kb,cgroup_current_bytes,cgroup_peak_bytes,oom_kills,mem_available_kb,temp_soc_mc,dropped_samples"
            )?;
            for sm in &samples {
                writeln!(
                    w,
                    "{},{},{},{},{},{},{},{},{},{}",
                    sm.ts_ns,
                    sm.rss_kb,
                    sm.pss_kb,
                    sm.vmhwm_kb,
                    sm.cgroup_current_bytes,
                    sm.cgroup_peak_bytes,
                    sm.oom_kills,
                    sm.mem_available_kb,
                    sm.temp_soc_mc,
                    sm.dropped_samples
                )?;
            }
            w.flush()?;
        }

        let jitters: Vec<u64> = scans
            .iter()
            .map(|s| s.wakeup_jitter_ns.unsigned_abs())
            .collect();
        let evals: Vec<u64> = scans.iter().map(|s| s.eval_dur_ns).collect();
        let cycles: Vec<u64> = scans.iter().map(|s| s.cycle_dur_ns).collect();

        let p_jitter = compute_percentiles(&jitters);
        let p_eval = compute_percentiles(&evals);
        let p_cycle = compute_percentiles(&cycles);

        // 4. histograms.json
        let histograms = serde_json::json!({
            "wakeup_jitter_ns": p_jitter,
            "eval_dur_ns": p_eval,
            "cycle_dur_ns": p_cycle,
        });
        fs::write(
            out_dir.join("histograms.json"),
            serde_json::to_vec_pretty(&histograms).unwrap(),
        )?;

        let peak_rss_kb = samples.iter().map(|s| s.rss_kb).max().unwrap_or(0);
        let peak_pss_kb = samples.iter().map(|s| s.pss_kb).max().unwrap_or(0);
        let min_mem_avail_kb = samples.iter().map(|s| s.mem_available_kb).min().unwrap_or(0);
        let max_temp_mc = samples.iter().map(|s| s.temp_soc_mc).max().unwrap_or(0);

        // 5. manifest.json
        fs::write(
            out_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )?;

        // 6. summary.json
        let summary = serde_json::json!({
            "runId": manifest.run_id,
            "engine": manifest.engine,
            "workload": manifest.workload,
            "mode": manifest.mode,
            "totalScans": scans.len(),
            "deadlineMisses": manifest.deadline_misses,
            "wakeupJitterNs": p_jitter,
            "evalDurNs": p_eval,
            "cycleDurNs": p_cycle,
            "peakRssKb": peak_rss_kb,
            "peakPssKb": peak_pss_kb,
            "minMemAvailableKb": min_mem_avail_kb,
            "maxTempSocMc": max_temp_mc,
            "droppedSamples": self.dropped.load(Ordering::Relaxed),
        });
        fs::write(
            out_dir.join("summary.json"),
            serde_json::to_vec_pretty(&summary).unwrap(),
        )?;

        // 7. report.md
        let md = format!(
            "# Benchmark Report: {run_id}\n\n\
            - Host: `{host}` (`{arch}`, kernel `{kernel}`, RT `{rt}`)\n\
            - Engine: `{eng}` | Workload: `{wl}` | Mode: `{mode}`\n\
            - Scans: `{scans}` | Deadline misses: `{miss}`\n\
            - Eval duration (ns): p50={ep50}, p95={ep95}, p99={ep99}, p99.9={ep999}, max={emax}\n\
            - Cycle duration (ns): p50={cp50}, p99={cp99}, max={cmax}\n\
            - Peak RSS: `{rss}` KB | Peak PSS: `{pss}` KB | Min MemAvailable: `{avail}` KB | Max SoC Temp: `{temp}` mC\n",
            run_id = manifest.run_id,
            host = manifest.host_id,
            arch = manifest.arch,
            kernel = manifest.kernel,
            rt = manifest.rt_scheduler,
            eng = manifest.engine,
            wl = manifest.workload,
            mode = manifest.mode,
            scans = scans.len(),
            miss = manifest.deadline_misses,
            ep50 = p_eval.p50_ns,
            ep95 = p_eval.p95_ns,
            ep99 = p_eval.p99_ns,
            ep999 = p_eval.p999_ns,
            emax = p_eval.max_ns,
            cp50 = p_cycle.p50_ns,
            cp99 = p_cycle.p99_ns,
            cmax = p_cycle.max_ns,
            rss = peak_rss_kb,
            pss = peak_pss_kb,
            avail = min_mem_avail_kb,
            temp = max_temp_mc,
        );
        fs::write(out_dir.join("report.md"), md)?;

        let _ = OpenOptions::new().write(true).open(out_dir.join("summary.json"));
        Ok(summary)
    }
}

fn parse_kb_field(content: &str, prefix: &str) -> u64 {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            if let Some(num) = rest.split_whitespace().next().and_then(|s| s.parse::<u64>().ok()) {
                return num;
            }
        }
    }
    0
}

pub fn read_proc_sample(pid: u32, cgroup_dir: Option<&str>, dropped_samples: u64) -> ResourceSample {
    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let status_s = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let rss_kb = parse_kb_field(&status_s, "VmRSS:");
    let vmhwm_kb = parse_kb_field(&status_s, "VmHWM:");

    let smaps_s = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap_or_default();
    let pss_kb = parse_kb_field(&smaps_s, "Pss:");

    let meminfo_s = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mem_available_kb = parse_kb_field(&meminfo_s, "MemAvailable:");

    let temp_soc_mc = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);

    let (cgroup_current_bytes, cgroup_peak_bytes, oom_kills) = if let Some(cg) = cgroup_dir {
        let cur = fs::read_to_string(format!("{cg}/memory.current"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let peak = fs::read_to_string(format!("{cg}/memory.peak"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let evs = fs::read_to_string(format!("{cg}/memory.events")).unwrap_or_default();
        let oom = evs
            .lines()
            .find_map(|l| l.strip_prefix("oom_kill "))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        (cur, peak, oom)
    } else {
        (0, 0, 0)
    };

    ResourceSample {
        ts_ns,
        rss_kb,
        pss_kb,
        vmhwm_kb,
        cgroup_current_bytes,
        cgroup_peak_bytes,
        oom_kills,
        mem_available_kb,
        temp_soc_mc,
        dropped_samples,
    }
}

pub fn kernel_release() -> String {
    #[cfg(unix)]
    {
        let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
        if unsafe { libc::uname(&mut uts) } == 0 {
            let s = unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr()) };
            return s.to_string_lossy().into_owned();
        }
    }
    "unknown".into()
}

pub fn hostname_str() -> String {
    #[cfg(unix)]
    {
        let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
        if unsafe { libc::uname(&mut uts) } == 0 {
            let s = unsafe { std::ffi::CStr::from_ptr(uts.nodename.as_ptr()) };
            return s.to_string_lossy().into_owned();
        }
    }
    "localhost".into()
}
