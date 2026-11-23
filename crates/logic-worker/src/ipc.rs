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
    let full_len = 4 + total_len;
    if full_len <= 512 {
        let mut stack = [0u8; 512];
        stack[0..4].copy_from_slice(&(total_len as u32).to_le_bytes());
        stack[4] = msg_type as u8;
        if !payload.is_empty() {
            stack[5..full_len].copy_from_slice(payload);
        }
        w.write_all(&stack[..full_len])
            .map_err(|e| WorkerError::Ipc(e.to_string()))?;
    } else {
        let mut heap = Vec::with_capacity(full_len);
        heap.extend_from_slice(&(total_len as u32).to_le_bytes());
        heap.push(msg_type as u8);
        heap.extend_from_slice(payload);
        w.write_all(&heap)
            .map_err(|e| WorkerError::Ipc(e.to_string()))?;
    }
    Ok(())
}

pub fn read_frame_with_timeout<R: Read + AsRawFd>(
    r: &mut R,
    timeout: Option<Duration>,
    buf: &mut Vec<u8>,
) -> Result<IpcMessageType, WorkerError> {
    read_frame_inner(r, timeout.map(|d| Instant::now() + d), true, buf)
}

pub fn read_runtime_frame_with_timeout<R: Read + AsRawFd>(
    r: &mut R,
    timeout: Option<Duration>,
    buf: &mut Vec<u8>,
) -> Result<IpcMessageType, WorkerError> {
    read_frame_inner(r, timeout.map(|d| Instant::now() + d), false, buf)
}

pub fn read_runtime_frame_until<R: Read + AsRawFd>(
    r: &mut R,
    deadline: Instant,
    buf: &mut Vec<u8>,
) -> Result<IpcMessageType, WorkerError> {
    read_frame_inner(r, Some(deadline), false, buf)
}

pub fn write_frame_until<W: Write + AsRawFd>(
    w: &mut W,
    frame_buf: &mut Vec<u8>,
    msg_type: IpcMessageType,
    payload: &[u8],
    deadline: Instant,
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
    frame_buf.clear();
    frame_buf.reserve(4 + total_len);
    frame_buf.extend_from_slice(&(total_len as u32).to_le_bytes());
    frame_buf.push(msg_type as u8);
    frame_buf.extend_from_slice(payload);

    let mut written = 0usize;
    while written < frame_buf.len() {
        let now = Instant::now();
        if now >= deadline {
            return Err(WorkerError::DeadlineExceeded);
        }
        match w.write(&frame_buf[written..]) {
            Ok(0) => return Err(WorkerError::Ipc("zero-byte write on worker IPC".into())),
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let rem = deadline.saturating_duration_since(Instant::now());
                if rem.is_zero() {
                    return Err(WorkerError::DeadlineExceeded);
                }
                wait_poll_event(w.as_raw_fd(), libc::POLLOUT, rem)?;
            }
            Err(e) => return Err(WorkerError::Ipc(e.to_string())),
        }
    }
    Ok(())
}

fn read_frame_inner<R: Read + AsRawFd>(
    r: &mut R,
    deadline: Option<Instant>,
    allow_init: bool,
    buf: &mut Vec<u8>,
) -> Result<IpcMessageType, WorkerError> {
    if let Some(dl) = deadline {
        if Instant::now() >= dl {
            return Err(WorkerError::DeadlineExceeded);
        }
    }
    let mut hdr5 = [0u8; 5];
    read_exact_until(r, &mut hdr5, deadline)?;
    let total_len = u32::from_le_bytes([hdr5[0], hdr5[1], hdr5[2], hdr5[3]]) as usize;
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
    let msg_type = IpcMessageType::from_u8(hdr5[4])?;
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
fn wait_poll_event(fd: RawFd, event_mask: i16, timeout: Duration) -> Result<(), WorkerError> {
    let deadline = Instant::now() + timeout;
    loop {
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            return Err(WorkerError::DeadlineExceeded);
        }
        let ms = rem.as_millis().min(i32::MAX as u128).max(1) as i32;
        let mut pfd = libc::pollfd {
            fd,
            events: event_mask,
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
            && (pfd.revents & event_mask) == 0
        {
            return Err(WorkerError::Ipc("worker IPC closed/hangup".into()));
        }
        if (pfd.revents & libc::POLLERR) != 0 && (event_mask & libc::POLLOUT) != 0 {
            return Err(WorkerError::Ipc("worker IPC error on write".into()));
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
            if Instant::now() >= dl {
                return Err(WorkerError::DeadlineExceeded);
            }
        }
        match r.read(&mut dst[read_so_far..]) {
            Ok(0) => return Err(WorkerError::Ipc("unexpected EOF from worker".into())),
            Ok(n) => read_so_far += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if let Some(dl) = deadline {
                    let rem = dl.saturating_duration_since(Instant::now());
                    if rem.is_zero() {
                        return Err(WorkerError::DeadlineExceeded);
                    }
                    wait_poll_event(r.as_raw_fd(), libc::POLLIN, rem)?;
                } else {
                    wait_poll_event(r.as_raw_fd(), libc::POLLIN, Duration::from_secs(3600))?;
                }
            }
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
    tx_frame_buf: Vec<u8>,
    rx_buf: Vec<u8>,
    tainted: bool,
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
                if let Err(e) = stream.set_nonblocking(true) {
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(ref d) = cg_dir {
                        let _ = std::fs::remove_dir(d);
                    }
                    return Err(WorkerError::Ipc(format!("set_nonblocking failed: {e}")));
                }
                Ok(Self {
                    child,
                    stream,
                    cg_dir,
                    init_ack,
                    req_buf: Vec::with_capacity(4096),
                    tx_frame_buf: Vec::with_capacity(4096),
                    rx_buf,
                    tainted: false,
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

    pub fn eval_until(
        &mut self,
        req: &EvalRequest,
        deadline: Instant,
    ) -> Result<EvalResult, WorkerError> {
        if self.tainted {
            return Err(WorkerError::Ipc(
                "worker IPC stream tainted; discard required".into(),
            ));
        }
        if Instant::now() >= deadline {
            self.tainted = true;
            return Err(WorkerError::DeadlineExceeded);
        }
        encode_eval_request(req, &mut self.req_buf)?;
        if let Err(e) = write_frame_until(
            &mut self.stream,
            &mut self.tx_frame_buf,
            IpcMessageType::EvalReq,
            &self.req_buf,
            deadline,
        ) {
            self.tainted = true;
            return Err(e);
        }
        if Instant::now() >= deadline {
            self.tainted = true;
            return Err(WorkerError::DeadlineExceeded);
        }
        let msg = match read_runtime_frame_until(&mut self.stream, deadline, &mut self.rx_buf) {
            Ok(m) => m,
            Err(e) => {
                self.tainted = true;
                return Err(e);
            }
        };
        match msg {
            IpcMessageType::EvalRes => match decode_eval_result(&self.rx_buf) {
                Ok(res) => Ok(res),
                Err(e) => {
                    self.tainted = true;
                    Err(e.into())
                }
            },
            IpcMessageType::Error => {
                self.tainted = true;
                Err(WorkerError::Runtime(
                    String::from_utf8_lossy(&self.rx_buf).into_owned(),
                ))
            }
            other => {
                self.tainted = true;
                Err(WorkerError::Ipc(format!("unexpected msg {other:?}")))
            }
        }
    }

    pub fn eval_within(
        &mut self,
        req: &EvalRequest,
        timeout: Duration,
    ) -> Result<EvalResult, WorkerError> {
        self.eval_until(req, Instant::now() + timeout)
    }

    pub fn is_tainted(&self) -> bool {
        self.tainted
    }

    pub fn sandbox_report(&self) -> &SandboxReport {
        &self.init_ack.sandbox
    }

    pub fn cgroup_dir(&self) -> Option<&std::path::Path> {
        self.cg_dir.as_deref()
    }

    pub fn set_socket_buf_sizes(
        &self,
        sndbuf: libc::c_int,
        rcvbuf: libc::c_int,
    ) -> Result<(), WorkerError> {
        let fd = self.stream.as_raw_fd();
        unsafe {
            if sndbuf > 0 {
                let rc = libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    &sndbuf as *const libc::c_int as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
                if rc != 0 {
                    return Err(WorkerError::Ipc(format!(
                        "SO_SNDBUF failed: {}",
                        std::io::Error::last_os_error()
                    )));
                }
            }
            if rcvbuf > 0 {
                let rc = libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    &rcvbuf as *const libc::c_int as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
                if rc != 0 {
                    return Err(WorkerError::Ipc(format!(
                        "SO_RCVBUF failed: {}",
                        std::io::Error::last_os_error()
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn revoke_and_kill(&mut self) {
        self.tainted = true;
        let _ = write_frame_until(
            &mut self.stream,
            &mut self.tx_frame_buf,
            IpcMessageType::Revoke,
            &[],
            Instant::now() + Duration::from_millis(2),
        );
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
