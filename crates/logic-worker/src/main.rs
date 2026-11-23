use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;

use control_abi::wire::{decode_eval_request, encode_eval_result, IpcMessageType};
use logic_worker::engine::{
    A0NativeIrEngine, A1WamrEngine, A2QuickJsEngine, EngineKind, LogicEngine,
};
use logic_worker::ipc::{
    read_frame_with_timeout, read_runtime_frame_with_timeout, sandbox_cfg_from_params, write_frame,
    WorkerInitAck, WorkerInitPayload,
};
use logic_worker::sandbox::{close_inherited_fds_except, harden_worker_process};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FaultMode {
    None,
    HangOnEval,      // F01: sleep past cycle deadline
    OomAlloc,        // F02: attempt large memory growth inside cgroup
    BadFrame,        // F03: return corrupted EvalResult
    EscapeSyscall,   // F06: test socket() + openat("/dev/gpiochip0") under seccomp
}

fn main() {
    #[cfg(target_os = "macos")]
    {
        let handle = thread::Builder::new()
            .name("plc-worker-rt".into())
            .stack_size(4 * 1024 * 1024)
            .spawn(run_worker)
            .expect("spawn worker runtime thread");
        let code = handle.join().unwrap_or(1);
        std::process::exit(code);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let code = run_worker();
        std::process::exit(code);
    }
}

fn run_worker() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let mut ipc_fd: Option<RawFd> = None;
    let mut fault = FaultMode::None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--ipc-fd" => {
                i += 1;
                if i < args.len() {
                    ipc_fd = args[i].parse().ok();
                }
            }
            "--fault-hang" => fault = FaultMode::HangOnEval,
            "--fault-oom" => fault = FaultMode::OomAlloc,
            "--fault-bad-frame" => fault = FaultMode::BadFrame,
            "--fault-escape" => fault = FaultMode::EscapeSyscall,
            _ => {}
        }
        i += 1;
    }

    let fd = match ipc_fd {
        Some(f) => f,
        None => {
            eprintln!("usage: plc-worker --ipc-fd <fd>");
            return 2;
        }
    };
    close_inherited_fds_except(&[fd, 2]);

    let mut stream = unsafe { UnixStream::from_raw_fd(fd) };
    let mut rx_buf = Vec::with_capacity(16 * 1024);

    // 1. Receive InitModule before locking down seccomp (needs malloc/warm)
    let msg = match read_frame_with_timeout(&mut stream, Some(Duration::from_secs(5)), &mut rx_buf) {
        Ok(m) => m,
        Err(e) => {
            let _ = write_frame(&mut stream, IpcMessageType::Error, e.to_string().as_bytes());
            return 1;
        }
    };
    if msg != IpcMessageType::InitModule {
        let _ = write_frame(&mut stream, IpcMessageType::Error, b"expected InitModule");
        return 1;
    }

    let init: WorkerInitPayload = match serde_json::from_slice(&rx_buf) {
        Ok(v) => v,
        Err(e) => {
            let _ = write_frame(&mut stream, IpcMessageType::Error, e.to_string().as_bytes());
            return 1;
        }
    };

    let mut engine: Box<dyn LogicEngine> = match init.engine {
        EngineKind::EmptyNoop => Box::new(A0NativeIrEngine::new_empty_noop()),
        EngineKind::NativeIr => Box::new(A0NativeIrEngine::new_ir()),
        EngineKind::WamrInterp => Box::new(A1WamrEngine::new(false)),
        EngineKind::WamrAot => Box::new(A1WamrEngine::new(true)),
        EngineKind::QuickJs => Box::new(A2QuickJsEngine::new(init.eval_budget_ns)),
        EngineKind::Wasmtime => {
            #[cfg(feature = "wasmtime-engine")]
            {
                match logic_worker::A3WasmtimeEngine::new(200_000) {
                    Ok(e) => Box::new(e),
                    Err(e) => {
                        let _ = write_frame(
                            &mut stream,
                            IpcMessageType::Error,
                            e.to_string().as_bytes(),
                        );
                        return 1;
                    }
                }
            }
            #[cfg(not(feature = "wasmtime-engine"))]
            {
                let _ = write_frame(
                    &mut stream,
                    IpcMessageType::Error,
                    b"wasmtime-engine feature not enabled on this target",
                );
                return 1;
            }
        }
    };

    if let Err(e) = engine
        .prepare(&init.package, &init.trusted_pub_hex)
        .and_then(|_| engine.warm())
    {
        let _ = write_frame(&mut stream, IpcMessageType::Error, e.to_string().as_bytes());
        return 1;
    }

    // 2. Apply hard sandbox (FD close, rlimits, UID nobody, NO_NEW_PRIVS, Seccomp-BPF)
    let sb_cfg = sandbox_cfg_from_params(&init.sandbox);
    let report = harden_worker_process(&sb_cfg, &[fd, 2]);
    let ack = WorkerInitAck {
        pid: std::process::id(),
        sandbox: report,
    };
    let ack_bytes = serde_json::to_vec(&ack).unwrap_or_default();
    if write_frame(&mut stream, IpcMessageType::InitAck, &ack_bytes).is_err() {
        return 1;
    }

    // 3. Evaluation loop (zero extra FDs, no network, no filesystem opens)
    let mut tx_buf = Vec::with_capacity(4096);
    loop {
        let m = match read_runtime_frame_with_timeout(&mut stream, None, &mut rx_buf) {
            Ok(m) => m,
            Err(_) => break,
        };
        match m {
            IpcMessageType::Ping => {
                let _ = write_frame(&mut stream, IpcMessageType::Pong, &[]);
            }
            IpcMessageType::Revoke => {
                engine.revoke();
                break;
            }
            IpcMessageType::EvalReq => {
                match fault {
                    FaultMode::HangOnEval => {
                        thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                    FaultMode::OomAlloc => {
                        // Touch pages until cgroup kills or allocation fails
                        let mut junk: Vec<Vec<u8>> = Vec::new();
                        for _ in 0..1024 {
                            let mut page = Vec::new();
                            if page.try_reserve_exact(1024 * 1024).is_err() {
                                break;
                            }
                            page.resize(1024 * 1024, 0xA5);
                            junk.push(page);
                        }
                        let _ = write_frame(&mut stream, IpcMessageType::Error, b"oom_attempted");
                        continue;
                    }
                    FaultMode::BadFrame => {
                        let _ = write_frame(&mut stream, IpcMessageType::EvalRes, b"BAD_TRUNCATED");
                        continue;
                    }
                    FaultMode::EscapeSyscall => {
                        // F06: attempt forbidden socket() and open("/dev/gpiochip0")
                        let sock_rc = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
                        let sock_err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                        let dev = std::ffi::CString::new("/dev/gpiochip0").unwrap();
                        let open_rc = unsafe { libc::open(dev.as_ptr(), libc::O_RDONLY) };
                        let open_err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                        let msg = format!(
                            "escape_blocked:socket_rc={sock_rc},socket_err={sock_err},open_rc={open_rc},open_err={open_err}"
                        );
                        let _ = write_frame(&mut stream, IpcMessageType::Error, msg.as_bytes());
                        continue;
                    }
                    FaultMode::None => {}
                }

                let req = match decode_eval_request(&rx_buf) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = write_frame(&mut stream, IpcMessageType::Error, e.to_string().as_bytes());
                        continue;
                    }
                };
                match engine.eval_sync(&req) {
                    Ok(res) => {
                        if let Err(e) = encode_eval_result(&res, &mut tx_buf) {
                            let _ = write_frame(&mut stream, IpcMessageType::Error, e.to_string().as_bytes());
                        } else {
                            let _ = write_frame(&mut stream, IpcMessageType::EvalRes, &tx_buf);
                        }
                    }
                    Err(e) => {
                        let _ = write_frame(&mut stream, IpcMessageType::Error, e.to_string().as_bytes());
                    }
                }
            }
            _ => {}
        }
    }
    0
}
