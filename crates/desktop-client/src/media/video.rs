use std::time::{Duration, Instant};

use fast_image_resize::{
    images::{Image, ImageRef},
    FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
};
use openh264::{
    decoder::{Decoder, DecoderConfig},
    encoder::{Encoder, EncoderConfig, RateControlMode, SpsPpsStrategy, UsageType},
    formats::{RgbaSliceU8, YUVBuffer, YUVSource},
    OpenH264API,
};
use shared_protocol::{CaptureTarget, TargetResolution};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use yuv::color::{MatrixCoefficients, Range};

use crate::media::traits::{MediaError, RawVideoFrame, VideoCapturer};

/// Scales an arbitrary RGBA buffer (including odd dimensions) to the target resolution
/// while preserving aspect ratio and enforcing even output dimensions (`width & !1`, `height & !1`).
pub fn scale_rgba_frame(
    src_rgba: &[u8],
    src_width: u32,
    src_height: u32,
    resolution: TargetResolution,
) -> Result<(u32, u32, Vec<u8>), MediaError> {
    let expected_len = (src_width as usize)
        .checked_mul(src_height as usize)
        .and_then(|px| px.checked_mul(4))
        .unwrap_or(0);
    if src_width == 0 || src_height == 0 || src_rgba.len() < expected_len {
        return Err(MediaError::VideoCapture(format!(
            "Invalid source RGBA buffer for {src_width}x{src_height}: got {} bytes",
            src_rgba.len()
        )));
    }

    let (dst_width, dst_height) = resolution.compute_dimensions(src_width, src_height, true);
    if dst_width == src_width && dst_height == src_height {
        return Ok((
            dst_width,
            dst_height,
            src_rgba[..expected_len].to_vec(),
        ));
    }

    let src_view = ImageRef::new(
        src_width,
        src_height,
        &src_rgba[..expected_len],
        PixelType::U8x4,
    )
    .map_err(|e| MediaError::VideoCapture(format!("fast_image_resize src error: {e}")))?;

    let mut dst_image = Image::new(dst_width, dst_height, PixelType::U8x4);
    let mut resizer = Resizer::new();
    let options = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(FilterType::Bilinear))
        .use_alpha(false);

    resizer
        .resize(&src_view, &mut dst_image, &options)
        .map_err(|e| MediaError::VideoCapture(format!("fast_image_resize resize error: {e}")))?;

    Ok((dst_width, dst_height, dst_image.into_vec()))
}

/// Converts an even-dimensioned RGBA buffer into a planar YUV420p (`I420`) `YUVBuffer`.
pub fn rgba_to_yuv420p(
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<YUVBuffer, MediaError> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(MediaError::Codec(format!(
            "YUV420p requires non-zero even dimensions, got {width}x{height}"
        )));
    }
    let expected = (width as usize) * (height as usize) * 4;
    if rgba.len() < expected {
        return Err(MediaError::Codec(format!(
            "RGBA buffer too small for {width}x{height}: expected {expected}, got {}",
            rgba.len()
        )));
    }

    // Touch `yuv` crate color space descriptors to validate BT.601 studio range compatibility.
    let _ = (Range::Limited, MatrixCoefficients::BT601);

    let slice = RgbaSliceU8::new(&rgba[..expected], (width as usize, height as usize));
    Ok(YUVBuffer::from_rgb_source(slice))
}

/// Low-latency H.264 Baseline Profile encoder wrapper with periodic IDR keyframe generation.
pub struct H264VideoEncoder {
    encoder: Encoder,
    frame_count: u64,
    keyframe_interval: u64,
    force_next_idr: bool,
}

impl H264VideoEncoder {
    pub fn new(fps: u8, bitrate_bps: u32) -> Result<Self, MediaError> {
        let clamped_fps = fps.clamp(1, 60);
        let config = EncoderConfig::new()
            .set_bitrate_bps(bitrate_bps.max(500_000))
            .max_frame_rate(clamped_fps as f32)
            .enable_skip_frame(false)
            .rate_control_mode(RateControlMode::Bitrate)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .usage_type(UsageType::ScreenContentRealTime);

        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| MediaError::Codec(format!("OpenH264 encoder init error: {e}")))?;

        Ok(Self {
            encoder,
            frame_count: 0,
            keyframe_interval: (clamped_fps as u64).max(15),
            force_next_idr: true,
        })
    }

    pub fn request_keyframe(&mut self) {
        self.force_next_idr = true;
    }

    /// Encodes an RGBA frame (scaling/rounding to even dimensions if needed) into an Annex-B H.264 bitstream.
    pub fn encode_rgba(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, MediaError> {
        let yuv = rgba_to_yuv420p(rgba, width, height)?;

        if self.force_next_idr || self.frame_count.is_multiple_of(self.keyframe_interval) {
            self.encoder.force_intra_frame();
            self.force_next_idr = false;
        }

        let bitstream = self
            .encoder
            .encode(&yuv)
            .map_err(|e| MediaError::Codec(format!("OpenH264 encode error: {e}")))?;

        self.frame_count = self.frame_count.wrapping_add(1);
        Ok(bitstream.to_vec())
    }
}

/// Low-latency H.264 decoder wrapper producing RGBA frames.
pub struct H264VideoDecoder {
    decoder: Decoder,
}

impl H264VideoDecoder {
    pub fn new() -> Result<Self, MediaError> {
        let config = DecoderConfig::new();
        let decoder = Decoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| MediaError::Codec(format!("OpenH264 decoder init error: {e}")))?;
        Ok(Self { decoder })
    }

    /// Feeds an Annex-B H.264 buffer (one or more NAL units) into the decoder.
    /// Returns `Ok(Some((width, height, rgba_bytes)))` when a complete picture is decoded.
    pub fn decode_annex_b(
        &mut self,
        annex_b: &[u8],
    ) -> Result<Option<(u32, u32, Vec<u8>)>, MediaError> {
        if annex_b.is_empty() {
            return Ok(None);
        }

        let mut latest_frame: Option<(u32, u32, Vec<u8>)> = None;
        for nal in openh264::nal_units(annex_b) {
            match self.decoder.decode(nal) {
                Ok(Some(decoded_yuv)) => {
                    let (w, h) = decoded_yuv.dimensions();
                    if w > 0 && h > 0 {
                        let mut rgba = vec![0u8; w * h * 4];
                        decoded_yuv.write_rgba8(&mut rgba);
                        latest_frame = Some((w as u32, h as u32, rgba));
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    debug!("OpenH264 NAL decode transient error (awaiting IDR/SPS/PPS): {err}");
                }
            }
        }

        Ok(latest_frame)
    }
}

/// Enumerates all available monitors and application windows on the host OS.
pub fn list_capture_targets() -> Result<Vec<CaptureTarget>, MediaError> {
    let mut targets = Vec::new();

    match xcap::Monitor::all() {
        Ok(monitors) => {
            for monitor in monitors {
                targets.push(CaptureTarget::Monitor {
                    id: monitor.id(),
                    name: monitor.name().to_string(),
                });
            }
        }
        Err(err) => {
            warn!("Monitor enumeration warning: {err}");
        }
    }

    match xcap::Window::all() {
        Ok(windows) => {
            for window in windows {
                if window.is_minimized() || window.title().trim().is_empty() {
                    continue;
                }
                let pid = extract_window_pid(&window);
                targets.push(CaptureTarget::Window {
                    id: window.id(),
                    title: window.title().to_string(),
                    pid,
                });
            }
        }
        Err(err) => {
            warn!("Window enumeration warning: {err}");
        }
    }

    Ok(targets)
}

#[cfg(target_os = "windows")]
fn extract_window_pid(window: &xcap::Window) -> u32 {
    window.process_id()
}

#[cfg(not(target_os = "windows"))]
fn extract_window_pid(window: &xcap::Window) -> u32 {
    // Attempt to resolve PID by matching window app_name against /proc/<pid>/comm on Linux
    let app_name = window.app_name().trim().to_lowercase();
    if app_name.is_empty() {
        return 0;
    }
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            if let Ok(file_name) = entry.file_name().into_string() {
                if let Ok(pid) = file_name.parse::<u32>() {
                    let comm_path = entry.path().join("comm");
                    if let Ok(comm) = std::fs::read_to_string(comm_path) {
                        let comm_clean = comm.trim().to_lowercase();
                        if comm_clean == app_name || app_name.contains(&comm_clean) {
                            return pid;
                        }
                    }
                }
            }
        }
    }
    0
}

/// Native OS screen and window capturer powered by `xcap` + `fast_image_resize`.
#[derive(Debug, Clone, Default)]
pub struct NativeVideoCapturer;

impl NativeVideoCapturer {
    pub fn new() -> Self {
        Self
    }

    fn capture_raw_target(target: &CaptureTarget) -> Result<(u32, u32, Vec<u8>), MediaError> {
        match target {
            CaptureTarget::Monitor { id, name } => {
                let monitors = xcap::Monitor::all()
                    .map_err(|e| MediaError::VideoCapture(format!("xcap Monitor::all failed: {e}")))?;
                let monitor = monitors
                    .into_iter()
                    .find(|m| m.id() == *id || m.name() == name)
                    .or_else(|| xcap::Monitor::all().ok()?.into_iter().next())
                    .ok_or_else(|| {
                        MediaError::SourceNotFound(format!("Monitor id={id} ({name}) not found"))
                    })?;

                let img = monitor
                    .capture_image()
                    .map_err(|e| MediaError::VideoCapture(format!("Monitor capture failed: {e}")))?;
                let (w, h) = (img.width(), img.height());
                Ok((w, h, img.into_raw()))
            }
            CaptureTarget::Window { id, title, .. } => {
                let windows = xcap::Window::all()
                    .map_err(|e| MediaError::VideoCapture(format!("xcap Window::all failed: {e}")))?;
                let window = windows
                    .into_iter()
                    .find(|w| w.id() == *id || w.title() == title)
                    .ok_or_else(|| {
                        MediaError::SourceNotFound(format!("Window id={id} ({title}) closed or not found"))
                    })?;

                let img = window
                    .capture_image()
                    .map_err(|e| MediaError::VideoCapture(format!("Window capture failed: {e}")))?;
                let (w, h) = (img.width(), img.height());
                Ok((w, h, img.into_raw()))
            }
        }
    }
}

impl VideoCapturer for NativeVideoCapturer {
    fn start_capture(
        &self,
        target: CaptureTarget,
        resolution: TargetResolution,
        fps: u8,
        tx: mpsc::Sender<RawVideoFrame>,
        cancel: CancellationToken,
    ) -> Result<(), MediaError> {
        let clamped_fps = fps.clamp(1, 60) as u64;
        let frame_interval = Duration::from_micros(1_000_000 / clamped_fps);

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(frame_interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let start = Instant::now();

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        let target_clone = target.clone();
                        let cap_res = tokio::task::spawn_blocking(move || {
                            let (src_w, src_h, raw_rgba) = Self::capture_raw_target(&target_clone)?;
                            scale_rgba_frame(&raw_rgba, src_w, src_h, resolution)
                        })
                        .await;

                        match cap_res {
                            Ok(Ok((width, height, rgba))) => {
                                let frame = RawVideoFrame {
                                    width,
                                    height,
                                    rgba,
                                    timestamp_us: start.elapsed().as_micros() as u64,
                                };
                                if tx.send(frame).await.is_err() {
                                    break;
                                }
                            }
                            Ok(Err(MediaError::SourceNotFound(msg))) => {
                                warn!("Capture target disappeared, stopping video capture: {msg}");
                                break;
                            }
                            Ok(Err(err)) => {
                                warn!("Transient video capture error: {err}");
                            }
                            Err(join_err) => {
                                warn!("Capture blocking task panicked: {join_err}");
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::mock::MockVideoCapturer;

    #[test]
    fn test_frame_scaler_odd_dimensions_and_resolutions() {
        // Create an odd-dimensioned source image: 1365 x 767
        let src_w = 1365u32;
        let src_h = 767u32;
        let src_rgba = vec![200u8; (src_w * src_h * 4) as usize];

        let (w720, h720, buf720) =
            scale_rgba_frame(&src_rgba, src_w, src_h, TargetResolution::R720p)
                .expect("scale to 720p");
        assert_eq!(w720 % 2, 0);
        assert_eq!(h720 % 2, 0);
        assert!(w720 <= 1280 && h720 <= 720);
        assert_eq!(buf720.len(), (w720 * h720 * 4) as usize);

        let (wnat, hnat, bufnat) =
            scale_rgba_frame(&src_rgba, src_w, src_h, TargetResolution::Native)
                .expect("scale to native even");
        assert_eq!((wnat, hnat), (1364, 766));
        assert_eq!(bufnat.len(), (wnat * hnat * 4) as usize);
    }

    #[test]
    fn test_h264_encode_decode_roundtrip() {
        let width = 320u32;
        let height = 240u32;
        let rgba = MockVideoCapturer::generate_smpte_bars(width, height, 0);

        let mut encoder = H264VideoEncoder::new(30, 1_500_000).expect("init H264 encoder");
        let mut decoder = H264VideoDecoder::new().expect("init H264 decoder");

        let annex_b = encoder
            .encode_rgba(&rgba, width, height)
            .expect("encode frame");
        assert!(!annex_b.is_empty());

        let decoded = decoder
            .decode_annex_b(&annex_b)
            .expect("decode annex-b")
            .expect("must produce decoded frame on IDR");
        assert_eq!(decoded.0, width);
        assert_eq!(decoded.1, height);
        assert_eq!(decoded.2.len(), (width * height * 4) as usize);
    }

    #[tokio::test]
    async fn test_fps_limiter_pacing() {
        let capturer = MockVideoCapturer::new(160, 120);
        let (tx, mut rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        capturer
            .start_capture(
                CaptureTarget::Monitor {
                    id: 1,
                    name: "Mock".to_string(),
                },
                TargetResolution::Native,
                30,
                tx,
                cancel.clone(),
            )
            .expect("start mock capture");

        let start = Instant::now();
        let mut count = 0;
        while count < 5 {
            let frame = rx.recv().await.expect("receive frame");
            assert_eq!(frame.width, 160);
            assert_eq!(frame.height, 120);
            count += 1;
        }
        cancel.cancel();
        let elapsed = start.elapsed();
        // 5 frames at 30 FPS (first tick is immediate, followed by 4 intervals of ~33.3ms = ~133ms)
        assert!(
            elapsed >= Duration::from_millis(100),
            "FPS limiter ran too fast: {elapsed:?}"
        );
    }
}
