use std::time::{Duration, Instant};

use shared_protocol::{
    sanitize_video_fps, vbr_bitrate_range, TargetResolution, ThrottleReason, VbrBitrateRange,
    MIN_ADAPTIVE_BITRATE_BPS,
};

use crate::net::QuicPathTelemetry;

const MILD_DECREASE_COOLDOWN: Duration = Duration::from_millis(700);
const SEVERE_DECREASE_COOLDOWN: Duration = Duration::from_millis(500);
const NET_METRIC_WINDOW: Duration = Duration::from_millis(500);
const STABLE_WINDOW_BEFORE_PROBE: Duration = Duration::from_millis(2500);
const BITRATE_PROBE_INTERVAL: Duration = Duration::from_millis(1500);
const FPS_CHANGE_COOLDOWN: Duration = Duration::from_millis(3000);
const FPS_RECOVERY_COOLDOWN: Duration = Duration::from_millis(4000);
const FLOOR_NETWORK_FPS_SUSTAIN: Duration = Duration::from_millis(2000);

#[derive(Debug, Clone, Copy, Default)]
pub struct AdaptiveInputs {
    pub telemetry: QuicPathTelemetry,
    pub sfu_target_max_bps: Option<u32>,
    pub sfu_min_fps_cap: Option<u8>,
    pub sfu_source: ThrottleReason,
    pub frame_sent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptiveDecision {
    pub target_bps: u32,
    pub vbr_max_bps: u32,
    pub max_bps: u32,
    pub effective_fps: u8,
    pub configured_fps: u8,
    pub reason: ThrottleReason,
    pub bitrate_changed: bool,
    pub fps_changed: bool,
}

fn initial_resolution_tier(resolution: TargetResolution) -> TargetResolution {
    match resolution {
        TargetResolution::Native => TargetResolution::R1080p,
        other => other,
    }
}

/// Deterministic AIMD + hysteresis controller driving table-bound VBR and network FPS fallback.
pub struct AdaptiveBitrateController {
    resolution_tier: TargetResolution,
    source_tier_applied: bool,
    configured_fps: u8,
    max_bps: u32,
    current_bps: u32,
    last_vbr_max_bps: u32,
    effective_fps: u8,
    fps_tiers: Vec<u8>,

    baseline_rtt_ms: Option<u32>,
    rtt_ewma_ms: f32,
    has_prev_counters: bool,
    last_sent_packets: u64,
    last_lost_packets: u64,
    last_congestion_events: u64,

    net_window_start: Instant,
    win_sent_packets: u64,
    win_lost_packets: u64,
    win_congestion_events: u64,
    window_mild_active: bool,
    window_severe_active: bool,

    congested_at_floor_since: Option<Instant>,
    last_decrease_at: Option<Instant>,
    last_increase_at: Option<Instant>,
    last_fps_change_at: Option<Instant>,
    healthy_since: Instant,
    active_reason: ThrottleReason,
}

impl AdaptiveBitrateController {
    pub fn new(
        resolution: TargetResolution,
        configured_fps: u8,
        now: Instant,
    ) -> Self {
        let clamped_fps = sanitize_video_fps(configured_fps);
        let resolution_tier = initial_resolution_tier(resolution);
        let range = vbr_bitrate_range(resolution_tier, clamped_fps);
        let fps_tiers = build_fps_tiers(clamped_fps);

        Self {
            resolution_tier,
            source_tier_applied: false,
            configured_fps: clamped_fps,
            max_bps: range.max_bps,
            current_bps: range.initial_bps(),
            last_vbr_max_bps: range.max_bps,
            effective_fps: clamped_fps,
            fps_tiers,
            baseline_rtt_ms: None,
            rtt_ewma_ms: 0.0,
            has_prev_counters: false,
            last_sent_packets: 0,
            last_lost_packets: 0,
            last_congestion_events: 0,
            net_window_start: now,
            win_sent_packets: 0,
            win_lost_packets: 0,
            win_congestion_events: 0,
            window_mild_active: false,
            window_severe_active: false,
            congested_at_floor_since: None,
            last_decrease_at: None,
            last_increase_at: None,
            last_fps_change_at: None,
            healthy_since: now,
            active_reason: ThrottleReason::None,
        }
    }

    pub fn active_range(&self) -> VbrBitrateRange {
        vbr_bitrate_range(self.resolution_tier, self.effective_fps)
    }

    pub fn current_bps(&self) -> u32 {
        self.current_bps
    }

    pub fn vbr_max_bps(&self) -> u32 {
        self.last_vbr_max_bps
    }

    pub fn max_bps(&self) -> u32 {
        self.max_bps
    }

    pub fn effective_fps(&self) -> u8 {
        self.effective_fps
    }

    pub fn configured_fps(&self) -> u8 {
        self.configured_fps
    }

    pub fn reason(&self) -> ThrottleReason {
        self.active_reason
    }

    fn compute_vbr_max_bps(&self, range: VbrBitrateRange, effective_cap: u32) -> u32 {
        if self.active_reason == ThrottleReason::None && effective_cap >= range.max_bps {
            range.max_bps
        } else {
            let burst = ((u64::from(self.current_bps) * 115) / 100) as u32;
            burst
                .min(effective_cap)
                .min(range.max_bps)
                .max(self.current_bps)
        }
    }

    /// Refines the bitrate table cell once the real captured frame dimensions are known.
    pub fn apply_source_resolution(
        &mut self,
        configured_resolution: TargetResolution,
        frame_width: u32,
        frame_height: u32,
    ) -> bool {
        let tier = configured_resolution.effective_bitrate_tier(frame_width, frame_height);
        let tier_changed = tier != self.resolution_tier;
        let first_apply = !self.source_tier_applied;
        self.source_tier_applied = true;
        self.resolution_tier = tier;

        let range = self.active_range();
        let prev_bps = self.current_bps;
        let prev_vbr_max = self.last_vbr_max_bps;
        self.max_bps = range.max_bps;

        if first_apply && tier_changed {
            self.current_bps = range.initial_bps();
        } else {
            self.current_bps = self.current_bps.clamp(MIN_ADAPTIVE_BITRATE_BPS, range.max_bps);
        }
        self.last_vbr_max_bps = self.compute_vbr_max_bps(range, range.max_bps);

        self.current_bps != prev_bps || self.last_vbr_max_bps != prev_vbr_max
    }

    pub fn step(&mut self, inputs: AdaptiveInputs, now: Instant) -> AdaptiveDecision {
        let prev_bps = self.current_bps;
        let prev_vbr_max = self.last_vbr_max_bps;
        let prev_fps = self.effective_fps;

        let (sent_delta, lost_delta, cong_delta) = if self.has_prev_counters {
            (
                inputs.telemetry.sent_packets.saturating_sub(self.last_sent_packets),
                inputs.telemetry.lost_packets.saturating_sub(self.last_lost_packets),
                inputs
                    .telemetry
                    .congestion_events
                    .saturating_sub(self.last_congestion_events),
            )
        } else {
            self.has_prev_counters = true;
            (0, 0, 0)
        };
        self.last_sent_packets = inputs.telemetry.sent_packets;
        self.last_lost_packets = inputs.telemetry.lost_packets;
        self.last_congestion_events = inputs.telemetry.congestion_events;

        self.win_sent_packets = self.win_sent_packets.saturating_add(sent_delta);
        self.win_lost_packets = self.win_lost_packets.saturating_add(lost_delta);
        self.win_congestion_events = self.win_congestion_events.saturating_add(cong_delta);

        let rtt = inputs.telemetry.rtt_ms;
        if rtt > 0 {
            self.baseline_rtt_ms = Some(match self.baseline_rtt_ms {
                Some(b) => b.min(rtt),
                None => rtt,
            });
            if self.rtt_ewma_ms <= 0.0 {
                self.rtt_ewma_ms = rtt as f32;
            } else {
                self.rtt_ewma_ms = 0.25 * (rtt as f32) + 0.75 * self.rtt_ewma_ms;
            }
        }

        if now.duration_since(self.net_window_start) >= NET_METRIC_WINDOW {
            let loss_mild = (self.win_sent_packets >= 15
                && self.win_lost_packets >= 2
                && self.win_lost_packets.saturating_mul(100)
                    >= self.win_sent_packets.saturating_mul(3))
                || self.win_congestion_events >= 3;
            let loss_severe = (self.win_sent_packets >= 15
                && self.win_lost_packets >= 4
                && self.win_lost_packets.saturating_mul(100)
                    >= self.win_sent_packets.saturating_mul(6))
                || self.win_congestion_events >= 6;

            let rtt_mild = self.baseline_rtt_ms.is_some_and(|b| {
                let b_f = b as f32;
                self.rtt_ewma_ms > (b_f * 1.8).max(b_f + 60.0) && self.rtt_ewma_ms > 100.0
            });
            let rtt_severe = self.baseline_rtt_ms.is_some_and(|b| {
                let b_f = b as f32;
                self.rtt_ewma_ms > (b_f * 2.5).max(b_f + 110.0) && self.rtt_ewma_ms > 160.0
            });

            self.window_mild_active = loss_mild || rtt_mild;
            self.window_severe_active = loss_severe || rtt_severe;
            self.win_sent_packets = 0;
            self.win_lost_packets = 0;
            self.win_congestion_events = 0;
            self.net_window_start = now;
        }

        let network_severe = !inputs.frame_sent
            || inputs.telemetry.send_stall_flagged
            || inputs.telemetry.in_flight_video >= 6
            || self.window_severe_active;
        let network_mild =
            network_severe || inputs.telemetry.in_flight_video >= 4 || self.window_mild_active;

        let mut range = self.active_range();
        self.max_bps = range.max_bps;
        let sfu_cap = inputs
            .sfu_target_max_bps
            .map(|b| b.max(MIN_ADAPTIVE_BITRATE_BPS));
        let mut effective_cap = sfu_cap.map_or(range.max_bps, |cap| range.max_bps.min(cap));
        let viewer_limiting = sfu_cap.is_some_and(|c| c < range.max_bps)
            || inputs.sfu_min_fps_cap.is_some_and(|f| f < self.configured_fps);

        if network_mild || viewer_limiting {
            self.healthy_since = now;
        }

        // 1. Respect SFU viewer cap immediately (including emergency reduction below table min)
        if self.current_bps > effective_cap {
            self.current_bps = effective_cap;
            self.last_decrease_at = Some(now);
            self.active_reason = if inputs.sfu_source != ThrottleReason::None {
                inputs.sfu_source
            } else {
                ThrottleReason::Viewer
            };
        }

        // 2. Local QUIC upload congestion AIMD decrease
        //    - Mild stays within table [min, max] (or SFU cap if lower)
        //    - Severe engages emergency backoff below table min down to 400 kbps
        if network_severe {
            let cooldown_ok = self
                .last_decrease_at
                .is_none_or(|t| now.duration_since(t) >= SEVERE_DECREASE_COOLDOWN);
            if cooldown_ok && self.current_bps > MIN_ADAPTIVE_BITRATE_BPS {
                let next = ((u64::from(self.current_bps) * 65) / 100) as u32;
                self.current_bps = next.clamp(MIN_ADAPTIVE_BITRATE_BPS, effective_cap);
                self.last_decrease_at = Some(now);
                self.active_reason = ThrottleReason::Network;
            }
        } else if network_mild {
            let mild_floor = range.min_bps.min(effective_cap);
            let cooldown_ok = self
                .last_decrease_at
                .is_none_or(|t| now.duration_since(t) >= MILD_DECREASE_COOLDOWN);
            if cooldown_ok && self.current_bps > mild_floor {
                let next = ((u64::from(self.current_bps) * 80) / 100) as u32;
                self.current_bps = next.clamp(mild_floor, effective_cap);
                self.last_decrease_at = Some(now);
                self.active_reason = ThrottleReason::Network;
            }
        }

        // 3. Network/SFU FPS fallback ONLY for 60 -> 30 FPS (30/15/5 remain fixed)
        if let Some(sfu_fps) = inputs.sfu_min_fps_cap {
            if sfu_fps < self.effective_fps {
                let target_tier = self.highest_fps_not_exceeding(sfu_fps);
                if target_tier < self.effective_fps {
                    self.effective_fps = target_tier;
                    self.last_fps_change_at = Some(now);
                    self.active_reason = ThrottleReason::Viewer;
                    range = self.active_range();
                    self.max_bps = range.max_bps;
                    effective_cap = sfu_cap.map_or(range.max_bps, |cap| range.max_bps.min(cap));
                    self.current_bps = self.current_bps.min(effective_cap);
                }
            }
        }

        let floor_network_sustained =
            if self.effective_fps > 30 && network_mild && self.current_bps < range.min_bps {
                let start = *self.congested_at_floor_since.get_or_insert(now);
                now.duration_since(start) >= FLOOR_NETWORK_FPS_SUSTAIN
            } else {
                self.congested_at_floor_since = None;
                false
            };

        if floor_network_sustained
            && self
                .last_fps_change_at
                .is_none_or(|t| now.duration_since(t) >= FPS_CHANGE_COOLDOWN)
        {
            if let Some(lower_fps) = self.next_lower_fps() {
                self.effective_fps = lower_fps;
                self.last_fps_change_at = Some(now);
                self.congested_at_floor_since = None;
                self.active_reason = ThrottleReason::Network;
                range = self.active_range();
                self.max_bps = range.max_bps;
                effective_cap = sfu_cap.map_or(range.max_bps, |cap| range.max_bps.min(cap));
                self.current_bps = self.current_bps.min(effective_cap);
            }
        }

        // 4. Probe-up recovery when path is stable (FPS recovery -> probe inside VBR table)
        let stable_enough = now.duration_since(self.healthy_since) >= STABLE_WINDOW_BEFORE_PROBE
            && self
                .last_decrease_at
                .is_none_or(|t| now.duration_since(t) >= STABLE_WINDOW_BEFORE_PROBE);

        if !network_mild && stable_enough {
            if self.effective_fps < self.configured_fps {
                let fps_cooldown_ok = self
                    .last_fps_change_at
                    .is_none_or(|t| now.duration_since(t) >= FPS_RECOVERY_COOLDOWN);
                if fps_cooldown_ok {
                    if let Some(higher_fps) = self.next_higher_fps() {
                        let higher_range = vbr_bitrate_range(self.resolution_tier, higher_fps);
                        let allowed_by_sfu_fps =
                            inputs.sfu_min_fps_cap.is_none_or(|c| higher_fps <= c);
                        let allowed_by_sfu_bps =
                            sfu_cap.is_none_or(|c| c >= higher_range.min_bps);
                        let bitrate_ready = self.current_bps >= (range.max_bps * 85) / 100
                            || (effective_cap >= range.max_bps
                                && now.duration_since(self.healthy_since) >= FPS_RECOVERY_COOLDOWN);
                        if allowed_by_sfu_fps && allowed_by_sfu_bps && bitrate_ready {
                            self.effective_fps = higher_fps;
                            self.last_fps_change_at = Some(now);
                            range = higher_range;
                            self.max_bps = range.max_bps;
                            effective_cap =
                                sfu_cap.map_or(range.max_bps, |cap| range.max_bps.min(cap));
                            self.current_bps = self
                                .current_bps
                                .max(range.min_bps)
                                .min(effective_cap);
                        }
                    }
                }
            }

            if self.current_bps < effective_cap
                && self
                    .last_increase_at
                    .is_none_or(|t| now.duration_since(t) >= BITRATE_PROBE_INTERVAL)
            {
                let step = (self.current_bps / 10).clamp(50_000, 1_500_000);
                self.current_bps = self.current_bps.saturating_add(step).min(effective_cap);
                self.last_increase_at = Some(now);
            }
        }

        // Clear active throttle banner once recovered near table ceiling at full FPS without limits
        if !network_mild
            && !viewer_limiting
            && self.effective_fps == self.configured_fps
            && self.current_bps >= (self.max_bps * 95) / 100
        {
            self.active_reason = ThrottleReason::None;
        } else if viewer_limiting
            && self.active_reason == ThrottleReason::None
            && (self.current_bps < self.max_bps || self.effective_fps < self.configured_fps)
        {
            self.active_reason = ThrottleReason::Viewer;
        }

        let vbr_max_bps = self.compute_vbr_max_bps(range, effective_cap);
        self.last_vbr_max_bps = vbr_max_bps;

        AdaptiveDecision {
            target_bps: self.current_bps,
            vbr_max_bps,
            max_bps: self.max_bps,
            effective_fps: self.effective_fps,
            configured_fps: self.configured_fps,
            reason: self.active_reason,
            bitrate_changed: self.current_bps != prev_bps || vbr_max_bps != prev_vbr_max,
            fps_changed: self.effective_fps != prev_fps,
        }
    }

    fn next_lower_fps(&self) -> Option<u8> {
        let idx = self
            .fps_tiers
            .iter()
            .position(|&f| f == self.effective_fps)?;
        self.fps_tiers.get(idx + 1).copied()
    }

    fn next_higher_fps(&self) -> Option<u8> {
        let idx = self
            .fps_tiers
            .iter()
            .position(|&f| f == self.effective_fps)?;
        if idx == 0 {
            None
        } else {
            self.fps_tiers.get(idx - 1).copied()
        }
    }

    fn highest_fps_not_exceeding(&self, cap: u8) -> u8 {
        self.fps_tiers
            .iter()
            .copied()
            .find(|&f| f <= cap)
            .unwrap_or(*self.fps_tiers.last().unwrap_or(&self.effective_fps))
    }
}

fn build_fps_tiers(configured_fps: u8) -> Vec<u8> {
    if configured_fps >= 45 {
        vec![configured_fps, 30]
    } else {
        vec![configured_fps]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_inputs() -> AdaptiveInputs {
        AdaptiveInputs {
            telemetry: QuicPathTelemetry {
                rtt_ms: 25,
                cwnd_bytes: 120_000,
                sent_packets: 100,
                lost_packets: 0,
                congestion_events: 0,
                in_flight_video: 0,
                send_stall_flagged: false,
            },
            sfu_target_max_bps: None,
            sfu_min_fps_cap: None,
            sfu_source: ThrottleReason::None,
            frame_sent: true,
        }
    }

    #[test]
    fn test_vbr_bounds_across_table_and_emergency_backoff() {
        let t0 = Instant::now();
        let mut ctrl = AdaptiveBitrateController::new(TargetResolution::R1080p, 30, t0);
        // 1080p @ 30 FPS: 2.8 – 4.5 Mbps
        assert_eq!(ctrl.max_bps(), 4_500_000);
        assert!(ctrl.current_bps() > 2_800_000 && ctrl.current_bps() < 4_500_000);
        assert_eq!(ctrl.vbr_max_bps(), 4_500_000);
        assert_eq!(ctrl.reason(), ThrottleReason::None);

        // Prime baseline counters
        let _ = ctrl.step(healthy_inputs(), t0);

        // Mild congestion navigates inside [2.8 Mbps, 4.5 Mbps] without dropping below table min
        let mut mild = healthy_inputs();
        mild.telemetry.in_flight_video = 4;
        let mut t = t0;
        for _ in 0..8 {
            t += Duration::from_millis(800);
            mild.telemetry.sent_packets += 50;
            ctrl.step(mild, t);
        }
        assert_eq!(ctrl.current_bps(), 2_800_000);
        assert_eq!(ctrl.effective_fps(), 30);

        // Severe upload stall drops below table min toward 400 kbps emergency floor
        let mut severe = healthy_inputs();
        severe.telemetry.in_flight_video = 6;
        severe.frame_sent = false;
        for _ in 0..6 {
            t += Duration::from_millis(600);
            severe.telemetry.sent_packets += 50;
            severe.telemetry.lost_packets += 5;
            ctrl.step(severe, t);
        }
        assert!(ctrl.current_bps() < 2_800_000);
        assert_eq!(ctrl.effective_fps(), 30);
        assert_eq!(ctrl.reason(), ThrottleReason::Network);

        // Healthy recovery probes smoothly back up to 4.5 Mbps ceiling
        let mut ok = healthy_inputs();
        ok.telemetry.sent_packets = severe.telemetry.sent_packets;
        ok.telemetry.lost_packets = severe.telemetry.lost_packets;
        for _ in 0..32 {
            t += Duration::from_millis(1600);
            ok.telemetry.sent_packets += 50;
            ctrl.step(ok, t);
        }
        assert_eq!(ctrl.current_bps(), 4_500_000);
        assert_eq!(ctrl.vbr_max_bps(), 4_500_000);
        assert_eq!(ctrl.reason(), ThrottleReason::None);
    }

    #[test]
    fn test_720p_reading_mode_5fps_never_drops_fps_and_respects_600_900k_band() {
        let t0 = Instant::now();
        let mut ctrl = AdaptiveBitrateController::new(TargetResolution::R720p, 5, t0);
        assert_eq!(ctrl.configured_fps(), 5);
        assert_eq!(ctrl.effective_fps(), 5);
        assert_eq!(ctrl.max_bps(), 900_000);
        assert!(ctrl.current_bps() >= 600_000 && ctrl.current_bps() <= 900_000);

        let _ = ctrl.step(healthy_inputs(), t0);

        // Mild congestion keeps bitrate at 600 kbps table floor and never lowers 5 FPS
        let mut mild = healthy_inputs();
        mild.telemetry.in_flight_video = 4;
        let mut t = t0;
        for _ in 0..8 {
            t += Duration::from_millis(800);
            ctrl.step(mild, t);
        }
        assert_eq!(ctrl.current_bps(), 600_000);
        assert_eq!(ctrl.effective_fps(), 5);

        // Healthy path probes gently up to 900 kbps without overshooting
        let mut ok = healthy_inputs();
        for _ in 0..10 {
            t += Duration::from_millis(1600);
            let dec = ctrl.step(ok, t);
            assert!(dec.vbr_max_bps <= 900_000);
            ok.telemetry.sent_packets += 20;
        }
        assert_eq!(ctrl.current_bps(), 900_000);
        assert_eq!(ctrl.effective_fps(), 5);
    }

    #[test]
    fn test_moderate_in_flight_and_single_loss_do_not_throttle_60fps_stream() {
        let t0 = Instant::now();
        let mut ctrl = AdaptiveBitrateController::new(TargetResolution::R1080p, 60, t0);
        let _ = ctrl.step(healthy_inputs(), t0);
        let initial = ctrl.current_bps();

        let mut normal = healthy_inputs();
        for i in 1..=60 {
            normal.telemetry.sent_packets += 12;
            normal.telemetry.in_flight_video = if i % 4 == 0 { 3 } else { 2 };
            if i == 15 {
                normal.telemetry.lost_packets += 1;
                normal.telemetry.congestion_events += 1;
            }
            let dec = ctrl.step(normal, t0 + Duration::from_millis(i * 22));
            assert_eq!(dec.effective_fps, 60);
            assert_eq!(dec.reason, ThrottleReason::None);
        }
        assert!(ctrl.current_bps() >= initial);
    }

    #[test]
    fn test_60fps_fallback_to_30fps_switches_to_30fps_vbr_range_and_recovers() {
        let t0 = Instant::now();
        let mut ctrl = AdaptiveBitrateController::new(TargetResolution::R1080p, 60, t0);
        assert_eq!(ctrl.max_bps(), 7_000_000);
        let _ = ctrl.step(healthy_inputs(), t0);

        // Severe stall pushes bitrate below 4.5 Mbps floor toward 400 kbps
        let mut stalled = healthy_inputs();
        stalled.frame_sent = false;
        stalled.telemetry.in_flight_video = 6;
        let mut t = t0 + Duration::from_millis(600);
        for _ in 0..8 {
            t += Duration::from_millis(550);
            ctrl.step(stalled, t);
        }
        assert_eq!(ctrl.current_bps(), MIN_ADAPTIVE_BITRATE_BPS);

        // Hold stall >= 2s below cell min -> steps FPS 60 -> 30 and switches range to 1080p30
        let dec2 = ctrl.step(stalled, t + Duration::from_millis(2200));
        assert_eq!(dec2.effective_fps, 30);
        assert_eq!(dec2.max_bps, 4_500_000);

        // Prolonged severe stall never drops below 30 FPS
        ctrl.step(stalled, t + Duration::from_millis(4500));
        let dec3 = ctrl.step(stalled, t + Duration::from_millis(8000));
        assert_eq!(dec3.effective_fps, 30);

        // Clean recovery probes up and returns smoothly from 30 -> 60 FPS
        let mut ok = healthy_inputs();
        ok.telemetry.sent_packets = 1000;
        let mut rec = dec3;
        let mut t_ok = t + Duration::from_millis(8500);
        for _ in 0..32 {
            t_ok += Duration::from_millis(1600);
            ok.telemetry.sent_packets += 50;
            rec = ctrl.step(ok, t_ok);
        }
        assert_eq!(rec.effective_fps, 60);
        assert_eq!(rec.max_bps, 7_000_000);
        assert_eq!(rec.target_bps, 7_000_000);
    }

    #[test]
    fn test_native_4k_source_clamps_to_1440p_tier() {
        let t0 = Instant::now();
        let mut ctrl = AdaptiveBitrateController::new(TargetResolution::Native, 60, t0);
        assert!(ctrl.apply_source_resolution(TargetResolution::Native, 3840, 2160));
        assert_eq!(ctrl.max_bps(), 12_000_000);
        assert_eq!(ctrl.current_bps(), 10_500_000);
        assert_eq!(ctrl.vbr_max_bps(), 12_000_000);
    }
}
