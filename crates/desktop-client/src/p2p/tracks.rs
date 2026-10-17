use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::RwLock;
use shared_protocol::{
    decode_any_frame_packet, encode_frame_packet, encode_yuv420p_frame_packet, FramePixelFormat,
    PeerId,
};
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use webrtc::{
    api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS},
    media::Sample,
    rtp::{codecs::h264::H264Packet, packetizer::Depacketizer},
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::{
        track_local::track_local_static_sample::TrackLocalStaticSample,
        track_remote::TrackRemote,
    },
};

use crate::media::{
    traits::{MediaError, RawAudioFrame, RawVideoFrame},
    video::{
        rgba_to_yuv420p_into, yuv420p_planar_to_rgba, H264VideoDecoder, H264VideoEncoder,
    },
    CpalAudioPlayer, OpusAudioDecoder, OpusAudioEncoder,
};

#[derive(Debug, Default)]
pub struct PeerMediaCounters {
    pub decoded_video_frames: AtomicU64,
    pub decoded_audio_packets: AtomicU64,
}

#[derive(Debug, Clone)]
pub struct DecodedVideoFrame {
    pub peer_id: PeerId,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone)]
struct CachedFramePacket {
    seq: u64,
    packet: Arc<Vec<u8>>,
}

#[derive(Clone)]
struct LocalPreviewFrame {
    seq: u64,
    packet: Arc<Vec<u8>>,
}

/// Thread-safe store of remote decoded video frames, active stream status, and per-peer media counters.
#[derive(Clone, Default)]
pub struct RemoteMediaStore {
    counters: Arc<DashMap<PeerId, Arc<PeerMediaCounters>>>,
    latest_frames: Arc<RwLock<HashMap<PeerId, CachedFramePacket>>>,
    last_delivered_seq: Arc<DashMap<PeerId, u64>>,
    local_preview_frame: Arc<RwLock<Option<LocalPreviewFrame>>>,
    local_preview_seq: Arc<AtomicU64>,
    last_delivered_local_seq: Arc<AtomicU64>,
    frame_subscribers: Arc<RwLock<Vec<mpsc::Sender<Vec<u8>>>>>,
    streaming_peers: Arc<RwLock<HashSet<PeerId>>>,
    watched_peers: Arc<RwLock<Option<HashSet<PeerId>>>>,
}

impl RemoteMediaStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enables opt-in on-demand stream watching (`watched_peers` starts empty).
    /// When not called (e.g. in headless integration tests), all incoming streams are decoded by default.
    pub fn enable_on_demand_watch(&self) {
        *self.watched_peers.write() = Some(HashSet::new());
    }

    pub fn set_peer_watched(&self, peer_id: PeerId, watched: bool) {
        {
            let mut guard = self.watched_peers.write();
            let set = guard.get_or_insert_with(HashSet::new);
            if watched {
                set.insert(peer_id);
            } else {
                set.remove(&peer_id);
            }
        }
        self.last_delivered_seq.remove(&peer_id);
        if !watched {
            self.latest_frames.write().remove(&peer_id);
        }
    }

    pub fn is_peer_watched(&self, peer_id: PeerId) -> bool {
        match self.watched_peers.read().as_ref() {
            None => true,
            Some(set) => set.contains(&peer_id),
        }
    }

    pub fn watched_peers(&self) -> Vec<PeerId> {
        let mut list: Vec<PeerId> = match self.watched_peers.read().as_ref() {
            None => self.streaming_peers.read().iter().copied().collect(),
            Some(set) => set.iter().copied().collect(),
        };
        list.sort();
        list
    }

    pub fn mark_peer_streaming(&self, peer_id: PeerId) {
        self.streaming_peers.write().insert(peer_id);
    }

    pub fn mark_peer_stopped(&self, peer_id: PeerId) {
        self.streaming_peers.write().remove(&peer_id);
        self.latest_frames.write().remove(&peer_id);
        self.last_delivered_seq.remove(&peer_id);
        if let Some(set) = self.watched_peers.write().as_mut() {
            set.remove(&peer_id);
        }
    }

    pub fn streaming_peers(&self) -> Vec<PeerId> {
        let mut list: Vec<PeerId> = self.streaming_peers.read().iter().copied().collect();
        list.sort();
        list
    }

    pub fn counters_for(&self, peer_id: PeerId) -> Arc<PeerMediaCounters> {
        self.counters
            .entry(peer_id)
            .or_insert_with(|| Arc::new(PeerMediaCounters::default()))
            .value()
            .clone()
    }

    pub fn decoded_video_frames(&self, peer_id: PeerId) -> u64 {
        self.counters
            .get(&peer_id)
            .map(|c| c.decoded_video_frames.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    pub fn decoded_audio_packets(&self, peer_id: PeerId) -> u64 {
        self.counters
            .get(&peer_id)
            .map(|c| c.decoded_audio_packets.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    pub fn record_video_frame(&self, frame: DecodedVideoFrame) {
        let mut i420 = Vec::new();
        if rgba_to_yuv420p_into(&frame.rgba, frame.width, frame.height, &mut i420).is_ok() {
            self.record_video_frame_yuv(frame.peer_id, frame.width, frame.height, &i420);
        }
    }

    /// Records a decoded planar I420 (`YUV420p`) video frame directly without CPU YUV->RGBA conversion.
    pub fn record_video_frame_yuv(&self, peer_id: PeerId, width: u32, height: u32, i420: &[u8]) {
        self.mark_peer_streaming(peer_id);
        let counters = self.counters_for(peer_id);
        let seq = counters.decoded_video_frames.fetch_add(1, Ordering::Relaxed) + 1;

        let Ok(packet_vec) = encode_yuv420p_frame_packet(peer_id, width, height, i420) else {
            return;
        };

        let subs = self.frame_subscribers.read().clone();
        for sub in subs {
            let _ = sub.try_send(packet_vec.clone());
        }

        self.latest_frames.write().insert(
            peer_id,
            CachedFramePacket {
                seq,
                packet: Arc::new(packet_vec),
            },
        );
    }

    pub fn record_local_preview_frame(&self, width: u32, height: u32, rgba: Vec<u8>) {
        let mut i420 = Vec::new();
        if rgba_to_yuv420p_into(&rgba, width, height, &mut i420).is_ok() {
            self.record_local_preview_yuv(width, height, &i420);
        }
    }

    /// Records a local preview frame using an already-computed planar I420 (`YUV420p`) buffer.
    pub fn record_local_preview_yuv(&self, width: u32, height: u32, i420: &[u8]) {
        let Ok(packet_vec) = encode_yuv420p_frame_packet(PeerId::nil(), width, height, i420) else {
            return;
        };
        let seq = self.local_preview_seq.fetch_add(1, Ordering::Relaxed) + 1;
        *self.local_preview_frame.write() = Some(LocalPreviewFrame {
            seq,
            packet: Arc::new(packet_vec),
        });
    }

    pub fn clear_local_preview_frame(&self) {
        *self.local_preview_frame.write() = None;
        self.last_delivered_local_seq.store(0, Ordering::Relaxed);
    }

    pub fn take_new_local_preview_packet(&self) -> Option<Vec<u8>> {
        let cached = self.local_preview_frame.read().clone()?;
        let last_seq = self.last_delivered_local_seq.load(Ordering::Relaxed);
        if cached.seq <= last_seq {
            return None;
        }
        self.last_delivered_local_seq
            .store(cached.seq, Ordering::Relaxed);
        Some(cached.packet.as_ref().clone())
    }

    pub fn record_audio_packet(&self, peer_id: PeerId) {
        let counters = self.counters_for(peer_id);
        counters
            .decoded_audio_packets
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn latest_frame_packet(&self, peer_id: PeerId) -> Option<Vec<u8>> {
        let packet_arc = {
            let guard = self.latest_frames.read();
            guard.get(&peer_id)?.packet.clone()
        };
        match decode_any_frame_packet(&packet_arc) {
            Ok((pid, w, h, FramePixelFormat::I420, i420)) => {
                let rgba = yuv420p_planar_to_rgba(i420, w, h).ok()?;
                encode_frame_packet(pid, w, h, &rgba).ok()
            }
            _ => Some(packet_arc.as_ref().clone()),
        }
    }

    /// Returns the pre-encoded binary frame packet (`I420` or `RGBA`) only if a newer frame
    /// (`seq > last_delivered_seq`) has been decoded since the last IPC poll for `peer_id`.
    pub fn take_new_frame_packet(&self, peer_id: PeerId) -> Option<Vec<u8>> {
        let guard = self.latest_frames.read();
        let cached = guard.get(&peer_id)?;
        let last_seq = self
            .last_delivered_seq
            .get(&peer_id)
            .map(|v| *v)
            .unwrap_or(0);
        if cached.seq <= last_seq {
            return None;
        }
        self.last_delivered_seq.insert(peer_id, cached.seq);
        Some(cached.packet.as_ref().clone())
    }

    pub fn all_latest_frame_packets(&self) -> Vec<Vec<u8>> {
        let guard = self.latest_frames.read();
        guard.values().map(|c| c.packet.as_ref().clone()).collect()
    }

    pub fn subscribe_binary_frames(&self) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel(32);
        self.frame_subscribers.write().push(tx);
        rx
    }

    pub fn remove_peer(&self, peer_id: PeerId) {
        self.mark_peer_stopped(peer_id);
        self.counters.remove(&peer_id);
    }
}

pub fn create_h264_video_track() -> Arc<TrackLocalStaticSample> {
    Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line:
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
            rtcp_feedback: vec![],
        },
        "video".to_owned(),
        "screen-share".to_owned(),
    ))
}

pub fn create_opus_audio_track() -> Arc<TrackLocalStaticSample> {
    Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48_000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        },
        "audio".to_owned(),
        "screen-share".to_owned(),
    ))
}

/// Spawns a dedicated encoder thread that encodes raw RGBA video frames into H.264 Annex-B NAL units
/// and an async writer task that transmits RTP samples on `video_track`.
pub fn spawn_outgoing_video_pipeline(
    mut video_rx: mpsc::Receiver<RawVideoFrame>,
    video_track: Arc<TrackLocalStaticSample>,
    fps: u8,
    keyframe_notify: Arc<Notify>,
    media_store: RemoteMediaStore,
    cancel: CancellationToken,
) -> Result<(), MediaError> {
    let target_bitrate = if fps >= 60 {
        8_000_000
    } else if fps >= 30 {
        5_000_000
    } else {
        3_000_000
    };
    let mut encoder = H264VideoEncoder::new(fps, target_bitrate)?;
    let frame_duration = Duration::from_micros(1_000_000 / (fps.clamp(1, 60) as u64));
    let (nal_tx, mut nal_rx) = mpsc::channel::<Bytes>(8);

    let force_kf = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let force_kf_flag = Arc::clone(&force_kf);
    let cancel_kf = cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_kf.cancelled() => break,
                _ = keyframe_notify.notified() => {
                    force_kf_flag.store(true, Ordering::Release);
                }
            }
        }
    });

    let cancel_enc = cancel.clone();
    std::thread::Builder::new()
        .name("h264-video-encoder".to_string())
        .spawn(move || {
            let mut preview_counter = 0u64;
            while !cancel_enc.is_cancelled() {
                let Some(mut frame) = video_rx.blocking_recv() else {
                    break;
                };
                // Drain any queued older frames so we always encode the freshest frame
                while let Ok(newer) = video_rx.try_recv() {
                    frame = newer;
                }
                if force_kf.swap(false, Ordering::AcqRel) {
                    encoder.request_keyframe();
                }
                match encoder.encode_rgba(&frame.rgba, frame.width, frame.height) {
                    Ok(annex_b) if !annex_b.is_empty() => {
                        if nal_tx.blocking_send(Bytes::from(annex_b)).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        warn!("H.264 frame encode error: {err}");
                    }
                }
                // Reuse the already-converted I420 planar buffer from the encoder for the local
                // preview (at up to 30 FPS when capturing at 60 FPS so PiP preview IPC never
                // competes with remote 60 FPS streams).
                if fps <= 30 || preview_counter.is_multiple_of(2) {
                    media_store.record_local_preview_yuv(
                        frame.width,
                        frame.height,
                        encoder.last_yuv420p(),
                    );
                }
                preview_counter = preview_counter.wrapping_add(1);
            }
        })
        .map_err(|e| MediaError::Codec(format!("Failed to spawn H.264 encoder thread: {e}")))?;

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                maybe_nal = nal_rx.recv() => {
                    let Some(annex_b) = maybe_nal else { break };
                    let sample = Sample {
                        data: annex_b,
                        duration: frame_duration,
                        ..Default::default()
                    };
                    if let Err(err) = video_track.write_sample(&sample).await {
                        debug!("Failed writing H.264 sample to track: {err}");
                    }
                }
            }
        }
    });

    Ok(())
}

/// Spawns a task that encodes 20ms stereo 48kHz PCM frames into Opus packets and writes
/// RTP samples to `audio_track`.
pub fn spawn_outgoing_audio_pipeline(
    mut audio_rx: mpsc::Receiver<RawAudioFrame>,
    audio_track: Arc<TrackLocalStaticSample>,
    cancel: CancellationToken,
) -> Result<(), MediaError> {
    let mut encoder = OpusAudioEncoder::new()?;
    let sample_duration = Duration::from_millis(20);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                maybe_frame = audio_rx.recv() => {
                    let Some(frame) = maybe_frame else { break };
                    match encoder.encode_frame(&frame) {
                        Ok(opus_bytes) if !opus_bytes.is_empty() => {
                            let sample = Sample {
                                data: Bytes::from(opus_bytes),
                                duration: sample_duration,
                                ..Default::default()
                            };
                            if let Err(err) = audio_track.write_sample(&sample).await {
                                debug!("Failed writing Opus sample to track: {err}");
                            }
                        }
                        Ok(_) => {}
                        Err(err) => {
                            warn!("Opus frame encode error: {err}");
                        }
                    }
                }
            }
        }
    });

    Ok(())
}

/// Spawns the appropriate RTP reader and decoder loop for an incoming remote WebRTC track.
pub fn spawn_incoming_track_handler(
    peer_id: PeerId,
    track: Arc<TrackRemote>,
    media_store: RemoteMediaStore,
    audio_player: Option<CpalAudioPlayer>,
    keyframe_req_notify: Option<Arc<Notify>>,
    cancel: CancellationToken,
) {
    media_store.mark_peer_streaming(peer_id);
    let mime = track.codec().capability.mime_type.to_lowercase();
    if mime.contains("h264") {
        tokio::spawn(async move {
            run_incoming_h264_loop(peer_id, track, media_store, keyframe_req_notify, cancel).await;
        });
    } else if mime.contains("opus") {
        tokio::spawn(async move {
            run_incoming_opus_loop(peer_id, track, media_store, audio_player, cancel).await;
        });
    } else {
        warn!(%peer_id, %mime, "Unsupported incoming remote track codec");
    }
}

async fn run_incoming_h264_loop(
    peer_id: PeerId,
    track: Arc<TrackRemote>,
    media_store: RemoteMediaStore,
    keyframe_req_notify: Option<Arc<Notify>>,
    cancel: CancellationToken,
) {
    // Use a deep queue (128 access units) so encoded P-frames are never dropped between
    // RTP depacketization and OpenH264 decoding (dropping a single P-frame breaks H.264
    // inter-frame prediction until the next IDR keyframe).
    let (au_tx, mut au_rx) = mpsc::channel::<Vec<u8>>(128);
    let store_dec = media_store.clone();
    let cancel_dec = cancel.clone();
    let kf_notify_dec = keyframe_req_notify.clone();

    let _ = std::thread::Builder::new()
        .name(format!("h264-decoder-{peer_id}"))
        .spawn(move || {
            let mut decoder: Option<H264VideoDecoder> = None;
            while !cancel_dec.is_cancelled() {
                let Some(access_unit) = au_rx.blocking_recv() else {
                    break;
                };
                if !store_dec.is_peer_watched(peer_id) {
                    decoder = None;
                    continue;
                }
                let dec = match &mut decoder {
                    Some(d) => d,
                    None => match H264VideoDecoder::new() {
                        Ok(d) => {
                            if let Some(notify) = &kf_notify_dec {
                                notify.notify_one();
                            }
                            decoder.insert(d)
                        }
                        Err(err) => {
                            warn!(%peer_id, "Failed to initialize H264VideoDecoder: {err}");
                            return;
                        }
                    },
                };
                match dec.decode_annex_b_yuv(&access_unit) {
                    Ok((Some((width, height, i420)), had_error)) => {
                        store_dec.record_video_frame_yuv(peer_id, width, height, &i420);
                        if had_error {
                            if let Some(notify) = &kf_notify_dec {
                                notify.notify_one();
                            }
                        }
                    }
                    Ok((None, had_error)) => {
                        if had_error {
                            if let Some(notify) = &kf_notify_dec {
                                notify.notify_one();
                            }
                        }
                    }
                    Err(err) => {
                        debug!(%peer_id, "H264 decode error: {err}");
                        if let Some(notify) = &kf_notify_dec {
                            notify.notify_one();
                        }
                    }
                }
            }
        });

    let mut depacketizer = H264Packet::default();
    let mut access_unit_buf = Vec::with_capacity(64 * 1024);
    let mut last_seq: Option<u16> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            read_res = track.read_rtp() => {
                let (rtp_packet, _) = match read_res {
                    Ok(pair) => pair,
                    Err(err) => {
                        debug!(%peer_id, "Incoming video track ended: {err}");
                        break;
                    }
                };

                if rtp_packet.payload.is_empty() {
                    continue;
                }

                media_store.mark_peer_streaming(peer_id);

                // On-demand stream watching: drain RTP packets without CPU-heavy H.264 decoding
                // until the local user explicitly chooses to watch this peer's stream.
                if !media_store.is_peer_watched(peer_id) {
                    access_unit_buf.clear();
                    last_seq = None;
                    continue;
                }

                // Detect RTP sequence number gaps (packet loss) and immediately request an IDR keyframe.
                let seq = rtp_packet.header.sequence_number;
                if let Some(prev) = last_seq {
                    if seq != prev.wrapping_add(1) {
                        access_unit_buf.clear();
                        depacketizer = H264Packet::default();
                        if let Some(notify) = &keyframe_req_notify {
                            notify.notify_one();
                        }
                    }
                }
                last_seq = Some(seq);

                match depacketizer.depacketize(&rtp_packet.payload) {
                    Ok(nal_bytes) if !nal_bytes.is_empty() => {
                        access_unit_buf.extend_from_slice(&nal_bytes);
                        if rtp_packet.header.marker || access_unit_buf.len() > 256 * 1024 {
                            let au = std::mem::replace(
                                &mut access_unit_buf,
                                Vec::with_capacity(64 * 1024),
                            );
                            if au_tx.try_send(au).is_err() {
                                // Decoder queue overflowed; request a fresh IDR keyframe to resync.
                                if let Some(notify) = &keyframe_req_notify {
                                    notify.notify_one();
                                }
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        debug!(%peer_id, "H264 RTP depacketize error: {err}");
                        access_unit_buf.clear();
                        depacketizer = H264Packet::default();
                        if let Some(notify) = &keyframe_req_notify {
                            notify.notify_one();
                        }
                    }
                }
            }
        }
    }

    media_store.mark_peer_stopped(peer_id);
}

async fn run_incoming_opus_loop(
    peer_id: PeerId,
    track: Arc<TrackRemote>,
    media_store: RemoteMediaStore,
    audio_player: Option<CpalAudioPlayer>,
    cancel: CancellationToken,
) {
    let mut decoder: Option<OpusAudioDecoder> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            read_res = track.read_rtp() => {
                let (rtp_packet, _) = match read_res {
                    Ok(pair) => pair,
                    Err(err) => {
                        debug!(%peer_id, "Incoming audio track ended: {err}");
                        break;
                    }
                };

                if rtp_packet.payload.is_empty() {
                    continue;
                }

                // Skip Opus decoding and audio output if the user is not watching this peer's stream
                if !media_store.is_peer_watched(peer_id) {
                    continue;
                }

                let dec = match &mut decoder {
                    Some(d) => d,
                    None => match OpusAudioDecoder::new() {
                        Ok(d) => decoder.insert(d),
                        Err(err) => {
                            warn!(%peer_id, "Failed to initialize OpusAudioDecoder: {err}");
                            return;
                        }
                    },
                };

                match dec.decode_packet(&rtp_packet.payload) {
                    Ok(pcm_samples) if !pcm_samples.is_empty() => {
                        media_store.record_audio_packet(peer_id);
                        if let Some(player) = &audio_player {
                            player.push_samples(&pcm_samples);
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        debug!(%peer_id, "Opus packet decode error: {err}");
                    }
                }
            }
        }
    }
}

