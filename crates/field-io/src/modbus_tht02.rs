use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use control_abi::{InputSignal, SignalQuality, SIGNAL_TEMP_PROCESS};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModbusError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("CRC mismatch: expected 0x{expected:04x}, got 0x{actual:04x}")]
    CrcMismatch { expected: u16, actual: u16 },
    #[error("unexpected slave address: expected {expected}, got {actual}")]
    SlaveMismatch { expected: u8, actual: u8 },
    #[error("unexpected function code: expected 0x{expected:02x}, got 0x{actual:02x}")]
    FunctionMismatch { expected: u8, actual: u8 },
    #[error("unexpected byte count {0}")]
    ByteCount(u8),
    #[error("truncated or oversized frame length {0}")]
    FrameLength(usize),
    #[error("modbus exception 0x{0:02x}")]
    Exception(u8),
    #[error("sensor reported abnormal sentinel 0x7FFF")]
    AbnormalSentinel,
    #[error("reading out of valid range [{min}, {max}]: {value}")]
    OutOfRange { value: i32, min: i32, max: i32 },
    #[error("response timeout")]
    Timeout,
}

/// Standard Modbus RTU CRC-16 (polynomial 0xA001, little-endian on wire).
pub fn modbus_crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            if (crc & 0x0001) != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

/// Builds a Read Holding Registers (FC03) RTU request frame (8 bytes).
pub fn build_fc03_request(slave_addr: u8, start_reg: u16, count: u16) -> [u8; 8] {
    let mut frame = [0u8; 8];
    frame[0] = slave_addr;
    frame[1] = 0x03;
    frame[2..4].copy_from_slice(&start_reg.to_be_bytes());
    frame[4..6].copy_from_slice(&count.to_be_bytes());
    let crc = modbus_crc16(&frame[0..6]);
    frame[6..8].copy_from_slice(&crc.to_le_bytes());
    frame
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tht02Sample {
    pub temp_decic: i32,
    pub humidity_deci_pct: u16,
    pub quality: SignalQuality,
}

/// Decodes a THT-02 V1.0 FC03 response (2 registers starting at PDU offset 0).
/// Temperature is big-endian two's complement signed 16-bit in 0.1 degC.
/// 0x7FFF is explicitly treated as abnormal (`Invalid`), never silently converted to 0.
pub fn decode_tht02_fc03_response(
    frame: &[u8],
    expected_slave: u8,
    min_valid_decic: i32,
    max_valid_decic: i32,
) -> Result<Tht02Sample, ModbusError> {
    if frame.len() == 5 && (frame[1] & 0x80) != 0 {
        let expected_crc = modbus_crc16(&frame[0..3]);
        let actual_crc = u16::from_le_bytes([frame[3], frame[4]]);
        if expected_crc != actual_crc {
            return Err(ModbusError::CrcMismatch {
                expected: expected_crc,
                actual: actual_crc,
            });
        }
        if frame[0] != expected_slave {
            return Err(ModbusError::SlaveMismatch {
                expected: expected_slave,
                actual: frame[0],
            });
        }
        if (frame[1] & 0x7F) != 0x03 {
            return Err(ModbusError::FunctionMismatch {
                expected: 0x03,
                actual: frame[1],
            });
        }
        return Err(ModbusError::Exception(frame[2]));
    }
    // Expected: [slave, 0x03, 0x04, temp_hi, temp_lo, hum_hi, hum_lo, crc_lo, crc_hi] = 9 bytes
    if frame.len() != 9 {
        return Err(ModbusError::FrameLength(frame.len()));
    }
    let expected_crc = modbus_crc16(&frame[0..7]);
    let actual_crc = u16::from_le_bytes([frame[7], frame[8]]);
    if expected_crc != actual_crc {
        return Err(ModbusError::CrcMismatch {
            expected: expected_crc,
            actual: actual_crc,
        });
    }
    if frame[0] != expected_slave {
        return Err(ModbusError::SlaveMismatch {
            expected: expected_slave,
            actual: frame[0],
        });
    }
    if frame[1] != 0x03 {
        return Err(ModbusError::FunctionMismatch {
            expected: 0x03,
            actual: frame[1],
        });
    }
    if frame[2] != 4 {
        return Err(ModbusError::ByteCount(frame[2]));
    }

    let raw_temp_u16 = u16::from_be_bytes([frame[3], frame[4]]);
    if raw_temp_u16 == 0x7FFF {
        return Err(ModbusError::AbnormalSentinel);
    }
    let temp_decic = (raw_temp_u16 as i16) as i32;
    if temp_decic < min_valid_decic || temp_decic > max_valid_decic {
        return Err(ModbusError::OutOfRange {
            value: temp_decic,
            min: min_valid_decic,
            max: max_valid_decic,
        });
    }

    let raw_hum = u16::from_be_bytes([frame[5], frame[6]]);
    Ok(Tht02Sample {
        temp_decic,
        humidity_deci_pct: raw_hum,
        quality: SignalQuality::Good,
    })
}

#[derive(Debug)]
struct SensorStoreInner {
    last_valid_value: i32,
    has_ever_succeeded: bool,
    last_error_present: bool,
    last_quality: SignalQuality,
    last_good_epoch_ms: u64,
    input_seq: u64,
}

/// Thread-safe snapshot store shared between the off-scan Modbus reader thread and scan loop.
/// Uses coherent Mutex state so concurrent `record_good`/`record_fault` updates never tear.
/// Freshness check uses strictly `< freshness_ttl_ms` for `GOOD` and `>= freshness_ttl_ms` for `STALE`.
#[derive(Debug)]
pub struct SensorStateStore {
    inner: Mutex<SensorStoreInner>,
    freshness_ttl_ms: u32,
    start_instant: Instant,
}

impl SensorStateStore {
    pub fn new(freshness_ttl_ms: u32) -> Self {
        Self {
            inner: Mutex::new(SensorStoreInner {
                last_valid_value: i32::MIN,
                has_ever_succeeded: false,
                last_error_present: false,
                last_quality: SignalQuality::Unavailable,
                last_good_epoch_ms: 0,
                input_seq: 0,
            }),
            freshness_ttl_ms,
            start_instant: Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.start_instant.elapsed().as_millis() as u64
    }

    pub fn record_good(&self, value_decic: i32) {
        let now = self.now_ms();
        let mut g = self.inner.lock().unwrap();
        g.last_valid_value = value_decic;
        g.last_good_epoch_ms = now;
        g.has_ever_succeeded = true;
        g.last_error_present = false;
        g.last_quality = SignalQuality::Good;
        g.input_seq += 1;
    }

    pub fn record_fault(&self, q: SignalQuality) {
        let mut g = self.inner.lock().unwrap();
        g.last_error_present = true;
        g.last_quality = q;
        g.input_seq += 1;
    }

    /// Reads current diagnostic value and freshness-evaluated quality.
    /// When `age_ms >= freshness_ttl_ms` and no explicit hard fault overwrote it, returns `STALE`.
    pub fn snapshot(&self, signal_id: u32) -> (InputSignal, u64) {
        let now = self.now_ms();
        let g = self.inner.lock().unwrap();
        let seq = g.input_seq;
        if !g.has_ever_succeeded {
            return (
                InputSignal {
                    id: signal_id,
                    value: 0,
                    quality: g.last_quality,
                    sample_age_ms: u32::MAX,
                },
                seq,
            );
        }

        let age_ms = now.saturating_sub(g.last_good_epoch_ms).min(u32::MAX as u64) as u32;
        let effective_q = if g.last_error_present {
            g.last_quality
        } else if age_ms >= self.freshness_ttl_ms {
            SignalQuality::Stale
        } else {
            SignalQuality::Good
        };

        (
            InputSignal {
                id: signal_id,
                value: g.last_valid_value,
                quality: effective_q,
                sample_age_ms: age_ms,
            },
            seq,
        )
    }
}

#[cfg(unix)]
pub fn configure_raw_serial(fd: libc::c_int, baud_rate: u32) -> Result<(), ModbusError> {
    unsafe {
        let mut tio: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut tio) != 0 {
            return Err(ModbusError::Io(std::io::Error::last_os_error()));
        }
        libc::cfmakeraw(&mut tio);
        let speed = match baud_rate {
            9600 => libc::B9600,
            19200 => libc::B19200,
            38400 => libc::B38400,
            57600 => libc::B57600,
            115200 => libc::B115200,
            other => {
                return Err(ModbusError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unsupported baud_rate {other}"),
                )));
            }
        };
        libc::cfsetispeed(&mut tio, speed);
        libc::cfsetospeed(&mut tio, speed);
        // 8N1
        tio.c_cflag &= !(libc::PARENB | libc::CSTOPB | libc::CSIZE);
        tio.c_cflag |= libc::CS8 | libc::CLOCAL | libc::CREAD;
        tio.c_cc[libc::VMIN] = 0;
        tio.c_cc[libc::VTIME] = 1; // 100 ms inter-char read timeout
        if libc::tcsetattr(fd, libc::TCSANOW, &tio) != 0 {
            return Err(ModbusError::Io(std::io::Error::last_os_error()));
        }
        libc::tcflush(fd, libc::TCIOFLUSH);
    }
    Ok(())
}

/// Polls a real serial or PTY device with FC03 in a background thread.
pub struct SerialModbusPoller {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    pub store: Arc<SensorStateStore>,
}

impl SerialModbusPoller {
    #[cfg(unix)]
    pub fn start(
        device_path: String,
        slave_addr: u8,
        baud_rate: u32,
        poll_interval_ms: u64,
        freshness_ttl_ms: u32,
        min_valid_decic: i32,
        max_valid_decic: i32,
    ) -> Result<Self, ModbusError> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&device_path)?;
        use std::os::fd::AsRawFd;
        configure_raw_serial(file.as_raw_fd(), baud_rate)?;

        let store = Arc::new(SensorStateStore::new(freshness_ttl_ms));
        let store_bg = Arc::clone(&store);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);

        let handle = thread::Builder::new()
            .name("modbus-tht02-poller".into())
            .spawn(move || {
                let req = build_fc03_request(slave_addr, 0, 2);
                let mut rx = [0u8; 64];
                while !stop_bg.load(Ordering::Acquire) {
                    let cycle_start = Instant::now();
                    unsafe {
                        libc::tcflush(file.as_raw_fd(), libc::TCIFLUSH);
                    }
                    if file.write_all(&req).is_err() || file.flush().is_err() {
                        store_bg.record_fault(SignalQuality::Unavailable);
                    } else {
                        let mut total = 0usize;
                        let deadline = Instant::now() + Duration::from_millis(250);
                        while Instant::now() < deadline && total < 9 {
                            let mut pfd = libc::pollfd {
                                fd: file.as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            };
                            let rc = unsafe { libc::poll(&mut pfd, 1, 40) };
                            if rc < 0 || (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0 {
                                break;
                            }
                            if rc > 0 && (pfd.revents & libc::POLLIN) != 0 {
                                match file.read(&mut rx[total..9]) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        total += n;
                                        if total >= 5 && (rx[1] & 0x80) != 0 {
                                            break;
                                        }
                                        if total >= 9 {
                                            break;
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                        }
                        if total == 0 {
                            store_bg.record_fault(SignalQuality::Unavailable);
                        } else {
                            match decode_tht02_fc03_response(
                                &rx[..total],
                                slave_addr,
                                min_valid_decic,
                                max_valid_decic,
                            ) {
                                Ok(sample) => store_bg.record_good(sample.temp_decic),
                                Err(_) => store_bg.record_fault(SignalQuality::Invalid),
                            }
                        }
                    }
                    let elapsed = cycle_start.elapsed();
                    let interval = Duration::from_millis(poll_interval_ms);
                    if elapsed < interval && !stop_bg.load(Ordering::Acquire) {
                        thread::sleep(interval - elapsed);
                    }
                }
            })?;

        Ok(Self {
            stop,
            handle: Some(handle),
            store,
        })
    }

    pub fn snapshot(&self) -> (InputSignal, u64) {
        self.store.snapshot(SIGNAL_TEMP_PROCESS)
    }
}

impl Drop for SerialModbusPoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimFaultMode {
    Normal,
    AbnormalSentinel,
    CorruptCrc,
    SilentDrop,
}

#[derive(Debug)]
pub struct SimSensorControls {
    pub temp_decic: i32,
    pub hum_deci_pct: u16,
    pub fault_mode: SimFaultMode,
}

/// Real PTY-backed Modbus RTU THT-02 slave simulator for byte-accurate hardware-free HIL testing.
#[cfg(unix)]
pub struct Tht02PtySimulator {
    pub slave_path: String,
    pub controls: Arc<Mutex<SimSensorControls>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

#[cfg(unix)]
impl Tht02PtySimulator {
    pub fn start(slave_addr: u8, initial_temp_decic: i32) -> Result<Self, ModbusError> {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let mut name_buf = [0 as libc::c_char; 256];
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                name_buf.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(ModbusError::Io(std::io::Error::last_os_error()));
        }
        unsafe {
            libc::close(slave);
        }
        let cstr = unsafe { std::ffi::CStr::from_ptr(name_buf.as_ptr()) };
        let slave_path = cstr.to_string_lossy().into_owned();
        configure_raw_serial(master, 9600)?;

        use std::os::fd::FromRawFd;
        let mut master_file = unsafe { std::fs::File::from_raw_fd(master) };

        let controls = Arc::new(Mutex::new(SimSensorControls {
            temp_decic: initial_temp_decic,
            hum_deci_pct: 512,
            fault_mode: SimFaultMode::Normal,
        }));
        let controls_bg = Arc::clone(&controls);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);

        let handle = thread::Builder::new()
            .name("tht02-pty-sim".into())
            .spawn(move || {
                use std::os::fd::AsRawFd;
                let mut req = [0u8; 32];
                while !stop_bg.load(Ordering::Acquire) {
                    let mut pfd = libc::pollfd {
                        fd: master_file.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let prc = unsafe { libc::poll(&mut pfd, 1, 30) };
                    if prc <= 0 || (pfd.revents & libc::POLLIN) == 0 {
                        continue;
                    }
                    let mut got = 0usize;
                    let deadline = Instant::now() + Duration::from_millis(40);
                    while got < 8 && Instant::now() < deadline {
                        match master_file.read(&mut req[got..8]) {
                            Ok(0) => break,
                            Ok(n) => got += n,
                            Err(_) => break,
                        }
                    }
                    if got < 8 {
                        continue;
                    }
                    let crc_exp = modbus_crc16(&req[0..6]);
                    let crc_act = u16::from_le_bytes([req[6], req[7]]);
                    if crc_exp != crc_act || req[0] != slave_addr || req[1] != 0x03 {
                        continue;
                    }
                    let guard = controls_bg.lock().unwrap();
                    let mode = guard.fault_mode;
                    let temp_i16 = guard.temp_decic as i16;
                    let hum_u16 = guard.hum_deci_pct;
                    drop(guard);

                    if mode == SimFaultMode::SilentDrop {
                        continue;
                    }
                    let raw_temp = if mode == SimFaultMode::AbnormalSentinel {
                        0x7FFFu16
                    } else {
                        temp_i16 as u16
                    };
                    let mut resp = [0u8; 9];
                    resp[0] = slave_addr;
                    resp[1] = 0x03;
                    resp[2] = 0x04;
                    resp[3..5].copy_from_slice(&raw_temp.to_be_bytes());
                    resp[5..7].copy_from_slice(&hum_u16.to_be_bytes());
                    let mut crc = modbus_crc16(&resp[0..7]);
                    if mode == SimFaultMode::CorruptCrc {
                        crc ^= 0xFFFF;
                    }
                    resp[7..9].copy_from_slice(&crc.to_le_bytes());
                    let _ = master_file.write_all(&resp);
                    let _ = master_file.flush();
                }
            })?;

        Ok(Self {
            slave_path,
            controls,
            stop,
            handle: Some(handle),
        })
    }
}

#[cfg(unix)]
impl Drop for Tht02PtySimulator {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
