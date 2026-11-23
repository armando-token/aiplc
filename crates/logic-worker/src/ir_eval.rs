use control_abi::schema::{RuleOp, RuleSetV1};
use control_abi::types::{
    BlockType, EvalStatus, InputSignal, RetentionClass, StateBlock, ABI_VERSION,
    OUT_BIT_BLUE_VIRTUAL, OUT_BIT_GREEN, OUT_BIT_RED, SIGNAL_TEMP_PROCESS,
};
use control_abi::wire::{EvalRequest, EvalResult};

pub fn output_name_to_bit(name: &str) -> u32 {
    match name {
        "tower.red" => OUT_BIT_RED,
        "tower.green" => OUT_BIT_GREEN,
        "tower.blue" => OUT_BIT_BLUE_VIRTUAL,
        _ => {
            if let Some(rest) = name.strip_prefix("virt.") {
                if let Ok(idx) = rest.parse::<u32>() {
                    if idx < 29 {
                        return 1u32 << (3 + idx);
                    }
                }
            }
            0
        }
    }
}

pub fn signal_name_to_id(name: &str) -> u32 {
    match name {
        "temp.process" => SIGNAL_TEMP_PROCESS,
        "physical.inhibit" => control_abi::SIGNAL_PHYSICAL_INHIBIT,
        "start.stop" => control_abi::SIGNAL_START_STOP_PERMIT,
        "test.aux1" => control_abi::SIGNAL_TEST_AUX_1,
        _ => {
            if let Some(rest) = name.strip_prefix("sig.") {
                if let Ok(idx) = rest.parse::<u32>() {
                    return 100 + idx;
                }
            }
            0
        }
    }
}

pub fn stable_id_for_rule(rule: &RuleOp) -> u32 {
    if let Some(id) = rule.stable_block_id {
        return id;
    }
    // Deterministic FNV-1a 32-bit hash of ruleId
    let mut h: u32 = 0x811c9dc5;
    for &b in rule.rule_id.as_bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    if h == 0 {
        1
    } else {
        h & 0x7fff_ffff
    }
}

pub fn block_type_for_rule(rule: &RuleOp) -> BlockType {
    if let Some(bt) = rule.block_type {
        return bt;
    }
    match rule.template_id.as_str() {
        "thresholdHoldWithLatchedHysteresis" => BlockType::Ton,
        "bandPrealarmPulse" => BlockType::PulseGen,
        "offDelay" => BlockType::Tof,
        "pulseTimer" => BlockType::Tp,
        "counterTrigger" => BlockType::Ctu,
        _ => BlockType::ThresholdHyst,
    }
}

pub fn initial_blocks_for_ruleset(ruleset: &RuleSetV1) -> Vec<StateBlock> {
    let mut out = Vec::new();
    for rule in &ruleset.changes {
        if rule.op == "removeRule" {
            continue;
        }
        let sid = stable_id_for_rule(rule);
        let bt = block_type_for_rule(rule);
        out.push(StateBlock {
            stable_id: sid,
            block_type: bt,
            schema_version: 1,
            retention_class: RetentionClass::Cyclic,
            migration_policy: rule.timing_state_on_change.into(),
            prev_cond: 0,
            phase: 0,
            last_output: 0,
            flags: 0,
            count_or_param: 0,
            start_logical_ns: 0,
            elapsed_ns: 0,
            aux_i64: 0,
        });
    }
    out
}

fn find_signal<'a>(signals: &'a [InputSignal], id: u32) -> Option<&'a InputSignal> {
    signals.iter().find(|s| s.id == id)
}

fn find_block_idx(blocks: &[StateBlock], stable_id: u32) -> Option<usize> {
    blocks.iter().position(|b| b.stable_id == stable_id)
}

/// Evaluates a `RuleSetV1` deterministically using only `req.logical_ns` and the supplied
/// canonical `StateBlock` slice. Returns an `EvalResult` with updated state blocks.
pub fn evaluate_ruleset_a0(ruleset: &RuleSetV1, req: &EvalRequest, elapsed_eval_ns: u32) -> EvalResult {
    let mut next_blocks = req.blocks.clone();
    let mut desired_mask: u32 = 0;
    let mut valid_mask: u32 = 0;

    for rule in &ruleset.changes {
        if rule.op == "removeRule" {
            continue;
        }
        let out_bit = output_name_to_bit(&rule.output);
        if out_bit == 0 {
            continue;
        }
        valid_mask |= out_bit;

        let sig_id = signal_name_to_id(&rule.input);
        let sig = find_signal(&req.signals, sig_id);
        let quality_ok = sig
            .map(|s| s.quality == rule.required_quality)
            .unwrap_or(false);

        let sid = stable_id_for_rule(rule);
        let bt = block_type_for_rule(rule);
        let idx = match find_block_idx(&next_blocks, sid) {
            Some(i) => i,
            None => {
                next_blocks.push(StateBlock {
                    stable_id: sid,
                    block_type: bt,
                    schema_version: 1,
                    retention_class: RetentionClass::Cyclic,
                    migration_policy: rule.timing_state_on_change.into(),
                    prev_cond: 0,
                    phase: 0,
                    last_output: 0,
                    flags: 0,
                    count_or_param: 0,
                    start_logical_ns: 0,
                    elapsed_ns: 0,
                    aux_i64: 0,
                });
                next_blocks.len() - 1
            }
        };
        let blk = &mut next_blocks[idx];

        if !quality_ok {
            match rule.on_input_fault.as_str() {
                "COMMAND_OFF_AND_RESET" | "SAFE_FALLBACK" => {
                    blk.prev_cond = 0;
                    blk.phase = 0;
                    blk.last_output = 0;
                    blk.start_logical_ns = 0;
                    blk.elapsed_ns = 0;
                }
                "COMMAND_OFF_HOLD" => {
                    blk.last_output = 0;
                }
                _ => {
                    blk.last_output = 0;
                }
            }
            continue;
        }

        let val = sig.unwrap().value;

        match rule.template_id.as_str() {
            "thresholdHoldWithLatchedHysteresis" => {
                let trig = rule.trigger_at_or_above.unwrap_or(450);
                let clear = rule.clear_at_or_below.unwrap_or(trig);
                let hold_ns = (rule.hold_ms.unwrap_or(0) as u64) * 1_000_000;

                if blk.last_output == 1 {
                    // Already latched ON: hold until value drops to or below clear threshold
                    if val <= clear {
                        blk.last_output = 0;
                        blk.phase = 0;
                        blk.prev_cond = 0;
                        blk.start_logical_ns = 0;
                        blk.elapsed_ns = 0;
                    } else {
                        blk.phase = 2;
                        blk.prev_cond = if val >= trig { 1 } else { 0 };
                    }
                } else {
                    // Not yet latched ON
                    if val >= trig {
                        if hold_ns == 0 {
                            blk.phase = 2;
                            blk.last_output = 1;
                            blk.prev_cond = 1;
                            blk.start_logical_ns = req.logical_ns;
                            blk.elapsed_ns = 0;
                        } else if blk.phase != 1 {
                            // Rising edge starts TON timer at current logical_ns (or resumes from preserved elapsed)
                            blk.phase = 1;
                            blk.prev_cond = 1;
                            blk.start_logical_ns = req.logical_ns.saturating_sub(blk.elapsed_ns);
                            if blk.elapsed_ns >= hold_ns {
                                blk.phase = 2;
                                blk.last_output = 1;
                            }
                        } else {
                            let elapsed = req.logical_ns.saturating_sub(blk.start_logical_ns);
                            blk.elapsed_ns = elapsed;
                            blk.prev_cond = 1;
                            if elapsed >= hold_ns {
                                blk.phase = 2;
                                blk.last_output = 1;
                            }
                        }
                    } else {
                        // Condition fell below trigger before hold completed -> reset TON
                        blk.phase = 0;
                        blk.prev_cond = 0;
                        blk.last_output = 0;
                        blk.start_logical_ns = 0;
                        blk.elapsed_ns = 0;
                    }
                }
            }
            "bandPrealarmPulse" => {
                let low = rule.band_low.unwrap_or(400);
                let high = rule.band_high.unwrap_or(450);
                let period_ns = (rule.pulse_period_ms.unwrap_or(2000) as u64).max(40) * 1_000_000;
                let half = period_ns / 2;
                if val >= low && val < high {
                    if blk.phase == 0 {
                        blk.phase = 1;
                        blk.start_logical_ns = req.logical_ns;
                        blk.elapsed_ns = 0;
                    } else {
                        blk.elapsed_ns = req.logical_ns.saturating_sub(blk.start_logical_ns);
                    }
                    let rem = blk.elapsed_ns % period_ns;
                    blk.last_output = if rem < half { 1 } else { 0 };
                    blk.prev_cond = 1;
                } else {
                    blk.phase = 0;
                    blk.prev_cond = 0;
                    blk.last_output = 0;
                    blk.start_logical_ns = 0;
                    blk.elapsed_ns = 0;
                }
            }
            "copyBoolean" => {
                let cond = if val != 0 { 1 } else { 0 };
                blk.prev_cond = cond;
                blk.last_output = cond;
                blk.phase = cond;
            }
            "manualMask" => {
                let trig = rule.trigger_at_or_above.unwrap_or(1);
                let on = if val >= trig { 1 } else { 0 };
                blk.prev_cond = on;
                blk.last_output = on;
                blk.phase = on;
            }
            "pulseTimer" => {
                // TP: non-retriggerable pulse on rising edge
                let trig = rule.trigger_at_or_above.unwrap_or(1);
                let hold_ns = (rule.hold_ms.unwrap_or(1000) as u64) * 1_000_000;
                let cond = val >= trig;
                let rising = cond && blk.prev_cond == 0;
                blk.prev_cond = if cond { 1 } else { 0 };
                if blk.phase == 1 {
                    let elapsed = req.logical_ns.saturating_sub(blk.start_logical_ns);
                    blk.elapsed_ns = elapsed;
                    if elapsed >= hold_ns {
                        blk.phase = 0;
                        blk.last_output = 0;
                    } else {
                        blk.last_output = 1;
                    }
                } else if rising && hold_ns > 0 {
                    blk.phase = 1;
                    blk.start_logical_ns = req.logical_ns;
                    blk.elapsed_ns = 0;
                    blk.last_output = 1;
                } else {
                    blk.last_output = 0;
                }
            }
            "offDelay" => {
                // TOF: output immediately ON while cond true, stays ON hold_ns after falling edge
                let trig = rule.trigger_at_or_above.unwrap_or(1);
                let hold_ns = (rule.hold_ms.unwrap_or(1000) as u64) * 1_000_000;
                let cond = val >= trig;
                if cond {
                    blk.phase = 1;
                    blk.last_output = 1;
                    blk.start_logical_ns = req.logical_ns;
                    blk.elapsed_ns = 0;
                } else if blk.phase == 1 {
                    blk.phase = 2;
                    blk.start_logical_ns = req.logical_ns;
                    blk.elapsed_ns = 0;
                    blk.last_output = if hold_ns > 0 { 1 } else { 0 };
                } else if blk.phase == 2 {
                    let elapsed = req.logical_ns.saturating_sub(blk.start_logical_ns);
                    blk.elapsed_ns = elapsed;
                    if elapsed >= hold_ns {
                        blk.phase = 0;
                        blk.last_output = 0;
                    } else {
                        blk.last_output = 1;
                    }
                } else {
                    blk.last_output = 0;
                }
                blk.prev_cond = if cond { 1 } else { 0 };
            }
            "counterTrigger" => {
                // CTU: increments on rising edge up to hold_ms (used as preset count)
                let trig = rule.trigger_at_or_above.unwrap_or(1);
                let preset = rule.hold_ms.unwrap_or(3) as i32;
                let cond = val >= trig;
                if cond && blk.prev_cond == 0 && blk.count_or_param < i32::MAX {
                    blk.count_or_param += 1;
                }
                blk.prev_cond = if cond { 1 } else { 0 };
                blk.last_output = if blk.count_or_param >= preset { 1 } else { 0 };
            }
            _ => {
                blk.last_output = 0;
            }
        }

        if blk.last_output != 0 {
            desired_mask |= out_bit;
        }
    }

    EvalResult {
        abi_version: ABI_VERSION,
        status: EvalStatus::Ok,
        lease_epoch: req.lease_epoch,
        generation: req.generation,
        scan_id: req.scan_id,
        state_revision: req.state_revision,
        desired_mask,
        valid_mask,
        diag_code: 0,
        eval_duration_ns: elapsed_eval_ns,
        blocks: next_blocks,
    }
}
