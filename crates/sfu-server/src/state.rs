use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};
use shared_protocol::{
    ControlErrorCode, PeerId, PeerInfo, RoomId, ServerControl, StreamMetadata,
};
use tokio::sync::mpsc;
use uuid::Uuid;

const KEYFRAME_COOLDOWN: Duration = Duration::from_millis(1500);
const VIDEO_QUEUE_CAPACITY: usize = 64;
const AUDIO_QUEUE_CAPACITY: usize = 128;

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
pub struct ViewerSubscription {
    pub video: bool,
    pub audio: bool,
    pub awaiting_keyframe: AtomicBool,
}

impl ViewerSubscription {
    pub fn new(video: bool, audio: bool) -> Self {
        Self {
            video,
            audio,
            awaiting_keyframe: AtomicBool::new(video),
        }
    }
}

pub struct ConnectedPeer {
    pub peer_id: PeerId,
    pub nickname: RwLock<String>,
    pub control_tx: mpsc::UnboundedSender<ServerControl>,
    pub video_tx: mpsc::Sender<OutboundMedia>,
    pub audio_tx: mpsc::Sender<OutboundMedia>,
    pub active_stream: RwLock<Option<StreamMetadata>>,
    last_keyframe_req: Mutex<Option<Instant>>,
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
            last_keyframe_req: Mutex::new(None),
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
        let mut guard = self.last_keyframe_req.lock();
        let now = Instant::now();
        if let Some(prev) = *guard {
            if now.duration_since(prev) < KEYFRAME_COOLDOWN {
                return;
            }
        }
        *guard = Some(now);
        drop(guard);
        self.send_control(ServerControl::ForceKeyframe);
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
        }

        let Some(room) = self.rooms.get(&room_id).map(|r| Arc::clone(r.value())) else {
            return;
        };

        room.peers.remove(&peer_id);
        room.subscriptions.remove(&peer_id);
        for mut sub_map in room.subscriptions.iter_mut() {
            sub_map.value_mut().remove(&peer_id);
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

    pub fn forward_video(&self, broadcaster_id: PeerId, is_keyframe: bool, packet: Bytes) {
        let Some(room) = self.room_for_peer(broadcaster_id) else {
            return;
        };
        let Some(subs) = room.subscriptions.get(&broadcaster_id) else {
            return;
        };

        let mut need_keyframe = false;
        for entry in subs.iter() {
            let viewer_id = *entry.key();
            let sub = entry.value();
            if !sub.video {
                continue;
            }
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
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    sub.awaiting_keyframe.store(true, Ordering::Release);
                    need_keyframe = true;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
        drop(subs);

        if need_keyframe {
            if let Some(broadcaster) = room.peers.get(&broadcaster_id) {
                broadcaster.try_request_keyframe();
            }
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
