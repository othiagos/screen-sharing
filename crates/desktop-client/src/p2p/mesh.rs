use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use parking_lot::RwLock;
use shared_protocol::{
    ClientSignal, IceServerConfig, PeerControlMessage, PeerId, RoomId, ServerSignal, StreamConfig,
};
use thiserror::Error;
use tokio::sync::{mpsc, Mutex as AsyncMutex, Notify, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors, media_engine::MediaEngine,
        setting_engine::SettingEngine, APIBuilder,
    },
    data_channel::{
        data_channel_init::RTCDataChannelInit, data_channel_message::DataChannelMessage,
        RTCDataChannel,
    },
    ice::network_type::NetworkType,
    ice_transport::{
        ice_candidate::{RTCIceCandidate, RTCIceCandidateInit},
        ice_server::RTCIceServer,
    },
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        sdp::sdp_type::RTCSdpType, sdp::session_description::RTCSessionDescription,
        signaling_state::RTCSignalingState, RTCPeerConnection,
    },
    rtp_transceiver::{rtp_receiver::RTCRtpReceiver, rtp_sender::RTCRtpSender, RTCRtpTransceiver},
    track::{
        track_local::{track_local_static_sample::TrackLocalStaticSample, TrackLocal},
        track_remote::TrackRemote,
    },
};

use crate::{
    media::{
        traits::{AudioCapturer, MediaError, VideoCapturer},
        CpalAudioPlayer,
    },
    p2p::tracks::{
        create_h264_video_track, create_opus_audio_track, spawn_incoming_track_handler,
        spawn_outgoing_audio_pipeline, spawn_outgoing_video_pipeline, RemoteMediaStore,
    },
    signaling::{SignalingClient, SignalingError},
};

pub const MESH_CONTROL_LABEL: &str = "mesh-control";

#[derive(Debug, Error)]
pub enum MeshError {
    #[error("Signaling error: {0}")]
    Signaling(#[from] SignalingError),
    #[error("WebRTC error: {0}")]
    WebRtc(#[from] webrtc::Error),
    #[error("Media error: {0}")]
    Media(#[from] MediaError),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Room join/create timed out or failed: {0}")]
    RoomHandshake(String),
}

struct LocalShareSession {
    video_track: Arc<TrackLocalStaticSample>,
    audio_track: Arc<TrackLocalStaticSample>,
    keyframe_notify: Arc<Notify>,
    cancel: CancellationToken,
}

pub struct PeerSession {
    pub peer_id: PeerId,
    pub pc: Arc<RTCPeerConnection>,
    pub control_dc: Arc<RTCDataChannel>,
    pub dc_open: Arc<AtomicBool>,
    pub dc_open_notify: Arc<Notify>,
    pub state_notify: Arc<Notify>,
    pending_remote_ice: AsyncMutex<Vec<RTCIceCandidateInit>>,
    active_senders: AsyncMutex<Vec<Arc<RTCRtpSender>>>,
    negotiation_lock: AsyncMutex<()>,
    cancel: CancellationToken,
}

impl PeerSession {
    pub fn is_connected(&self) -> bool {
        self.pc.connection_state() == RTCPeerConnectionState::Connected
    }

    pub fn is_control_dc_open(&self) -> bool {
        self.dc_open.load(Ordering::Acquire)
    }

    pub async fn send_control_message(&self, msg: &PeerControlMessage) -> Result<bool, MeshError> {
        if !self.is_control_dc_open() {
            return Ok(false);
        }
        let text = serde_json::to_string(msg)?;
        match self.control_dc.send_text(text).await {
            Ok(_) => Ok(true),
            Err(err) => {
                debug!(peer_id = %self.peer_id, "Failed sending over mesh-control DC: {err}");
                Ok(false)
            }
        }
    }
}

/// Manages the Full-Mesh (N-to-N) `HashMap<PeerId, Arc<RTCPeerConnection>>` and serverless `"mesh-control"` renegotiation.
#[derive(Clone)]
pub struct MeshPeerManager {
    inner: Arc<MeshInner>,
}

struct MeshInner {
    signaling: SignalingClient,
    local_peer_id: RwLock<Option<PeerId>>,
    room_id: RwLock<Option<RoomId>>,
    ice_servers: RwLock<Vec<IceServerConfig>>,
    peers: DashMap<PeerId, Arc<PeerSession>>,
    local_share: AsyncMutex<Option<LocalShareSession>>,
    media_store: RemoteMediaStore,
    audio_player: Option<CpalAudioPlayer>,
    cancel: CancellationToken,
}

impl MeshPeerManager {
    /// Connects to `signaling_url` and initializes a new `MeshPeerManager`.
    pub async fn connect(
        signaling_url: &str,
        custom_ice_servers: Vec<IceServerConfig>,
        enable_audio_output: bool,
    ) -> Result<(Self, oneshot::Receiver<Result<(RoomId, PeerId), String>>), MeshError> {
        let cancel = CancellationToken::new();
        let (signaling, mut sig_rx) = SignalingClient::connect(signaling_url, cancel.clone()).await?;

        let mut ice_servers = IceServerConfig::default_stun_servers();
        for srv in custom_ice_servers {
            if !srv.urls.is_empty() {
                ice_servers.push(srv);
            }
        }

        let media_store = RemoteMediaStore::new();
        let audio_player = if enable_audio_output {
            media_store.enable_on_demand_watch();
            Some(CpalAudioPlayer::new(cancel.clone()))
        } else {
            None
        };

        let inner = Arc::new(MeshInner {
            signaling,
            local_peer_id: RwLock::new(None),
            room_id: RwLock::new(None),
            ice_servers: RwLock::new(ice_servers),
            peers: DashMap::new(),
            local_share: AsyncMutex::new(None),
            media_store,
            audio_player,
            cancel: cancel.clone(),
        });

        let (room_ready_tx, room_ready_rx) = oneshot::channel();
        let manager = Self { inner };
        let loop_mgr = manager.clone();

        tokio::spawn(async move {
            let mut ready_tx = Some(room_ready_tx);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    maybe_sig = sig_rx.recv() => {
                        let Some(signal) = maybe_sig else {
                            // Signaling server disconnected! Keep active P2P mesh running uninterrupted.
                            info!("Signaling channel ended; active P2P mesh streams continue serverless");
                            break;
                        };
                        loop_mgr.handle_server_signal(signal, &mut ready_tx).await;
                    }
                }
            }
        });

        Ok((manager, room_ready_rx))
    }

    pub async fn create_room(
        &self,
        room_ready_rx: oneshot::Receiver<Result<(RoomId, PeerId), String>>,
    ) -> Result<(RoomId, PeerId), MeshError> {
        self.inner.signaling.send(ClientSignal::CreateRoom)?;
        let res = tokio::time::timeout(Duration::from_secs(10), room_ready_rx)
            .await
            .map_err(|_| MeshError::RoomHandshake("Timed out waiting for RoomCreated".to_string()))?
            .map_err(|_| MeshError::RoomHandshake("Signaling closed before RoomCreated".to_string()))?
            .map_err(MeshError::RoomHandshake)?;
        Ok(res)
    }

    pub async fn join_room(
        &self,
        room_id: RoomId,
        room_ready_rx: oneshot::Receiver<Result<(RoomId, PeerId), String>>,
    ) -> Result<(RoomId, PeerId), MeshError> {
        self.inner
            .signaling
            .send(ClientSignal::JoinRoom { room_id })?;
        let res = tokio::time::timeout(Duration::from_secs(10), room_ready_rx)
            .await
            .map_err(|_| MeshError::RoomHandshake("Timed out waiting for RoomJoined".to_string()))?
            .map_err(|_| MeshError::RoomHandshake("Signaling closed before RoomJoined".to_string()))?
            .map_err(MeshError::RoomHandshake)?;
        Ok(res)
    }

    pub fn local_peer_id(&self) -> Option<PeerId> {
        *self.inner.local_peer_id.read()
    }

    pub fn room_id(&self) -> Option<RoomId> {
        self.inner.room_id.read().clone()
    }

    pub fn media_store(&self) -> RemoteMediaStore {
        self.inner.media_store.clone()
    }

    /// Sets whether the local user wants to watch/listen to `peer_id`'s stream (opt-in on-demand playback).
    /// When enabling watch (`watched == true`), immediately requests an H.264 IDR keyframe from `peer_id`.
    pub async fn set_peer_watched(&self, peer_id: PeerId, watched: bool) {
        self.inner.media_store.set_peer_watched(peer_id, watched);
        if watched {
            let maybe_session = self.inner.peers.get(&peer_id).map(|e| e.value().clone());
            if let Some(session) = maybe_session {
                let _ = session
                    .send_control_message(&PeerControlMessage::RequestKeyframe)
                    .await;
            }
        }
    }

    /// Returns a snapshot of active peer connections as a `HashMap<PeerId, Arc<RTCPeerConnection>>`.
    pub fn peer_connections(&self) -> HashMap<PeerId, Arc<RTCPeerConnection>> {
        self.inner
            .peers
            .iter()
            .map(|entry| (*entry.key(), Arc::clone(&entry.value().pc)))
            .collect()
    }

    pub fn connected_peer_ids(&self) -> Vec<PeerId> {
        let mut ids: Vec<PeerId> = self
            .inner
            .peers
            .iter()
            .filter(|e| e.value().is_connected())
            .map(|e| *e.key())
            .collect();
        ids.sort();
        ids
    }

    pub fn all_peer_ids(&self) -> Vec<PeerId> {
        let mut ids: Vec<PeerId> = self.inner.peers.iter().map(|e| *e.key()).collect();
        ids.sort();
        ids
    }

    /// Waits until `expected_peers` connections reach `RTCPeerConnectionState::Connected`
    /// AND their `"mesh-control"` DataChannels are open.
    pub async fn wait_for_mesh_ready(
        &self,
        expected_peers: usize,
        timeout_dur: Duration,
    ) -> Result<(), MeshError> {
        let deadline = tokio::time::Instant::now() + timeout_dur;
        loop {
            let ready_count = self
                .inner
                .peers
                .iter()
                .filter(|entry| {
                    let s = entry.value();
                    s.is_connected() && s.is_control_dc_open()
                })
                .count();

            if ready_count >= expected_peers {
                return Ok(());
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(MeshError::RoomHandshake(format!(
                    "Timed out waiting for {expected_peers} mesh peers (ready: {ready_count})"
                )));
            }

            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Starts encoding and streaming local video and audio to all peers in the mesh.
    /// Routes SDP renegotiation over `"mesh-control"` DataChannel first (so it works even if `signaling-server` is offline).
    pub async fn start_sharing(
        &self,
        config: StreamConfig,
        video_capturer: Arc<dyn VideoCapturer>,
        audio_capturer: Arc<dyn AudioCapturer>,
    ) -> Result<(), MeshError> {
        self.stop_sharing().await?;

        let share_cancel = self.inner.cancel.child_token();
        let video_track = create_h264_video_track();
        let audio_track = create_opus_audio_track();
        let keyframe_notify = Arc::new(Notify::new());

        let fps = config.sanitized_fps();
        let (video_tx, video_rx) = mpsc::channel(8);
        let (audio_tx, audio_rx) = mpsc::channel(32);

        spawn_outgoing_video_pipeline(
            video_rx,
            Arc::clone(&video_track),
            fps,
            Arc::clone(&keyframe_notify),
            self.inner.media_store.clone(),
            share_cancel.clone(),
        )?;

        spawn_outgoing_audio_pipeline(
            audio_rx,
            Arc::clone(&audio_track),
            share_cancel.clone(),
        )?;

        video_capturer.start_capture(
            config.target.clone(),
            config.resolution,
            fps,
            video_tx,
            share_cancel.clone(),
        )?;

        audio_capturer.start_capture(
            config.audio_mode,
            audio_tx,
            share_cancel.clone(),
        )?;

        {
            let mut guard = self.inner.local_share.lock().await;
            *guard = Some(LocalShareSession {
                video_track: Arc::clone(&video_track),
                audio_track: Arc::clone(&audio_track),
                keyframe_notify: Arc::clone(&keyframe_notify),
                cancel: share_cancel,
            });
        }

        let sessions: Vec<Arc<PeerSession>> =
            self.inner.peers.iter().map(|e| e.value().clone()).collect();

        for session in sessions {
            if session.is_control_dc_open() {
                self.attach_tracks_and_renegotiate(
                    &session,
                    Arc::clone(&video_track),
                    Arc::clone(&audio_track),
                    Arc::clone(&keyframe_notify),
                )
                .await?;
            }
        }

        Ok(())
    }

    /// Stops any active local screen/audio capture and removes outgoing tracks from mesh peers.
    pub async fn stop_sharing(&self) -> Result<(), MeshError> {
        self.inner.media_store.clear_local_preview_frame();
        let prev = {
            let mut guard = self.inner.local_share.lock().await;
            guard.take()
        };

        if let Some(session_share) = prev {
            session_share.cancel.cancel();

            let sessions: Vec<Arc<PeerSession>> =
                self.inner.peers.iter().map(|e| e.value().clone()).collect();

            for session in sessions {
                let _neg_guard = session.negotiation_lock.lock().await;
                let mut senders = session.active_senders.lock().await;
                for sender in senders.drain(..) {
                    let _ = session.pc.remove_track(&sender).await;
                }
                drop(senders);

                let _ = session
                    .send_control_message(&PeerControlMessage::StreamStopped)
                    .await;
                let _ = self.send_renegotiation_offer(&session).await;
            }
        }

        Ok(())
    }

    /// Leaves the current room, closes all peer connections, and cancels background capture tasks.
    pub async fn shutdown(&self) {
        let _ = self.stop_sharing().await;
        let _ = self.inner.signaling.send(ClientSignal::LeaveRoom);
        self.inner.cancel.cancel();
        self.inner.signaling.shutdown();

        let sessions: Vec<Arc<PeerSession>> = self
            .inner
            .peers
            .iter()
            .map(|e| e.value().clone())
            .collect();
        self.inner.peers.clear();

        for session in sessions {
            session.cancel.cancel();
            session.dc_open.store(false, Ordering::Release);
            if session.pc.connection_state() != RTCPeerConnectionState::Closed {
                let _ = session.pc.close().await;
            }
        }
    }

    async fn handle_server_signal(
        &self,
        signal: ServerSignal,
        ready_tx: &mut Option<oneshot::Sender<Result<(RoomId, PeerId), String>>>,
    ) {
        match signal {
            ServerSignal::RoomCreated {
                room_id,
                local_peer_id,
            } => {
                *self.inner.local_peer_id.write() = Some(local_peer_id);
                *self.inner.room_id.write() = Some(room_id.clone());
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Ok((room_id, local_peer_id)));
                }
            }
            ServerSignal::RoomJoined {
                room_id,
                local_peer_id,
                existing_peers,
            } => {
                *self.inner.local_peer_id.write() = Some(local_peer_id);
                *self.inner.room_id.write() = Some(room_id.clone());
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Ok((room_id, local_peer_id)));
                }

                // Rule from Section 6.1: The newcomer initiates an RTCPeerConnection
                // and creates an SdpOffer for every peer in existing_peers.
                for peer_id in existing_peers {
                    if peer_id == local_peer_id {
                        continue;
                    }
                    if let Err(err) = self.initiate_offer_to_peer(peer_id).await {
                        warn!(%peer_id, "Failed initiating offer to existing peer: {err}");
                    }
                }
            }
            ServerSignal::PeerJoined { peer_id } => {
                debug!(%peer_id, "Remote peer joined room; awaiting its SdpOffer");
                let _ = self.get_or_create_peer_session(peer_id).await;
            }
            ServerSignal::PeerLeft { peer_id } => {
                info!(%peer_id, "Remote peer left room; cleaning up RTCPeerConnection");
                if let Some((_, session)) = self.inner.peers.remove(&peer_id) {
                    session.cancel.cancel();
                    session.dc_open.store(false, Ordering::Release);
                    if session.pc.connection_state() != RTCPeerConnectionState::Closed {
                        let _ = session.pc.close().await;
                    }
                }
                self.inner.media_store.remove_peer(peer_id);
            }
            ServerSignal::IncomingOffer { from, sdp } => {
                if let Err(err) = self.handle_incoming_offer(from, sdp, false).await {
                    warn!(%from, "Failed handling IncomingOffer from signaling: {err}");
                }
            }
            ServerSignal::IncomingAnswer { from, sdp } => {
                if let Err(err) = self.handle_incoming_answer(from, sdp).await {
                    warn!(%from, "Failed handling IncomingAnswer from signaling: {err}");
                }
            }
            ServerSignal::IncomingIceCandidate {
                from,
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => {
                if let Err(err) = self
                    .handle_incoming_ice(from, candidate, sdp_mid, sdp_mline_index)
                    .await
                {
                    debug!(%from, "Failed adding ICE candidate: {err}");
                }
            }
            ServerSignal::Error { message } => {
                warn!("Signaling server error: {message}");
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(message));
                }
            }
        }
    }

    async fn build_webrtc_peer_connection(&self) -> Result<Arc<RTCPeerConnection>, MeshError> {
        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs()?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)?;

        let mut setting_engine = SettingEngine::default();
        setting_engine.set_network_types(vec![NetworkType::Udp4]);

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine)
            .build();

        let ice_servers: Vec<RTCIceServer> = self
            .inner
            .ice_servers
            .read()
            .iter()
            .map(|cfg| RTCIceServer {
                urls: cfg.urls.clone(),
                username: cfg.username.clone().unwrap_or_default(),
                credential: cfg.credential.clone().unwrap_or_default(),
                ..Default::default()
            })
            .collect();

        let rtc_config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };

        let pc = api.new_peer_connection(rtc_config).await?;
        Ok(Arc::new(pc))
    }

    async fn get_or_create_peer_session(
        &self,
        remote_peer_id: PeerId,
    ) -> Result<Arc<PeerSession>, MeshError> {
        if let Some(existing) = self.inner.peers.get(&remote_peer_id) {
            return Ok(Arc::clone(existing.value()));
        }

        let pc = self.build_webrtc_peer_connection().await?;
        let peer_cancel = self.inner.cancel.child_token();

        // Pre-negotiated reliable RTCDataChannel "mesh-control" (id = 0) guarantees
        // the initial SDP always has an `m=application` section even before media tracks are added,
        // and opens symmetrically on both offerer and answerer without race conditions.
        let dc_init = RTCDataChannelInit {
            ordered: Some(true),
            negotiated: Some(0),
            ..Default::default()
        };
        let control_dc = pc
            .create_data_channel(MESH_CONTROL_LABEL, Some(dc_init))
            .await?;

        let dc_open = Arc::new(AtomicBool::new(false));
        let dc_open_notify = Arc::new(Notify::new());
        let state_notify = Arc::new(Notify::new());

        let session = Arc::new(PeerSession {
            peer_id: remote_peer_id,
            pc: Arc::clone(&pc),
            control_dc: Arc::clone(&control_dc),
            dc_open: Arc::clone(&dc_open),
            dc_open_notify: Arc::clone(&dc_open_notify),
            state_notify: Arc::clone(&state_notify),
            pending_remote_ice: AsyncMutex::new(Vec::new()),
            active_senders: AsyncMutex::new(Vec::new()),
            negotiation_lock: AsyncMutex::new(()),
            cancel: peer_cancel.clone(),
        });

        self.setup_peer_callbacks(&session);

        self.inner
            .peers
            .insert(remote_peer_id, Arc::clone(&session));
        Ok(session)
    }

    fn setup_peer_callbacks(&self, session: &Arc<PeerSession>) {
        let remote_peer_id = session.peer_id;
        let dc_open = Arc::clone(&session.dc_open);
        let dc_open_notify = Arc::clone(&session.dc_open_notify);
        let mgr_open = self.clone();
        let weak_sess_open = Arc::downgrade(session);

        session.control_dc.on_open(Box::new(move || {
            debug!(%remote_peer_id, "mesh-control RTCDataChannel opened");
            let mgr = mgr_open.clone();
            let maybe_sess = weak_sess_open.upgrade();
            let dc_open = Arc::clone(&dc_open);
            let dc_open_notify = Arc::clone(&dc_open_notify);

            tokio::spawn(async move {
                let Some(sess) = maybe_sess else { return };

                // Wait briefly for the RTCPeerConnection to reach Connected and for the remote
                // peer's `start_sctp()` to finish calling `open_stream(0)` on pre-negotiated
                // stream 0 before transmitting any SCTP DATA chunks (avoids webrtc-sctp
                // `ErrStreamAlreadyExist` when one side finishes COOKIE-ACK slightly earlier).
                let wait_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
                while !sess.is_connected() && tokio::time::Instant::now() < wait_deadline {
                    tokio::select! {
                        _ = sess.cancel.cancelled() => return,
                        _ = sess.state_notify.notified() => {}
                        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                    }
                }
                tokio::select! {
                    _ = sess.cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(150)) => {}
                }

                dc_open.store(true, Ordering::Release);
                dc_open_notify.notify_waiters();

                // If the local peer is ALREADY sharing when a new peer joins the room and its
                // mesh-control DataChannel opens, attach tracks and send a RenegotiateOffer now
                // (since an SDP Answer to the newcomer's initial offer cannot add new m=video/m=audio lines).
                let active_share = {
                    let guard = mgr.inner.local_share.lock().await;
                    guard.as_ref().map(|s| {
                        (
                            Arc::clone(&s.video_track),
                            Arc::clone(&s.audio_track),
                            Arc::clone(&s.keyframe_notify),
                        )
                    })
                };
                if let Some((v_track, a_track, kf_notify)) = active_share {
                    if let Err(err) = mgr
                        .attach_tracks_and_renegotiate(&sess, v_track, a_track, kf_notify)
                        .await
                    {
                        warn!(
                            %remote_peer_id,
                            "Failed renegotiating active stream to newly joined peer: {err}"
                        );
                        return;
                    }

                    // Fallback retry in case the remote peer's DataChannel handler was still
                    // initializing under heavy CI load when the first RenegotiateOffer arrived.
                    tokio::select! {
                        _ = sess.cancel.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    }
                    if sess.pc.signaling_state() == RTCSignalingState::HaveLocalOffer {
                        if let Some(local_desc) = sess.pc.local_description().await {
                            let _ = mgr.inner.signaling.send(ClientSignal::SdpOffer {
                                target: remote_peer_id,
                                sdp: local_desc.sdp,
                            });
                        }
                    }
                }
            });

            Box::pin(async {})
        }));

        let dc_open_close = Arc::clone(&session.dc_open);
        session.control_dc.on_close(Box::new(move || {
            dc_open_close.store(false, Ordering::Release);
            Box::pin(async {})
        }));

        let mgr_dc = self.clone();
        session
            .control_dc
            .on_message(Box::new(move |msg: DataChannelMessage| {
                let mgr = mgr_dc.clone();
                if let Ok(text) = std::str::from_utf8(&msg.data) {
                    match serde_json::from_str::<PeerControlMessage>(text) {
                        Ok(ctrl_msg) => {
                            tokio::spawn(async move {
                                mgr.handle_control_message(remote_peer_id, ctrl_msg).await;
                            });
                        }
                        Err(err) => {
                            warn!(%remote_peer_id, "Invalid PeerControlMessage JSON: {err}");
                        }
                    }
                }
                Box::pin(async {})
            }));

        let state_notify = Arc::clone(&session.state_notify);
        session
            .pc
            .on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
                debug!(%remote_peer_id, ?state, "RTCPeerConnection state changed");
                state_notify.notify_waiters();
                Box::pin(async {})
            }));

        let mgr_ice = self.clone();
        let weak_session = Arc::downgrade(session);
        session.pc.on_ice_candidate(Box::new(
            move |candidate: Option<RTCIceCandidate>| {
                let mgr = mgr_ice.clone();
                let maybe_sess = weak_session.upgrade();
                Box::pin(async move {
                    let Some(cand) = candidate else { return };
                    let Ok(json) = cand.to_json() else { return };

                    // Route ICE candidate over mesh-control DC if already open, else via signaling-server
                    if let Some(sess) = maybe_sess {
                        if sess.is_control_dc_open() {
                            let ctrl = PeerControlMessage::IceCandidate {
                                candidate: json.candidate.clone(),
                                sdp_mid: json.sdp_mid.clone(),
                                sdp_mline_index: json.sdp_mline_index,
                            };
                            if let Ok(true) = sess.send_control_message(&ctrl).await {
                                return;
                            }
                        }
                    }

                    let _ = mgr.inner.signaling.send(ClientSignal::IceCandidate {
                        target: remote_peer_id,
                        candidate: json.candidate,
                        sdp_mid: json.sdp_mid,
                        sdp_mline_index: json.sdp_mline_index,
                    });
                })
            },
        ));

        let media_store = self.inner.media_store.clone();
        let audio_player = self.inner.audio_player.clone();
        let peer_cancel = session.cancel.clone();
        let weak_sess_track = Arc::downgrade(session);

        session.pc.on_track(Box::new(
            move |track: Arc<TrackRemote>,
                  _receiver: Arc<RTCRtpReceiver>,
                  _transceiver: Arc<RTCRtpTransceiver>| {
                let store = media_store.clone();
                let player = audio_player.clone();
                let cancel = peer_cancel.clone();
                let maybe_sess = weak_sess_track.upgrade();

                Box::pin(async move {
                    info!(
                        %remote_peer_id,
                        codec = %track.codec().capability.mime_type,
                        "Incoming remote media track established"
                    );
                    spawn_incoming_track_handler(remote_peer_id, track, store, player, cancel);

                    if let Some(sess) = maybe_sess {
                        let _ = sess
                            .send_control_message(&PeerControlMessage::RequestKeyframe)
                            .await;
                    }
                })
            },
        ));
    }

    async fn initiate_offer_to_peer(&self, remote_peer_id: PeerId) -> Result<(), MeshError> {
        let session = self.get_or_create_peer_session(remote_peer_id).await?;
        let _guard = session.negotiation_lock.lock().await;

        let offer = session.pc.create_offer(None).await?;
        let sdp = offer.sdp.clone();
        session.pc.set_local_description(offer).await?;

        self.inner
            .signaling
            .send(ClientSignal::SdpOffer {
                target: remote_peer_id,
                sdp,
            })?;
        Ok(())
    }

    async fn attach_tracks_only(
        &self,
        session: &Arc<PeerSession>,
        video_track: Arc<TrackLocalStaticSample>,
        audio_track: Arc<TrackLocalStaticSample>,
        keyframe_notify: Arc<Notify>,
    ) -> Result<(), MeshError> {
        let mut senders = session.active_senders.lock().await;
        if !senders.is_empty() {
            return Ok(());
        }

        let v_sender = session
            .pc
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        let a_sender = session
            .pc
            .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        Self::spawn_rtcp_reader(Arc::clone(&v_sender), keyframe_notify, session.cancel.clone());
        Self::spawn_rtcp_reader(
            Arc::clone(&a_sender),
            Arc::new(Notify::new()),
            session.cancel.clone(),
        );

        senders.push(v_sender);
        senders.push(a_sender);
        Ok(())
    }

    fn spawn_rtcp_reader(
        sender: Arc<RTCRtpSender>,
        keyframe_notify: Arc<Notify>,
        cancel: CancellationToken,
    ) {
        tokio::spawn(async move {
            let mut rtcp_buf = vec![0u8; 1500];
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    res = sender.read(&mut rtcp_buf) => {
                        match res {
                            Ok((packets, _)) => {
                                for pkt in packets {
                                    if pkt
                                        .as_any()
                                        .downcast_ref::<webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication>()
                                        .is_some()
                                    {
                                        keyframe_notify.notify_waiters();
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });
    }

    async fn attach_tracks_and_renegotiate(
        &self,
        session: &Arc<PeerSession>,
        video_track: Arc<TrackLocalStaticSample>,
        audio_track: Arc<TrackLocalStaticSample>,
        keyframe_notify: Arc<Notify>,
    ) -> Result<(), MeshError> {
        let _guard = session.negotiation_lock.lock().await;
        self.attach_tracks_only(session, video_track, audio_track, keyframe_notify)
            .await?;
        self.send_renegotiation_offer(session).await
    }

    async fn send_renegotiation_offer(&self, session: &Arc<PeerSession>) -> Result<(), MeshError> {
        let offer = session.pc.create_offer(None).await?;
        let sdp = offer.sdp.clone();
        session.pc.set_local_description(offer).await?;

        // Critical Requirement (Section 6.1): Route SDP renegotiation over "mesh-control" first,
        // falling back to signaling-server only if the data channel isn't open yet.
        let ctrl_offer = PeerControlMessage::RenegotiateOffer { sdp: sdp.clone() };
        if session.send_control_message(&ctrl_offer).await? {
            debug!(peer_id = %session.peer_id, "Sent RenegotiateOffer over mesh-control DataChannel");
            return Ok(());
        }

        let _ = self.inner.signaling.send(ClientSignal::SdpOffer {
            target: session.peer_id,
            sdp,
        });
        Ok(())
    }

    async fn handle_control_message(&self, from: PeerId, msg: PeerControlMessage) {
        match msg {
            PeerControlMessage::RenegotiateOffer { sdp } => {
                if let Err(err) = self.handle_incoming_offer(from, sdp, true).await {
                    warn!(%from, "Failed handling RenegotiateOffer over mesh-control: {err}");
                }
            }
            PeerControlMessage::RenegotiateAnswer { sdp } => {
                if let Err(err) = self.handle_incoming_answer(from, sdp).await {
                    warn!(%from, "Failed handling RenegotiateAnswer over mesh-control: {err}");
                }
            }
            PeerControlMessage::IceCandidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => {
                let _ = self
                    .handle_incoming_ice(from, candidate, sdp_mid, sdp_mline_index)
                    .await;
            }
            PeerControlMessage::StreamStopped => {
                self.inner.media_store.remove_peer(from);
            }
            PeerControlMessage::RequestKeyframe => {
                let guard = self.inner.local_share.lock().await;
                if let Some(share) = guard.as_ref() {
                    share.keyframe_notify.notify_waiters();
                }
            }
        }
    }

    async fn handle_incoming_offer(
        &self,
        from: PeerId,
        sdp: String,
        via_data_channel: bool,
    ) -> Result<(), MeshError> {
        let session = self.get_or_create_peer_session(from).await?;
        let _guard = session.negotiation_lock.lock().await;

        let local_id = self.local_peer_id().unwrap_or_default();
        let is_polite = local_id < from;

        if session.pc.signaling_state() != RTCSignalingState::Stable {
            if !is_polite {
                debug!(%from, "Impolite peer ignoring colliding SDP offer");
                return Ok(());
            }
            debug!(%from, "Polite peer rolling back local offer to accept remote offer");
            let mut rollback = RTCSessionDescription::default();
            rollback.sdp_type = RTCSdpType::Rollback;
            let _ = session.pc.set_local_description(rollback).await;
        }

        let has_active_video = sdp.contains("m=video") && !sdp.contains("a=inactive");
        let remote_desc = RTCSessionDescription::offer(sdp)?;
        session.pc.set_remote_description(remote_desc).await?;
        self.flush_pending_ice(&session).await;

        if has_active_video {
            self.inner.media_store.mark_peer_streaming(from);
        }

        let answer = session.pc.create_answer(None).await?;
        let answer_sdp = answer.sdp.clone();
        session.pc.set_local_description(answer).await?;

        if via_data_channel || session.is_control_dc_open() {
            let reply = PeerControlMessage::RenegotiateAnswer {
                sdp: answer_sdp.clone(),
            };
            if session.send_control_message(&reply).await? {
                return Ok(());
            }
        }

        self.inner.signaling.send(ClientSignal::SdpAnswer {
            target: from,
            sdp: answer_sdp,
        })?;
        Ok(())
    }

    async fn handle_incoming_answer(&self, from: PeerId, sdp: String) -> Result<(), MeshError> {
        let Some(session) = self.inner.peers.get(&from).map(|e| e.value().clone()) else {
            return Ok(());
        };
        let _guard = session.negotiation_lock.lock().await;

        let remote_desc = RTCSessionDescription::answer(sdp)?;
        session.pc.set_remote_description(remote_desc).await?;
        self.flush_pending_ice(&session).await;

        let guard = self.inner.local_share.lock().await;
        if let Some(share) = guard.as_ref() {
            share.keyframe_notify.notify_waiters();
        }

        Ok(())
    }

    async fn handle_incoming_ice(
        &self,
        from: PeerId,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) -> Result<(), MeshError> {
        let session = self.get_or_create_peer_session(from).await?;
        let init = RTCIceCandidateInit {
            candidate,
            sdp_mid,
            sdp_mline_index,
            username_fragment: None,
        };

        if session.pc.remote_description().await.is_none() {
            session.pending_remote_ice.lock().await.push(init);
        } else {
            session.pc.add_ice_candidate(init).await?;
        }
        Ok(())
    }

    async fn flush_pending_ice(&self, session: &Arc<PeerSession>) {
        let mut pending = session.pending_remote_ice.lock().await;
        for cand in pending.drain(..) {
            if let Err(err) = session.pc.add_ice_candidate(cand).await {
                debug!(peer_id = %session.peer_id, "Buffered ICE candidate error: {err}");
            }
        }
    }
}
