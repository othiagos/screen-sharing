use std::ops::Deref;
use std::sync::Arc;

use bytes::BytesMut;
use parking_lot::Mutex;
use shared_protocol::{AudioCaptureMode, CaptureTarget, TargetResolution};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_POOLED_FRAME_BUFFERS: usize = 4;

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

/// Thread-safe reusable pixel buffer pool to eliminate per-frame 8+ MB heap allocations.
#[derive(Debug, Default)]
pub struct FrameBufferPool {
    buffers: Mutex<Vec<Vec<u8>>>,
}

impl FrameBufferPool {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Takes a recycled buffer sized exactly to `len` (allocating only when empty or resized).
    pub fn take_with_len(&self, len: usize) -> Vec<u8> {
        let mut guard = self.buffers.lock();
        if let Some(pos) = guard.iter().position(|b| b.len() == len) {
            return guard.swap_remove(pos);
        }
        if let Some(mut buf) = guard.pop() {
            buf.resize(len, 0);
            return buf;
        }
        drop(guard);
        vec![0u8; len]
    }

    /// Wraps an owned `Vec<u8>` in a `SharedFrameBuffer` bound to this pool.
    pub fn wrap(self: &Arc<Self>, buf: Vec<u8>) -> SharedFrameBuffer {
        SharedFrameBuffer {
            data: Some(Arc::new(buf)),
            pool: Some(Arc::clone(self)),
        }
    }

    fn recycle(&self, buf: Vec<u8>) {
        if buf.is_empty() {
            return;
        }
        let mut guard = self.buffers.lock();
        if guard.first().is_some_and(|first| first.len() != buf.len()) {
            guard.clear();
        }
        if guard.len() < MAX_POOLED_FRAME_BUFFERS {
            guard.push(buf);
        }
    }
}

/// Zero-copy shareable frame pixel buffer with automatic recycling back to `FrameBufferPool`.
#[derive(Debug)]
pub struct SharedFrameBuffer {
    data: Option<Arc<Vec<u8>>>,
    pool: Option<Arc<FrameBufferPool>>,
}

impl SharedFrameBuffer {
    pub fn from_vec(vec: Vec<u8>) -> Self {
        Self {
            data: Some(Arc::new(vec)),
            pool: None,
        }
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        self.data.as_deref().map_or(&[], Vec::as_slice)
    }

    /// Consumes this handle and returns an owned `Vec<u8>`, moving zero-copy if uniquely owned.
    pub fn into_vec_or_clone(mut self) -> Vec<u8> {
        let _ = self.pool.take();
        match self.data.take() {
            Some(arc) => Arc::try_unwrap(arc).unwrap_or_else(|shared| (*shared).clone()),
            None => Vec::new(),
        }
    }
}

impl Clone for SharedFrameBuffer {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            pool: self.pool.clone(),
        }
    }
}

impl Drop for SharedFrameBuffer {
    fn drop(&mut self) {
        if let (Some(arc), Some(pool)) = (self.data.take(), self.pool.take()) {
            if let Ok(vec) = Arc::try_unwrap(arc) {
                pool.recycle(vec);
            }
        }
    }
}

impl Deref for SharedFrameBuffer {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl AsRef<[u8]> for SharedFrameBuffer {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl From<Vec<u8>> for SharedFrameBuffer {
    fn from(vec: Vec<u8>) -> Self {
        Self::from_vec(vec)
    }
}

/// Raw video frame produced by a `VideoCapturer` (RGBA or BGRA pixel order).
#[derive(Debug, Clone)]
pub struct RawVideoFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: SharedFrameBuffer,
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
    /// Starts capturing frames at `fps` and `resolution`, sending scaled frames to `tx`
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

    /// Starts capturing audio and surfaces non-fatal fallback notices via `warning_tx`.
    fn start_capture_with_warnings(
        &self,
        mode: AudioCaptureMode,
        tx: mpsc::Sender<RawAudioFrame>,
        _warning_tx: Option<mpsc::UnboundedSender<String>>,
        cancel: CancellationToken,
    ) -> Result<(), MediaError> {
        self.start_capture(mode, tx, cancel)
    }
}

/// Uniform H.264 encoder interface implemented by hardware MFT and tuned OpenH264.
pub trait VideoEncoder: Send + 'static {
    /// Appends encoded H.264 Annex-B bytes directly into `out_annex_b` and returns `true` if IDR/keyframe.
    fn encode_frame_into(
        &mut self,
        frame: &RawVideoFrame,
        out_annex_b: &mut BytesMut,
    ) -> Result<bool, MediaError>;
    fn request_keyframe(&mut self);
    fn set_bitrate(&mut self, target_bps: u32, max_bps: u32) -> Result<(), MediaError>;
    fn set_target_fps(&mut self, _fps: u8) -> Result<(), MediaError> {
        Ok(())
    }
    fn backend_name(&self) -> &'static str;
}

/// Uniform H.264 decoder outputting tightly-packed BGRA pixels for direct `RenderImage` upload.
pub trait VideoDecoder: Send + 'static {
    fn decode_annex_b_bgra(
        &mut self,
        annex_b: &[u8],
    ) -> Result<Option<(u32, u32, Vec<u8>)>, MediaError>;
    fn backend_name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_buffer_pool_recycling() {
        let pool = FrameBufferPool::new();
        let len = 640 * 360 * 4;
        let vec = pool.take_with_len(len);
        let ptr_before = vec.as_ptr();
        let shared = pool.wrap(vec);

        // O(1) clone shares the exact same backing allocation without copying
        let cloned = shared.clone();
        assert_eq!(shared.as_ptr(), ptr_before);
        assert_eq!(cloned.as_ptr(), ptr_before);

        // Dropping one handle keeps the buffer alive; dropping the last returns it to the pool
        drop(shared);
        assert!(pool.buffers.lock().is_empty());
        drop(cloned);
        assert_eq!(pool.buffers.lock().len(), 1);

        let reused = pool.take_with_len(len);
        assert_eq!(reused.as_ptr(), ptr_before);
        assert_eq!(reused.len(), len);

        // Changing resolution clears stale-sized buffers instead of hoarding memory
        let small = pool.wrap(vec![1u8; 256]);
        drop(small);
        assert_eq!(pool.buffers.lock().len(), 1);
        let big = pool.wrap(reused);
        drop(big);
        assert_eq!(pool.buffers.lock().len(), 1);
        assert_eq!(pool.buffers.lock()[0].len(), len);
    }
}
