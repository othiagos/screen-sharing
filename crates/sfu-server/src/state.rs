use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};
use shared_protocol::{
    ControlErrorCode, PeerId, PeerInfo, RoomId, ServerControl, StreamMetadata, ThrottleReason,
    MAX_ADAPTIVE_BITRATE_BPS, MIN_ADAPTIVE_BITRATE_BPS, SFU_MINORITY_FLOOR_BPS,
};
use tokio::sync::mpsc;
use uuid::Uuid;

const KEYFRAME_COOLDOWN: Duration = Duration::from_millis(1500);
const OUTLIER_KEYFRAME_COOLDOWN: Duration = Duration::from_millis(4000);
const VIDEO_QUEUE_CAPACITY: usize = 16;
const AUDIO_QUEUE_CAPACITY: usize = 128;
const UNCONSTRAINED_BPS: u32 = u32::MAX;
const VIEWER_DECREASE_COOLDOWN: Duration = Duration::from_millis(700);
const VIEWER_INCREASE_INTERVAL: Duration = Duration::from_millis(1500);
const VIEWER_STABLE_BEFORE_PROBE: Duration = Duration::from_millis(2000);
const HINT_KEEPALIVE_INTERVAL: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone)]
pub enum OutboundMedia {
    Video {
        broadcaster_id: PeerId,
        is_keyframe: bool,
        packet: Bytes,
    },
    Audio {
        broadcaster_id: PeerId,
        packet: Bytes,
    },
}

#[derive(Debug)]
pub struct ViewerCongestionState {
    pub suggested_bps: u32,
    pub last_decrease_at: Option<Instant>,
    pub last_increase_at: Option<Instant>,
    pub last_congested_at: Option<Instant>,
    pub clean_since: Option<Instant>,
}

impl Default for ViewerCongestionState {
    fn default() -> Self {
        Self {
            suggested_bps: UNCONSTRAINED_BPS,
            last_decrease_at: None,
            last_increase_at: None,
            last_congested_at: None,
            clean_since: None,
        }
    }
}

impl ViewerCongestionState {
    fn note_congestion(&mut self, stream_bps: u32, severe: bool, now: Instant) -> bool {
        self.last_congested_at = Some(now);
        self.clean_since = None;
        if let Some(prev) = self.last_decrease_at {
            let cooldown = if severe {
                Duration::from_millis(500)
            } else {
                VIEWER_DECREASE_COOLDOWN
            };
            if now.duration_since(prev) < cooldown {
                return false;
            }
        }
        let anchor = if self.suggested_bps == UNCONSTRAINED_BPS {
            let measured = if stream_bps > 0 { stream_bps } else { 8_000_000 };
            measured.clamp(MIN_ADAPTIVE_BITRATE_BPS, MAX_ADAPTIVE_BITRATE_BPS)
        } else {
            self.suggested_bps
        };
        let factor = if severe { 0.68 } else { 0.80 };
        let next = ((anchor as f64) * factor).round() as u32;
        let clamped = next.max(MIN_ADAPTIVE_BITRATE_BPS);
        let changed = clamped != self.suggested_bps;
        self.suggested_bps = clamped;
        self.last_decrease_at = Some(now);
        changed
    }

    fn note_clean(&mut self, stream_bps: u32, now: Instant) -> bool {
        if self.suggested_bps == UNCONSTRAINED_BPS {
            return false;
        }
        if self
            .last_congested_at
            .is_some_and(|t| now.duration_since(t) < VIEWER_STABLE_BEFORE_PROBE)
        {
            self.clean_since = None;
            return false;
        }
        let clean_start = *self.clean_since.get_or_insert(now);
        if now.duration_since(clean_start) < VIEWER_STABLE_BEFORE_PROBE {
            return false;
        }
        if let Some(prev) = self.last_increase_at {
            if now.duration_since(prev) < VIEWER_INCREASE_INTERVAL {
                return false;
            }
        }
        let next = ((self.suggested_bps as f64) * 1.15).round() as u32 + 150_000;
        let release_threshold = stream_bps.max(8_000_000).saturating_mul(5) / 4;
        self.last_increase_at = Some(now);
        if next >= release_threshold.min(20_000_000) {
            self.suggested_bps = UNCONSTRAINED_BPS;
            self.clean_since = None;
        } else {
            self.suggested_bps = next;
        }
        true
    }
}

#[derive(Debug)]
pub struct ViewerSubscription {
    pub video: bool,
    pub audio: bool,
    pub awaiting_keyframe: AtomicBool,
    pub congestion: Mutex<ViewerCongestionState>,
}

impl ViewerSubscription {
    pub fn new(video: bool, audio: bool) -> Self {
        Self {
            video,
            audio,
            awaiting_keyframe: AtomicBool::new(video),
            congestion: Mutex::new(ViewerCongestionState::default()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SentHintState {
    target_max_bps: Option<u32>,
    min_fps_cap: Option<u8>,
    congestion_source: ThrottleReason,
    sent_at: Instant,
}

#[derive(Debug, Default)]
struct BitrateMeter {
    window_start: Option<Instant>,
    window_bytes: u64,
    last_bps: u32,
}

impl BitrateMeter {
    fn record(&mut self, bytes: usize, now: Instant) -> u32 {
        let start = *self.window_start.get_or_insert(now);
        self.window_bytes += bytes as u64;
        let elapsed = now.duration_since(start);
        if elapsed >= Duration::from_secs(1) {
            let bits = (self.window_bytes as f64) * 8.0 / elapsed.as_secs_f64();
            self.last_bps = bits.round() as u32;
            self.window_bytes = 0;
            self.window_start = Some(now);
        }
        self.last_bps
    }

    fn reset(&mut self) {
        self.window_start = None;
        self.window_bytes = 0;
        self.last_bps = 0;
    }
}

pub struct ConnectedPeer {
    pub peer_id: PeerId,
    pub nickname: RwLock<String>,
    pub control_tx: mpsc::UnboundedSender<ServerControl>,
    pub video_tx: mpsc::Sender<OutboundMedia>,
    pub audio_tx: mpsc::Sender<OutboundMedia>,
    pub active_stream: RwLock<Option<StreamMetadata>>,
    pub slow_outbound_flag: AtomicBool,
    measured_video_bps: AtomicU32,
    rate_meter: Mutex<BitrateMeter>,
    last_keyframe_req: Mutex<Option<Instant>>,
    severe_majority_since: Mutex<Option<Instant>>,
    last_sent_hint: Mutex<Option<SentHintState>>,
}

impl ConnectedPeer {
    pub fn new(
        peer_id: PeerId,
        nickname: String,
        control_tx: mpsc::UnboundedSender<ServerControl>,
    ) -> (
        Self,
        mpsc::Receiver<OutboundMedia>,
        mpsc::Receiver<OutboundMedia>,
    ) {
        let (video_tx, video_rx) = mpsc::channel(VIDEO_QUEUE_CAPACITY);
        let (audio_tx, audio_rx) = mpsc::channel(AUDIO_QUEUE_CAPACITY);
        let peer = Self {
            peer_id,
            nickname: RwLock::new(nickname),
            control_tx,
            video_tx,
            audio_tx,
            active_stream: RwLock::new(None),
            slow_outbound_flag: AtomicBool::new(false),
            measured_video_bps: AtomicU32::new(0),
            rate_meter: Mutex::new(BitrateMeter::default()),
            last_keyframe_req: Mutex::new(None),
            severe_majority_since: Mutex::new(None),
            last_sent_hint: Mutex::new(None),
        };
        (peer, video_rx, audio_rx)
    }

    pub fn to_peer_info(&self) -> PeerInfo {
        PeerInfo {
            peer_id: self.peer_id,
            nickname: self.nickname.read().clone(),
            active_stream: self.active_stream.read().clone(),
        }
    }

    pub fn send_control(&self, msg: ServerControl) {
        let _ = self.control_tx.send(msg);
    }

    pub fn try_request_keyframe(&self) {
        self.try_request_keyframe_with_cooldown(KEYFRAME_COOLDOWN);
    }

    pub fn try_request_keyframe_with_cooldown(&self, cooldown: Duration) {
        let mut guard = self.last_keyframe_req.lock();
        let now = Instant::now();
        if let Some(prev) = *guard {
            if now.duration_since(prev) < cooldown {
                return;
            }
        }
        *guard = Some(now);
        drop(guard);
        self.send_control(ServerControl::ForceKeyframe);
    }

    pub fn record_inbound_video_bytes(&self, bytes: usize, now: Instant) -> u32 {
        let bps = self.rate_meter.lock().record(bytes, now);
        if bps > 0 {
            self.measured_video_bps.store(bps, Ordering::Relaxed);
        }
        self.measured_video_bps.load(Ordering::Relaxed)
    }

    pub fn reset_broadcast_telemetry(&self) {
        self.measured_video_bps.store(0, Ordering::Relaxed);
        self.rate_meter.lock().reset();
        *self.severe_majority_since.lock() = None;
        *self.last_sent_hint.lock() = None;
    }
}

pub struct RoomState {
    pub room_id: RoomId,
    password_hash: Option<[u8; 32]>,
    pub peers: DashMap<PeerId, Arc<ConnectedPeer>>,
    /// Maps `broadcaster_id` -> Map of `viewer_id` -> `ViewerSubscription`.
    pub subscriptions: DashMap<PeerId, DashMap<PeerId, ViewerSubscription>>,
}

impl RoomState {
    fn new(room_id: RoomId, password: Option<&str>) -> Self {
        let password_hash = password
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| hash_room_password(&room_id, p));
        Self {
            room_id,
            password_hash,
            peers: DashMap::new(),
            subscriptions: DashMap::new(),
        }
    }

    fn verify_password(&self, candidate: Option<&str>) -> bool {
        match self.password_hash {
            None => true,
            Some(expected) => {
                let provided = candidate.unwrap_or("").trim();
                if provided.is_empty() {
                    return false;
                }
                let actual = hash_room_password(&self.room_id, provided);
                constant_time_eq(&expected, &actual)
            }
        }
    }
}

pub struct SfuState {
    auth_token: Option<String>,
    peers: DashMap<PeerId, Arc<ConnectedPeer>>,
    rooms: DashMap<RoomId, Arc<RoomState>>,
    peer_rooms: DashMap<PeerId, RoomId>,
}

impl SfuState {
    pub fn new(auth_token: Option<String>) -> Arc<Self> {
        let normalized = auth_token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        Arc::new(Self {
            auth_token: normalized,
            peers: DashMap::new(),
            rooms: DashMap::new(),
            peer_rooms: DashMap::new(),
        })
    }

    pub fn verify_auth_token(&self, candidate: Option<&str>) -> bool {
        match &self.auth_token {
            None => true,
            Some(expected) => {
                let provided = candidate.unwrap_or("").trim();
                constant_time_eq(expected.as_bytes(), provided.as_bytes())
            }
        }
    }

    pub fn register_peer(&self, peer: Arc<ConnectedPeer>) {
        self.peers.insert(peer.peer_id, peer);
    }

    pub fn active_room_count(&self) -> usize {
        self.rooms.len()
    }

    pub fn active_peer_count(&self) -> usize {
        self.peers.len()
    }

    pub fn create_room(&self, peer_id: PeerId, password: Option<&str>) -> Result<RoomId, (ControlErrorCode, String)> {
        self.leave_current_room(peer_id);

        let peer = self
            .peers
            .get(&peer_id)
            .map(|p| Arc::clone(p.value()))
            .ok_or_else(|| (ControlErrorCode::Internal, "Peer session not registered".to_string()))?;

        let room_id = generate_short_room_id();
        let room = Arc::new(RoomState::new(room_id.clone(), password));
        room.peers.insert(peer_id, Arc::clone(&peer));

        self.rooms.insert(room_id.clone(), room);
        self.peer_rooms.insert(peer_id, room_id.clone());

        peer.send_control(ServerControl::RoomJoined {
            room_id: room_id.clone(),
            local_peer_id: peer_id,
            peers: Vec::new(),
        });

        Ok(room_id)
    }

    pub fn join_room(
        &self,
        room_id: &str,
        peer_id: PeerId,
        password: Option<&str>,
    ) -> Result<(), (ControlErrorCode, String)> {
        let trimmed_id = room_id.trim();
        if trimmed_id.is_empty() {
            return Err((
                ControlErrorCode::BadRequest,
                "Room ID cannot be empty".to_string(),
            ));
        }

        let room = self
            .rooms
            .get(trimmed_id)
            .map(|r| Arc::clone(r.value()))
            .ok_or_else(|| {
                (
                    ControlErrorCode::RoomNotFound,
                    format!("Room '{trimmed_id}' not found"),
                )
            })?;

        if !room.verify_password(password) {
            return Err((
                ControlErrorCode::InvalidPassword,
                "Invalid room password".to_string(),
            ));
        }

        self.leave_current_room(peer_id);

        let peer = self
            .peers
            .get(&peer_id)
            .map(|p| Arc::clone(p.value()))
            .ok_or_else(|| (ControlErrorCode::Internal, "Peer session not registered".to_string()))?;

        let existing_peers: Vec<PeerInfo> = room
            .peers
            .iter()
            .filter(|entry| *entry.key() != peer_id)
            .map(|entry| entry.value().to_peer_info())
            .collect();

        let new_peer_info = peer.to_peer_info();
        for existing in room.peers.iter() {
            if *existing.key() != peer_id {
                existing.value().send_control(ServerControl::PeerJoined {
                    peer: new_peer_info.clone(),
                });
            }
        }

        room.peers.insert(peer_id, Arc::clone(&peer));
        self.peer_rooms.insert(peer_id, trimmed_id.to_string());

        peer.send_control(ServerControl::RoomJoined {
            room_id: trimmed_id.to_string(),
            local_peer_id: peer_id,
            peers: existing_peers,
        });

        Ok(())
    }

    pub fn leave_current_room(&self, peer_id: PeerId) {
        let Some((_, room_id)) = self.peer_rooms.remove(&peer_id) else {
            return;
        };

        if let Some(peer) = self.peers.get(&peer_id) {
            *peer.active_stream.write() = None;
            peer.reset_broadcast_telemetry();
        }

        let Some(room) = self.rooms.get(&room_id).map(|r| Arc::clone(r.value())) else {
            return;
        };

        room.peers.remove(&peer_id);
        room.subscriptions.remove(&peer_id);
        let mut affected_broadcasters = Vec::new();
        for mut sub_map in room.subscriptions.iter_mut() {
            if sub_map.value_mut().remove(&peer_id).is_some() {
                affected_broadcasters.push(*sub_map.key());
            }
        }
        let now = Instant::now();
        for broadcaster_id in affected_broadcasters {
            Self::recompute_broadcaster_hint(&room, broadcaster_id, now);
        }

        for other in room.peers.iter() {
            other.value().send_control(ServerControl::PeerLeft { peer_id });
        }

        if room.peers.is_empty() {
            self.rooms.remove_if(&room_id, |_, r| r.peers.is_empty());
        }
    }

    pub fn remove_peer(&self, peer_id: PeerId) {
        self.leave_current_room(peer_id);
        self.peers.remove(&peer_id);
    }

    pub fn set_broadcast(&self, peer_id: PeerId, metadata: Option<StreamMetadata>) {
        let Some(room) = self.room_for_peer(peer_id) else {
            return;
        };

        if let Some(peer) = room.peers.get(&peer_id) {
            *peer.active_stream.write() = metadata.clone();
            peer.reset_broadcast_telemetry();
        }

        match metadata {
            Some(meta) => {
                for entry in room.peers.iter() {
                    if *entry.key() != peer_id {
                        entry.value().send_control(ServerControl::PeerStreamStarted {
                            peer_id,
                            metadata: meta.clone(),
                        });
                    }
                }
            }
            None => {
                room.subscriptions.remove(&peer_id);
                for entry in room.peers.iter() {
                    if *entry.key() != peer_id {
                        entry
                            .value()
                            .send_control(ServerControl::PeerStreamStopped { peer_id });
                    }
                }
            }
        }
    }

    pub fn set_subscription(
        &self,
        viewer_id: PeerId,
        broadcaster_id: PeerId,
        video: bool,
        audio: bool,
    ) {
        if viewer_id == broadcaster_id {
            return;
        }
        let Some(room) = self.room_for_peer(viewer_id) else {
            return;
        };
        if !room.peers.contains_key(&broadcaster_id) {
            return;
        }

        if !video && !audio {
            if let Some(subs) = room.subscriptions.get(&broadcaster_id) {
                subs.remove(&viewer_id);
            }
            Self::recompute_broadcaster_hint(&room, broadcaster_id, Instant::now());
            return;
        }

        let subs = room
            .subscriptions
            .entry(broadcaster_id)
            .or_default();
        subs.insert(viewer_id, ViewerSubscription::new(video, audio));
        drop(subs);

        if video {
            if let Some(broadcaster) = room.peers.get(&broadcaster_id) {
                broadcaster.try_request_keyframe();
            }
        } else {
            Self::recompute_broadcaster_hint(&room, broadcaster_id, Instant::now());
        }
    }

    pub fn request_keyframe(&self, requester_id: PeerId, broadcaster_id: PeerId) {
        let Some(room) = self.room_for_peer(requester_id) else {
            return;
        };
        if let Some(broadcaster) = room.peers.get(&broadcaster_id) {
            broadcaster.try_request_keyframe();
        };
    }

    pub fn report_viewer_feedback(
        &self,
        viewer_id: PeerId,
        broadcaster_id: PeerId,
        queue_drops: u32,
        decode_errors: u32,
    ) {
        if queue_drops == 0 && decode_errors <= 1 {
            return;
        }
        let Some(room) = self.room_for_peer(viewer_id) else {
            return;
        };
        let stream_bps = room
            .peers
            .get(&broadcaster_id)
            .map(|b| b.measured_video_bps.load(Ordering::Relaxed))
            .unwrap_or(0);
        let now = Instant::now();
        let severe = queue_drops > 0;
        let updated = if let Some(subs) = room.subscriptions.get(&broadcaster_id) {
            if let Some(sub) = subs.get(&viewer_id) {
                if sub.video {
                    sub.congestion.lock().note_congestion(stream_bps, severe, now)
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };
        if updated {
            Self::recompute_broadcaster_hint(&room, broadcaster_id, now);
        }
    }

    pub fn evaluate_viewer_link(
        &self,
        viewer_id: PeerId,
        path_congested: bool,
        path_severe: bool,
    ) {
        let Some(room) = self.room_for_peer(viewer_id) else {
            return;
        };
        let Some(viewer) = room.peers.get(&viewer_id).map(|p| Arc::clone(p.value())) else {
            return;
        };
        let queue_depth = VIDEO_QUEUE_CAPACITY.saturating_sub(viewer.video_tx.capacity());
        let slow_outbound = viewer.slow_outbound_flag.swap(false, Ordering::AcqRel);
        let congested = path_congested || slow_outbound || queue_depth >= 5;
        let severe = path_severe || slow_outbound || queue_depth >= 10;
        let now = Instant::now();

        let broadcaster_ids: Vec<PeerId> = room
            .subscriptions
            .iter()
            .filter_map(|entry| {
                let has_video = entry.value().get(&viewer_id).is_some_and(|s| s.video);
                has_video.then_some(*entry.key())
            })
            .collect();

        for broadcaster_id in broadcaster_ids {
            let stream_bps = room
                .peers
                .get(&broadcaster_id)
                .map(|b| b.measured_video_bps.load(Ordering::Relaxed))
                .unwrap_or(0);
            let mut changed = false;
            if let Some(subs) = room.subscriptions.get(&broadcaster_id) {
                if let Some(sub) = subs.get(&viewer_id) {
                    if sub.video {
                        let mut cg = sub.congestion.lock();
                        changed = if congested {
                            cg.note_congestion(stream_bps, severe, now)
                        } else {
                            cg.note_clean(stream_bps, now)
                        };
                    }
                }
            }
            if changed || Self::should_send_keepalive_hint(&room, broadcaster_id, now) {
                Self::recompute_broadcaster_hint(&room, broadcaster_id, now);
            }
        }
    }

    pub fn forward_video(&self, broadcaster_id: PeerId, is_keyframe: bool, packet: Bytes) {
        let Some(room) = self.room_for_peer(broadcaster_id) else {
            return;
        };
        let now = Instant::now();
        let stream_bps = room
            .peers
            .get(&broadcaster_id)
            .map(|b| b.record_inbound_video_bytes(packet.len(), now))
            .unwrap_or(0);

        let Some(subs) = room.subscriptions.get(&broadcaster_id) else {
            return;
        };

        let mut total_video_viewers = 0usize;
        let mut full_drops = 0usize;
        let mut updated_congestion = false;

        for entry in subs.iter() {
            let viewer_id = *entry.key();
            let sub = entry.value();
            if !sub.video {
                continue;
            }
            total_video_viewers += 1;
            if !is_keyframe && sub.awaiting_keyframe.load(Ordering::Acquire) {
                continue;
            }
            let Some(viewer) = room.peers.get(&viewer_id) else {
                continue;
            };
            let msg = OutboundMedia::Video {
                broadcaster_id,
                is_keyframe,
                packet: packet.clone(),
            };
            match viewer.video_tx.try_send(msg) {
                Ok(()) => {
                    if is_keyframe {
                        sub.awaiting_keyframe.store(false, Ordering::Release);
                    }
                    let queued_after =
                        VIDEO_QUEUE_CAPACITY.saturating_sub(viewer.video_tx.capacity());
                    if queued_after >= 5 {
                        updated_congestion |= sub.congestion.lock().note_congestion(
                            stream_bps,
                            queued_after >= 10,
                            now,
                        );
                    }
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    sub.awaiting_keyframe.store(true, Ordering::Release);
                    full_drops += 1;
                    updated_congestion |= sub.congestion.lock().note_congestion(stream_bps, true, now);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
        drop(subs);

        if full_drops > 0 {
            if let Some(broadcaster) = room.peers.get(&broadcaster_id) {
                let majority_dropping =
                    total_video_viewers <= 1 || (full_drops * 2) > total_video_viewers;
                let cooldown = if majority_dropping {
                    KEYFRAME_COOLDOWN
                } else {
                    OUTLIER_KEYFRAME_COOLDOWN
                };
                broadcaster.try_request_keyframe_with_cooldown(cooldown);
            }
        }

        if updated_congestion {
            Self::recompute_broadcaster_hint(&room, broadcaster_id, now);
        }
    }

    fn should_send_keepalive_hint(room: &RoomState, broadcaster_id: PeerId, now: Instant) -> bool {
        let Some(broadcaster) = room.peers.get(&broadcaster_id) else {
            return false;
        };
        let guard = broadcaster.last_sent_hint.lock();
        matches!(
            *guard,
            Some(prev) if prev.target_max_bps.is_some()
                && now.duration_since(prev.sent_at) >= HINT_KEEPALIVE_INTERVAL
        )
    }

    fn recompute_broadcaster_hint(room: &RoomState, broadcaster_id: PeerId, now: Instant) {
        let Some(broadcaster) = room.peers.get(&broadcaster_id).map(|b| Arc::clone(b.value())) else {
            return;
        };

        let mut total_viewers = 0usize;
        let mut congested_viewers = 0usize;
        let mut min_viewer_bps = UNCONSTRAINED_BPS;
        let mut at_floor_viewers = 0usize;

        if let Some(subs) = room.subscriptions.get(&broadcaster_id) {
            for entry in subs.iter() {
                let sub = entry.value();
                if !sub.video {
                    continue;
                }
                total_viewers += 1;
                let cg = sub.congestion.lock();
                if cg.suggested_bps < UNCONSTRAINED_BPS {
                    congested_viewers += 1;
                    min_viewer_bps = min_viewer_bps.min(cg.suggested_bps);
                    if cg.suggested_bps <= 500_000 {
                        at_floor_viewers += 1;
                    }
                }
            }
        }

        let (target_max_bps, min_fps_cap, congestion_source) =
            if total_viewers == 0 || congested_viewers == 0 {
                *broadcaster.severe_majority_since.lock() = None;
                (None, None, ThrottleReason::None)
            } else {
                let majority_congested =
                    total_viewers == 1 || (congested_viewers * 2) > total_viewers;
                if !majority_congested {
                    *broadcaster.severe_majority_since.lock() = None;
                    let safe_floor = min_viewer_bps.max(SFU_MINORITY_FLOOR_BPS);
                    (Some(safe_floor), None, ThrottleReason::Viewer)
                } else {
                    let extreme_floor = min_viewer_bps.max(MIN_ADAPTIVE_BITRATE_BPS);
                    let majority_at_floor =
                        total_viewers == 1 || (at_floor_viewers * 2) > total_viewers;
                    let fps_cap = if majority_at_floor {
                        let mut severe_since = broadcaster.severe_majority_since.lock();
                        let start = *severe_since.get_or_insert(now);
                        if now.duration_since(start) >= Duration::from_secs(2) {
                            Some(30)
                        } else {
                            None
                        }
                    } else {
                        *broadcaster.severe_majority_since.lock() = None;
                        None
                    };
                    (Some(extreme_floor), fps_cap, ThrottleReason::Viewer)
                }
            };

        let mut last_hint = broadcaster.last_sent_hint.lock();
        let should_send = match *last_hint {
            None => target_max_bps.is_some(),
            Some(prev) => {
                if prev.target_max_bps.is_some() != target_max_bps.is_some()
                    || prev.min_fps_cap != min_fps_cap
                {
                    true
                } else if let (Some(a), Some(b)) = (prev.target_max_bps, target_max_bps) {
                    let diff = (a as i64 - b as i64).unsigned_abs();
                    diff * 10 >= u64::from(a.max(1))
                        || now.duration_since(prev.sent_at) >= HINT_KEEPALIVE_INTERVAL
                } else {
                    false
                }
            }
        };

        if should_send {
            *last_hint = Some(SentHintState {
                target_max_bps,
                min_fps_cap,
                congestion_source,
                sent_at: now,
            });
            drop(last_hint);
            broadcaster.send_control(ServerControl::BitrateHint {
                target_max_bps,
                min_fps_cap,
                congestion_source,
            });
        }
    }

    pub fn forward_audio(&self, broadcaster_id: PeerId, packet: Bytes) {
        let Some(room) = self.room_for_peer(broadcaster_id) else {
            return;
        };
        let Some(subs) = room.subscriptions.get(&broadcaster_id) else {
            return;
        };

        for entry in subs.iter() {
            let viewer_id = *entry.key();
            let sub = entry.value();
            if !sub.audio {
                continue;
            }
            if let Some(viewer) = room.peers.get(&viewer_id) {
                let _ = viewer.audio_tx.try_send(OutboundMedia::Audio {
                    broadcaster_id,
                    packet: packet.clone(),
                });
            }
        }
    }

    fn room_for_peer(&self, peer_id: PeerId) -> Option<Arc<RoomState>> {
        let room_id = self.peer_rooms.get(&peer_id)?.value().clone();
        self.rooms.get(&room_id).map(|r| Arc::clone(r.value()))
    }
}

fn generate_short_room_id() -> RoomId {
    let raw = Uuid::new_v4().simple().to_string();
    raw[..8].to_string()
}

fn hash_room_password(room_id: &str, password: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(room_id.as_bytes());
    hasher.update(b":");
    hasher.update(password.as_bytes());
    hasher.finalize().into()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minority_vs_majority_viewer_congestion_aggregation() {
        let state = SfuState::new(None);
        let broadcaster_id = Uuid::new_v4();
        let v1_id = Uuid::new_v4();
        let v2_id = Uuid::new_v4();
        let v3_id = Uuid::new_v4();

        let (b_ctrl_tx, mut b_ctrl_rx) = mpsc::unbounded_channel();
        let (b_peer, _b_vrx, _b_arx) = ConnectedPeer::new(broadcaster_id, "Host".into(), b_ctrl_tx);
        state.register_peer(Arc::new(b_peer));

        let (v1_tx, _v1_vrx, _v1_arx) = ConnectedPeer::new(v1_id, "V1".into(), mpsc::unbounded_channel().0);
        let (v2_tx, _v2_vrx, _v2_arx) = ConnectedPeer::new(v2_id, "V2".into(), mpsc::unbounded_channel().0);
        let (v3_tx, _v3_vrx, _v3_arx) = ConnectedPeer::new(v3_id, "V3".into(), mpsc::unbounded_channel().0);
        state.register_peer(Arc::new(v1_tx));
        state.register_peer(Arc::new(v2_tx));
        state.register_peer(Arc::new(v3_tx));

        let room_id = state.create_room(broadcaster_id, None).expect("create room");
        state.join_room(&room_id, v1_id, None).expect("join v1");
        state.join_room(&room_id, v2_id, None).expect("join v2");
        state.join_room(&room_id, v3_id, None).expect("join v3");
        while b_ctrl_rx.try_recv().is_ok() {}

        state.set_subscription(v1_id, broadcaster_id, true, false);
        state.set_subscription(v2_id, broadcaster_id, true, false);
        state.set_subscription(v3_id, broadcaster_id, true, false);

        // Drive viewer 1 (minority: 1 of 3) below 1 Mbps -> broadcaster hint must stay >= 1 Mbps
        let room = state.room_for_peer(broadcaster_id).expect("room");
        let base = Instant::now();
        if let Some(subs) = room.subscriptions.get(&broadcaster_id) {
            let sub1 = subs.get(&v1_id).expect("sub1");
            let mut cg = sub1.congestion.lock();
            for step in 0..10 {
                cg.note_congestion(10_000_000, true, base + Duration::from_secs(step));
            }
            assert_eq!(cg.suggested_bps, MIN_ADAPTIVE_BITRATE_BPS);
        }
        SfuState::recompute_broadcaster_hint(&room, broadcaster_id, base + Duration::from_secs(10));
        let mut latest_hint = None;
        while let Ok(msg) = b_ctrl_rx.try_recv() {
            if let ServerControl::BitrateHint { target_max_bps, min_fps_cap, congestion_source } = msg {
                latest_hint = Some((target_max_bps, min_fps_cap, congestion_source));
            }
        }
        assert_eq!(
            latest_hint,
            Some((Some(SFU_MINORITY_FLOOR_BPS), None, ThrottleReason::Viewer))
        );

        // Now viewer 2 also drops to floor (2 of 3 = majority) -> allows 400 kbps & FPS cap after 2s
        if let Some(subs) = room.subscriptions.get(&broadcaster_id) {
            let sub2 = subs.get(&v2_id).expect("sub2");
            let mut cg = sub2.congestion.lock();
            for step in 0..10 {
                cg.note_congestion(10_000_000, true, base + Duration::from_secs(12 + step));
            }
        }
        SfuState::recompute_broadcaster_hint(&room, broadcaster_id, base + Duration::from_secs(22));
        SfuState::recompute_broadcaster_hint(&room, broadcaster_id, base + Duration::from_secs(25));
        SfuState::recompute_broadcaster_hint(&room, broadcaster_id, base + Duration::from_secs(30));
        while let Ok(msg) = b_ctrl_rx.try_recv() {
            if let ServerControl::BitrateHint { target_max_bps, min_fps_cap, congestion_source } = msg {
                latest_hint = Some((target_max_bps, min_fps_cap, congestion_source));
            }
        }
        assert_eq!(
            latest_hint,
            Some((Some(MIN_ADAPTIVE_BITRATE_BPS), Some(30), ThrottleReason::Viewer))
        );

        // Unsubscribing both congested viewers immediately lifts throttling
        state.set_subscription(v1_id, broadcaster_id, false, false);
        state.set_subscription(v2_id, broadcaster_id, false, false);
        while let Ok(msg) = b_ctrl_rx.try_recv() {
            if let ServerControl::BitrateHint { target_max_bps, min_fps_cap, congestion_source } = msg {
                latest_hint = Some((target_max_bps, min_fps_cap, congestion_source));
            }
        }
        assert_eq!(latest_hint, Some((None, None, ThrottleReason::None)));
    }
}
