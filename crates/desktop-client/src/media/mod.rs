#[cfg(target_os = "linux")]
pub mod audio_lin;
#[cfg(target_os = "windows")]
pub mod audio_win;
pub mod mock;
pub mod traits;
pub mod video;

use std::convert::TryInto;
use std::sync::Arc;
use std::time::Duration;

use audiopus::{
    coder::{Decoder as OpusDecoder, Encoder as OpusEncoder},
    packet::Packet as OpusPacket,
    Application, Bitrate, Channels, MutSignals, SampleRate,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use ringbuf::{traits::*, HeapRb};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::media::traits::{AudioCapturer, MediaError, RawAudioFrame};

/// Opus audio encoder configured for 48kHz, Stereo, 20ms low-delay frames (960 samples/channel).
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

    /// Encodes a 20ms stereo 48kHz `RawAudioFrame` (1,920 interleaved `f32` samples) into an Opus packet.
    pub fn encode_frame(&mut self, frame: &RawAudioFrame) -> Result<Vec<u8>, MediaError> {
        if frame.samples.len() != RawAudioFrame::TOTAL_SAMPLES_20MS {
            return Err(MediaError::Codec(format!(
                "Opus 20ms stereo frame requires {} f32 samples, got {}",
                RawAudioFrame::TOTAL_SAMPLES_20MS,
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

/// Opus audio decoder configured for 48kHz, Stereo, 20ms frames.
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
            pcm_out: vec![0.0f32; RawAudioFrame::TOTAL_SAMPLES_20MS],
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
}

/// Low-latency native audio output sink powered by `cpal` and a lock-free `ringbuf` SPSC buffer.
/// Lazily initializes `cpal::Stream` on a dedicated audio thread only when audio samples are pushed.
#[derive(Clone)]
pub struct CpalAudioPlayer {
    producer: Arc<Mutex<ringbuf::HeapProd<f32>>>,
    consumer: Arc<Mutex<Option<ringbuf::HeapCons<f32>>>>,
    cancel: CancellationToken,
}

impl CpalAudioPlayer {
    /// Creates a new native `cpal` audio output sink without opening the hardware device until needed.
    pub fn new(cancel: CancellationToken) -> Self {
        // Buffer up to 400ms of 48kHz stereo audio (38,400 samples)
        let rb = HeapRb::<f32>::new(RawAudioFrame::TOTAL_SAMPLES_20MS * 20);
        let (prod, cons) = rb.split();
        Self {
            producer: Arc::new(Mutex::new(prod)),
            consumer: Arc::new(Mutex::new(Some(cons))),
            cancel,
        }
    }

    fn ensure_started(&self) {
        let maybe_cons = self.consumer.lock().take();
        let Some(mut cons) = maybe_cons else {
            return;
        };

        let thread_cancel = self.cancel.clone();
        let _ = std::thread::Builder::new()
            .name("cpal-audio-output".to_string())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let host = cpal::default_host();
                    let Some(device) = host.default_output_device() else {
                        debug!("No default audio output device available (headless environment)");
                        while !thread_cancel.is_cancelled() {
                            let _ = cons.clear();
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        return;
                    };

                    let preferred_config = cpal::StreamConfig {
                        channels: RawAudioFrame::CHANNELS,
                        sample_rate: cpal::SampleRate(RawAudioFrame::SAMPLE_RATE),
                        buffer_size: cpal::BufferSize::Default,
                    };

                    let (config, out_channels) = match device.default_output_config() {
                        Ok(default_cfg)
                            if default_cfg.channels() != RawAudioFrame::CHANNELS
                                || default_cfg.sample_rate().0 != RawAudioFrame::SAMPLE_RATE =>
                        {
                            let ch = default_cfg.channels().max(1);
                            (
                                cpal::StreamConfig {
                                    channels: ch,
                                    sample_rate: default_cfg.sample_rate(),
                                    buffer_size: cpal::BufferSize::Default,
                                },
                                ch as usize,
                            )
                        }
                        _ => (preferred_config, RawAudioFrame::CHANNELS as usize),
                    };

                    let stream_res = device.build_output_stream(
                        &config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            if out_channels == 2 {
                                let popped = cons.pop_slice(data);
                                for sample in &mut data[popped..] {
                                    *sample = 0.0;
                                }
                            } else {
                                for frame_out in data.chunks_mut(out_channels) {
                                    let l = cons.try_pop().unwrap_or(0.0);
                                    let r = cons.try_pop().unwrap_or(l);
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
                            while !thread_cancel.is_cancelled() {
                                std::thread::sleep(Duration::from_millis(50));
                            }
                        }
                        Err(err) => {
                            debug!("Could not initialize cpal output stream (headless/busy): {err}");
                            while !thread_cancel.is_cancelled() {
                                std::thread::sleep(Duration::from_millis(50));
                            }
                        }
                    }
                }));
            });
    }

    /// Pushes decoded 48kHz stereo `f32` samples into the lock-free playback ring buffer.
    pub fn push_samples(&self, samples: &[f32]) {
        if self.cancel.is_cancelled() || samples.is_empty() {
            return;
        }
        self.ensure_started();
        let mut guard = self.producer.lock();
        let _ = guard.push_slice(samples);
    }
}

/// Returns the default platform-specific `AudioCapturer` implementation.
pub fn default_os_audio_capturer() -> Arc<dyn AudioCapturer> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(audio_lin::PipeWireAudioCapturer::new())
    }
    #[cfg(target_os = "windows")]
    {
        Arc::new(audio_win::WindowsAudioCapturer::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::mock::MockAudioCapturer;

    #[test]
    fn test_opus_encode_decode_roundtrip() {
        let mut encoder = OpusAudioEncoder::new().expect("init Opus encoder");
        let mut decoder = OpusAudioDecoder::new().expect("init Opus decoder");

        let mut phase = 0.0f32;
        let raw_frame = MockAudioCapturer::generate_sine_frame(&mut phase, 440.0);
        assert_eq!(raw_frame.samples.len(), RawAudioFrame::TOTAL_SAMPLES_20MS);

        let opus_packet = encoder.encode_frame(&raw_frame).expect("encode Opus frame");
        assert!(!opus_packet.is_empty());

        let decoded_pcm = decoder
            .decode_packet(&opus_packet)
            .expect("decode Opus packet");
        assert_eq!(decoded_pcm.len(), RawAudioFrame::TOTAL_SAMPLES_20MS);
    }
}
