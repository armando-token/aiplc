use std::thread;
use std::time::Duration;

use control_abi::{DriverStatus, GuardReason, OutputFrame, SignalQuality};
use field_io::{
    build_fc03_request, decode_tht02_fc03_response, modbus_crc16, FieldIoDriver, ModbusError,
    SensorStateStore, SerialModbusPoller, SimFaultMode, Tht02PtySimulator, VirtualBenchDriver,
};

#[test]
fn modbus_tht02_decodes_positive_negative_and_rejects_sentinel_and_crc() {
    let req = build_fc03_request(1, 0, 2);
    assert_eq!(req.len(), 8);
    assert_eq!(req[0], 1);
    assert_eq!(req[1], 3);

    // Helper to build a response
    let make_resp = |slave: u8, raw_temp: u16, hum: u16, bad_crc: bool| -> [u8; 9] {
        let mut r = [0u8; 9];
        r[0] = slave;
        r[1] = 0x03;
        r[2] = 0x04;
        r[3..5].copy_from_slice(&raw_temp.to_be_bytes());
        r[5..7].copy_from_slice(&hum.to_be_bytes());
        let mut crc = modbus_crc16(&r[0..7]);
        if bad_crc {
            crc ^= 0x00FF;
        }
        r[7..9].copy_from_slice(&crc.to_le_bytes());
        r
    };

    // 0x00FA -> +25.0 degC
    let ok_pos = decode_tht02_fc03_response(&make_resp(1, 0x00FA, 512, false), 1, -400, 1250).unwrap();
    assert_eq!(ok_pos.temp_decic, 250);
    assert_eq!(ok_pos.quality, SignalQuality::Good);

    // 0xFFF6 -> -1.0 degC
    let ok_neg = decode_tht02_fc03_response(&make_resp(1, 0xFFF6, 512, false), 1, -400, 1250).unwrap();
    assert_eq!(ok_neg.temp_decic, -10);

    // 0x7FFF -> abnormal sentinel -> MUST error, never 0
    assert!(matches!(
        decode_tht02_fc03_response(&make_resp(1, 0x7FFF, 0, false), 1, -400, 1250),
        Err(ModbusError::AbnormalSentinel)
    ));

    // Bad CRC
    assert!(matches!(
        decode_tht02_fc03_response(&make_resp(1, 0x00FA, 512, true), 1, -400, 1250),
        Err(ModbusError::CrcMismatch { .. })
    ));
}

#[test]
fn freshness_ttl_transitions_good_to_stale_without_losing_diag_value() {
    let store = SensorStateStore::new(80);
    store.record_good(451);
    let (s1, _) = store.snapshot(1);
    assert_eq!(s1.quality, SignalQuality::Good);
    assert_eq!(s1.value, 451);

    thread::sleep(Duration::from_millis(90));
    let (s2, _) = store.snapshot(1);
    assert_eq!(s2.quality, SignalQuality::Stale);
    assert_eq!(s2.value, 451); // last value kept purely for diagnostics
}

#[test]
fn modbus_poller_talks_real_rtu_bytes_over_pty_simulator() {
    let sim = Tht02PtySimulator::start(1, 250).unwrap();
    let poller = SerialModbusPoller::start(
        sim.slave_path.clone(),
        1,
        9600,
        50,
        1500,
        -400,
        1250,
    )
    .unwrap();

    // Wait for first poll
    thread::sleep(Duration::from_millis(120));
    let (s, _) = poller.snapshot();
    assert_eq!(s.quality, SignalQuality::Good);
    assert_eq!(s.value, 250);

    // Negative temperature -1.0 C
    sim.controls.lock().unwrap().temp_decic = -10;
    thread::sleep(Duration::from_millis(120));
    let (s_neg, _) = poller.snapshot();
    assert_eq!(s_neg.quality, SignalQuality::Good);
    assert_eq!(s_neg.value, -10);

    // Abnormal sentinel 0x7FFF
    sim.controls.lock().unwrap().fault_mode = SimFaultMode::AbnormalSentinel;
    thread::sleep(Duration::from_millis(120));
    let (s_inv, _) = poller.snapshot();
    assert_eq!(s_inv.quality, SignalQuality::Invalid);

    // Recovery
    sim.controls.lock().unwrap().fault_mode = SimFaultMode::Normal;
    sim.controls.lock().unwrap().temp_decic = 455;
    thread::sleep(Duration::from_millis(120));
    let (s_rec, _) = poller.snapshot();
    assert_eq!(s_rec.quality, SignalQuality::Good);
    assert_eq!(s_rec.value, 455);
}

#[test]
fn output_gate_rejects_expired_and_stale_sequences() {
    let mut drv = VirtualBenchDriver::new(100);
    let ok = OutputFrame {
        boot_id: 100,
        scan_id: 1,
        lease_epoch: 1,
        active_revision: 1,
        output_seq: 1,
        desired_mask: 1,
        applied_mask: 1,
        expiry_ns: 2_000,
        guard_reason: GuardReason::None,
    };
    let obs1 = drv.apply_output(&ok, 1_000);
    assert_eq!(obs1.driver_status, DriverStatus::Applied);

    // Expired order
    let exp = OutputFrame {
        output_seq: 2,
        expiry_ns: 2_000,
        ..ok.clone()
    };
    let obs2 = drv.apply_output(&exp, 2_000);
    assert_eq!(obs2.driver_status, DriverStatus::Expired);

    // Old sequence rejected
    let old = OutputFrame {
        output_seq: 1,
        expiry_ns: 5_000,
        ..ok
    };
    let obs3 = drv.apply_output(&old, 2_500);
    assert_eq!(obs3.driver_status, DriverStatus::IoUnknown);
}
