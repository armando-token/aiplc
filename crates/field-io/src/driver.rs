use std::io::Write;
use std::sync::{Arc, Mutex};

use control_abi::{
    DriverStatus, InputSignal, Observation, OutputFrame, SignalQuality, SIGNAL_PHYSICAL_INHIBIT,
    SIGNAL_TEMP_PROCESS,
};

use crate::gpio_linux::{GpioChardevV2, GpioLineSpec};
use crate::modbus_tht02::SerialModbusPoller;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverHealth {
    pub healthy: bool,
    pub last_status: DriverStatus,
    pub physical_outputs_bound: bool,
    pub last_error: Option<String>,
}

pub trait FieldIoDriver: Send {
    fn sample_inputs(&mut self, now_ns: u64) -> (u64, Vec<InputSignal>);
    fn apply_output(&mut self, frame: &OutputFrame, now_ns: u64) -> Observation;
    fn force_safe_off(&mut self, now_ns: u64) -> Observation;
    fn health(&self) -> DriverHealth;
}

/// Validates frame freshness, bootId, and monotonic output sequence before driver actuation.
pub struct OutputOrderGate {
    expected_boot_id: u64,
    last_output_seq: u64,
    last_lease_epoch: u64,
}

impl OutputOrderGate {
    pub fn new(boot_id: u64) -> Self {
        Self {
            expected_boot_id: boot_id,
            last_output_seq: 0,
            last_lease_epoch: 0,
        }
    }

    pub fn validate(&mut self, frame: &OutputFrame, now_ns: u64) -> Result<(), DriverStatus> {
        if frame.boot_id != self.expected_boot_id {
            return Err(DriverStatus::IoFault);
        }
        if frame.lease_epoch < self.last_lease_epoch {
            return Err(DriverStatus::IoUnknown);
        }
        if frame.output_seq <= self.last_output_seq {
            return Err(DriverStatus::IoUnknown);
        }
        if now_ns >= frame.expiry_ns {
            return Err(DriverStatus::Expired);
        }
        self.last_lease_epoch = frame.lease_epoch;
        self.last_output_seq = frame.output_seq;
        Ok(())
    }
}

/// Null/in-memory driver for unit tests & benchmark baseline without hardware I/O.
pub struct NullDriver {
    gate: OutputOrderGate,
    temp_decic: i32,
    temp_quality: SignalQuality,
    temp_age_ms: u32,
    input_seq: u64,
    applied_mask: u32,
    last_status: DriverStatus,
}

impl NullDriver {
    pub fn new(boot_id: u64) -> Self {
        Self {
            gate: OutputOrderGate::new(boot_id),
            temp_decic: 250,
            temp_quality: SignalQuality::Good,
            temp_age_ms: 50,
            input_seq: 1,
            applied_mask: 0,
            last_status: DriverStatus::Applied,
        }
    }

    pub fn set_temp(&mut self, decic: i32, quality: SignalQuality, age_ms: u32) {
        self.temp_decic = decic;
        self.temp_quality = quality;
        self.temp_age_ms = age_ms;
        self.input_seq += 1;
    }
}

impl FieldIoDriver for NullDriver {
    fn sample_inputs(&mut self, _now_ns: u64) -> (u64, Vec<InputSignal>) {
        (
            self.input_seq,
            vec![
                InputSignal {
                    id: SIGNAL_TEMP_PROCESS,
                    value: self.temp_decic,
                    quality: self.temp_quality,
                    sample_age_ms: self.temp_age_ms,
                },
                InputSignal {
                    id: SIGNAL_PHYSICAL_INHIBIT,
                    value: 0,
                    quality: SignalQuality::Good,
                    sample_age_ms: 0,
                },
            ],
        )
    }

    fn apply_output(&mut self, frame: &OutputFrame, now_ns: u64) -> Observation {
        match self.gate.validate(frame, now_ns) {
            Ok(()) => {
                self.applied_mask = frame.applied_mask;
                self.last_status = DriverStatus::Applied;
                Observation {
                    output_seq: frame.output_seq,
                    driver_status: DriverStatus::Applied,
                    observed_mask: None, // No physical feedback sensor on NullDriver
                    source: "null-driver".into(),
                    observed_at_ns: now_ns,
                    quality: SignalQuality::Good,
                }
            }
            Err(status) => {
                self.last_status = status;
                Observation {
                    output_seq: frame.output_seq,
                    driver_status: status,
                    observed_mask: None,
                    source: "null-driver".into(),
                    observed_at_ns: now_ns,
                    quality: SignalQuality::Invalid,
                }
            }
        }
    }

    fn force_safe_off(&mut self, now_ns: u64) -> Observation {
        self.applied_mask = 0;
        Observation {
            output_seq: self.gate.last_output_seq,
            driver_status: DriverStatus::Applied,
            observed_mask: None,
            source: "null-driver-safe-off".into(),
            observed_at_ns: now_ns,
            quality: SignalQuality::Good,
        }
    }

    fn health(&self) -> DriverHealth {
        DriverHealth {
            healthy: true,
            last_status: self.last_status,
            physical_outputs_bound: false,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VirtualBenchState {
    pub temp_decic: i32,
    pub temp_quality: SignalQuality,
    pub temp_age_ms: u32,
    pub physical_inhibit: bool,
    pub input_seq: u64,
    pub applied_mask: u32,
    pub observed_mask: Option<u32>,
    pub inject_fault: Option<DriverStatus>,
    /// D04: optional witness discrepancy override (e.g. relay welded shut).
    pub witness_override_mask: Option<u32>,
    pub history: Vec<(u64, u32, Option<u32>, DriverStatus)>,
}

/// Interactive virtual bench driver supporting D04 discrepancy tests & external witness traces.
pub struct VirtualBenchDriver {
    gate: OutputOrderGate,
    pub shared: Arc<Mutex<VirtualBenchState>>,
}

impl VirtualBenchDriver {
    pub fn new(boot_id: u64) -> Self {
        Self {
            gate: OutputOrderGate::new(boot_id),
            shared: Arc::new(Mutex::new(VirtualBenchState {
                temp_decic: 250,
                temp_quality: SignalQuality::Good,
                temp_age_ms: 40,
                physical_inhibit: false,
                input_seq: 1,
                applied_mask: 0,
                observed_mask: Some(0),
                inject_fault: None,
                witness_override_mask: None,
                history: Vec::new(),
            })),
        }
    }
}

impl FieldIoDriver for VirtualBenchDriver {
    fn sample_inputs(&mut self, _now_ns: u64) -> (u64, Vec<InputSignal>) {
        let st = self.shared.lock().unwrap();
        (
            st.input_seq,
            vec![
                InputSignal {
                    id: SIGNAL_TEMP_PROCESS,
                    value: st.temp_decic,
                    quality: st.temp_quality,
                    sample_age_ms: st.temp_age_ms,
                },
                InputSignal {
                    id: SIGNAL_PHYSICAL_INHIBIT,
                    value: if st.physical_inhibit { 1 } else { 0 },
                    quality: SignalQuality::Good,
                    sample_age_ms: 0,
                },
            ],
        )
    }

    fn apply_output(&mut self, frame: &OutputFrame, now_ns: u64) -> Observation {
        let mut st = self.shared.lock().unwrap();
        if let Some(fault) = st.inject_fault {
            let obs = st.observed_mask;
            st.history.push((frame.scan_id, frame.applied_mask, obs, fault));
            return Observation {
                output_seq: frame.output_seq,
                driver_status: fault,
                observed_mask: obs,
                source: "virtual-bench-fault-injected".into(),
                observed_at_ns: now_ns,
                quality: SignalQuality::Invalid,
            };
        }
        match self.gate.validate(frame, now_ns) {
            Ok(()) => {
                st.applied_mask = frame.applied_mask;
                let obs = st.witness_override_mask.unwrap_or(frame.applied_mask);
                st.observed_mask = Some(obs);
                st.history
                    .push((frame.scan_id, frame.applied_mask, Some(obs), DriverStatus::Applied));
                Observation {
                    output_seq: frame.output_seq,
                    driver_status: DriverStatus::Applied,
                    observed_mask: Some(obs),
                    source: "virtual-witness".into(),
                    observed_at_ns: now_ns,
                    quality: SignalQuality::Good,
                }
            }
            Err(status) => {
                let obs = st.observed_mask;
                st.history
                    .push((frame.scan_id, frame.applied_mask, obs, status));
                Observation {
                    output_seq: frame.output_seq,
                    driver_status: status,
                    observed_mask: obs,
                    source: "virtual-bench-gate".into(),
                    observed_at_ns: now_ns,
                    quality: SignalQuality::Invalid,
                }
            }
        }
    }

    fn force_safe_off(&mut self, now_ns: u64) -> Observation {
        let mut st = self.shared.lock().unwrap();
        st.applied_mask = 0;
        let obs = st.witness_override_mask.unwrap_or(0);
        st.observed_mask = Some(obs);
        Observation {
            output_seq: self.gate.last_output_seq,
            driver_status: DriverStatus::Applied,
            observed_mask: Some(obs),
            source: "virtual-bench-safe-off".into(),
            observed_at_ns: now_ns,
            quality: SignalQuality::Good,
        }
    }

    fn health(&self) -> DriverHealth {
        let st = self.shared.lock().unwrap();
        DriverHealth {
            healthy: st.inject_fault.is_none(),
            last_status: st.inject_fault.unwrap_or(DriverStatus::Applied),
            physical_outputs_bound: false,
            last_error: st.inject_fault.map(|f| format!("injected {f:?}")),
        }
    }
}

/// Optional non-blocking LCD IPC helper for `/var/run/kvm_lyra_plc_display.sock`.
pub struct DisplayBridge {
    sock_path: String,
}

impl DisplayBridge {
    pub fn new(sock_path: impl Into<String>) -> Self {
        Self {
            sock_path: sock_path.into(),
        }
    }

    /// Best-effort non-blocking label update to `plc_display`. Never throws or blocks scan loop.
    #[cfg(unix)]
    pub fn try_update_label(&self, obj: &str, text: &str) -> Result<(), String> {
        use std::os::unix::net::UnixStream;
        use std::time::Duration;
        let mut stream = UnixStream::connect(&self.sock_path).map_err(|e| e.to_string())?;
        let _ = stream.set_write_timeout(Some(Duration::from_millis(50)));
        let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
        let payload = serde_json::json!({
            "method": "ui_set_text",
            "params": {
                "obj": obj,
                "text": text
            }
        });
        let line = payload.to_string() + "\n";
        stream.write_all(line.as_bytes()).map_err(|e| e.to_string())
    }

    #[cfg(not(unix))]
    pub fn try_update_label(&self, _obj: &str, _text: &str) -> Result<(), String> {
        Err("unix sockets unsupported".into())
    }
}

/// Real Luckfox Lyra hardware driver: exclusive GPIO chardev v2 for relay0 (17) and relay1 (18),
/// plus optional Serial/PTY Modbus THT-02 poller.
pub struct LyraHardwareDriver {
    gate: OutputOrderGate,
    gpio: GpioChardevV2,
    modbus: Option<SerialModbusPoller>,
    fallback_temp_store: Arc<Mutex<(i32, SignalQuality, u32, u64)>>,
    last_status: DriverStatus,
    last_error: Option<String>,
}

impl LyraHardwareDriver {
    pub fn open(
        boot_id: u64,
        gpio_chip: &str,
        relay0_offset: u32,
        relay1_offset: u32,
        modbus_device: Option<&str>,
        slave_addr: u8,
        baud_rate: u32,
        freshness_ttl_ms: u32,
    ) -> Result<Self, String> {
        let gpio = GpioChardevV2::open_outputs(
            gpio_chip,
            "plc-auto-supervisor",
            &[
                GpioLineSpec {
                    offset: relay0_offset,
                    active_low: false,
                },
                GpioLineSpec {
                    offset: relay1_offset,
                    active_low: false,
                },
            ],
        )
        .map_err(|e| format!("GPIO open error: {e}"))?;

        let modbus = if let Some(dev) = modbus_device {
            Some(
                SerialModbusPoller::start(
                    dev.to_string(),
                    slave_addr,
                    baud_rate,
                    500,
                    freshness_ttl_ms,
                    -400,
                    1250,
                )
                .map_err(|e| format!("Modbus open error: {e}"))?,
            )
        } else {
            None
        };

        Ok(Self {
            gate: OutputOrderGate::new(boot_id),
            gpio,
            modbus,
            fallback_temp_store: Arc::new(Mutex::new((250, SignalQuality::Good, 0, 1))),
            last_status: DriverStatus::Applied,
            last_error: None,
        })
    }

    pub fn inject_temp_when_no_modbus(&self, decic: i32, quality: SignalQuality, age_ms: u32) {
        let mut g = self.fallback_temp_store.lock().unwrap();
        g.0 = decic;
        g.1 = quality;
        g.2 = age_ms;
        g.3 += 1;
    }

    pub fn read_gpio_bits(&self) -> Result<u64, String> {
        self.gpio.get_values().map_err(|e| e.to_string())
    }
}

impl FieldIoDriver for LyraHardwareDriver {
    fn sample_inputs(&mut self, _now_ns: u64) -> (u64, Vec<InputSignal>) {
        let (temp_sig, seq) = if let Some(ref poller) = self.modbus {
            poller.snapshot()
        } else {
            let g = self.fallback_temp_store.lock().unwrap();
            (
                InputSignal {
                    id: SIGNAL_TEMP_PROCESS,
                    value: g.0,
                    quality: g.1,
                    sample_age_ms: g.2,
                },
                g.3,
            )
        };
        (
            seq,
            vec![
                temp_sig,
                InputSignal {
                    id: SIGNAL_PHYSICAL_INHIBIT,
                    value: 0,
                    quality: SignalQuality::Good,
                    sample_age_ms: 0,
                },
            ],
        )
    }

    fn apply_output(&mut self, frame: &OutputFrame, now_ns: u64) -> Observation {
        match self.gate.validate(frame, now_ns) {
            Ok(()) => {
                // Only bits 0 and 1 are physical relays; bit 2 (tower.blue) is virtual.
                let phys_bits = (frame.applied_mask & 0x03) as u64;
                match self.gpio.set_values(phys_bits) {
                    Ok(()) => {
                        let readback = self.gpio.get_values().ok().map(|b| b as u32);
                        self.last_status = DriverStatus::Applied;
                        self.last_error = None;
                        Observation {
                            output_seq: frame.output_seq,
                            driver_status: DriverStatus::Applied,
                            observed_mask: readback,
                            source: "gpio-chardev-v2-readback".into(),
                            observed_at_ns: now_ns,
                            quality: SignalQuality::Good,
                        }
                    }
                    Err(e) => {
                        let _ = self.gpio.set_values(0);
                        self.last_status = DriverStatus::IoFault;
                        self.last_error = Some(e.to_string());
                        Observation {
                            output_seq: frame.output_seq,
                            driver_status: DriverStatus::IoFault,
                            observed_mask: None,
                            source: "gpio-chardev-v2".into(),
                            observed_at_ns: now_ns,
                            quality: SignalQuality::Invalid,
                        }
                    }
                }
            }
            Err(status) => {
                let _ = self.gpio.set_values(0);
                self.last_status = status;
                self.last_error = Some(format!("output_order_gate:{status:?}"));
                Observation {
                    output_seq: frame.output_seq,
                    driver_status: status,
                    observed_mask: None,
                    source: "gpio-order-gate".into(),
                    observed_at_ns: now_ns,
                    quality: SignalQuality::Invalid,
                }
            }
        }
    }

    fn force_safe_off(&mut self, now_ns: u64) -> Observation {
        match self.gpio.set_values(0) {
            Ok(()) => {
                let readback = self.gpio.get_values().ok().map(|b| b as u32);
                self.last_status = DriverStatus::Applied;
                Observation {
                    output_seq: self.gate.last_output_seq,
                    driver_status: DriverStatus::Applied,
                    observed_mask: readback,
                    source: "gpio-chardev-v2-safe-off".into(),
                    observed_at_ns: now_ns,
                    quality: if readback == Some(0) {
                        SignalQuality::Good
                    } else {
                        SignalQuality::Invalid
                    },
                }
            }
            Err(e) => {
                self.last_status = DriverStatus::IoFault;
                self.last_error = Some(e.to_string());
                Observation {
                    output_seq: self.gate.last_output_seq,
                    driver_status: DriverStatus::IoFault,
                    observed_mask: None,
                    source: "gpio-chardev-v2-safe-off".into(),
                    observed_at_ns: now_ns,
                    quality: SignalQuality::Invalid,
                }
            }
        }
    }

    fn health(&self) -> DriverHealth {
        DriverHealth {
            healthy: self.last_error.is_none(),
            last_status: self.last_status,
            physical_outputs_bound: true,
            last_error: self.last_error.clone(),
        }
    }
}
