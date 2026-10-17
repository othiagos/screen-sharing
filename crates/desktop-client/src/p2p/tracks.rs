use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::RwLock;
use shared_protocol::{encode_frame_packet, PeerId};
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
    video::{H264VideoDecoder, H264VideoEncoder},
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

/// Thread-safe store of remote decoded video frames, active stream status, and per-peer media counters.
#[derive(Clone, Default)]
pub struct RemoteMediaStore {
    counters: Arc<DashMap<PeerId, Arc<PeerMediaCounters>>>,
    latest_frames: Arc<RwLock<HashMap<PeerId, DecodedVideoFrame>>>,
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
        self.mark_peer_streaming(frame.peer_id);
        let counters = self.counters_for(frame.peer_id);
        counters.decoded_video_frames.fetch_add(1, Ordering::Relaxed);

        let subs = self.frame_subscribers.read().clone();
        if !subs.is_empty() {
            if let Ok(packet) =
                encode_frame_packet(frame.peer_id, frame.width, frame.height, &frame.rgba)
            {
                for sub in subs {
                    let _ = sub.try_send(packet.clone());
                }
            }
        }

        self.latest_frames.write().insert(frame.peer_id, frame);
    }

    pub fn record_audio_packet(&self, peer_id: PeerId) {
        let counters = self.counters_for(peer_id);
        counters
            .decoded_audio_packets
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn latest_frame_packet(&self, peer_id: PeerId) -> Option<Vec<u8>> {
        let guard = self.latest_frames.read();
        let frame = guard.get(&peer_id)?;
        encode_frame_packet(frame.peer_id, frame.width, frame.height, &frame.rgba).ok()
    }

    pub fn all_latest_frame_packets(&self) -> Vec<Vec<u8>> {
        let guard = self.latest_frames.read();
        guard
            .values()
            .filter_map(|f| encode_frame_packet(f.peer_id, f.width, f.height, &f.rgba).ok())
            .collect()
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

/// Spawns a task that encodes raw RGBA video frames into H.264 Annex-B NAL units and writes
/// RTP samples to `video_track`.
pub fn spawn_outgoing_video_pipeline(
    mut video_rx: mpsc::Receiver<RawVideoFrame>,
    video_track: Arc<TrackLocalStaticSample>,
    fps: u8,
    keyframe_notify: Arc<Notify>,
    cancel: CancellationToken,
) -> Result<(), MediaError> {
    let mut encoder = H264VideoEncoder::new(fps, 2_500_000)?;
    let frame_duration = Duration::from_micros(1_000_000 / (fps.clamp(1, 60) as u64));

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = keyframe_notify.notified() => {
                    encoder.request_keyframe();
                }
                maybe_frame = video_rx.recv() => {
                    let Some(frame) = maybe_frame else { break };
                    match encoder.encode_rgba(&frame.rgba, frame.width, frame.height) {
                        Ok(annex_b) if !annex_b.is_empty() => {
                            let sample = Sample {
                                data: Bytes::from(annex_b),
                                duration: frame_duration,
                                ..Default::default()
                            };
                            if let Err(err) = video_track.write_sample(&sample).await {
                                debug!("Failed writing H.264 sample to track: {err}");
                            }
                        }
                        Ok(_) => {}
                        Err(err) => {
                            warn!("H.264 frame encode error: {err}");
                        }
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
    cancel: CancellationToken,
) {
    media_store.mark_peer_streaming(peer_id);
    let mime = track.codec().capability.mime_type.to_lowercase();
    if mime.contains("h264") {
        tokio::spawn(async move {
            run_incoming_h264_loop(peer_id, track, media_store, cancel).await;
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
    cancel: CancellationToken,
) {
    let mut decoder: Option<H264VideoDecoder> = None;
    let mut depacketizer = H264Packet::default();
    let mut access_unit_buf = Vec::with_capacity(64 * 1024);

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
                    decoder = None;
                    access_unit_buf.clear();
                    continue;
                }

                let dec = match &mut decoder {
                    Some(d) => d,
                    None => match H264VideoDecoder::new() {
                        Ok(d) => decoder.insert(d),
                        Err(err) => {
                            warn!(%peer_id, "Failed to initialize H264VideoDecoder: {err}");
                            return;
                        }
                    },
                };

                match depacketizer.depacketize(&rtp_packet.payload) {
                    Ok(nal_bytes) if !nal_bytes.is_empty() => {
                        access_unit_buf.extend_from_slice(&nal_bytes);
                        // Decode immediately when marker bit is set or buffer contains complete NALs
                        if rtp_packet.header.marker || access_unit_buf.len() > 4 {
                            match dec.decode_annex_b(&access_unit_buf) {
                                Ok(Some((width, height, rgba))) => {
                                    media_store.record_video_frame(DecodedVideoFrame {
                                        peer_id,
                                        width,
                                        height,
                                        rgba,
                                    });
                                }
                                Ok(None) => {}
                                Err(err) => {
                                    debug!(%peer_id, "H264 decode error: {err}");
                                }
                            }
                            access_unit_buf.clear();
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        debug!(%peer_id, "H264 RTP depacketize error: {err}");
                        access_unit_buf.clear();
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

