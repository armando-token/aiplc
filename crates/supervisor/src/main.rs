use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use control_abi::crypto::verify_site_profile_signature;
use control_abi::schema::{ModulePackageV1, SiteProfileV1};
use field_io::{DisplayBridge, FieldIoDriver, LyraHardwareDriver, VirtualBenchDriver};
use logic_worker::engine::{A0NativeIrEngine, A1WamrEngine, A2QuickJsEngine, LogicEngine};
use supervisor::api::{serve_http_blocking, ApiContext, AuthTokens};
use supervisor::clock::{ControlClock, MonotonicClock};
use supervisor::deploy::DeploymentManager;
use supervisor::journal::DurableJournal;
use supervisor::scan_loop::ScanSupervisor;

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
    let mut engine_kind = String::from("wamr-aot");
    let mut auto_arm = false;

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
            "--engine" => {
                i += 1;
                engine_kind = args[i].clone();
            }
            "--auto-arm" => auto_arm = true,
            _ => {}
        }
        i += 1;
    }

    let profile: SiteProfileV1 =
        serde_json::from_str(&fs::read_to_string(&profile_path).expect("read profile"))
            .expect("parse profile");
    let pub_hex = fs::read_to_string(&pub_key_path)
        .expect("read pubkey")
        .trim()
        .to_string();
    verify_site_profile_signature(&profile, &pub_hex).expect("verify profile signature");

    let boot_id = gen_boot_id();
    let journal = DurableJournal::open(&data_dir).expect("open journal");
    let mut deploy = DeploymentManager::init(profile.clone(), pub_hex.clone(), journal, boot_id)
        .expect("init deployment manager");

    // If no active package was recovered from journal and initial_pkg provided, stage & promote cleanly
    if deploy.active_package.is_none() {
        if let Some(ref ppath) = initial_pkg_path {
            let pkg: ModulePackageV1 =
                serde_json::from_str(&fs::read_to_string(ppath).expect("read initial pkg"))
                    .expect("parse initial pkg");
            let empty_arena = supervisor::arena::StateArena::new(0);
            deploy
                .stage_and_verify("boot-init-m1".into(), pkg, &empty_arena)
                .expect("verify initial pkg");
            let pkg_hash = deploy.transactions["boot-init-m1"]
                .revision
                .package_hash
                .clone();
            deploy
                .authorize("boot-init-m1", "system-boot".into(), "Engineer".into(), &pkg_hash, 0, 60_000)
                .expect("authorize initial pkg");
            let act = deploy
                .prepare_activation("boot-init-m1", 0, &empty_arena)
                .expect("prepare init")
                .expect("pending init");
            deploy
                .commit_boundary_activation(&act, 0)
                .expect("commit init");
        }
    }

    let clock = Arc::new(MonotonicClock);
    let active_pkg = deploy.active_package.clone();
    let mut active_eng: Box<dyn LogicEngine> = match engine_kind.as_str() {
        "wamr-aot" => Box::new(A1WamrEngine::new(true)),
        "wamr-interp" => Box::new(A1WamrEngine::new(false)),
        "quickjs" => Box::new(A2QuickJsEngine::new(5_000_000)),
        _ => Box::new(A0NativeIrEngine::new_ir()),
    };
    if let Some(ref pkg) = active_pkg {
        active_eng
            .prepare(pkg, &pub_hex)
            .and_then(|_| active_eng.warm())
            .expect("prepare/warm active engine");
    }

    let driver: Box<dyn FieldIoDriver> = if hardware_gpio {
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
        Box::new(hw)
    } else {
        Box::new(VirtualBenchDriver::new(boot_id))
    };

    // Wrapper delegating Box<dyn FieldIoDriver>
    struct BoxDriver(Box<dyn FieldIoDriver>);
    impl FieldIoDriver for BoxDriver {
        fn sample_inputs(&mut self, now_ns: u64) -> (u64, Vec<control_abi::InputSignal>) {
            self.0.sample_inputs(now_ns)
        }
        fn apply_output(
            &mut self,
            frame: &control_abi::OutputFrame,
            now_ns: u64,
        ) -> control_abi::Observation {
            self.0.apply_output(frame, now_ns)
        }
        fn force_safe_off(&mut self, now_ns: u64) -> control_abi::Observation {
            self.0.force_safe_off(now_ns)
        }
        fn health(&self) -> field_io::DriverHealth {
            self.0.health()
        }
    }

    let mut sup = ScanSupervisor::new(Arc::clone(&clock), BoxDriver(driver), profile.clone(), deploy, boot_id);
    if active_pkg.is_some() {
        sup.install_active_engine(active_eng);
    }
    if auto_arm && !sup.deploy.needs_operator_review {
        sup.arm_control();
    }

    let ctx = Arc::new(ApiContext::new(sup, AuthTokens::default(), console_dir));
    let ctx_api = Arc::clone(&ctx);

    let listener = TcpListener::bind(&bind_addr).expect("bind HTTP API");
    thread::Builder::new()
        .name("api-http".into())
        .spawn(move || serve_http_blocking(listener, ctx_api))
        .expect("spawn http thread");

    // Optional off-scan display bridge heartbeat (best effort)
    let display = DisplayBridge::new("/var/run/kvm_lyra_plc_display.sock");
    let ctx_disp = Arc::clone(&ctx);
    thread::Builder::new()
        .name("lcd-bridge".into())
        .spawn(move || {
            while !STOP_REQUESTED.load(Ordering::Acquire) {
                let text = {
                    let sup = ctx_disp.supervisor.lock().unwrap();
                    if let Some(sig) = sup.last_temp_signal() {
                        format!(
                            "T:{:.1}C {:?} R{}",
                            sig.value as f64 / 10.0,
                            sig.quality,
                            sup.deploy.active_revision
                        )
                    } else {
                        format!("R{}", sup.deploy.active_revision)
                    }
                };
                let _ = display.try_update_label("Monitor_CPU_Temp_Label", &text);
                thread::sleep(Duration::from_secs(2));
            }
        })
        .ok();

    println!(
        "plc-supervisor started: device={} boot_id={} api={} hardware_gpio={} armed={}",
        profile.device_id, boot_id, bind_addr, hardware_gpio, auto_arm
    );

    let period_ns = (profile.scan_period_ms as u64) * 1_000_000;
    let mut next_tick = clock.now_ns() + period_ns;
    while !STOP_REQUESTED.load(Ordering::Acquire) {
        {
            let mut sup = ctx.supervisor.lock().unwrap();
            sup.step_once();
        }
        clock.sleep_until_ns(next_tick);
        next_tick += period_ns;
        let now = clock.now_ns();
        if now > next_tick + period_ns {
            next_tick = now + period_ns;
        }
    }

    {
        let mut sup = ctx.supervisor.lock().unwrap();
        sup.disarm_and_safe_off();
    }
    println!("plc-supervisor stopped safely with outputs OFF.");
}
