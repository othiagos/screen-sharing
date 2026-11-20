use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bytes::Bytes;
use parking_lot::Mutex;
use quinn::{Connection, Endpoint, RecvStream, SendDatagramError, SendStream};
use shared_protocol::{
    decode_control_payload, encode_audio_packet, encode_control_frame, encode_video_packet,
    AudioPacketHeader, ClientControl, ControlErrorCode, PeerId, PeerInfo, RoomId, ServerControl,
    StreamMetadata, ThrottleReason, VideoFrameHeader, AUDIO_HEADER_LEN, MAX_AUDIO_PACKET_BYTES,
    MAX_CONTROL_FRAME_BYTES, MAX_VIDEO_FRAME_BYTES, MEDIA_KIND_AUDIO, MEDIA_KIND_VIDEO,
    PROTOCOL_VERSION, VIDEO_FLAG_KEYFRAME, VIDEO_HEADER_LEN,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::net::tls::{build_quic_client_config, sni_for_host, TofuCheckResult, TofuServerCertVerifier};

const MAX_IN_FLIGHT_VIDEO_STREAMS: usize = 6;

/// High-level events emitted by `SfuClient` for the UI or application controller.
#[derive(Debug, Clone)]
pub enum SfuEvent {
    TrustRequired {
        server_addr: String,
        fingerprint: String,
        changed_from: Option<String>,
    },
    Connected {
        local_peer_id: PeerId,
        server_addr: String,
    },
    RoomJoined {
        room_id: RoomId,
        local_peer_id: PeerId,
        peers: Vec<PeerInfo>,
    },
    PeerJoined(PeerInfo),
    PeerLeft {
        peer_id: PeerId,
    },
    PeerBroadcastUpdated {
        peer_id: PeerId,
        metadata: Option<StreamMetadata>,
    },
    ForceKeyframe,
    RttUpdated {
        rtt_ms: u32,
    },
    ServerError {
        code: ControlErrorCode,
        message: String,
    },
    Disconnected {
        reason: String,
    },
}

/// Callback hooks for incoming SFU media packets.
pub trait MediaSink: Send + Sync + 'static {
    fn on_video_frame(&self, header: VideoFrameHeader, payload: Bytes);
    fn on_audio_packet(&self, header: AudioPacketHeader, payload: Bytes);
}

/// Live QUIC SFU client session handle (cheaply cloneable for UI + media pipelines).
#[derive(Clone)]
pub struct SfuClient {
    conn: Connection,
    endpoint: Endpoint,
    rt_handle: tokio::runtime::Handle,
    ctrl_tx: mpsc::UnboundedSender<ClientControl>,
    cancel: CancellationToken,
    local_peer_id: PeerId,
    force_keyframe: Arc<AtomicBool>,
    rtt_ms: Arc<AtomicU64>,
    in_flight_video: Arc<AtomicUsize>,
    send_stall_flag: Arc<AtomicBool>,
    sfu_bitrate_hint: Arc<Mutex<Option<SfuBitrateHintSnapshot>>>,
    bytes_sent: Arc<AtomicU64>,
    bytes_received: Arc<AtomicU64>,
}

/// Snapshot of client-side network throughput and latency counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct NetworkCounters {
    pub rtt_ms: u32,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub in_flight_video: usize,
}

/// Real-time QUIC path telemetry used by the adaptive bitrate controller.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuicPathTelemetry {
    pub rtt_ms: u32,
    pub cwnd_bytes: u64,
    pub sent_packets: u64,
    pub lost_packets: u64,
    pub congestion_events: u64,
    pub in_flight_video: usize,
    pub send_stall_flagged: bool,
}

/// Latest SFU viewer-congestion hint delivered to the broadcaster.
#[derive(Debug, Clone, Copy)]
pub struct SfuBitrateHintSnapshot {
    pub target_max_bps: Option<u32>,
    pub min_fps_cap: Option<u8>,
    pub congestion_source: ThrottleReason,
    pub updated_at: Instant,
}

/// Parameters required to connect and authenticate with an SFU node.
#[derive(Debug, Clone)]
pub struct ConnectRequest {
    pub server_addr: String,
    pub nickname: String,
    pub auth_token: Option<String>,
    pub pinned_fingerprint: Option<String>,
}

/// Outcome of `connect_sfu`: either an active authenticated session or a TOFU verification prompt.
pub enum ConnectOutcome {
    Connected(SfuClient),
    TrustRequired {
        server_addr: String,
        fingerprint: String,
        changed_from: Option<String>,
    },
}

/// Connects to an SFU server over QUIC with TOFU fingerprint enforcement and performs `Hello`/`Welcome`.
pub async fn connect_sfu(
    req: ConnectRequest,
    event_tx: mpsc::UnboundedSender<SfuEvent>,
    media_sink: Arc<dyn MediaSink>,
) -> Result<ConnectOutcome> {
    let (socket_addr, sni_host) = resolve_server_addr(&req.server_addr).await?;

    let verifier = match &req.pinned_fingerprint {
        Some(fp) => TofuServerCertVerifier::pinned(fp),
        None => TofuServerCertVerifier::probe(),
    };

    let client_cfg = build_quic_client_config(Arc::clone(&verifier))?;
    let bind_addr: SocketAddr = if socket_addr.is_ipv6() {
        "[::]:0".parse()?
    } else {
        "0.0.0.0:0".parse()?
    };
    let mut endpoint = Endpoint::client(bind_addr).context("Failed binding local UDP socket")?;
    endpoint.set_default_client_config(client_cfg);

    let connecting = endpoint
        .connect(socket_addr, &sni_host)
        .with_context(|| format!("Failed initiating QUIC connection to {socket_addr}"))?;

    let conn = match connecting.await {
        Ok(c) => c,
        Err(err) => {
            if let TofuCheckResult::Mismatch { expected, actual } = verifier.check_result() {
                endpoint.close(0u32.into(), b"cert mismatch");
                let _ = event_tx.send(SfuEvent::TrustRequired {
                    server_addr: req.server_addr.clone(),
                    fingerprint: actual.clone(),
                    changed_from: Some(expected.clone()),
                });
                return Ok(ConnectOutcome::TrustRequired {
                    server_addr: req.server_addr,
                    fingerprint: actual,
                    changed_from: Some(expected),
                });
            }
            return Err(err).with_context(|| format!("QUIC TLS handshake failed to {}", req.server_addr));
        }
    };

    // If no fingerprint was pinned yet, abort before sending credentials and prompt user to trust.
    if let TofuCheckResult::UntrustedNew { fingerprint } = verifier.check_result() {
        conn.close(0u32.into(), b"tofu confirm");
        endpoint.close(0u32.into(), b"tofu confirm");
        let _ = event_tx.send(SfuEvent::TrustRequired {
            server_addr: req.server_addr.clone(),
            fingerprint: fingerprint.clone(),
            changed_from: None,
        });
        return Ok(ConnectOutcome::TrustRequired {
            server_addr: req.server_addr,
            fingerprint,
            changed_from: None,
        });
    }

    let (mut ctrl_send, mut ctrl_recv) = conn
        .open_bi()
        .await
        .context("Failed opening QUIC control stream")?;

    let hello = ClientControl::Hello {
        protocol_version: PROTOCOL_VERSION,
        nickname: req.nickname.clone(),
        auth_token: req.auth_token.clone(),
    };
    write_control_msg(&mut ctrl_send, &hello).await?;

    let first_msg = tokio::time::timeout(Duration::from_secs(6), read_control_msg(&mut ctrl_recv))
        .await
        .context("Timed out waiting for SFU Welcome")??
        .context("SFU closed control stream before Welcome")?;

    let local_peer_id = match first_msg {
        ServerControl::Welcome { local_peer_id } => local_peer_id,
        ServerControl::Error { code, message } => {
            conn.close(0u32.into(), b"rejected");
            endpoint.close(0u32.into(), b"rejected");
            anyhow::bail!("SFU rejected Hello ({code:?}): {message}");
        }
        other => {
            conn.close(0u32.into(), b"unexpected handshake");
            endpoint.close(0u32.into(), b"unexpected handshake");
            anyhow::bail!("Unexpected handshake message: {other:?}");
        }
    };

    info!(%local_peer_id, server = %req.server_addr, "Authenticated with QUIC SFU");
    let _ = event_tx.send(SfuEvent::Connected {
        local_peer_id,
        server_addr: req.server_addr,
    });

    let cancel = CancellationToken::new();
    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
    let force_keyframe = Arc::new(AtomicBool::new(false));
    let rtt_ms = Arc::new(AtomicU64::new(0));
    let in_flight_video = Arc::new(AtomicUsize::new(0));
    let send_stall_flag = Arc::new(AtomicBool::new(false));
    let sfu_bitrate_hint = Arc::new(Mutex::new(None));
    let bytes_sent = Arc::new(AtomicU64::new(0));
    let bytes_received = Arc::new(AtomicU64::new(0));

    spawn_control_writer(ctrl_send, ctrl_rx, cancel.clone());
    spawn_control_reader(
        ctrl_recv,
        event_tx.clone(),
        Arc::clone(&force_keyframe),
        Arc::clone(&rtt_ms),
        Arc::clone(&sfu_bitrate_hint),
        cancel.clone(),
    );
    spawn_ping_loop(ctrl_tx.clone(), cancel.clone());
    spawn_uni_media_acceptor(
        conn.clone(),
        media_sink.clone(),
        Arc::clone(&bytes_received),
        cancel.clone(),
    );
    spawn_datagram_audio_reader(
        conn.clone(),
        media_sink,
        Arc::clone(&bytes_received),
        cancel.clone(),
    );

    // Watch connection close to propagate clean Disconnected event.
    {
        let conn_watch = conn.clone();
        let cancel_watch = cancel.clone();
        let ev = event_tx;
        tokio::spawn(async move {
            let reason = tokio::select! {
                _ = cancel_watch.cancelled() => return,
                closed = conn_watch.closed() => format!("{closed}"),
            };
            cancel_watch.cancel();
            let _ = ev.send(SfuEvent::Disconnected { reason });
        });
    }

    Ok(ConnectOutcome::Connected(SfuClient {
        conn,
        endpoint,
        rt_handle: tokio::runtime::Handle::current(),
        ctrl_tx,
        cancel,
        local_peer_id,
        force_keyframe,
        rtt_ms,
        in_flight_video,
        send_stall_flag,
        sfu_bitrate_hint,
        bytes_sent,
        bytes_received,
    }))
}

impl SfuClient {
    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    pub fn send_control(&self, msg: ClientControl) {
        let _ = self.ctrl_tx.send(msg);
    }

    pub fn create_room(&self, password: Option<String>) {
        self.send_control(ClientControl::CreateRoom { password });
    }

    pub fn join_room(&self, room_id: String, password: Option<String>) {
        self.send_control(ClientControl::JoinRoom { room_id, password });
    }

    pub fn leave_room(&self) {
        self.send_control(ClientControl::LeaveRoom);
    }

    pub fn start_broadcast(&self, metadata: StreamMetadata) {
        self.send_control(ClientControl::StartBroadcast { metadata });
    }

    pub fn stop_broadcast(&self) {
        *self.sfu_bitrate_hint.lock() = None;
        self.send_stall_flag.store(false, Ordering::Release);
        self.send_control(ClientControl::StopBroadcast);
    }

    pub fn set_subscription(&self, broadcaster_id: PeerId, video: bool, audio: bool) {
        self.send_control(ClientControl::SetSubscription {
            broadcaster_id,
            video,
            audio,
        });
    }

    pub fn request_keyframe(&self, broadcaster_id: PeerId) {
        self.send_control(ClientControl::RequestKeyframe { broadcaster_id });
    }

    pub fn send_viewer_feedback(
        &self,
        broadcaster_id: PeerId,
        queue_drops: u32,
        decode_errors: u32,
    ) {
        self.send_control(ClientControl::ViewerFeedback {
            broadcaster_id,
            queue_drops,
            decode_errors,
        });
    }

    /// Consumes and resets any server-requested IDR keyframe trigger.
    pub fn take_force_keyframe(&self) -> bool {
        self.force_keyframe.swap(false, Ordering::AcqRel)
    }

    /// Snapshot of current RTT and byte counters.
    pub fn counters(&self) -> NetworkCounters {
        let ping_rtt = self.rtt_ms.load(Ordering::Relaxed) as u32;
        let quic_rtt = self.conn.rtt().as_millis() as u32;
        NetworkCounters {
            rtt_ms: if ping_rtt > 0 { ping_rtt } else { quic_rtt },
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            in_flight_video: self.in_flight_video.load(Ordering::Relaxed),
        }
    }

    /// Reads current QUIC path telemetry (RTT, cwnd, loss, congestion events, queue depth).
    pub fn path_telemetry(&self) -> QuicPathTelemetry {
        let path = self.conn.stats().path;
        let path_rtt_ms = path.rtt.as_millis() as u32;
        let fallback_rtt = self.rtt_ms.load(Ordering::Relaxed) as u32;
        QuicPathTelemetry {
            rtt_ms: if path_rtt_ms > 0 {
                path_rtt_ms
            } else {
                fallback_rtt
            },
            cwnd_bytes: path.cwnd,
            sent_packets: path.sent_packets,
            lost_packets: path.lost_packets,
            congestion_events: path.congestion_events,
            in_flight_video: self.in_flight_video.load(Ordering::Acquire),
            send_stall_flagged: self.send_stall_flag.swap(false, Ordering::AcqRel),
        }
    }

    /// Returns the latest non-expired SFU bitrate hint received for the local broadcaster.
    pub fn sfu_bitrate_hint(&self) -> Option<SfuBitrateHintSnapshot> {
        let guard = self.sfu_bitrate_hint.lock();
        guard.filter(|snap| snap.updated_at.elapsed() <= Duration::from_secs(6))
    }

    /// Enqueues an encoded H.264 frame over an independent QUIC UniStream (prevents frame head-of-line stalling).
    pub fn send_video_frame(&self, header: &VideoFrameHeader, payload: &[u8]) -> Result<bool> {
        let is_keyframe = (header.flags & VIDEO_FLAG_KEYFRAME) != 0;
        let current = self.in_flight_video.load(Ordering::Acquire);
        if current >= MAX_IN_FLIGHT_VIDEO_STREAMS && !is_keyframe {
            return Ok(false);
        }

        let packet = encode_video_packet(header, payload)?;
        self.send_video_packet(packet, is_keyframe)
    }

    /// Enqueues a pre-framed binary video packet (`VIDEO_HEADER_LEN + annex_b`) directly over QUIC with zero extra copies.
    pub fn send_video_packet(&self, packet: Bytes, is_keyframe: bool) -> Result<bool> {
        let current = self.in_flight_video.load(Ordering::Acquire);
        if current >= MAX_IN_FLIGHT_VIDEO_STREAMS && !is_keyframe {
            return Ok(false);
        }

        let len = packet.len() as u64;
        let conn = self.conn.clone();
        let in_flight = Arc::clone(&self.in_flight_video);
        let send_stall_flag = Arc::clone(&self.send_stall_flag);
        let bytes_sent = Arc::clone(&self.bytes_sent);

        in_flight.fetch_add(1, Ordering::AcqRel);
        self.rt_handle.spawn(async move {
            let started = Instant::now();
            let result = async {
                let mut send = conn.open_uni().await?;
                if is_keyframe {
                    let _ = send.set_priority(10);
                }
                send.write_all(&packet).await?;
                let _ = send.finish();
                Ok::<(), anyhow::Error>(())
            }
            .await;

            in_flight.fetch_sub(1, Ordering::AcqRel);
            if started.elapsed() >= Duration::from_millis(180) {
                send_stall_flag.store(true, Ordering::Release);
            }
            if let Err(err) = result {
                debug!("Failed sending QUIC video UniStream: {err}");
            } else {
                bytes_sent.fetch_add(len, Ordering::Relaxed);
            }
        });

        Ok(true)
    }

    /// Sends a 20ms Opus packet over unreliable QUIC datagram (with UniStream fallback if datagrams unavailable).
    pub fn send_audio_packet(&self, header: &AudioPacketHeader, payload: &[u8]) -> Result<()> {
        let packet = encode_audio_packet(header, payload)?;
        let len = packet.len() as u64;

        match self.conn.send_datagram(packet.clone()) {
            Ok(()) => {
                self.bytes_sent.fetch_add(len, Ordering::Relaxed);
                Ok(())
            }
            Err(SendDatagramError::TooLarge)
            | Err(SendDatagramError::Disabled)
            | Err(SendDatagramError::UnsupportedByPeer) => {
                let conn = self.conn.clone();
                let bytes_sent = Arc::clone(&self.bytes_sent);
                self.rt_handle.spawn(async move {
                    if let Ok(mut s) = conn.open_uni().await {
                        if s.write_all(&packet).await.is_ok() {
                            bytes_sent.fetch_add(len, Ordering::Relaxed);
                        }
                        let _ = s.finish();
                    }
                });
                Ok(())
            }
            Err(SendDatagramError::ConnectionLost(err)) => {
                Err(err).context("QUIC connection lost sending audio")
            }
        }
    }

    pub fn disconnect(&self) {
        self.cancel.cancel();
        self.conn.close(0u32.into(), b"client disconnect");
        self.endpoint.close(0u32.into(), b"client disconnect");
    }
}

async fn resolve_server_addr(addr_str: &str) -> Result<(SocketAddr, String)> {
    let trimmed = addr_str.trim();
    if let Ok(sock) = trimmed.parse::<SocketAddr>() {
        return Ok((sock, "localhost".to_string()));
    }

    let (host, _) = trimmed
        .rsplit_once(':')
        .context("Server address must be in host:port format")?;

    let mut resolved = tokio::net::lookup_host(trimmed)
        .await
        .with_context(|| format!("Failed resolving SFU host '{trimmed}'"))?;
    let sock = resolved
        .next()
        .with_context(|| format!("No IP address resolved for '{trimmed}'"))?;

    Ok((sock, sni_for_host(host)))
}

fn spawn_control_writer(
    mut ctrl_send: SendStream,
    mut ctrl_rx: mpsc::UnboundedReceiver<ClientControl>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                maybe = ctrl_rx.recv() => {
                    let Some(msg) = maybe else { break };
                    if let Err(err) = write_control_msg(&mut ctrl_send, &msg).await {
                        warn!("QUIC control write failed: {err}");
                        break;
                    }
                }
            }
        }
        let _ = ctrl_send.finish();
    });
}

fn spawn_control_reader(
    mut ctrl_recv: RecvStream,
    event_tx: mpsc::UnboundedSender<SfuEvent>,
    force_keyframe: Arc<AtomicBool>,
    rtt_ms: Arc<AtomicU64>,
    sfu_bitrate_hint: Arc<Mutex<Option<SfuBitrateHintSnapshot>>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                _ = cancel.cancelled() => break,
                res = read_control_msg(&mut ctrl_recv) => match res {
                    Ok(Some(m)) => m,
                    Ok(None) => break,
                    Err(err) => {
                        debug!("Control read ended: {err}");
                        break;
                    }
                }
            };

            match msg {
                ServerControl::Welcome { .. } => {}
                ServerControl::RoomJoined {
                    room_id,
                    local_peer_id,
                    peers,
                } => {
                    let _ = event_tx.send(SfuEvent::RoomJoined {
                        room_id,
                        local_peer_id,
                        peers,
                    });
                }
                ServerControl::PeerJoined { peer } => {
                    let _ = event_tx.send(SfuEvent::PeerJoined(peer));
                }
                ServerControl::PeerLeft { peer_id } => {
                    let _ = event_tx.send(SfuEvent::PeerLeft { peer_id });
                }
                ServerControl::PeerStreamStarted { peer_id, metadata } => {
                    let _ = event_tx.send(SfuEvent::PeerBroadcastUpdated {
                        peer_id,
                        metadata: Some(metadata),
                    });
                }
                ServerControl::PeerStreamStopped { peer_id } => {
                    let _ = event_tx.send(SfuEvent::PeerBroadcastUpdated {
                        peer_id,
                        metadata: None,
                    });
                }
                ServerControl::ForceKeyframe => {
                    force_keyframe.store(true, Ordering::Release);
                    let _ = event_tx.send(SfuEvent::ForceKeyframe);
                }
                ServerControl::BitrateHint {
                    target_max_bps,
                    min_fps_cap,
                    congestion_source,
                } => {
                    *sfu_bitrate_hint.lock() = Some(SfuBitrateHintSnapshot {
                        target_max_bps,
                        min_fps_cap,
                        congestion_source,
                        updated_at: Instant::now(),
                    });
                }
                ServerControl::Error { code, message } => {
                    let _ = event_tx.send(SfuEvent::ServerError { code, message });
                }
                ServerControl::Pong { client_time_ms } => {
                    let now = now_unix_ms();
                    let sample = now.saturating_sub(client_time_ms);
                    rtt_ms.store(sample, Ordering::Relaxed);
                    let _ = event_tx.send(SfuEvent::RttUpdated {
                        rtt_ms: sample as u32,
                    });
                }
            }
        }
        cancel.cancel();
    });
}

fn spawn_ping_loop(ctrl_tx: mpsc::UnboundedSender<ClientControl>, cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tick.tick() => {
                    let _ = ctrl_tx.send(ClientControl::Ping {
                        client_time_ms: now_unix_ms(),
                    });
                }
            }
        }
    });
}

fn spawn_uni_media_acceptor(
    conn: Connection,
    media_sink: Arc<dyn MediaSink>,
    bytes_received: Arc<AtomicU64>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            let recv = tokio::select! {
                _ = cancel.cancelled() => break,
                res = conn.accept_uni() => match res {
                    Ok(r) => r,
                    Err(err) => {
                        debug!("QUIC accept_uni ended: {err}");
                        break;
                    }
                }
            };

            let sink = Arc::clone(&media_sink);
            let rx_bytes = Arc::clone(&bytes_received);
            tokio::spawn(async move {
                if let Err(err) = read_uni_media_stream(recv, sink, rx_bytes).await {
                    debug!("Dropping malformed incoming QUIC UniStream: {err}");
                }
            });
        }
    });
}

async fn read_uni_media_stream(
    mut recv: RecvStream,
    media_sink: Arc<dyn MediaSink>,
    bytes_received: Arc<AtomicU64>,
) -> Result<()> {
    let buf = recv
        .read_to_end(VIDEO_HEADER_LEN + MAX_VIDEO_FRAME_BYTES)
        .await?;
    if buf.is_empty() {
        return Ok(());
    }
    let kind = buf[0];
    bytes_received.fetch_add(buf.len() as u64, Ordering::Relaxed);

    if kind == MEDIA_KIND_VIDEO {
        let (header, payload) = VideoFrameHeader::decode(&buf)?;
        media_sink.on_video_frame(header, Bytes::copy_from_slice(payload));
        return Ok(());
    }

    if kind == MEDIA_KIND_AUDIO {
        if buf.len() > AUDIO_HEADER_LEN + MAX_AUDIO_PACKET_BYTES {
            anyhow::bail!(
                "Audio payload exceeds max {MAX_AUDIO_PACKET_BYTES} bytes"
            );
        }
        let (header, payload) = AudioPacketHeader::decode(&buf)?;
        media_sink.on_audio_packet(header, Bytes::copy_from_slice(payload));
        return Ok(());
    }

    anyhow::bail!("Unknown incoming QUIC UniStream kind={kind}")
}

fn spawn_datagram_audio_reader(
    conn: Connection,
    media_sink: Arc<dyn MediaSink>,
    bytes_received: Arc<AtomicU64>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            let datagram = tokio::select! {
                _ = cancel.cancelled() => break,
                res = conn.read_datagram() => match res {
                    Ok(d) => d,
                    Err(err) => {
                        debug!("QUIC read_datagram ended: {err}");
                        break;
                    }
                }
            };

            bytes_received.fetch_add(datagram.len() as u64, Ordering::Relaxed);
            if let Ok((header, payload)) = AudioPacketHeader::decode(&datagram) {
                media_sink.on_audio_packet(header, Bytes::copy_from_slice(payload));
            }
        }
    });
}

async fn write_control_msg(send: &mut SendStream, msg: &ClientControl) -> Result<()> {
    let framed = encode_control_frame(msg)?;
    send.write_all(&framed).await?;
    Ok(())
}

async fn read_control_msg(recv: &mut RecvStream) -> Result<Option<ServerControl>> {
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_CONTROL_FRAME_BYTES {
        anyhow::bail!("Server control frame {len} exceeds {MAX_CONTROL_FRAME_BYTES}");
    }
    let mut payload = vec![0u8; len];
    recv.read_exact(&mut payload).await?;
    let msg: ServerControl = decode_control_payload(&payload)?;
    Ok(Some(msg))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
