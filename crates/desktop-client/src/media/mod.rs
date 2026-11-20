pub mod adaptive;
#[cfg(target_os = "linux")]
pub mod audio_lin;
pub mod audio_out;
#[cfg(target_os = "windows")]
pub mod audio_win;
#[cfg(target_os = "linux")]
pub mod capture_lin;
#[cfg(target_os = "windows")]
pub mod capture_win;
#[cfg(target_os = "windows")]
pub mod codec_win;
pub mod mock;
pub mod pipeline;
pub mod traits;
pub mod video;

use std::sync::Arc;

pub use audio_out::{OpusAudioDecoder, OpusAudioEncoder, PeerAudioMixer};
pub use pipeline::{
    BroadcastStats, DecodedVideoFrame, LocalBroadcastSession, PeerMediaReceiver, PeerVideoStore,
};
pub use traits::{AudioCapturer, MediaError, RawAudioFrame, RawVideoFrame, VideoCapturer};

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
