use std::f32::consts::TAU;
use std::time::{Duration, Instant};

use shared_protocol::{AudioCaptureMode, CaptureTarget, TargetResolution};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::media::traits::{
    AudioCapturer, MediaError, RawAudioFrame, RawVideoFrame, VideoCapturer,
};

/// Generates moving SMPTE color bars at configurable FPS and resolution for headless CI and E2E testing.
#[derive(Debug, Clone, Default)]
pub struct MockVideoCapturer {
    pub base_width: u32,
    pub base_height: u32,
}

impl MockVideoCapturer {
    pub fn new(base_width: u32, base_height: u32) -> Self {
        Self {
            base_width: base_width.max(64),
            base_height: base_height.max(64),
        }
    }

    /// Generates a single moving SMPTE color bar RGBA frame for `frame_idx`.
    pub fn generate_smpte_bars(width: u32, height: u32, frame_idx: u64) -> Vec<u8> {
        const BARS: [[u8; 4]; 7] = [
            [192, 192, 192, 255], // White/Gray
            [192, 192, 0, 255],   // Yellow
            [0, 192, 192, 255],   // Cyan
            [0, 192, 0, 255],     // Green
            [192, 0, 192, 255],   // Magenta
            [192, 0, 0, 255],     // Red
            [0, 0, 192, 255],     // Blue
        ];

        let w = width.max(2) as usize;
        let h = height.max(2) as usize;
        let mut rgba = vec![0u8; w * h * 4];
        let shift = (frame_idx as usize * 4) % w;

        for y in 0..h {
            for x in 0..w {
                let shifted_x = (x + shift) % w;
                let bar_idx = (shifted_x * BARS.len()) / w;
                let color = BARS[bar_idx.min(BARS.len() - 1)];
                let offset = (y * w + x) * 4;
                rgba[offset..offset + 4].copy_from_slice(&color);
            }
        }

        rgba
    }
}

impl VideoCapturer for MockVideoCapturer {
    fn start_capture(
        &self,
        _target: CaptureTarget,
        resolution: TargetResolution,
        fps: u8,
        tx: mpsc::Sender<RawVideoFrame>,
        cancel: CancellationToken,
    ) -> Result<(), MediaError> {
        let src_w = if self.base_width == 0 {
            640
        } else {
            self.base_width
        };
        let src_h = if self.base_height == 0 {
            360
        } else {
            self.base_height
        };
        let (width, height) = resolution.compute_dimensions(src_w, src_h, true);
        let target_fps = fps.clamp(1, 60) as u64;
        let frame_interval = Duration::from_micros(1_000_000 / target_fps);

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(frame_interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let start = Instant::now();
            let mut frame_idx = 0u64;

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        let rgba = Self::generate_smpte_bars(width, height, frame_idx);
                        frame_idx = frame_idx.wrapping_add(1);
                        let frame = RawVideoFrame {
                            width,
                            height,
                            rgba,
                            is_bgra: false,
                            timestamp_us: start.elapsed().as_micros() as u64,
                        };
                        if tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(())
    }
}

/// Generates a continuous 440Hz stereo sine wave in 20ms (960 samples/ch at 48kHz) frames.
#[derive(Debug, Clone, Default)]
pub struct MockAudioCapturer;

impl MockAudioCapturer {
    pub fn new() -> Self {
        Self
    }

    pub fn generate_sine_frame(phase: &mut f32, freq_hz: f32) -> RawAudioFrame {
        let mut samples = Vec::with_capacity(RawAudioFrame::TOTAL_SAMPLES_20MS);
        let phase_step = TAU * freq_hz / (RawAudioFrame::SAMPLE_RATE as f32);

        for _ in 0..RawAudioFrame::SAMPLES_PER_CHANNEL_20MS {
            let sample = (*phase).sin() * 0.25;
            samples.push(sample); // Left
            samples.push(sample); // Right
            *phase = (*phase + phase_step) % TAU;
        }

        RawAudioFrame {
            sample_rate: RawAudioFrame::SAMPLE_RATE,
            channels: RawAudioFrame::CHANNELS,
            samples,
        }
    }
}

impl AudioCapturer for MockAudioCapturer {
    fn start_capture(
        &self,
        mode: AudioCaptureMode,
        tx: mpsc::Sender<RawAudioFrame>,
        cancel: CancellationToken,
    ) -> Result<(), MediaError> {
        if matches!(mode, AudioCaptureMode::None) {
            return Ok(());
        }

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(20));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut phase = 0.0f32;

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        let frame = Self::generate_sine_frame(&mut phase, 440.0);
                        if tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(())
    }
}
