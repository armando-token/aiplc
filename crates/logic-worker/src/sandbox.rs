use std::fs;
use std::os::fd::RawFd;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("OS error: {0}")]
    Os(#[from] std::io::Error),
    #[error("unsupported architecture for seccomp filter")]
    UnsupportedArch,
    #[error("failed applying {0}: {1}")]
    StageFailed(&'static str, String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxReport {
    pub cgroup_applied: bool,
    pub cgroup_path: Option<String>,
    pub seccomp_applied: bool,
    pub uid_dropped: bool,
    pub effective_uid: u32,
    pub network_namespace: bool,
    pub memory_max_bytes: u64,
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub slot_name: String,
    pub memory_max_bytes: u64,
    pub cpu_quota_us: u64,
    pub cpu_period_us: u64,
    pub drop_to_nobody: bool,
    pub enable_seccomp: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            slot_name: "slot-a".into(),
            memory_max_bytes: 16 * 1024 * 1024, // 16 MiB baseline (§5.1)
            cpu_quota_us: 50_000,
            cpu_period_us: 100_000,
            drop_to_nobody: true,
            enable_seccomp: true,
        }
    }
}

/// Closes all open file descriptors >= 3 except those listed in `keep_fds`.
pub fn close_inherited_fds_except(keep_fds: &[RawFd]) {
    #[cfg(unix)]
    {
        if let Ok(entries) = fs::read_dir("/proc/self/fd") {
            let mut to_close = Vec::new();
            for e in entries.flatten() {
                if let Some(s) = e.file_name().to_str() {
                    if let Ok(fd) = s.parse::<RawFd>() {
                        if fd >= 3 && !keep_fds.contains(&fd) {
                            to_close.push(fd);
                        }
                    }
                }
            }
            for fd in to_close {
                unsafe {
                    libc::close(fd);
                }
            }
            return;
        }
        // Fallback for macOS / non-procfs
        for fd in 3..256 {
            if !keep_fds.contains(&fd) {
                unsafe {
                    libc::close(fd);
                }
            }
        }
    }
}

/// Attaches `pid` to a dedicated cgroup v2 directory with `memory.max` and `cpu.max`.
pub fn attach_cgroup_v2(
    slot_name: &str,
    pid: u32,
    memory_max_bytes: u64,
    cpu_quota_us: u64,
    cpu_period_us: u64,
) -> Result<PathBuf, SandboxError> {
    #[cfg(target_os = "linux")]
    {
        let root = PathBuf::from("/sys/fs/cgroup");
        if !root.exists() {
            return Err(SandboxError::StageFailed(
                "cgroup2",
                "/sys/fs/cgroup missing".into(),
            ));
        }
        let sanitized: String = slot_name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if sanitized.is_empty() {
            return Err(SandboxError::StageFailed("cgroup2", "empty slot name".into()));
        }
        // Ensure memory and cpu controllers are enabled in root subtree_control
        if fs::write(root.join("cgroup.subtree_control"), "+memory +cpu").is_err() {
            let _ = fs::write(root.join("cgroup.subtree_control"), "+memory");
            let _ = fs::write(root.join("cgroup.subtree_control"), "+cpu");
        }
        let cg_dir = root.join(format!("plc-worker-{sanitized}"));
        fs::create_dir_all(&cg_dir)?;
        fs::write(cg_dir.join("memory.max"), memory_max_bytes.to_string())?;
        // Disable swap growth if memory.swap.max exists
        let _ = fs::write(cg_dir.join("memory.swap.max"), "0");
        let _ = fs::write(
            cg_dir.join("cpu.max"),
            format!("{cpu_quota_us} {cpu_period_us}"),
        );
        fs::write(cg_dir.join("cgroup.procs"), pid.to_string())?;
        Ok(cg_dir)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (slot_name, pid, memory_max_bytes, cpu_quota_us, cpu_period_us);
        Err(SandboxError::StageFailed(
            "cgroup2",
            "only available on Linux".into(),
        ))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CgroupMemStats {
    pub current_bytes: u64,
    pub peak_bytes: u64,
    pub oom_events: u64,
}

pub fn read_cgroup_mem_stats(cg_dir: &std::path::Path) -> CgroupMemStats {
    let mut stats = CgroupMemStats::default();
    if let Ok(s) = fs::read_to_string(cg_dir.join("memory.current")) {
        stats.current_bytes = s.trim().parse().unwrap_or(0);
    }
    if let Ok(s) = fs::read_to_string(cg_dir.join("memory.peak")) {
        stats.peak_bytes = s.trim().parse().unwrap_or(0);
    }
    if let Ok(s) = fs::read_to_string(cg_dir.join("memory.events")) {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("oom_kill ") {
                stats.oom_events += rest.trim().parse::<u64>().unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("oom ") {
                stats.oom_events += rest.trim().parse::<u64>().unwrap_or(0);
            }
        }
    }
    stats
}

/// Applies resource limits, drops privileges to UID/GID 65534 (`nobody`), sets NO_NEW_PRIVS,
/// and installs a strict Seccomp-BPF filter blocking sockets, device opens, execve, and fork/clone.
pub fn harden_worker_process(cfg: &SandboxConfig, keep_fds: &[RawFd]) -> SandboxReport {
    close_inherited_fds_except(keep_fds);

    #[cfg(target_os = "linux")]
    {
        let mut uid_dropped = false;
        let mut seccomp_applied = false;
        let mut notes = Vec::new();

        // Read /proc/self/cgroup BEFORE seccomp blocks openat()!
        let mut cgroup_applied = false;
        let mut actual_cg_path: Option<String> = None;
        if let Ok(cg_content) = fs::read_to_string("/proc/self/cgroup") {
            let expected = format!("/plc-worker-{}", cfg.slot_name);
            for line in cg_content.lines() {
                if let Some((_, p)) = line.rsplit_once(':') {
                    if p == expected {
                        cgroup_applied = true;
                        actual_cg_path = Some(format!("/sys/fs/cgroup{p}"));
                    }
                }
            }
        }

        unsafe {
            // Core dumps & disk writes disabled
            let zero_lim = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &zero_lim);
            libc::setrlimit(libc::RLIMIT_FSIZE, &zero_lim);
            let fd_lim = libc::rlimit {
                rlim_cur: 32,
                rlim_max: 32,
            };
            libc::setrlimit(libc::RLIMIT_NOFILE, &fd_lim);

            // Drop privileges to nobody (65534:65534) clearing supplementary groups first
            if cfg.drop_to_nobody {
                if libc::geteuid() == 0 {
                    let sg = libc::setgroups(0, std::ptr::null());
                    if sg == 0 && libc::setgid(65534) == 0 && libc::setuid(65534) == 0 {
                        uid_dropped = true;
                    } else {
                        notes.push("uid_drop_failed".to_string());
                    }
                } else if libc::geteuid() == 65534 {
                    uid_dropped = true;
                } else {
                    notes.push(format!("not_root_euid_{}", libc::geteuid()));
                }
            }

            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                notes.push("no_new_privs_failed".to_string());
            }
        }

        let net_ns = std::path::Path::new("/proc/self/ns/net").exists()
            && std::env::var("PLC_WORKER_NEWNET").as_deref() == Ok("1");
        if !std::path::Path::new("/proc/self/ns/net").exists() {
            notes.push("kernel_no_netns_using_seccomp_bpf".into());
        }

        if cfg.enable_seccomp {
            match install_seccomp_deny_capabilities() {
                Ok(()) => seccomp_applied = true,
                Err(e) => notes.push(format!("seccomp_err:{e}")),
            }
        }

        SandboxReport {
            cgroup_applied,
            cgroup_path: actual_cg_path,
            seccomp_applied,
            uid_dropped,
            effective_uid: unsafe { libc::geteuid() },
            network_namespace: net_ns,
            memory_max_bytes: cfg.memory_max_bytes,
            note: notes.join(";"),
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = cfg;
        SandboxReport {
            cgroup_applied: false,
            cgroup_path: None,
            seccomp_applied: false,
            uid_dropped: false,
            effective_uid: unsafe { libc::geteuid() },
            network_namespace: false,
            memory_max_bytes: cfg.memory_max_bytes,
            note: "host-non-linux-sandbox-fd-only".into(),
        }
    }
}

#[cfg(target_os = "linux")]
fn install_seccomp_deny_capabilities() -> Result<(), SandboxError> {
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SockFilter {
        code: u16,
        jt: u8,
        jf: u8,
        k: u32,
    }
    #[repr(C)]
    struct SockFprog {
        len: u16,
        filter: *const SockFilter,
    }

    const BPF_LD: u16 = 0x00;
    const BPF_W: u16 = 0x00;
    const BPF_ABS: u16 = 0x20;
    const BPF_JMP: u16 = 0x05;
    const BPF_JEQ: u16 = 0x10;
    const BPF_K: u16 = 0x00;
    const BPF_RET: u16 = 0x06;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_RET_ERRNO_EPERM: u32 = 0x0005_0000 | (libc::EPERM as u32);
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;

    // seccomp_data offsets: nr = 0, arch = 4
    const fn stmt(code: u16, k: u32) -> SockFilter {
        SockFilter {
            code,
            jt: 0,
            jf: 0,
            k,
        }
    }
    const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> SockFilter {
        SockFilter { code, jt, jf, k }
    }

    #[cfg(target_arch = "arm")]
    const EXPECTED_ARCH: u32 = 0x4000_0028; // AUDIT_ARCH_ARM (verified on Luckfox PLC)
    #[cfg(target_arch = "aarch64")]
    const EXPECTED_ARCH: u32 = 0xc000_00b7; // AUDIT_ARCH_AARCH64
    #[cfg(target_arch = "x86_64")]
    const EXPECTED_ARCH: u32 = 0xc000_003e; // AUDIT_ARCH_X86_64

    // Syscall numbers verified on target Linux headers
    #[cfg(target_arch = "arm")]
    const DENIED_SYSCALLS: &[u32] = &[
        281, // socket
        288, // socketpair
        283, // connect
        282, // bind
        284, // listen
        285, // accept
        366, // accept4
        5,   // open
        322, // openat
        437, // openat2
        11,  // execve
        387, // execveat
        2,   // fork
        190, // vfork
        120, // clone
        435, // clone3
        54,  // ioctl
        26,  // ptrace
        37,  // kill
        238, // tkill
        268, // tgkill
        21,  // mount
        10,  // unlink
        328, // unlinkat
        39,  // mkdir
        323, // mkdirat
        38,  // rename
        329, // renameat
        382, // renameat2
        8,   // creat
        385, // memfd_create
        376, // process_vm_readv
        377, // process_vm_writev
        425, // io_uring_setup
        426, // io_uring_enter
        427, // io_uring_register
        434, // pidfd_open
        438, // pidfd_getfd
    ];

    #[cfg(target_arch = "aarch64")]
    const DENIED_SYSCALLS: &[u32] = &[
        198, // socket
        199, // socketpair
        203, // connect
        200, // bind
        201, // listen
        202, // accept
        242, // accept4
        56,  // openat
        437, // openat2
        221, // execve
        281, // execveat
        220, // clone
        435, // clone3
        29,  // ioctl
        117, // ptrace
        129, // kill
        130, // tkill
        131, // tgkill
        40,  // mount
        35,  // unlinkat
        34,  // mkdirat
        38,  // renameat
        276, // renameat2
        279, // memfd_create
        270, // process_vm_readv
        271, // process_vm_writev
        425, // io_uring_setup
        426, // io_uring_enter
        427, // io_uring_register
        434, // pidfd_open
        438, // pidfd_getfd
    ];

    #[cfg(target_arch = "x86_64")]
    const DENIED_SYSCALLS: &[u32] = &[
        41, 53, 42, 49, 50, 43, 288, 2, 257, 437, 59, 322, 57, 58, 56, 435, 16, 101, 62, 200, 234,
        165, 87, 263, 83, 258, 82, 264, 316, 85, 319, 310, 311, 425, 426, 427, 434, 438,
    ];

    let mut prog: Vec<SockFilter> = Vec::with_capacity(6 + DENIED_SYSCALLS.len());
    // 1. Validate arch
    prog.push(stmt(BPF_LD | BPF_W | BPF_ABS, 4));
    prog.push(jump(BPF_JMP | BPF_JEQ | BPF_K, EXPECTED_ARCH, 1, 0));
    prog.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS));
    // 2. Load syscall nr
    prog.push(stmt(BPF_LD | BPF_W | BPF_ABS, 0));
    // 3. Deny dangerous syscalls with EPERM
    let n = DENIED_SYSCALLS.len();
    for (idx, &nr) in DENIED_SYSCALLS.iter().enumerate() {
        let jump_to_deny = (n - 1 - idx + 1) as u8; // skip remaining checks + ALLOW
        prog.push(jump(BPF_JMP | BPF_JEQ | BPF_K, nr, jump_to_deny, 0));
    }
    prog.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
    prog.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO_EPERM));

    let fprog = SockFprog {
        len: prog.len() as u16,
        filter: prog.as_ptr(),
    };
    let rc = unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &fprog as *const SockFprog,
        )
    };
    if rc != 0 {
        return Err(SandboxError::Os(std::io::Error::last_os_error()));
    }
    Ok(())
}
