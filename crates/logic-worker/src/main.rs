use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
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
    HangOnEval,      // F01: sleep past cycle deadline without replying
    OomAlloc,        // F02: attempt large memory growth inside cgroup
    BadFrame,        // F03: return corrupted EvalResult
    EscapeSyscall,   // F06: test socket() + openat("/dev/gpiochip0") under seccomp
    SlowMs(u64),     // N1.1/N1.2: sleep <ms> and then emit a late EvalRes
    PartialEof,      // N1.2: write partial header/payload bytes and exit abruptly
    Backpressure,    // N1.1: stop draining socket FD completely to fill TX buffer
    Diverge,         // N1.3/N1.5: return valid EvalRes with mutated desired_mask
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
    let mut fault_after_evals: u64 = 0;

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
            "--fault-slow-ms" => {
                i += 1;
                if i < args.len() {
                    fault = FaultMode::SlowMs(args[i].parse().unwrap_or(50));
                }
            }
            "--fault-partial-eof" => fault = FaultMode::PartialEof,
            "--fault-backpressure" => fault = FaultMode::Backpressure,
            "--fault-diverge" => fault = FaultMode::Diverge,
            "--fault-after-evals" => {
                i += 1;
                if i < args.len() {
                    fault_after_evals = args[i].parse().unwrap_or(0);
                }
            }
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

    // Optional shrink RCVBUF on worker side when testing backpressure
    if matches!(fault, FaultMode::Backpressure) && fault_after_evals == 0 {
        let small: libc::c_int = 4096;
        unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &small as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        // Refuse to drain incoming socket bytes so parent write_frame_until hits backpressure timeout
        loop {
            thread::sleep(Duration::from_millis(50));
        }
    }

    // 3. Evaluation loop (zero extra FDs, no network, no filesystem opens)
    let mut tx_buf = Vec::with_capacity(4096);
    let mut eval_count: u64 = 0;
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
                eval_count += 1;
                let active_fault = if eval_count > fault_after_evals {
                    fault
                } else {
                    FaultMode::None
                };
                match active_fault {
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
                    FaultMode::SlowMs(ms) => {
                        thread::sleep(Duration::from_millis(ms));
                        let req = match decode_eval_request(&rx_buf) {
                            Ok(r) => r,
                            Err(e) => {
                                let _ = write_frame(&mut stream, IpcMessageType::Error, e.to_string().as_bytes());
                                continue;
                            }
                        };
                        match engine.eval_sync(&req) {
                            Ok(res) => {
                                if encode_eval_result(&res, &mut tx_buf).is_ok() {
                                    let _ = write_frame(&mut stream, IpcMessageType::EvalRes, &tx_buf);
                                }
                            }
                            Err(e) => {
                                let _ = write_frame(&mut stream, IpcMessageType::Error, e.to_string().as_bytes());
                            }
                        }
                        continue;
                    }
                    FaultMode::PartialEof => {
                        // Write truncated frame declaring 120 bytes of EvalRes, send only 8 bytes, then die
                        let mut partial = Vec::with_capacity(13);
                        partial.extend_from_slice(&120u32.to_le_bytes());
                        partial.push(IpcMessageType::EvalRes as u8);
                        partial.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04]);
                        let _ = stream.write_all(&partial);
                        let _ = stream.flush();
                        return 1;
                    }
                    FaultMode::Backpressure => {
                        loop {
                            thread::sleep(Duration::from_millis(50));
                        }
                    }
                    FaultMode::Diverge | FaultMode::None => {}
                }

                let req = match decode_eval_request(&rx_buf) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = write_frame(&mut stream, IpcMessageType::Error, e.to_string().as_bytes());
                        continue;
                    }
                };
                match engine.eval_sync(&req) {
                    Ok(mut res) => {
                        if matches!(active_fault, FaultMode::Diverge) {
                            res.desired_mask ^= 1;
                        }
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
