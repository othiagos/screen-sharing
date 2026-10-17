use shared_protocol::{AudioCaptureMode, CaptureTarget, TargetResolution};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("Capture source not found: {0}")]
    SourceNotFound(String),
    #[error("Video capture failed: {0}")]
    VideoCapture(String),
    #[error("Audio capture failed: {0}")]
    AudioCapture(String),
    #[error("Codec error: {0}")]
    Codec(String),
    #[error("Audio playback error: {0}")]
    AudioPlayback(String),
}

/// Raw video frame produced by a `VideoCapturer` (RGBA or BGRA pixel order).
#[derive(Debug, Clone)]
pub struct RawVideoFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub is_bgra: bool,
    pub timestamp_us: u64,
}

/// 20ms stereo PCM frame at 48,000 Hz (960 samples per channel = 1,920 interleaved `f32` samples).
#[derive(Debug, Clone)]
pub struct RawAudioFrame {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

impl RawAudioFrame {
    pub const SAMPLE_RATE: u32 = 48_000;
    pub const CHANNELS: u16 = 2;
    pub const SAMPLES_PER_CHANNEL_20MS: usize = 960;
    pub const TOTAL_SAMPLES_20MS: usize =
        Self::SAMPLES_PER_CHANNEL_20MS * (Self::CHANNELS as usize);
}

pub trait VideoCapturer: Send + Sync + 'static {
    /// Starts capturing frames at `fps` and `resolution`, sending scaled RGBA frames to `tx`
    /// until `cancel` is triggered.
    fn start_capture(
        &self,
        target: CaptureTarget,
        resolution: TargetResolution,
        fps: u8,
        tx: mpsc::Sender<RawVideoFrame>,
        cancel: CancellationToken,
    ) -> Result<(), MediaError>;
}

pub trait AudioCapturer: Send + Sync + 'static {
    /// Starts capturing 20ms 48kHz stereo PCM frames according to `mode`, sending them to `tx`
    /// until `cancel` is triggered.
    fn start_capture(
        &self,
        mode: AudioCaptureMode,
        tx: mpsc::Sender<RawAudioFrame>,
        cancel: CancellationToken,
    ) -> Result<(), MediaError>;
}
