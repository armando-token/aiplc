use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use control_abi::schema::ModulePackageV1;
use control_abi::types::MAX_IPC_FRAME_BYTES;
use control_abi::wire::{decode_eval_result, encode_eval_request, EvalRequest, EvalResult, IpcMessageType};
use serde::{Deserialize, Serialize};

use crate::engine::{EngineKind, WorkerError};
use crate::sandbox::{attach_cgroup_v2, read_cgroup_mem_stats, CgroupMemStats, SandboxConfig, SandboxReport};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInitPayload {
    pub engine: EngineKind,
    pub package: ModulePackageV1,
    pub trusted_pub_hex: String,
    pub eval_budget_ns: u64,
    pub sandbox: WorkerSandboxParams,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSandboxParams {
    pub slot_name: String,
    pub memory_max_bytes: u64,
    pub drop_to_nobody: bool,
    pub enable_seccomp: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInitAck {
    pub pid: u32,
    pub sandbox: SandboxReport,
}

pub const MAX_INIT_FRAME_BYTES: usize = 2 * 1024 * 1024;

pub fn write_frame<W: Write>(
    w: &mut W,
    msg_type: IpcMessageType,
    payload: &[u8],
) -> Result<(), WorkerError> {
    let total_len = 1usize + payload.len();
    let max_bytes = if msg_type == IpcMessageType::InitModule {
        MAX_INIT_FRAME_BYTES
    } else {
        MAX_IPC_FRAME_BYTES
    };
    if total_len > max_bytes {
        return Err(WorkerError::Ipc(format!(
            "frame length {total_len} exceeds max {max_bytes}"
        )));
    }
    let mut header = [0u8; 5];
    header[0..4].copy_from_slice(&(total_len as u32).to_le_bytes());
    header[4] = msg_type as u8;
    w.write_all(&header)
        .map_err(|e| WorkerError::Ipc(e.to_string()))?;
    if !payload.is_empty() {
        w.write_all(payload)
            .map_err(|e| WorkerError::Ipc(e.to_string()))?;
    }
    w.flush().map_err(|e| WorkerError::Ipc(e.to_string()))?;
    Ok(())
}

pub fn read_frame_with_timeout<R: Read + AsRawFd>(
    r: &mut R,
    timeout: Option<Duration>,
    buf: &mut Vec<u8>,
) -> Result<IpcMessageType, WorkerError> {
    read_frame_inner(r, timeout, true, buf)
}

pub fn read_runtime_frame_with_timeout<R: Read + AsRawFd>(
    r: &mut R,
    timeout: Option<Duration>,
    buf: &mut Vec<u8>,
) -> Result<IpcMessageType, WorkerError> {
    read_frame_inner(r, timeout, false, buf)
}

fn read_frame_inner<R: Read + AsRawFd>(
    r: &mut R,
    timeout: Option<Duration>,
    allow_init: bool,
    buf: &mut Vec<u8>,
) -> Result<IpcMessageType, WorkerError> {
    let deadline = timeout.map(|d| Instant::now() + d);
    if let Some(dl) = deadline {
        let rem = dl.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            return Err(WorkerError::DeadlineExceeded);
        }
        wait_readable(r.as_raw_fd(), rem)?;
    }
    let mut len_buf = [0u8; 4];
    read_exact_until(r, &mut len_buf, deadline)?;
    let total_len = u32::from_le_bytes(len_buf) as usize;
    let max_allowed = if allow_init {
        MAX_INIT_FRAME_BYTES
    } else {
        MAX_IPC_FRAME_BYTES
    };
    if total_len == 0 || total_len > max_allowed {
        return Err(WorkerError::Ipc(format!(
            "invalid incoming frame length {total_len} (max {max_allowed})"
        )));
    }
    let mut type_byte = [0u8; 1];
    read_exact_until(r, &mut type_byte, deadline)?;
    let msg_type = IpcMessageType::from_u8(type_byte[0])?;
    if !allow_init && msg_type == IpcMessageType::InitModule {
        return Err(WorkerError::Ipc("unexpected InitModule frame after init".into()));
    }
    if msg_type != IpcMessageType::InitModule && total_len > MAX_IPC_FRAME_BYTES {
        return Err(WorkerError::Ipc(format!(
            "frame length {total_len} exceeds max {MAX_IPC_FRAME_BYTES}"
        )));
    }
    let payload_len = total_len - 1;
    buf.resize(payload_len, 0);
    if payload_len > 0 {
        read_exact_until(r, buf, deadline)?;
    }
    Ok(msg_type)
}

#[cfg(unix)]
fn wait_readable(fd: RawFd, timeout: Duration) -> Result<(), WorkerError> {
    let deadline = Instant::now() + timeout;
    loop {
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            return Err(WorkerError::DeadlineExceeded);
        }
        let ms = rem.as_millis().min(i32::MAX as u128).max(1) as i32;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc == 0 {
            return Err(WorkerError::DeadlineExceeded);
        }
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(WorkerError::Ipc(err.to_string()));
        }
        if (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0
            && (pfd.revents & libc::POLLIN) == 0
        {
            return Err(WorkerError::Ipc("worker IPC closed/hangup".into()));
        }
        return Ok(());
    }
}

#[cfg(unix)]
fn read_exact_until<R: Read + AsRawFd>(
    r: &mut R,
    dst: &mut [u8],
    deadline: Option<Instant>,
) -> Result<(), WorkerError> {
    let mut read_so_far = 0usize;
    while read_so_far < dst.len() {
        if let Some(dl) = deadline {
            let now = Instant::now();
            if now >= dl {
                return Err(WorkerError::DeadlineExceeded);
            }
            wait_readable(r.as_raw_fd(), dl - now)?;
        }
        match r.read(&mut dst[read_so_far..]) {
            Ok(0) => return Err(WorkerError::Ipc("unexpected EOF from worker".into())),
            Ok(n) => read_so_far += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(WorkerError::Ipc(e.to_string())),
        }
    }
    Ok(())
}

/// Manages an isolated child `plc-worker` process communicating over a private Unix socketpair.
pub struct WorkerProcessClient {
    child: Child,
    stream: std::os::unix::net::UnixStream,
    cg_dir: Option<PathBuf>,
    pub init_ack: WorkerInitAck,
    req_buf: Vec<u8>,
    rx_buf: Vec<u8>,
}

impl WorkerProcessClient {
    pub fn spawn(
        worker_bin: &std::path::Path,
        init: &WorkerInitPayload,
        extra_args: &[&str],
    ) -> Result<Self, WorkerError> {
        use std::os::fd::IntoRawFd;
        use std::os::unix::net::UnixStream;
        use std::os::unix::process::CommandExt;

        let (parent_sock, child_sock) =
            UnixStream::pair().map_err(|e| WorkerError::Ipc(e.to_string()))?;
        let child_fd = child_sock.into_raw_fd();

        // Clear FD_CLOEXEC on child_fd inside pre_exec and pass its fd number
        let mut cmd = Command::new(worker_bin);
        cmd.arg("--ipc-fd").arg(child_fd.to_string());
        for a in extra_args {
            cmd.arg(a);
        }
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::inherit());
        unsafe {
            cmd.pre_exec(move || {
                let flags = libc::fcntl(child_fd, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(child_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                }
                Ok(())
            });
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| WorkerError::Ipc(format!("failed spawning worker {}: {e}", worker_bin.display())))?;
        unsafe {
            libc::close(child_fd);
        }

        // Attach child to dedicated cgroup v2 slot on Linux
        let cg_dir = attach_cgroup_v2(
            &init.sandbox.slot_name,
            child.id(),
            init.sandbox.memory_max_bytes,
            50_000,
            100_000,
        )
        .ok();

        let payload = serde_json::to_vec(init).map_err(|e| WorkerError::Ipc(e.to_string()))?;
        let mut stream = parent_sock;
        if let Err(e) = write_frame(&mut stream, IpcMessageType::InitModule, &payload) {
            let _ = child.kill();
            let _ = child.wait();
            if let Some(ref d) = cg_dir {
                let _ = std::fs::remove_dir(d);
            }
            return Err(e);
        }

        let mut rx_buf = Vec::with_capacity(4096);
        let msg = read_frame_with_timeout(&mut stream, Some(Duration::from_secs(5)), &mut rx_buf)
            .map_err(|e| {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(ref d) = cg_dir {
                    let _ = std::fs::remove_dir(d);
                }
                e
            })?;
        match msg {
            IpcMessageType::InitAck => {
                let init_ack: WorkerInitAck = serde_json::from_slice(&rx_buf).map_err(|e| {
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(ref d) = cg_dir {
                        let _ = std::fs::remove_dir(d);
                    }
                    WorkerError::Ipc(format!("bad InitAck: {e}"))
                })?;
                Ok(Self {
                    child,
                    stream,
                    cg_dir,
                    init_ack,
                    req_buf: Vec::with_capacity(4096),
                    rx_buf,
                })
            }
            IpcMessageType::Error => {
                let msg = String::from_utf8_lossy(&rx_buf).into_owned();
                let _ = child.kill();
                let _ = child.wait();
                if let Some(ref d) = cg_dir {
                    let _ = std::fs::remove_dir(d);
                }
                Err(WorkerError::Runtime(msg))
            }
            other => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(ref d) = cg_dir {
                    let _ = std::fs::remove_dir(d);
                }
                Err(WorkerError::Ipc(format!("unexpected init response {other:?}")))
            }
        }
    }

    pub fn eval_within(
        &mut self,
        req: &EvalRequest,
        deadline: Duration,
    ) -> Result<EvalResult, WorkerError> {
        let total_deadline = Instant::now() + deadline;
        encode_eval_request(req, &mut self.req_buf)?;
        write_frame(&mut self.stream, IpcMessageType::EvalReq, &self.req_buf)?;
        let rem = total_deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            return Err(WorkerError::DeadlineExceeded);
        }
        let msg = read_runtime_frame_with_timeout(&mut self.stream, Some(rem), &mut self.rx_buf)?;
        match msg {
            IpcMessageType::EvalRes => Ok(decode_eval_result(&self.rx_buf)?),
            IpcMessageType::Error => {
                Err(WorkerError::Runtime(String::from_utf8_lossy(&self.rx_buf).into_owned()))
            }
            other => Err(WorkerError::Ipc(format!("unexpected msg {other:?}"))),
        }
    }

    pub fn revoke_and_kill(&mut self) {
        let _ = write_frame(&mut self.stream, IpcMessageType::Revoke, &[]);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    pub fn cgroup_mem_stats(&self) -> Option<CgroupMemStats> {
        self.cg_dir.as_ref().map(|d| read_cgroup_mem_stats(d))
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for WorkerProcessClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(ref d) = self.cg_dir {
            let _ = std::fs::remove_dir(d);
        }
    }
}

pub fn sandbox_cfg_from_params(p: &WorkerSandboxParams) -> SandboxConfig {
    SandboxConfig {
        slot_name: p.slot_name.clone(),
        memory_max_bytes: p.memory_max_bytes,
        cpu_quota_us: 50_000,
        cpu_period_us: 100_000,
        drop_to_nobody: p.drop_to_nobody,
        enable_seccomp: p.enable_seccomp,
    }
}
