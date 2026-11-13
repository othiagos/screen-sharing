use std::collections::{HashMap, VecDeque};
use std::convert::TryInto;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use audiopus::{
    coder::{Decoder as OpusDecoder, Encoder as OpusEncoder},
    packet::Packet as OpusPacket,
    Application, Bitrate, Channels, MutSignals, SampleRate,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use shared_protocol::PeerId;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::media::traits::{MediaError, RawAudioFrame};

const FRAME_SAMPLES: usize = RawAudioFrame::TOTAL_SAMPLES_20MS;
const JITTER_TARGET_SAMPLES: usize = FRAME_SAMPLES * 2; // 40ms
const JITTER_MAX_SAMPLES: usize = FRAME_SAMPLES * 7; // 140ms soft-trim threshold
const JITTER_TRIM_TO_SAMPLES: usize = FRAME_SAMPLES * 3; // 60ms after trim
const MAX_PLC_FRAMES_PER_GAP: u32 = 4;

/// Opus audio encoder configured for 48kHz, Stereo, 20ms low-delay frames.
pub struct OpusAudioEncoder {
    encoder: OpusEncoder,
    out_buf: Vec<u8>,
}

impl OpusAudioEncoder {
    pub fn new() -> Result<Self, MediaError> {
        let mut encoder = OpusEncoder::new(
            SampleRate::Hz48000,
            Channels::Stereo,
            Application::LowDelay,
        )
        .map_err(|e| MediaError::Codec(format!("Opus encoder init failed: {e}")))?;
        let _ = encoder.set_bitrate(Bitrate::BitsPerSecond(96_000));
        Ok(Self {
            encoder,
            out_buf: vec![0u8; 4000],
        })
    }

    /// Encodes a 20ms stereo 48kHz `RawAudioFrame` (1,920 interleaved `f32` samples) into Opus.
    pub fn encode_frame(&mut self, frame: &RawAudioFrame) -> Result<Vec<u8>, MediaError> {
        if frame.samples.len() != FRAME_SAMPLES {
            return Err(MediaError::Codec(format!(
                "Opus 20ms stereo frame requires {FRAME_SAMPLES} f32 samples, got {}",
                frame.samples.len()
            )));
        }

        let len = self
            .encoder
            .encode_float(&frame.samples, &mut self.out_buf)
            .map_err(|e| MediaError::Codec(format!("Opus encode_float failed: {e}")))?;

        Ok(self.out_buf[..len].to_vec())
    }
}

/// Opus audio decoder configured for 48kHz, Stereo, 20ms frames with Packet Loss Concealment (PLC).
pub struct OpusAudioDecoder {
    decoder: OpusDecoder,
    pcm_out: Vec<f32>,
}

impl OpusAudioDecoder {
    pub fn new() -> Result<Self, MediaError> {
        let decoder = OpusDecoder::new(SampleRate::Hz48000, Channels::Stereo)
            .map_err(|e| MediaError::Codec(format!("Opus decoder init failed: {e}")))?;
        Ok(Self {
            decoder,
            pcm_out: vec![0.0f32; FRAME_SAMPLES],
        })
    }

    /// Decodes an Opus packet into 48kHz interleaved stereo `f32` PCM samples.
    pub fn decode_packet(&mut self, packet_bytes: &[u8]) -> Result<Vec<f32>, MediaError> {
        if packet_bytes.is_empty() {
            return Err(MediaError::Codec("Empty Opus packet".to_string()));
        }

        let packet: OpusPacket<'_> = packet_bytes
            .try_into()
            .map_err(|e| MediaError::Codec(format!("Invalid Opus packet slice: {e}")))?;
        let signals: MutSignals<'_, f32> = (&mut self.pcm_out[..])
            .try_into()
            .map_err(|e| MediaError::Codec(format!("Invalid Opus output slice: {e}")))?;

        let decoded_samples_per_ch = self
            .decoder
            .decode_float(Some(packet), signals, false)
            .map_err(|e| MediaError::Codec(format!("Opus decode_float failed: {e}")))?;

        let total = (decoded_samples_per_ch * (RawAudioFrame::CHANNELS as usize))
            .min(self.pcm_out.len());
        Ok(self.pcm_out[..total].to_vec())
    }

    /// Generates 20ms of Packet Loss Concealment (PLC) audio when a packet is dropped.
    pub fn decode_plc(&mut self) -> Result<Vec<f32>, MediaError> {
        let signals: MutSignals<'_, f32> = (&mut self.pcm_out[..])
            .try_into()
            .map_err(|e| MediaError::Codec(format!("Invalid Opus output slice: {e}")))?;

        let decoded_samples_per_ch = self
            .decoder
            .decode_float(None, signals, false)
            .map_err(|e| MediaError::Codec(format!("Opus PLC decode_float failed: {e}")))?;

        let total = (decoded_samples_per_ch * (RawAudioFrame::CHANNELS as usize))
            .min(self.pcm_out.len());
        Ok(self.pcm_out[..total].to_vec())
    }
}

struct PeerAudioTrack {
    decoder: OpusAudioDecoder,
    buffer: VecDeque<f32>,
    primed: bool,
    last_seq: Option<u32>,
    volume: f32,
    muted: bool,
    plc_generated: u64,
    decoded_packets: u64,
}

impl PeerAudioTrack {
    fn new(volume: f32, muted: bool) -> Result<Self, MediaError> {
        Ok(Self {
            decoder: OpusAudioDecoder::new()?,
            buffer: VecDeque::with_capacity(JITTER_MAX_SAMPLES),
            primed: false,
            last_seq: None,
            volume: volume.clamp(0.0, 2.0),
            muted,
            plc_generated: 0,
            decoded_packets: 0,
        })
    }

    fn push_packet(&mut self, seq: u32, payload: &[u8]) {
        if let Some(prev) = self.last_seq {
            if seq <= prev && prev.wrapping_sub(seq) < 1000 {
                // Late duplicate or reordered old packet
                return;
            }
            let missed = seq.saturating_sub(prev + 1);
            if missed > 0 && missed <= MAX_PLC_FRAMES_PER_GAP {
                for _ in 0..missed {
                    if let Ok(plc) = self.decoder.decode_plc() {
                        self.buffer.extend(plc);
                        self.plc_generated = self.plc_generated.wrapping_add(1);
                    }
                }
            } else if missed > MAX_PLC_FRAMES_PER_GAP {
                // Large discontinuity: reset pre-buffer cleanly
                self.buffer.clear();
                self.primed = false;
            }
        }
        self.last_seq = Some(seq);

        if let Ok(pcm) = self.decoder.decode_packet(payload) {
            self.buffer.extend(pcm);
            self.decoded_packets = self.decoded_packets.wrapping_add(1);
        }

        // Soft-trim oldest samples if drift/jitter grows above 140ms
        if self.buffer.len() > JITTER_MAX_SAMPLES {
            let to_drop = self.buffer.len() - JITTER_TRIM_TO_SAMPLES;
            // Drop whole stereo sample pairs
            let aligned = to_drop & !1;
            let _ = self.buffer.drain(..aligned);
        }

        if !self.primed && self.buffer.len() >= JITTER_TARGET_SAMPLES {
            self.primed = true;
        }
    }
}

#[derive(Default)]
struct MixerState {
    tracks: HashMap<PeerId, PeerAudioTrack>,
    saved_prefs: HashMap<PeerId, (f32, bool)>,
}

/// Multi-peer audio jitter-buffer mixer and `cpal` output engine.
#[derive(Clone)]
pub struct PeerAudioMixer {
    state: Arc<Mutex<MixerState>>,
    started: Arc<AtomicBool>,
    cancel: CancellationToken,
}

impl PeerAudioMixer {
    /// Creates an audio mixer that opens the OS output stream lazily on first incoming audio packet.
    pub fn new(cancel: CancellationToken) -> Self {
        Self {
            state: Arc::new(Mutex::new(MixerState::default())),
            started: Arc::new(AtomicBool::new(false)),
            cancel,
        }
    }

    pub fn set_peer_volume(&self, peer_id: PeerId, volume: f32) {
        let clamped = volume.clamp(0.0, 2.0);
        let mut st = self.state.lock();
        let prev_muted = st.saved_prefs.get(&peer_id).map(|p| p.1).unwrap_or(false);
        st.saved_prefs.insert(peer_id, (clamped, prev_muted));
        if let Some(track) = st.tracks.get_mut(&peer_id) {
            track.volume = clamped;
        }
    }

    pub fn set_peer_muted(&self, peer_id: PeerId, muted: bool) {
        let mut st = self.state.lock();
        let prev_vol = st.saved_prefs.get(&peer_id).map(|p| p.0).unwrap_or(1.0);
        st.saved_prefs.insert(peer_id, (prev_vol, muted));
        if let Some(track) = st.tracks.get_mut(&peer_id) {
            track.muted = muted;
        }
    }

    pub fn remove_peer(&self, peer_id: PeerId) {
        self.state.lock().tracks.remove(&peer_id);
    }

    pub fn clear_all(&self) {
        self.state.lock().tracks.clear();
    }

    /// Pushes an incoming encoded Opus packet into `peer_id`'s adaptive jitter buffer.
    pub fn push_opus_packet(&self, peer_id: PeerId, seq: u32, payload: &[u8]) {
        if self.cancel.is_cancelled() || payload.is_empty() {
            return;
        }
        self.ensure_started();

        let mut st = self.state.lock();
        if !st.tracks.contains_key(&peer_id) {
            let (vol, muted) = st.saved_prefs.get(&peer_id).copied().unwrap_or((1.0, false));
            match PeerAudioTrack::new(vol, muted) {
                Ok(track) => {
                    st.tracks.insert(peer_id, track);
                }
                Err(err) => {
                    warn!(%peer_id, "Failed initializing Opus decoder track: {err}");
                    return;
                }
            }
        }
        if let Some(track) = st.tracks.get_mut(&peer_id) {
            track.push_packet(seq, payload);
        }
    }

    /// Mixes up to `out_stereo.len()` interleaved stereo `f32` samples across all primed peers.
    pub fn mix_stereo_into(&self, out_stereo: &mut [f32]) {
        out_stereo.fill(0.0);
        let mut st = self.state.lock();
        for track in st.tracks.values_mut() {
            if !track.primed {
                continue;
            }
            if track.buffer.len() < 2 {
                track.primed = false;
                continue;
            }

            let gain = if track.muted { 0.0 } else { track.volume };
            let available = track.buffer.len().min(out_stereo.len()) & !1;
            for (dst, sample) in out_stereo[..available]
                .iter_mut()
                .zip(track.buffer.drain(..available))
            {
                if gain > 0.0 {
                    *dst += sample * gain;
                }
            }
            // Drain unread remainder matching out_stereo length if unaligned, or leave for next callback
            if track.buffer.is_empty() {
                track.primed = false;
            }
        }

        // Soft limiter prevents harsh digital clipping when multiple streams are loud
        for sample in out_stereo.iter_mut() {
            let v = *sample;
            if v > 0.95 || v < -0.95 {
                *sample = v.tanh();
            }
        }
    }

    /// Returns `(buffered_samples, primed, plc_generated, decoded_packets)` for `peer_id`.
    pub fn peer_audio_stats(&self, peer_id: PeerId) -> Option<(usize, bool, u64, u64)> {
        self.state
            .lock()
            .tracks
            .get(&peer_id)
            .map(|t| (t.buffer.len(), t.primed, t.plc_generated, t.decoded_packets))
    }

    /// Returns diagnostic buffered sample count and PLC frame count for `peer_id`.
    #[cfg(test)]
    pub fn track_diagnostics(&self, peer_id: PeerId) -> Option<(usize, bool, u64)> {
        self.state
            .lock()
            .tracks
            .get(&peer_id)
            .map(|t| (t.buffer.len(), t.primed, t.plc_generated))
    }

    fn ensure_started(&self) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let mixer = self.clone();
        let cancel = self.cancel.clone();
        let _ = std::thread::Builder::new()
            .name("cpal-audio-mixer".to_string())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_cpal_output_thread(mixer, cancel);
                }));
            });
    }
}

fn run_cpal_output_thread(mixer: PeerAudioMixer, cancel: CancellationToken) {
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        debug!("No default audio output device available (headless environment)");
        while !cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(50));
        }
        return;
    };

    let preferred = cpal::StreamConfig {
        channels: RawAudioFrame::CHANNELS,
        sample_rate: cpal::SampleRate(RawAudioFrame::SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    let (config, out_channels, src_rate) = match device.default_output_config() {
        Ok(def)
            if def.channels() != RawAudioFrame::CHANNELS
                || def.sample_rate().0 != RawAudioFrame::SAMPLE_RATE =>
        {
            let ch = def.channels().max(1);
            (
                cpal::StreamConfig {
                    channels: ch,
                    sample_rate: def.sample_rate(),
                    buffer_size: cpal::BufferSize::Default,
                },
                ch as usize,
                def.sample_rate().0,
            )
        }
        _ => (
            preferred,
            RawAudioFrame::CHANNELS as usize,
            RawAudioFrame::SAMPLE_RATE,
        ),
    };

    let mut scratch_stereo = Vec::<f32>::with_capacity(FRAME_SAMPLES * 2);
    let stream_res = device.build_output_stream(
        &config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            if out_channels == 2 && src_rate == RawAudioFrame::SAMPLE_RATE {
                mixer.mix_stereo_into(data);
                return;
            }

            let frames = data.len() / out_channels.max(1);
            let needed_stereo = ((frames as u64 * RawAudioFrame::SAMPLE_RATE as u64)
                / u64::from(src_rate.max(1))) as usize
                * 2;
            let needed_stereo = (needed_stereo.max(frames * 2)) & !1;
            scratch_stereo.resize(needed_stereo, 0.0);
            mixer.mix_stereo_into(&mut scratch_stereo);

            for (i, frame_out) in data.chunks_mut(out_channels).enumerate() {
                let src_frame = if src_rate == RawAudioFrame::SAMPLE_RATE {
                    i
                } else {
                    (i * RawAudioFrame::SAMPLE_RATE as usize) / src_rate.max(1) as usize
                };
                let base = (src_frame * 2).min(scratch_stereo.len().saturating_sub(2));
                let l = scratch_stereo.get(base).copied().unwrap_or(0.0);
                let r = scratch_stereo.get(base + 1).copied().unwrap_or(l);
                if out_channels == 1 {
                    frame_out[0] = (l + r) * 0.5;
                } else {
                    frame_out[0] = l;
                    frame_out[1] = r;
                    for extra in &mut frame_out[2..] {
                        *extra = 0.0;
                    }
                }
            }
        },
        move |err| {
            warn!("cpal output stream error: {err}");
        },
        None,
    );

    match stream_res {
        Ok(stream) => {
            if let Err(err) = stream.play() {
                warn!("cpal stream.play() failed: {err}");
                return;
            }
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Err(err) => {
            debug!("Could not open cpal output stream (headless/busy): {err}");
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::mock::MockAudioCapturer;

    #[test]
    fn test_opus_encode_decode_and_plc_roundtrip() {
        let mut encoder = OpusAudioEncoder::new().expect("init Opus encoder");
        let mut decoder = OpusAudioDecoder::new().expect("init Opus decoder");

        let mut phase = 0.0f32;
        let raw_frame = MockAudioCapturer::generate_sine_frame(&mut phase, 440.0);
        let opus_packet = encoder.encode_frame(&raw_frame).expect("encode Opus");
        let decoded = decoder.decode_packet(&opus_packet).expect("decode Opus");
        assert_eq!(decoded.len(), FRAME_SAMPLES);

        let plc = decoder.decode_plc().expect("decode PLC");
        assert_eq!(plc.len(), FRAME_SAMPLES);
    }

    #[test]
    fn test_peer_audio_mixer_jitter_plc_trim_and_volume_mute() {
        let cancel = CancellationToken::new();
        let mixer = PeerAudioMixer {
            state: Arc::new(Mutex::new(MixerState::default())),
            // Prevent opening OS hardware stream during deterministic mixer unit test
            started: Arc::new(AtomicBool::new(true)),
            cancel,
        };

        let mut enc = OpusAudioEncoder::new().unwrap();
        let mut phase = 0.0f32;
        let pkt0 = enc
            .encode_frame(&MockAudioCapturer::generate_sine_frame(&mut phase, 440.0))
            .unwrap();
        let pkt2 = enc
            .encode_frame(&MockAudioCapturer::generate_sine_frame(&mut phase, 440.0))
            .unwrap();

        let peer = PeerId::new_v4();
        mixer.push_opus_packet(peer, 0, &pkt0);
        // 20ms alone should not prime yet (40ms target)
        let diag0 = mixer.track_diagnostics(peer).unwrap();
        assert!(!diag0.1);

        // Skipping seq=1 triggers 1 PLC frame + pkt2 => 60ms buffered & primed
        mixer.push_opus_packet(peer, 2, &pkt2);
        let diag2 = mixer.track_diagnostics(peer).unwrap();
        assert!(diag2.1);
        assert_eq!(diag2.2, 1);

        // Mix out 20ms at 50% volume -> non-zero audio
        mixer.set_peer_volume(peer, 0.5);
        let mut out = vec![0.0f32; FRAME_SAMPLES];
        mixer.mix_stereo_into(&mut out);
        assert!(out.iter().any(|&s| s.abs() > 0.001));

        // Muting should output silence while still draining track smoothly
        mixer.set_peer_muted(peer, true);
        mixer.mix_stereo_into(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));

        // Pushing burst > 140ms triggers soft trim to <= 60ms
        for seq in 3..15 {
            let pkt = enc
                .encode_frame(&MockAudioCapturer::generate_sine_frame(&mut phase, 440.0))
                .unwrap();
            mixer.push_opus_packet(peer, seq, &pkt);
        }
        let diag_trim = mixer.track_diagnostics(peer).unwrap();
        assert!(diag_trim.0 <= JITTER_MAX_SAMPLES);
    }
}
