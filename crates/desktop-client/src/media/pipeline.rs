use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::Mutex;
use shared_protocol::{
    sanitize_video_fps, vbr_bitrate_range, AudioCaptureMode, AudioPacketHeader, CaptureTarget,
    PeerId, StreamConfig, StreamMetadata, ThrottleReason, VideoFrameHeader, AUDIO_FLAG_OPUS,
    VIDEO_FLAG_H264, VIDEO_FLAG_KEYFRAME,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::media::adaptive::{AdaptiveBitrateController, AdaptiveInputs};
use crate::media::audio_out::{OpusAudioEncoder, PeerAudioMixer};
use crate::media::default_os_audio_capturer;
use crate::media::traits::{AudioCapturer, RawAudioFrame, RawVideoFrame, VideoCapturer};
use crate::media::video::{
    create_best_video_encoder, is_h264_keyframe, rgba_to_bgra_in_place, H264VideoDecoder,
    NativeVideoCapturer,
};
use crate::net::{MediaSink, SfuClient};

const DECODER_QUEUE_CAPACITY: usize = 4;

/// Decoded latest BGRA video frame ready for GPUI `RenderImage` presentation.
#[derive(Clone)]
pub struct DecodedVideoFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pub generation: u64,
    pub frame_id: u32,
    pub fps_estimate: u32,
}

/// Live broadcast telemetry rendered in the UI stats overlay.
#[derive(Debug, Clone, Default)]
pub struct BroadcastStats {
    pub backend: String,
    pub width: u32,
    pub height: u32,
    pub encoded_fps: u32,
    pub configured_fps: u32,
    pub effective_fps: u32,
    pub target_bitrate_kbps: u32,
    pub max_bitrate_kbps: u32,
    pub throttle_reason: ThrottleReason,
    pub dropped_frames: u64,
    pub warning: Option<String>,
}

/// Thread-safe store holding the latest decoded BGRA frame per peer plus optional local preview.
pub struct PeerVideoStore {
    peers: DashMap<PeerId, Arc<Mutex<Option<DecodedVideoFrame>>>>,
    local_preview: Arc<Mutex<Option<DecodedVideoFrame>>>,
    next_generation: AtomicU64,
}

impl Default for PeerVideoStore {
    fn default() -> Self {
        Self {
            peers: DashMap::new(),
            local_preview: Arc::new(Mutex::new(None)),
            next_generation: AtomicU64::new(1),
        }
    }
}

impl PeerVideoStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn update_peer(
        &self,
        peer_id: PeerId,
        width: u32,
        height: u32,
        bgra: Vec<u8>,
        frame_id: u32,
        fps_estimate: u32,
    ) {
        let gen = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let slot = self
            .peers
            .entry(peer_id)
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone();
        *slot.lock() = Some(DecodedVideoFrame {
            width,
            height,
            bgra,
            generation: gen,
            frame_id,
            fps_estimate,
        });
    }

    pub fn update_local_preview(&self, width: u32, height: u32, bgra: Vec<u8>, fps: u32) {
        let gen = self.next_generation.fetch_add(1, Ordering::Relaxed);
        *self.local_preview.lock() = Some(DecodedVideoFrame {
            width,
            height,
            bgra,
            generation: gen,
            frame_id: gen as u32,
            fps_estimate: fps,
        });
    }

    pub fn clear_local_preview(&self) {
        *self.local_preview.lock() = None;
    }

    pub fn remove_peer(&self, peer_id: PeerId) {
        self.peers.remove(&peer_id);
    }

    pub fn clear_all(&self) {
        self.peers.clear();
        self.clear_local_preview();
    }

    pub fn latest_peer_frame(&self, peer_id: PeerId) -> Option<DecodedVideoFrame> {
        self.peers.get(&peer_id).and_then(|s| s.lock().clone())
    }

    pub fn latest_peer_generation(&self, peer_id: PeerId) -> u64 {
        self.peers
            .get(&peer_id)
            .and_then(|s| s.lock().as_ref().map(|f| f.generation))
            .unwrap_or(0)
    }

    pub fn latest_local_preview(&self) -> Option<DecodedVideoFrame> {
        self.local_preview.lock().clone()
    }

    pub fn latest_local_generation(&self) -> u64 {
        self.local_preview
            .lock()
            .as_ref()
            .map(|f| f.generation)
            .unwrap_or(0)
    }
}

struct PeerDecoderHandle {
    tx: mpsc::Sender<(VideoFrameHeader, Bytes)>,
    cancel: CancellationToken,
}

/// Manages bounded per-peer H.264 decoder workers and routes Opus audio into `PeerAudioMixer`.
pub struct PeerMediaReceiver {
    video_store: Arc<PeerVideoStore>,
    audio_mixer: PeerAudioMixer,
    decoders: Mutex<HashMap<PeerId, PeerDecoderHandle>>,
    sfu_handle: Mutex<Option<SfuClient>>,
    on_frame_notify: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    feedback_state: Mutex<HashMap<PeerId, (u32, u32, Instant)>>,
}

impl PeerMediaReceiver {
    pub fn new(video_store: Arc<PeerVideoStore>, audio_mixer: PeerAudioMixer) -> Arc<Self> {
        Arc::new(Self {
            video_store,
            audio_mixer,
            decoders: Mutex::new(HashMap::new()),
            sfu_handle: Mutex::new(None),
            on_frame_notify: Mutex::new(None),
            feedback_state: Mutex::new(HashMap::new()),
        })
    }

    pub fn attach_sfu(&self, sfu: Option<SfuClient>) {
        *self.sfu_handle.lock() = sfu;
    }

    pub fn set_frame_notifier(&self, cb: Arc<dyn Fn() + Send + Sync>) {
        *self.on_frame_notify.lock() = Some(cb);
    }

    pub fn stop_peer(&self, peer_id: PeerId) {
        if let Some(handle) = self.decoders.lock().remove(&peer_id) {
            handle.cancel.cancel();
        }
        self.video_store.remove_peer(peer_id);
        self.audio_mixer.remove_peer(peer_id);
    }

    pub fn stop_all(&self) {
        let mut guard = self.decoders.lock();
        for (_, handle) in guard.drain() {
            handle.cancel.cancel();
        }
        self.video_store.clear_all();
        self.audio_mixer.clear_all();
    }

    fn get_or_spawn_decoder(&self, peer_id: PeerId) -> mpsc::Sender<(VideoFrameHeader, Bytes)> {
        let mut guard = self.decoders.lock();
        if let Some(existing) = guard.get(&peer_id) {
            return existing.tx.clone();
        }

        let (tx, mut rx) = mpsc::channel::<(VideoFrameHeader, Bytes)>(DECODER_QUEUE_CAPACITY);
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let store = Arc::clone(&self.video_store);
        let notifier = self.on_frame_notify.lock().clone();
        let sfu_for_kf = self.sfu_handle.lock().clone();

        std::thread::Builder::new()
            .name(format!("h264-dec-{}", &peer_id.to_string()[..8]))
            .spawn(move || {
                let mut decoder = match H264VideoDecoder::new() {
                    Ok(d) => d,
                    Err(err) => {
                        warn!(%peer_id, "Failed creating H264 decoder: {err}");
                        return;
                    }
                };
                let mut fps_win_start = Instant::now();
                let mut fps_frames = 0u32;
                let mut current_fps = 0u32;
                let mut needs_keyframe = false;
                let mut last_decode_fb = Instant::now() - Duration::from_secs(1);

                while !worker_cancel.is_cancelled() {
                    let Some((header, payload)) = rx.blocking_recv() else {
                        break;
                    };

                    // Drain backlog cleanly if behind, keeping only the newest keyframe if available.
                    let mut latest = (header, payload);
                    while let Ok((next_hdr, next_pay)) = rx.try_recv() {
                        let is_kf = (next_hdr.flags & VIDEO_FLAG_KEYFRAME) != 0;
                        if is_kf || !needs_keyframe {
                            latest = (next_hdr, next_pay);
                        }
                    }

                    let (hdr, pay) = latest;
                    let is_kf = (hdr.flags & VIDEO_FLAG_KEYFRAME) != 0;
                    if needs_keyframe && !is_kf {
                        continue;
                    }

                    match decoder.decode_annex_b_bgra(&pay) {
                        Ok(Some((w, h, bgra))) => {
                            needs_keyframe = false;
                            fps_frames += 1;
                            let elapsed = fps_win_start.elapsed();
                            if elapsed >= Duration::from_secs(1) {
                                current_fps = ((f64::from(fps_frames) / elapsed.as_secs_f64()).round()
                                    as u32)
                                    .max(1);
                                fps_frames = 0;
                                fps_win_start = Instant::now();
                            }
                            store.update_peer(
                                peer_id,
                                w,
                                h,
                                bgra,
                                hdr.frame_seq,
                                current_fps,
                            );
                            if let Some(cb) = &notifier {
                                cb();
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            debug!(%peer_id, "H.264 decode error (requesting IDR): {err}");
                            if !needs_keyframe {
                                needs_keyframe = true;
                                if let Some(sfu) = &sfu_for_kf {
                                    sfu.request_keyframe(peer_id);
                                }
                            }
                            if last_decode_fb.elapsed() >= Duration::from_millis(500) {
                                last_decode_fb = Instant::now();
                                if let Some(sfu) = &sfu_for_kf {
                                    sfu.send_viewer_feedback(peer_id, 0, 1);
                                }
                            }
                        }
                    }
                }
            })
            .ok();

        guard.insert(
            peer_id,
            PeerDecoderHandle {
                tx: tx.clone(),
                cancel,
            },
        );
        tx
    }
}

impl PeerMediaReceiver {
    fn note_viewer_drop_or_error(&self, broadcaster_id: PeerId, queue_drop: bool, decode_err: bool) {
        let now = Instant::now();
        let mut to_send: Option<(u32, u32)> = None;
        {
            let mut guard = self.feedback_state.lock();
            let entry = guard
                .entry(broadcaster_id)
                .or_insert_with(|| (0, 0, now - Duration::from_secs(1)));
            if queue_drop {
                entry.0 = entry.0.saturating_add(1);
            }
            if decode_err {
                entry.1 = entry.1.saturating_add(1);
            }
            if (entry.0 > 0 || entry.1 > 1) && now.duration_since(entry.2) >= Duration::from_millis(500) {
                to_send = Some((entry.0, entry.1));
                entry.0 = 0;
                entry.1 = 0;
                entry.2 = now;
            }
        }
        if let Some((q, d)) = to_send {
            if let Some(sfu) = self.sfu_handle.lock().as_ref() {
                sfu.send_viewer_feedback(broadcaster_id, q, d);
            }
        }
    }
}

impl MediaSink for PeerMediaReceiver {
    fn on_video_frame(&self, header: VideoFrameHeader, payload: Bytes) {
        let sender = self.get_or_spawn_decoder(header.sender_id);
        if let Err(mpsc::error::TrySendError::Full(_)) = sender.try_send((header, payload)) {
            if let Some(sfu) = self.sfu_handle.lock().as_ref() {
                sfu.request_keyframe(header.sender_id);
            }
            self.note_viewer_drop_or_error(header.sender_id, true, false);
        }
    }

    fn on_audio_packet(&self, header: AudioPacketHeader, payload: Bytes) {
        self.audio_mixer
            .push_opus_packet(header.sender_id, header.audio_seq, &payload);
    }
}

/// Manages active local screen & audio capture, encoding, adaptive bitrate, and SFU upload.
pub struct LocalBroadcastSession {
    cancel: CancellationToken,
    stats: Arc<Mutex<BroadcastStats>>,
    preview_enabled: Arc<AtomicBool>,
    stopped_manually: Arc<AtomicBool>,
    unexpected_stop_reason: Arc<Mutex<Option<String>>>,
}

impl LocalBroadcastSession {
    pub fn start(
        sfu: SfuClient,
        config: StreamConfig,
        video_store: Arc<PeerVideoStore>,
        on_preview_notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> anyhow::Result<Self> {
        Self::start_with_capturers(
            sfu,
            config,
            Arc::new(NativeVideoCapturer::new()),
            default_os_audio_capturer(),
            video_store,
            on_preview_notify,
        )
    }

    pub fn start_with_capturers(
        sfu: SfuClient,
        config: StreamConfig,
        video_capturer: Arc<dyn VideoCapturer>,
        audio_capturer: Arc<dyn AudioCapturer>,
        video_store: Arc<PeerVideoStore>,
        on_preview_notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> anyhow::Result<Self> {
        let cancel = CancellationToken::new();
        let preview_enabled = Arc::new(AtomicBool::new(false));
        let stopped_manually = Arc::new(AtomicBool::new(false));
        let unexpected_stop_reason = Arc::new(Mutex::new(None));
        let clamped_fps = sanitize_video_fps(config.fps);
        let initial_range = vbr_bitrate_range(config.resolution, clamped_fps);
        let initial_bitrate = initial_range.initial_bps();
        let initial_max_bitrate = initial_range.max_bps;
        let stats = Arc::new(Mutex::new(BroadcastStats {
            backend: "initializing".to_string(),
            width: 0,
            height: 0,
            encoded_fps: 0,
            configured_fps: u32::from(clamped_fps),
            effective_fps: u32::from(clamped_fps),
            target_bitrate_kbps: initial_bitrate / 1000,
            max_bitrate_kbps: initial_max_bitrate / 1000,
            throttle_reason: ThrottleReason::None,
            dropped_frames: 0,
            warning: None,
        }));

        let (video_tx, mut video_rx) = mpsc::channel::<RawVideoFrame>(3);
        let (audio_tx, mut audio_rx) = mpsc::channel::<RawAudioFrame>(16);
        let (warning_tx, mut warning_rx) = mpsc::unbounded_channel::<String>();

        let encoder = create_best_video_encoder(clamped_fps, initial_bitrate, initial_max_bitrate)?;
        let backend_name = encoder.backend_name().to_string();
        stats.lock().backend = backend_name.clone();

        video_capturer.start_capture(
            config.target.clone(),
            config.resolution,
            clamped_fps,
            video_tx,
            cancel.clone(),
        )?;

        let has_audio = !matches!(config.audio_mode, AudioCaptureMode::None);
        if has_audio {
            audio_capturer.start_capture_with_warnings(
                config.audio_mode,
                audio_tx,
                Some(warning_tx),
                cancel.clone(),
            )?;
        }

        // Metadata announcement
        sfu.start_broadcast(StreamMetadata {
            width: 1920,
            height: 1080,
            fps: clamped_fps,
            has_audio,
            codec_name: backend_name,
        });

        let video_seq = Arc::new(AtomicU32::new(1));
        let audio_seq = Arc::new(AtomicU32::new(1));

        // Warning watcher
        {
            let cancel_w = cancel.clone();
            let stats_w = Arc::clone(&stats);
            tokio::spawn(async move {
                while !cancel_w.is_cancelled() {
                    tokio::select! {
                        _ = cancel_w.cancelled() => break,
                        maybe = warning_rx.recv() => {
                            let Some(w) = maybe else { break };
                            stats_w.lock().warning = Some(w);
                        }
                    }
                }
            });
        }

        // Dedicated video encoding + adaptive control + QUIC UniStream dispatch thread
        {
            let cancel_v = cancel.clone();
            let sfu_v = sfu.clone();
            let seq_v = Arc::clone(&video_seq);
            let stats_v = Arc::clone(&stats);
            let store_v = Arc::clone(&video_store);
            let preview_enabled_v = Arc::clone(&preview_enabled);
            let stopped_manually_v = Arc::clone(&stopped_manually);
            let unexpected_stop_v = Arc::clone(&unexpected_stop_reason);
            let stop_notice = match config.target {
                CaptureTarget::Window { .. } => {
                    "Transmissão encerrada: a janela ou processo capturado foi fechado."
                }
                CaptureTarget::Monitor { .. } | CaptureTarget::PortalWayland => {
                    "Transmissão encerrada: a captura de tela foi finalizada."
                }
            };
            let config_resolution = config.resolution;
            let announce_fps = clamped_fps;

            std::thread::Builder::new()
                .name("video-encode-worker".to_string())
                .spawn(move || {
                    let mut encoder = encoder;
                    let mut controller = AdaptiveBitrateController::new(
                        config_resolution,
                        announce_fps,
                        Instant::now(),
                    );
                    let mut last_dims = (0u32, 0u32);
                    let mut fps_win = Instant::now();
                    let mut frames_in_win = 0u32;
                    let mut last_stats_sync = Instant::now();
                    let mut last_encoded_at: Option<Instant> = None;
                    let mut last_preview_update = Instant::now() - Duration::from_secs(1);
                    let mut pending_external_kf = false;
                    let mut pending_gop_recovery = false;
                    let mut dropped_frames: u64 = 0;

                    while !cancel_v.is_cancelled() {
                        let Some(mut frame) = video_rx.blocking_recv() else {
                            if !stopped_manually_v.load(Ordering::Acquire)
                                && !cancel_v.is_cancelled()
                            {
                                let mut slot = unexpected_stop_v.lock();
                                if slot.is_none() {
                                    *slot = Some(stop_notice.to_string());
                                }
                                drop(slot);
                                cancel_v.cancel();
                                sfu_v.stop_broadcast();
                                info!("Broadcast terminated automatically after capture stream ended");
                            }
                            break;
                        };
                        while let Ok(newer) = video_rx.try_recv() {
                            frame = newer;
                        }

                        if sfu_v.take_force_keyframe() {
                            pending_external_kf = true;
                        }

                        // Pre-encode FPS pacing when effective FPS was stepped down dynamically by network
                        let eff_fps = controller.effective_fps();
                        if eff_fps < announce_fps && !pending_external_kf && !pending_gop_recovery {
                            let target_interval =
                                Duration::from_micros(1_000_000 / u64::from(eff_fps.max(1)));
                            if last_encoded_at.is_some_and(|t| {
                                t.elapsed() + Duration::from_millis(2) < target_interval
                            }) {
                                continue;
                            }
                        }

                        if last_dims != (frame.width, frame.height) && frame.width >= 2 {
                            let is_first = last_dims == (0, 0);
                            last_dims = (frame.width, frame.height);
                            if is_first {
                                if controller.apply_source_resolution(
                                    config_resolution,
                                    frame.width,
                                    frame.height,
                                ) {
                                    let _ = encoder
                                        .set_bitrate(controller.current_bps(), controller.vbr_max_bps());
                                }
                                let mut st = stats_v.lock();
                                st.width = frame.width;
                                st.height = frame.height;
                                st.target_bitrate_kbps = controller.current_bps() / 1000;
                                st.max_bitrate_kbps = controller.max_bps() / 1000;
                            }
                            sfu_v.start_broadcast(StreamMetadata {
                                width: frame.width,
                                height: frame.height,
                                fps: announce_fps,
                                has_audio,
                                codec_name: encoder.backend_name().to_string(),
                            });
                        }

                        let telemetry_pre = sfu_v.path_telemetry();
                        if pending_gop_recovery {
                            // Wait until in-flight congestion subsides before encoding a clean IDR
                            if telemetry_pre.in_flight_video > 3 {
                                dropped_frames = dropped_frames.saturating_add(1);
                                let hint = sfu_v.sfu_bitrate_hint();
                                let decision = controller.step(
                                    AdaptiveInputs {
                                        telemetry: telemetry_pre,
                                        sfu_target_max_bps: hint.and_then(|h| h.target_max_bps),
                                        sfu_min_fps_cap: hint.and_then(|h| h.min_fps_cap),
                                        sfu_source: hint
                                            .map_or(ThrottleReason::None, |h| h.congestion_source),
                                        frame_sent: false,
                                    },
                                    Instant::now(),
                                );
                                if decision.bitrate_changed {
                                    let _ = encoder
                                        .set_bitrate(decision.target_bps, decision.vbr_max_bps);
                                }
                                if decision.fps_changed {
                                    let _ = encoder.set_target_fps(decision.effective_fps);
                                }
                                continue;
                            }
                            pending_external_kf = true;
                        }

                        if pending_external_kf {
                            encoder.request_keyframe();
                            pending_external_kf = false;
                        }

                        match encoder.encode_frame(&frame) {
                            Ok(annex_b) if !annex_b.is_empty() => {
                                last_encoded_at = Some(Instant::now());

                                let seq = seq_v.fetch_add(1, Ordering::Relaxed);
                                let is_kf = is_h264_keyframe(&annex_b);
                                let flags = VIDEO_FLAG_H264
                                    | if is_kf { VIDEO_FLAG_KEYFRAME } else { 0 };
                                let hdr = VideoFrameHeader {
                                    sender_id: sfu_v.local_peer_id(),
                                    frame_seq: seq,
                                    timestamp_us: frame.timestamp_us,
                                    width: frame.width as u16,
                                    height: frame.height as u16,
                                    flags,
                                };

                                let frame_sent = match sfu_v.send_video_frame(&hdr, &annex_b) {
                                    Ok(true) => {
                                        if pending_gop_recovery && is_kf {
                                            pending_gop_recovery = false;
                                        }
                                        true
                                    }
                                    Ok(false) => {
                                        dropped_frames = dropped_frames.saturating_add(1);
                                        pending_gop_recovery = true;
                                        false
                                    }
                                    Err(err) => {
                                        debug!("Video QUIC send stopped: {err}");
                                        break;
                                    }
                                };

                                let telemetry_now = sfu_v.path_telemetry();
                                let hint = sfu_v.sfu_bitrate_hint();
                                let decision = controller.step(
                                    AdaptiveInputs {
                                        telemetry: telemetry_now,
                                        sfu_target_max_bps: hint.and_then(|h| h.target_max_bps),
                                        sfu_min_fps_cap: hint.and_then(|h| h.min_fps_cap),
                                        sfu_source: hint
                                            .map_or(ThrottleReason::None, |h| h.congestion_source),
                                        frame_sent,
                                    },
                                    Instant::now(),
                                );

                                if decision.bitrate_changed {
                                    let _ = encoder
                                        .set_bitrate(decision.target_bps, decision.vbr_max_bps);
                                }
                                if decision.fps_changed {
                                    let _ = encoder.set_target_fps(decision.effective_fps);
                                }

                                // Update local preview at ~15fps max only when broadcaster opted in
                                if preview_enabled_v.load(Ordering::Acquire)
                                    && last_preview_update.elapsed() >= Duration::from_millis(66)
                                {
                                    last_preview_update = Instant::now();
                                    let preview_bgra = if frame.is_bgra {
                                        frame.rgba
                                    } else {
                                        let mut b = frame.rgba;
                                        rgba_to_bgra_in_place(&mut b);
                                        b
                                    };
                                    store_v.update_local_preview(
                                        frame.width,
                                        frame.height,
                                        preview_bgra,
                                        stats_v.lock().encoded_fps,
                                    );
                                    if let Some(cb) = &on_preview_notify {
                                        cb();
                                    }
                                }

                                frames_in_win += 1;
                                let elapsed = fps_win.elapsed();
                                if elapsed >= Duration::from_secs(1) {
                                    let fps_now = ((f64::from(frames_in_win)
                                        / elapsed.as_secs_f64())
                                    .round() as u32)
                                        .max(1);
                                    let mut st = stats_v.lock();
                                    st.width = frame.width;
                                    st.height = frame.height;
                                    st.encoded_fps = fps_now;
                                    st.configured_fps = u32::from(decision.configured_fps);
                                    st.effective_fps = u32::from(decision.effective_fps);
                                    st.target_bitrate_kbps = decision.target_bps / 1000;
                                    st.max_bitrate_kbps = decision.max_bps / 1000;
                                    st.throttle_reason = decision.reason;
                                    st.dropped_frames = dropped_frames;
                                    frames_in_win = 0;
                                    fps_win = Instant::now();
                                    last_stats_sync = Instant::now();
                                } else if last_stats_sync.elapsed() >= Duration::from_millis(250) {
                                    let mut st = stats_v.lock();
                                    st.width = frame.width;
                                    st.height = frame.height;
                                    st.configured_fps = u32::from(decision.configured_fps);
                                    st.effective_fps = u32::from(decision.effective_fps);
                                    st.target_bitrate_kbps = decision.target_bps / 1000;
                                    st.max_bitrate_kbps = decision.max_bps / 1000;
                                    st.throttle_reason = decision.reason;
                                    st.dropped_frames = dropped_frames;
                                    last_stats_sync = Instant::now();
                                }
                            }
                            Ok(_) => {}
                            Err(err) => warn!("Video frame encode error: {err}"),
                        }
                    }
                    store_v.clear_local_preview();
                    info!("Local video encode worker stopped");
                })?;
        }

        // Opus audio encode + QUIC datagram sender task
        if has_audio {
            let cancel_a = cancel.clone();
            let sfu_a = sfu.clone();
            let seq_a = Arc::clone(&audio_seq);
            tokio::spawn(async move {
                let mut encoder = match OpusAudioEncoder::new() {
                    Ok(e) => e,
                    Err(err) => {
                        warn!("Opus encoder init failed: {err}");
                        return;
                    }
                };
                let mut ts_ms = 0u32;
                while !cancel_a.is_cancelled() {
                    tokio::select! {
                        _ = cancel_a.cancelled() => break,
                        maybe = audio_rx.recv() => {
                            let Some(frame) = maybe else { break };
                            match encoder.encode_frame(&frame) {
                                Ok(opus_bytes) => {
                                    let seq = seq_a.fetch_add(1, Ordering::Relaxed);
                                    let hdr = AudioPacketHeader {
                                        sender_id: sfu_a.local_peer_id(),
                                        audio_seq: seq,
                                        timestamp_ms: ts_ms,
                                        flags: AUDIO_FLAG_OPUS,
                                    };
                                    ts_ms = ts_ms.wrapping_add(20);
                                    if sfu_a.send_audio_packet(&hdr, &opus_bytes).is_err() {
                                        break;
                                    }
                                }
                                Err(err) => debug!("Opus encode warning: {err}"),
                            }
                        }
                    }
                }
            });
        }

        Ok(Self {
            cancel,
            stats,
            preview_enabled,
            stopped_manually,
            unexpected_stop_reason,
        })
    }

    pub fn set_local_preview_enabled(&self, enabled: bool) {
        self.preview_enabled.store(enabled, Ordering::Release);
    }

    pub fn stats_snapshot(&self) -> BroadcastStats {
        self.stats.lock().clone()
    }

    pub fn unexpected_stop_reason(&self) -> Option<String> {
        self.unexpected_stop_reason.lock().clone()
    }

    pub fn stop(&self, sfu: &SfuClient) {
        self.stopped_manually.store(true, Ordering::Release);
        self.cancel.cancel();
        sfu.stop_broadcast();
    }
}

impl Drop for LocalBroadcastSession {
    fn drop(&mut self) {
        self.stopped_manually.store(true, Ordering::Release);
        self.cancel.cancel();
    }
}
