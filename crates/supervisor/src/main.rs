use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use control_abi::crypto::verify_site_profile_signature;
use control_abi::schema::{ModulePackageV1, SiteProfileV1};
use field_io::{DisplayBridge, FieldIoDriver, LyraHardwareDriver, VirtualBenchDriver};
use supervisor::api::AuthTokens;
use supervisor::clock::{ControlClock, MonotonicClock};
use supervisor::deploy::DeploymentManager;
use supervisor::journal::DurableJournal;
use supervisor::runtime::{serve_decoupled_http, DecoupledRuntime, DecoupledRuntimeConfig};
use supervisor::storage::{BoundaryEpochTracker, StorageWriter};
use supervisor::telemetry_out::NightRunMetadata;
use supervisor::worker_pool::{engine_kind_from_str, WorkerSpawnSpec};

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sig(_sig: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::Release);
}

fn gen_boot_id() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
}

fn main() {
    unsafe {
        libc::signal(libc::SIGINT, handle_sig as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handle_sig as *const () as libc::sighandler_t);
    }

    let args: Vec<String> = std::env::args().collect();
    let mut profile_path = PathBuf::from("/userdata/plc-auto/profiles/site-profile.json");
    let mut pub_key_path = PathBuf::from("/userdata/plc-auto/profiles/test-bench.pub.hex");
    let mut data_dir = PathBuf::from("/userdata/plc-auto/data");
    let mut initial_pkg_path: Option<PathBuf> = None;
    let mut console_dir: Option<PathBuf> = None;
    let mut bind_addr = String::from("0.0.0.0:8443");
    let mut hardware_gpio = false;
    let mut modbus_dev: Option<String> = None;
    let mut no_lcd = false;
    let mut silent_night = false;
    let mut engine_str = String::from("wamr-aot");
    let mut in_process = false;
    let mut force_worker = false;
    let mut worker_bin: Option<PathBuf> = None;
    let mut strict_sandbox_override: Option<bool> = None;
    let mut worker_mem_bytes: u64 = 16 * 1024 * 1024;
    let mut worker_fault_args: Vec<String> = Vec::new();
    let mut auto_arm = false;
    let mut rt_fifo = false;
    let mut journal_delay_ms: u64 = 0;
    let mut metrics_dir: Option<PathBuf> = None;
    let mut run_id: Option<String> = None;
    let mut max_raw_scan_rows: u64 = 1_000_000;
    let mut stop_after_s: Option<u64> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--profile" => {
                i += 1;
                profile_path = PathBuf::from(&args[i]);
            }
            "--pubkey" => {
                i += 1;
                pub_key_path = PathBuf::from(&args[i]);
            }
            "--data-dir" => {
                i += 1;
                data_dir = PathBuf::from(&args[i]);
            }
            "--initial-pkg" => {
                i += 1;
                initial_pkg_path = Some(PathBuf::from(&args[i]));
            }
            "--console-dir" => {
                i += 1;
                console_dir = Some(PathBuf::from(&args[i]));
            }
            "--bind" => {
                i += 1;
                bind_addr = args[i].clone();
            }
            "--hardware-gpio" => hardware_gpio = true,
            "--modbus-dev" => {
                i += 1;
                modbus_dev = Some(args[i].clone());
            }
            "--no-lcd" => no_lcd = true,
            "--silent-night" => {
                silent_night = true;
                no_lcd = true;
            }
            "--engine" => {
                i += 1;
                engine_str = args[i].clone();
            }
            "--in-process" => in_process = true,
            "--force-worker" => force_worker = true,
            "--worker-bin" => {
                i += 1;
                worker_bin = Some(PathBuf::from(&args[i]));
            }
            "--strict-sandbox" => strict_sandbox_override = Some(true),
            "--no-strict-sandbox" => strict_sandbox_override = Some(false),
            "--worker-mem-bytes" => {
                i += 1;
                worker_mem_bytes = args[i].parse().expect("parse --worker-mem-bytes");
            }
            "--worker-fault-arg" => {
                i += 1;
                worker_fault_args.push(args[i].clone());
            }
            "--auto-arm" => auto_arm = true,
            "--rt-fifo" => rt_fifo = true,
            "--journal-delay-ms" => {
                i += 1;
                journal_delay_ms = args[i].parse().expect("parse --journal-delay-ms");
            }
            "--metrics-dir" => {
                i += 1;
                metrics_dir = Some(PathBuf::from(&args[i]));
            }
            "--run-id" => {
                i += 1;
                run_id = Some(args[i].clone());
            }
            "--max-raw-scan-rows" => {
                i += 1;
                max_raw_scan_rows = args[i].parse().expect("parse --max-raw-scan-rows");
            }
            "--stop-after-s" => {
                i += 1;
                stop_after_s = Some(args[i].parse().expect("parse --stop-after-s"));
            }
            _ => {}
        }
        i += 1;
    }

    if silent_night && (hardware_gpio || modbus_dev.is_some()) {
        eprintln!("FATAL: --silent-night forbids --hardware-gpio and --modbus-dev (physical I/O disabled)");
        std::process::exit(2);
    }
    if auto_arm && hardware_gpio {
        eprintln!("FATAL: --auto-arm is only allowed in virtual I/O mode (forbidden with --hardware-gpio)");
        std::process::exit(2);
    }

    let engine_kind = engine_kind_from_str(&engine_str).expect("invalid --engine");
    let profile: SiteProfileV1 =
        serde_json::from_str(&fs::read_to_string(&profile_path).expect("read profile"))
            .expect("parse profile");
    let pub_hex = fs::read_to_string(&pub_key_path)
        .expect("read pubkey")
        .trim()
        .to_string();
    verify_site_profile_signature(&profile, &pub_hex).expect("verify profile signature");

    let boot_id = gen_boot_id();

    let mut journal = DurableJournal::open(&data_dir).expect("open journal");
    let mut recovered = journal
        .recover_on_boot(boot_id)
        .expect("recover journal on boot");

    // Seed initial package strictly on a fresh store without re-running boot recovery over crash flags
    if recovered.last_stable_package.is_none() {
        if let Some(ref ppath) = initial_pkg_path {
            let preserved_review = recovered.needs_operator_review;
            drop(journal);
            {
                let seed_journal = DurableJournal::open(&data_dir).expect("open seed journal");
                let mut dm = DeploymentManager::init(
                    profile.clone(),
                    pub_hex.clone(),
                    seed_journal,
                    boot_id,
                )
                .expect("init seed deployment manager");
                if dm.active_package.is_none() {
                    let pkg: ModulePackageV1 = serde_json::from_str(
                        &fs::read_to_string(ppath).expect("read initial pkg"),
                    )
                    .expect("parse initial pkg");
                    let empty_arena = supervisor::arena::StateArena::new(0);
                    dm.stage_and_verify("boot-init-m1".into(), pkg, &empty_arena)
                        .expect("verify initial pkg");
                    let pkg_hash = dm.transactions["boot-init-m1"]
                        .revision
                        .package_hash
                        .clone();
                    dm.authorize(
                        "boot-init-m1",
                        "system-boot".into(),
                        "Engineer".into(),
                        &pkg_hash,
                        0,
                        60_000,
                    )
                    .expect("authorize initial pkg");
                    let act = dm
                        .prepare_activation("boot-init-m1", 0, &empty_arena)
                        .expect("prepare init")
                        .expect("pending init");
                    dm.commit_boundary_activation(&act, 0)
                        .expect("commit init");
                }
            }
            journal = DurableJournal::open(&data_dir).expect("reopen journal after seed");
            recovered = journal
                .recover_on_boot(boot_id)
                .expect("recover journal after seed");
            recovered.needs_operator_review |= preserved_review;
        }
    }
    let initial_rev = recovered
        .last_stable_revision
        .as_ref()
        .map(|r| r.revision)
        .unwrap_or(0);
    let initially_persisted = recovered.last_stable_package.is_some();

    let boundary_epoch = Arc::new(BoundaryEpochTracker::new(boot_id, initial_rev, 1));
    let storage = StorageWriter::start(
        journal,
        Arc::clone(&boundary_epoch),
        initial_rev,
        initially_persisted,
        journal_delay_ms,
    );

    let (driver, virtual_bench_shared): (
        Box<dyn FieldIoDriver>,
        Option<Arc<Mutex<field_io::driver::VirtualBenchState>>>,
    ) = if hardware_gpio {
        let hw = LyraHardwareDriver::open(
            boot_id,
            "/dev/gpiochip0",
            17,
            18,
            modbus_dev.as_deref(),
            1,
            9600,
            1500,
        )
        .expect("open LyraHardwareDriver");
        (Box::new(hw), None)
    } else {
        let vb = VirtualBenchDriver::new(boot_id);
        let shared = Arc::clone(&vb.shared);
        (Box::new(vb), Some(shared))
    };

    let io_mode = if hardware_gpio { "hardware" } else { "virtual" }.to_string();
    let serial_backend = modbus_dev.clone().unwrap_or_else(|| "none".into());
    let lcd_enabled = !no_lcd;

    let mut worker_spec = WorkerSpawnSpec::default();
    worker_spec.engine_kind = engine_kind;
    worker_spec.worker_bin = worker_bin;
    worker_spec.in_process = in_process;
    worker_spec.force_worker = force_worker;
    if let Some(s) = strict_sandbox_override {
        worker_spec.strict_sandbox = s;
    }
    worker_spec.eval_deadline_ms = profile.eval_deadline_ms.max(1) as u64;
    worker_spec.memory_max_bytes = worker_mem_bytes;
    worker_spec.rt_fifo_active_worker = rt_fifo;
    worker_spec.extra_args = worker_fault_args;

    let run_id_final = run_id.unwrap_or_else(|| format!("run-{boot_id}"));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let metrics_meta = NightRunMetadata {
        run_id: run_id_final.clone(),
        device_id: profile.device_id.clone(),
        boot_id,
        io_mode: io_mode.clone(),
        serial_backend: serial_backend.clone(),
        lcd_enabled,
        isolated_worker: worker_spec.should_isolate_active(),
        strict_sandbox: worker_spec.strict_sandbox,
        scan_period_ms: profile.scan_period_ms,
        eval_deadline_ms: profile.eval_deadline_ms,
        metrics_dir,
        max_raw_scan_rows,
    };

    let clock = Arc::new(MonotonicClock);
    let runtime = DecoupledRuntime::start(DecoupledRuntimeConfig {
        profile: profile.clone(),
        trusted_pub_hex: pub_hex,
        boot_id,
        recovered,
        storage,
        boundary_epoch,
        driver,
        virtual_bench_shared,
        clock: clock as Arc<dyn ControlClock>,
        worker_spec: worker_spec.clone(),
        io_mode: io_mode.clone(),
        serial_backend: serial_backend.clone(),
        lcd_enabled,
        auto_arm,
        rt_fifo,
        metrics_meta,
        console_dir,
        tokens: AuthTokens::default(),
        stop_flag: Some(Arc::clone(&stop_flag)),
    })
    .expect("start decoupled runtime");

    let listener = TcpListener::bind(&bind_addr).expect("bind HTTP API");
    let rt_http = Arc::clone(&runtime);
    let stop_http = Arc::clone(&stop_flag);
    let http_thread = thread::Builder::new()
        .name("api-http-accept".into())
        .spawn(move || serve_decoupled_http(listener, rt_http, stop_http))
        .expect("spawn http thread");

    let lcd_thread = if lcd_enabled {
        let display = DisplayBridge::new("/var/run/kvm_lyra_plc_display.sock");
        let rt_disp = Arc::clone(&runtime);
        let stop_disp = Arc::clone(&stop_flag);
        thread::Builder::new()
            .name("lcd-bridge".into())
            .spawn(move || {
                while !stop_disp.load(Ordering::Acquire) {
                    let rev = rt_disp.scalars.active_revision.load(Ordering::Acquire);
                    let text = {
                        let snap = rt_disp.telemetry_handle.snapshot.read().unwrap();
                        if let Some(sig) = snap.latest_row.as_ref().and_then(|r| {
                            r.signals
                                .iter()
                                .find(|s| s.id == control_abi::SIGNAL_TEMP_PROCESS)
                        }) {
                            format!("T:{:.1}C {:?} R{}", sig.value as f64 / 10.0, sig.quality, rev)
                        } else {
                            format!("R{rev}")
                        }
                    };
                    let _ = display.try_update_label("Monitor_CPU_Temp_Label", &text);
                    thread::sleep(Duration::from_secs(2));
                }
            })
            .ok()
    } else {
        None
    };

    let active_wpid = runtime.scalars.active_worker_pid.load(Ordering::Acquire);
    println!(
        "plc-supervisor started: device={} boot_id={} api={} io_mode={} serial_backend={} lcd_enabled={} isolated_worker={} strict_sandbox={} armed={} supervisor_pid={} active_worker_pid={} run_id={}",
        profile.device_id,
        boot_id,
        bind_addr,
        io_mode,
        serial_backend,
        lcd_enabled,
        worker_spec.should_isolate_active(),
        worker_spec.strict_sandbox,
        runtime.scalars.armed.load(Ordering::Acquire),
        std::process::id(),
        active_wpid,
        run_id_final
    );

    let start_instant = Instant::now();
    while !STOP_REQUESTED.load(Ordering::Acquire) {
        if let Some(max_s) = stop_after_s {
            if start_instant.elapsed() >= Duration::from_secs(max_s) {
                break;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }

    stop_flag.store(true, Ordering::Release);
    runtime.request_stop();
    let _ = http_thread.join();
    if let Some(h) = lcd_thread {
        let _ = h.join();
    }
    if let Ok(mut owned) = Arc::try_unwrap(runtime) {
        owned.join_threads();
    }
}
