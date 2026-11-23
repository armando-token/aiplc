use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(target_os = "linux"))]
use std::time::Duration;

pub trait ControlClock: Send + Sync {
    fn now_ns(&self) -> u64;
    fn sleep_until_ns(&self, target_ns: u64);
}

#[derive(Debug, Default)]
pub struct MonotonicClock;

impl ControlClock for MonotonicClock {
    fn now_ns(&self) -> u64 {
        #[cfg(unix)]
        unsafe {
            let mut ts: libc::timespec = std::mem::zeroed();
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
            (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
        }
        #[cfg(not(unix))]
        {
            0
        }
    }

    fn sleep_until_ns(&self, target_ns: u64) {
        #[cfg(target_os = "linux")]
        unsafe {
            let ts = libc::timespec {
                tv_sec: (target_ns / 1_000_000_000) as libc::time_t,
                tv_nsec: (target_ns % 1_000_000_000) as libc::c_long,
            };
            loop {
                let rc = libc::clock_nanosleep(
                    libc::CLOCK_MONOTONIC,
                    libc::TIMER_ABSTIME,
                    &ts,
                    std::ptr::null_mut(),
                );
                if rc != libc::EINTR {
                    break;
                }
            }
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            let now = self.now_ns();
            if target_ns > now {
                std::thread::sleep(Duration::from_nanos(target_ns - now));
            }
        }
    }
}

#[derive(Debug)]
pub struct VirtualClock {
    now: AtomicU64,
}

impl VirtualClock {
    pub fn new(initial_ns: u64) -> Self {
        Self {
            now: AtomicU64::new(initial_ns),
        }
    }

    pub fn set_ns(&self, ns: u64) {
        self.now.store(ns, Ordering::Release);
    }

    pub fn advance_ns(&self, delta_ns: u64) -> u64 {
        self.now.fetch_add(delta_ns, Ordering::AcqRel) + delta_ns
    }
}

impl ControlClock for VirtualClock {
    fn now_ns(&self) -> u64 {
        self.now.load(Ordering::Acquire)
    }

    fn sleep_until_ns(&self, target_ns: u64) {
        let cur = self.now_ns();
        if target_ns > cur {
            self.now.store(target_ns, Ordering::Release);
        }
    }
}
