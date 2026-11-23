use control_abi::schema::{RuleSetV1, SiteProfileV1};
use control_abi::types::{
    GuardReason, InputSignal, SignalQuality, SIGNAL_PHYSICAL_INHIBIT,
};

#[derive(Debug, Clone)]
pub struct ManualLease {
    pub boot_id: u64,
    pub mask: u32,
    pub valid_mask: u32,
    pub expires_logical_ns: u64,
    pub operator_id: String,
}

#[derive(Debug, Clone)]
pub struct ShieldDecision {
    pub desired_mask: u32,
    pub applied_mask: u32,
    pub guard_reason: GuardReason,
    pub is_contingency_off: bool,
}

/// Per-channel dwell & chatter sliding window tracker.
#[derive(Debug, Clone)]
struct ChannelTracker {
    mask_bit: u8,
    dwell_ns: u64,
    max_transitions: usize,
    window_ns: u64,
    last_change_ns: u64,
    has_changed_once: bool,
    transition_ring: Vec<u64>,
}

impl ChannelTracker {
    fn new(
        mask_bit: u8,
        dwell_ms: u32,
        max_transitions: u32,
        window_ms: u32,
    ) -> Self {
        Self {
            mask_bit,
            dwell_ns: (dwell_ms as u64) * 1_000_000,
            max_transitions: max_transitions.max(1) as usize,
            window_ns: (window_ms as u64) * 1_000_000,
            last_change_ns: 0,
            has_changed_once: false,
            transition_ring: Vec::with_capacity(max_transitions as usize + 2),
        }
    }

    fn can_toggle(&mut self, now_ns: u64) -> bool {
        if self.has_changed_once && self.dwell_ns > 0 && now_ns.saturating_sub(self.last_change_ns) < self.dwell_ns {
            return false;
        }
        // Evict timestamps outside sliding window
        let cutoff = now_ns.saturating_sub(self.window_ns);
        self.transition_ring.retain(|&ts| ts > cutoff);
        self.transition_ring.len() < self.max_transitions
    }

    fn record_change(&mut self, now_ns: u64) {
        self.last_change_ns = now_ns;
        self.has_changed_once = true;
        self.transition_ring.push(now_ns);
    }
}

/// Reactive Safety Shield enforcing exact §5.4 precedence order:
/// 1. Physical inhibit
/// 2. Startup disarmed / FallbackLatched / I/O fault
/// 3. Required input signal quality
/// 4. Active worker deadline/fault fallback
/// 5. Fixed site interlocks
/// 6. Manual lease (bounded monotonic TTL)
/// 7. Allowed channel mask
/// 8. Dwell / anti-chatter (contingency turn-off ALWAYS prevails)
pub struct ReactiveShield {
    allowed_all_mask: u32,
    safe_fallback_mask: u32,
    interlocks: Vec<(u32, String)>,
    trackers: Vec<ChannelTracker>,
    prev_applied_mask: u32,
}

impl ReactiveShield {
    pub fn from_profile(profile: &SiteProfileV1) -> Self {
        let mut allowed_all_mask = 0u32;
        let mut trackers = Vec::new();
        for ch in &profile.channels {
            allowed_all_mask |= 1u32 << ch.mask_bit;
            trackers.push(ChannelTracker::new(
                ch.mask_bit,
                ch.dwell_ms,
                ch.max_transitions_per_window,
                ch.transition_window_ms,
            ));
        }
        let interlocks = profile
            .interlocks
            .iter()
            .map(|r| (r.forbidden_mask, r.reason.clone()))
            .collect();
        Self {
            allowed_all_mask,
            safe_fallback_mask: profile.safe_fallback_mask,
            interlocks,
            trackers,
            prev_applied_mask: profile.safe_fallback_mask,
        }
    }

    pub fn prev_applied_mask(&self) -> u32 {
        self.prev_applied_mask
    }

    pub fn evaluate(
        &mut self,
        now_ns: u64,
        boot_id: u64,
        armed_control: bool,
        fallback_latched: bool,
        io_healthy: bool,
        worker_ok: bool,
        signals: &[InputSignal],
        active_ruleset: Option<&RuleSetV1>,
        manual_lease: &mut Option<ManualLease>,
        proposed_mask: u32,
    ) -> ShieldDecision {
        // 1. Physical inhibit
        let inhibit = signals
            .iter()
            .find(|s| s.id == SIGNAL_PHYSICAL_INHIBIT)
            .map(|s| s.value != 0 || s.quality != SignalQuality::Good)
            .unwrap_or(false);
        if inhibit {
            return self.apply_contingency(now_ns, GuardReason::PhysicalInhibit, proposed_mask);
        }

        // 2. Startup disarmed / FallbackLatched / I/O fault
        if !armed_control {
            return self.apply_contingency(now_ns, GuardReason::StartupDisarmed, proposed_mask);
        }
        if fallback_latched {
            return self.apply_contingency(now_ns, GuardReason::FallbackLatched, proposed_mask);
        }
        if !io_healthy {
            return self.apply_contingency(now_ns, GuardReason::IoFault, proposed_mask);
        }

        // 3. Required input signal quality & freshness TTL check
        let ttl_ms = 1500u32;
        if let Some(rs) = active_ruleset {
            for rule in &rs.changes {
                if rule.op == "removeRule" {
                    continue;
                }
                let sig_id = match rule.input.as_str() {
                    "temp.process" => control_abi::SIGNAL_TEMP_PROCESS,
                    "physical.inhibit" => control_abi::SIGNAL_PHYSICAL_INHIBIT,
                    "start.stop" => control_abi::SIGNAL_START_STOP_PERMIT,
                    "test.aux1" => control_abi::SIGNAL_TEST_AUX_1,
                    other => {
                        if let Some(rest) = other.strip_prefix("sig.") {
                            rest.parse::<u32>().map(|n| 100 + n).unwrap_or(0)
                        } else {
                            0
                        }
                    }
                };
                let ok = signals
                    .iter()
                    .find(|s| s.id == sig_id)
                    .map(|s| s.quality == rule.required_quality && s.sample_age_ms < ttl_ms)
                    .unwrap_or(false);
                if !ok {
                    return self.apply_contingency(
                        now_ns,
                        GuardReason::InputQualityFault,
                        proposed_mask,
                    );
                }
            }
        }

        // 4. Worker timeout or fault
        if !worker_ok {
            return self.apply_contingency(
                now_ns,
                GuardReason::WorkerDeadlineOrFault,
                proposed_mask,
            );
        }

        // 5. Manual lease (with monotonic TTL bound to boot_id)
        let mut reason = GuardReason::None;
        let mut candidate = proposed_mask;
        if let Some(lease) = manual_lease.as_ref() {
            if lease.boot_id != boot_id || now_ns >= lease.expires_logical_ns {
                *manual_lease = None;
                reason = GuardReason::ManualLeaseExpired;
            } else {
                candidate = (proposed_mask & !lease.valid_mask) | (lease.mask & lease.valid_mask);
            }
        }

        // 6. Range / allowed channels mask
        if (candidate & !self.allowed_all_mask) != 0 {
            candidate &= self.allowed_all_mask;
            if reason == GuardReason::None {
                reason = GuardReason::RangeLimit;
            }
        }

        // 7. Fixed site interlocks (e.g., red excludes blue prealarm)
        for &(forbidden_mask, _) in &self.interlocks {
            if (candidate & forbidden_mask) == forbidden_mask {
                // High-priority alarm bit 0 (red) preempts lower-priority virtual/prealarm bit
                let lowest_bit = forbidden_mask & forbidden_mask.wrapping_neg();
                let to_clear = forbidden_mask & !lowest_bit;
                candidate &= !to_clear;
                reason = GuardReason::InterlockVeto;
            }
        }

        // 8. Dwell & anti-chatter per channel
        let mut final_mask = self.prev_applied_mask;
        for tracker in &mut self.trackers {
            if tracker.mask_bit >= 32 {
                continue;
            }
            let bit = 1u32 << tracker.mask_bit;
            let want_on = (candidate & bit) != 0;
            let cur_on = (self.prev_applied_mask & bit) != 0;
            if want_on == cur_on {
                continue;
            }
            if tracker.can_toggle(now_ns) {
                if want_on {
                    final_mask |= bit;
                } else {
                    final_mask &= !bit;
                }
                tracker.record_change(now_ns);
            } else if reason == GuardReason::None {
                reason = GuardReason::DwellChatter;
            }
        }

        // 9. Post-dwell safety re-check: NEVER energize forbidden interlock pair even if dwell held higher/lower bit
        for &(forbidden_mask, _) in &self.interlocks {
            if (final_mask & forbidden_mask) == forbidden_mask {
                let lowest_bit = forbidden_mask & forbidden_mask.wrapping_neg();
                let to_clear = forbidden_mask & !lowest_bit;
                final_mask &= !to_clear;
                reason = GuardReason::InterlockVeto;
            }
        }

        self.prev_applied_mask = final_mask;
        ShieldDecision {
            desired_mask: candidate,
            applied_mask: final_mask,
            guard_reason: reason,
            is_contingency_off: false,
        }
    }

    fn apply_contingency(
        &mut self,
        now_ns: u64,
        reason: GuardReason,
        desired_mask: u32,
    ) -> ShieldDecision {
        // Contingency turn-off always bypasses dwell/anti-chatter
        let changed = self.prev_applied_mask ^ self.safe_fallback_mask;
        for tracker in &mut self.trackers {
            let bit = 1u32 << tracker.mask_bit;
            if (changed & bit) != 0 {
                tracker.record_change(now_ns);
            }
        }
        self.prev_applied_mask = self.safe_fallback_mask;
        ShieldDecision {
            desired_mask,
            applied_mask: self.safe_fallback_mask,
            guard_reason: reason,
            is_contingency_off: true,
        }
    }
}
