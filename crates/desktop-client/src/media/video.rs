use std::time::{Duration, Instant};

use fast_image_resize::{
    images::{Image, ImageRef},
    PixelType, ResizeAlg, ResizeOptions, Resizer,
};
use openh264::{
    decoder::{Decoder, DecoderConfig},
    encoder::{Encoder, EncoderConfig, RateControlMode, SpsPpsStrategy, UsageType},
    formats::{YUVBuffer, YUVSlices, YUVSource},
    OpenH264API, Timestamp,
};
use shared_protocol::{CaptureTarget, TargetResolution};
use tokio::sync::mpsc;
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
    let mut resizer = Resizer::new();
    scale_rgba_frame_with_resizer(&mut resizer, src_rgba, src_width, src_height, resolution)
}

pub fn scale_rgba_frame_with_resizer(
    resizer: &mut Resizer,
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
    let options = ResizeOptions::new()
        .resize_alg(ResizeAlg::Nearest)
        .use_alpha(false);

    resizer
        .resize(&src_view, &mut dst_image, &options)
        .map_err(|e| MediaError::VideoCapture(format!("fast_image_resize resize error: {e}")))?;

    Ok((dst_width, dst_height, dst_image.into_vec()))
}

/// Fast 2x2-block fixed-point integer RGBA to planar I420 (YUV420p) converter
/// structured with `chunks_exact` so LLVM auto-vectorizes the inner loop with AVX2/SSE2.
#[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
pub fn rgba_to_yuv420p_into(
    rgba: &[u8],
    width: u32,
    height: u32,
    yuv_buf: &mut Vec<u8>,
) -> Result<(), MediaError> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(MediaError::Codec(format!(
            "YUV420p requires non-zero even dimensions, got {width}x{height}"
        )));
    }
    let w = width as usize;
    let h = height as usize;
    let expected = w * h * 4;
    if rgba.len() < expected {
        return Err(MediaError::Codec(format!(
            "RGBA buffer too small for {width}x{height}: expected {expected}, got {}",
            rgba.len()
        )));
    }

    // Touch `yuv` crate color space descriptors to validate BT.601 studio range compatibility.
    let _ = (Range::Limited, MatrixCoefficients::BT601);

    let y_len = w * h;
    let uv_len = y_len / 4;
    let total_yuv = y_len + uv_len * 2;
    if yuv_buf.len() != total_yuv {
        yuv_buf.resize(total_yuv, 0);
    }

    let (y_plane, uv_planes) = yuv_buf.split_at_mut(y_len);
    let (u_plane, v_plane) = uv_planes.split_at_mut(uv_len);
    let half_w = w / 2;
    let half_h = h / 2;
    let row_stride = w * 4;

    // 1. High-throughput row-by-row luminance (Y) conversion.
    // Operating strictly on contiguous row slices allows LLVM to vectorize with AVX2/SSE2/NEON.
    for (rgba_row, y_row) in rgba.chunks_exact(row_stride).zip(y_plane.chunks_exact_mut(w)) {
        for (px, y_out) in rgba_row.chunks_exact(4).zip(y_row.iter_mut()) {
            let r = i32::from(px[0]);
            let g = i32::from(px[1]);
            let b = i32::from(px[2]);
            *y_out = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
        }
    }

    // 2. Chrominance (U, V) conversion with 2x2 box averaging.
    for by in 0..half_h {
        let py = by * 2;
        let rgba_row0 = &rgba[py * row_stride..(py + 1) * row_stride];
        let rgba_row1 = &rgba[(py + 1) * row_stride..(py + 2) * row_stride];
        let u_row = &mut u_plane[by * half_w..(by + 1) * half_w];
        let v_row = &mut v_plane[by * half_w..(by + 1) * half_w];

        for (((c0, c1), u_out), v_out) in rgba_row0
            .chunks_exact(8)
            .zip(rgba_row1.chunks_exact(8))
            .zip(u_row.iter_mut())
            .zip(v_row.iter_mut())
        {
            let (r00, g00, b00) = (i32::from(c0[0]), i32::from(c0[1]), i32::from(c0[2]));
            let (r01, g01, b01) = (i32::from(c0[4]), i32::from(c0[5]), i32::from(c0[6]));
            let (r10, g10, b10) = (i32::from(c1[0]), i32::from(c1[1]), i32::from(c1[2]));
            let (r11, g11, b11) = (i32::from(c1[4]), i32::from(c1[5]), i32::from(c1[6]));

            let r_avg = (r00 + r01 + r10 + r11 + 2) >> 2;
            let g_avg = (g00 + g01 + g10 + g11 + 2) >> 2;
            let b_avg = (b00 + b01 + b10 + b11 + 2) >> 2;

            *u_out = (((-38 * r_avg - 74 * g_avg + 112 * b_avg + 128) >> 8) + 128)
                .clamp(0, 255) as u8;
            *v_out = (((112 * r_avg - 94 * g_avg - 18 * b_avg + 128) >> 8) + 128)
                .clamp(0, 255) as u8;
        }
    }

    Ok(())
}

/// Converts an even-dimensioned RGBA buffer into a planar YUV420p (`I420`) `YUVBuffer`.
pub fn rgba_to_yuv420p(
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<YUVBuffer, MediaError> {
    let mut yuv_buf = Vec::new();
    rgba_to_yuv420p_into(rgba, width, height, &mut yuv_buf)?;
    Ok(YUVBuffer::from_vec(yuv_buf, width as usize, height as usize))
}

/// Converts a contiguous planar I420 (`YUV420p`) buffer into an RGBA buffer.
pub fn yuv420p_planar_to_rgba(
    yuv: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, MediaError> {
    let w = width as usize;
    let h = height as usize;
    let y_len = w * h;
    let uv_len = y_len / 4;
    if w == 0 || h == 0 || yuv.len() < y_len + uv_len * 2 {
        return Err(MediaError::Codec(format!(
            "Invalid planar I420 buffer for {width}x{height}: got {} bytes",
            yuv.len()
        )));
    }
    let y_plane = &yuv[..y_len];
    let u_plane = &yuv[y_len..y_len + uv_len];
    let v_plane = &yuv[y_len + uv_len..y_len + uv_len * 2];
    let mut rgba = vec![0u8; y_len * 4];
    yuv420p_to_rgba(
        y_plane,
        u_plane,
        v_plane,
        w,
        h,
        (w, w / 2, w / 2),
        &mut rgba,
    );
    Ok(rgba)
}

/// Fast 2x2-block fixed-point integer I420 (YUV420p) to RGBA converter.
#[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
fn yuv420p_to_rgba(
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    width: usize,
    height: usize,
    strides: (usize, usize, usize),
    rgba_out: &mut [u8],
) {
    #[inline(always)]
    fn write_px(dst: &mut [u8], y_val: u8, c_r: i32, c_g: i32, c_b: i32) {
        let y = i32::from(y_val);
        dst[0] = (y + c_r).clamp(0, 255) as u8;
        dst[1] = (y - c_g).clamp(0, 255) as u8;
        dst[2] = (y + c_b).clamp(0, 255) as u8;
        dst[3] = 255;
    }

    let half_w = width / 2;
    let half_h = height / 2;
    let rgba_stride = width * 4;

    for by in 0..half_h {
        let py = by * 2;
        let y_row0 = &y_plane[py * strides.0..py * strides.0 + width];
        let y_row1 = &y_plane[(py + 1) * strides.0..(py + 1) * strides.0 + width];
        let u_row = &u_plane[by * strides.1..by * strides.1 + half_w];
        let v_row = &v_plane[by * strides.2..by * strides.2 + half_w];
        let (out_top, out_rest) = rgba_out[py * rgba_stride..(py + 2) * rgba_stride]
            .split_at_mut(rgba_stride);

        for (((((y0, y1), &u_b), &v_b), top_px), bot_px) in y_row0
            .chunks_exact(2)
            .zip(y_row1.chunks_exact(2))
            .zip(u_row.iter())
            .zip(v_row.iter())
            .zip(out_top.chunks_exact_mut(8))
            .zip(out_rest.chunks_exact_mut(8))
        {
            let u = i32::from(u_b) - 128;
            let v = i32::from(v_b) - 128;
            let c_r = (359 * v + 128) >> 8;
            let c_g = (88 * u + 183 * v + 128) >> 8;
            let c_b = (454 * u + 128) >> 8;

            write_px(&mut top_px[0..4], y0[0], c_r, c_g, c_b);
            write_px(&mut top_px[4..8], y0[1], c_r, c_g, c_b);
            write_px(&mut bot_px[0..4], y1[0], c_r, c_g, c_b);
            write_px(&mut bot_px[4..8], y1[1], c_r, c_g, c_b);
        }
    }
}

/// Low-latency H.264 Baseline Profile encoder wrapper with periodic IDR keyframe generation.
pub struct H264VideoEncoder {
    encoder: Encoder,
    fps: u8,
    bitrate_bps: u32,
    frame_count: u64,
    keyframe_interval: u64,
    force_next_idr: bool,
    last_dimensions: Option<(u32, u32)>,
    yuv_scratch: Vec<u8>,
}

impl H264VideoEncoder {
    fn build_encoder(fps: u8, bitrate_bps: u32) -> Result<Encoder, MediaError> {
        let clamped_fps = fps.clamp(1, 60);
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().clamp(2, 4) as u16)
            .unwrap_or(2);
        let config = EncoderConfig::new()
            .set_bitrate_bps(bitrate_bps.max(1_000_000))
            .max_frame_rate(clamped_fps as f32)
            .enable_skip_frame(true)
            .rate_control_mode(RateControlMode::Bitrate)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .usage_type(UsageType::CameraVideoRealTime)
            .set_multiple_thread_idc(threads);

        Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| MediaError::Codec(format!("OpenH264 encoder init error: {e}")))
    }

    pub fn new(fps: u8, bitrate_bps: u32) -> Result<Self, MediaError> {
        let clamped_fps = fps.clamp(1, 60);
        let encoder = Self::build_encoder(clamped_fps, bitrate_bps)?;

        Ok(Self {
            encoder,
            fps: clamped_fps,
            bitrate_bps,
            frame_count: 0,
            keyframe_interval: ((clamped_fps as u64) * 4).max(60),
            force_next_idr: true,
            last_dimensions: None,
            yuv_scratch: Vec::new(),
        })
    }

    pub fn request_keyframe(&mut self) {
        self.force_next_idr = true;
    }

    /// Returns the planar I420 (`YUV420p`) buffer produced during the most recent `encode_rgba` call.
    pub fn last_yuv420p(&self) -> &[u8] {
        &self.yuv_scratch
    }

    /// Encodes an RGBA frame into an Annex-B H.264 bitstream reusing internal YUV planar buffers.
    pub fn encode_rgba(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, MediaError> {
        rgba_to_yuv420p_into(rgba, width, height, &mut self.yuv_scratch)?;

        // If the captured window changed dimensions (e.g. restored from minimized or resized),
        // recreate the OpenH264 encoder instance cleanly so SPS/PPS/IDR are re-emitted without
        // triggering OpenH264's broken internal SVC layer reinit path.
        if self.last_dimensions.is_some_and(|dim| dim != (width, height)) {
            self.encoder = Self::build_encoder(self.fps, self.bitrate_bps)?;
            self.last_dimensions = None;
            self.force_next_idr = true;
        }

        // Only call `force_intra_frame` after the underlying OpenH264 encoder has been
        // initialized by its first `encode()` call (the first frame is always an IDR frame).
        if self.last_dimensions.is_some()
            && (self.force_next_idr || self.frame_count.is_multiple_of(self.keyframe_interval))
        {
            self.encoder.force_intra_frame();
            self.force_next_idr = false;
        }

        let w = width as usize;
        let h = height as usize;
        let y_len = w * h;
        let uv_len = y_len / 4;
        let (y_plane, uv_planes) = self.yuv_scratch.split_at(y_len);
        let (u_plane, v_plane) = uv_planes.split_at(uv_len);
        let yuv_slices = YUVSlices::new((y_plane, u_plane, v_plane), (w, h), (w, w / 2, w / 2));

        let ts_ms = (self.frame_count * 1000) / u64::from(self.fps.max(1));
        let bitstream = self
            .encoder
            .encode_at(&yuv_slices, Timestamp::from_millis(ts_ms))
            .map_err(|e| MediaError::Codec(format!("OpenH264 encode error: {e}")))?;

        self.last_dimensions = Some((width, height));
        self.force_next_idr = false;
        self.frame_count = self.frame_count.wrapping_add(1);
        Ok(bitstream.to_vec())
    }
}

/// Decoded `(width, height, i420_bytes)` tuple produced by [`H264VideoDecoder::decode_annex_b_yuv`].
pub type DecodedYuvFrame = (u32, u32, Vec<u8>);

/// Low-latency H.264 decoder wrapper producing planar I420 or RGBA frames.
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

    /// Feeds an Annex-B H.264 buffer into the decoder and returns compact planar I420 (`YUV420p`)
    /// directly without CPU YUV->RGBA conversion, along with a boolean flag indicating if any NAL
    /// unit encountered a decoding error (so the caller can request an immediate IDR keyframe).
    pub fn decode_annex_b_yuv(
        &mut self,
        annex_b: &[u8],
    ) -> Result<(Option<DecodedYuvFrame>, bool), MediaError> {
        if annex_b.is_empty() {
            return Ok((None, false));
        }

        let mut latest_frame: Option<DecodedYuvFrame> = None;
        let mut had_error = false;
        for nal in openh264::nal_units(annex_b) {
            match self.decoder.decode(nal) {
                Ok(Some(decoded_yuv)) => {
                    let (w, h) = decoded_yuv.dimensions();
                    if w > 0 && h > 0 {
                        let strides = decoded_yuv.strides();
                        let half_w = w / 2;
                        let half_h = h / 2;
                        let y_len = w * h;
                        let uv_len = half_w * half_h;
                        let mut i420 = Vec::with_capacity(y_len + uv_len * 2);

                        let y_src = decoded_yuv.y();
                        for row in 0..h {
                            let start = row * strides.0;
                            i420.extend_from_slice(&y_src[start..start + w]);
                        }
                        let u_src = decoded_yuv.u();
                        for row in 0..half_h {
                            let start = row * strides.1;
                            i420.extend_from_slice(&u_src[start..start + half_w]);
                        }
                        let v_src = decoded_yuv.v();
                        for row in 0..half_h {
                            let start = row * strides.2;
                            i420.extend_from_slice(&v_src[start..start + half_w]);
                        }

                        latest_frame = Some((w as u32, h as u32, i420));
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    had_error = true;
                    debug!("OpenH264 NAL decode transient error (awaiting IDR/SPS/PPS): {err}");
                }
            }
        }

        Ok((latest_frame, had_error))
    }

    /// Feeds an Annex-B H.264 buffer (one or more NAL units) into the decoder.
    /// Returns `Ok(Some((width, height, rgba_bytes)))` when a complete picture is decoded.
    pub fn decode_annex_b(
        &mut self,
        annex_b: &[u8],
    ) -> Result<Option<(u32, u32, Vec<u8>)>, MediaError> {
        let (maybe_yuv, _) = self.decode_annex_b_yuv(annex_b)?;
        match maybe_yuv {
            Some((w, h, i420)) => {
                let rgba = yuv420p_planar_to_rgba(&i420, w, h)?;
                Ok(Some((w, h, rgba)))
            }
            None => Ok(None),
        }
    }
}

/// Enumerates all available monitors and application windows on the host OS.
pub fn list_capture_targets() -> Result<Vec<CaptureTarget>, MediaError> {
    let mut targets = Vec::new();

    match std::panic::catch_unwind(xcap::Monitor::all) {
        Ok(Ok(monitors)) => {
            for monitor in monitors {
                targets.push(CaptureTarget::Monitor {
                    id: monitor.id(),
                    name: monitor.name().to_string(),
                });
            }
        }
        Ok(Err(err)) => {
            warn!("Monitor enumeration warning: {err}");
        }
        Err(_) => {
            warn!("Monitor enumeration panicked inside xcap");
        }
    }

    #[cfg(target_os = "windows")]
    {
        let mut seen_ids = std::collections::HashSet::new();
        for win_target in win32_capture::list_windows_win32() {
            if let CaptureTarget::Window { id, .. } = &win_target {
                seen_ids.insert(*id);
            }
            targets.push(win_target);
        }

        if let Ok(Ok(windows)) = std::panic::catch_unwind(xcap::Window::all) {
            for window in windows {
                let id = window.id();
                if seen_ids.contains(&id) || window.title().trim().is_empty() {
                    continue;
                }
                let pid = extract_window_pid(&window);
                targets.push(CaptureTarget::Window {
                    id,
                    title: window.title().to_string(),
                    pid,
                });
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        match std::panic::catch_unwind(xcap::Window::all) {
            Ok(Ok(windows)) => {
                for window in windows {
                    if window.title().trim().is_empty() {
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
            Ok(Err(err)) => {
                warn!("Window enumeration warning: {err}");
            }
            Err(_) => {
                warn!("Window enumeration panicked inside xcap");
            }
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

#[cfg(target_os = "windows")]
mod win32_capture {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::ptr::null_mut;

    use fast_image_resize::Resizer;
    use shared_protocol::{CaptureTarget, TargetResolution};
    use windows::Win32::{
        Foundation::{BOOL, HANDLE, HWND, LPARAM, RECT},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
                D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
            },
            Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED},
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
                IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
                DXGI_OUTDUPL_FRAME_INFO,
            },
            Gdi::{
                BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush,
                GetDC, GetWindowDC, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
                DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
            },
        },
        Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS, PW_CLIENTONLY},
        UI::WindowsAndMessaging::{
            EnumWindows, GetClientRect, GetForegroundWindow, GetShellWindow, GetWindow,
            GetWindowLongW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
            GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, GWL_EXSTYLE, GW_OWNER,
            WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        },
    };
    use windows_core::Interface;

    use crate::media::{traits::MediaError, video::scale_rgba_frame_with_resizer};

    const PW_RENDERFULLCONTENT_FLAG: u32 = 0x00000002;

    #[inline]
    #[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
    fn bgra_to_rgba_in_place(buf: &mut [u8]) {
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
            px[3] = 255;
        }
    }

    #[inline]
    fn has_non_zero_sample(buf: &[u8]) -> bool {
        let pixel_count = buf.len() / 4;
        if pixel_count == 0 {
            return false;
        }
        let step_bytes = ((pixel_count / 64).max(1)) * 4;
        for base in (0..buf.len().saturating_sub(3)).step_by(step_bytes) {
            if buf[base] != 0 || buf[base + 1] != 0 || buf[base + 2] != 0 {
                return true;
            }
        }
        false
    }

    /// Hardware-accelerated GPU screen capturer using Direct3D 11 + `IDXGIOutputDuplication`.
    pub struct DxgiMonitorDuplicator {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        dupl: IDXGIOutputDuplication,
        staging_tex: Option<ID3D11Texture2D>,
        staging_dims: (u32, u32),
        last_bgra: Vec<u8>,
        last_scaled: Option<(u32, u32, Vec<u8>)>,
        width: u32,
        height: u32,
        monitor_id_hint: u32,
        monitor_name_hint: String,
    }

    impl DxgiMonitorDuplicator {
        pub fn new(monitor_id_hint: u32, monitor_name_hint: &str) -> Result<Self, MediaError> {
            unsafe {
                let factory: IDXGIFactory1 = CreateDXGIFactory1()
                    .map_err(|e| MediaError::VideoCapture(format!("CreateDXGIFactory1: {e}")))?;

                let mut fallback_pair = None;
                let mut matched_pair = None;
                let mut adapter_idx = 0u32;

                while let Ok(adapter) = factory.EnumAdapters1(adapter_idx) {
                    let mut output_idx = 0u32;
                    while let Ok(output) = adapter.EnumOutputs(output_idx) {
                        if let Ok(desc) = output.GetDesc() {
                            if desc.AttachedToDesktop.as_bool() {
                                let hmon_id = desc.Monitor.0 as usize as u32;
                                let dev_name_len = desc
                                    .DeviceName
                                    .iter()
                                    .position(|&c| c == 0)
                                    .unwrap_or(desc.DeviceName.len());
                                let dev_name =
                                    String::from_utf16_lossy(&desc.DeviceName[..dev_name_len]);

                                if fallback_pair.is_none() {
                                    fallback_pair = Some((adapter.clone(), output.clone()));
                                }
                                if hmon_id == monitor_id_hint
                                    || (!monitor_name_hint.is_empty()
                                        && dev_name.contains(monitor_name_hint))
                                {
                                    matched_pair = Some((adapter.clone(), output));
                                    break;
                                }
                            }
                        }
                        output_idx += 1;
                    }
                    if matched_pair.is_some() {
                        break;
                    }
                    adapter_idx += 1;
                }

                let (adapter, output) = matched_pair.or(fallback_pair).ok_or_else(|| {
                    MediaError::VideoCapture("No attached DXGI desktop outputs found".to_string())
                })?;

                let mut device: Option<ID3D11Device> = None;
                let mut context: Option<ID3D11DeviceContext> = None;
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    None,
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
                .map_err(|e| MediaError::VideoCapture(format!("D3D11CreateDevice failed: {e}")))?;

                let device = device.ok_or_else(|| {
                    MediaError::VideoCapture("D3D11CreateDevice returned null device".to_string())
                })?;
                let context = context.ok_or_else(|| {
                    MediaError::VideoCapture("D3D11CreateDevice returned null context".to_string())
                })?;

                let output1: IDXGIOutput1 = output
                    .cast()
                    .map_err(|e| MediaError::VideoCapture(format!("IDXGIOutput1 cast: {e}")))?;
                let dupl = output1
                    .DuplicateOutput(&device)
                    .map_err(|e| MediaError::VideoCapture(format!("DuplicateOutput: {e}")))?;

                let dupl_desc = dupl.GetDesc();
                let width = dupl_desc.ModeDesc.Width;
                let height = dupl_desc.ModeDesc.Height;

                Ok(Self {
                    device,
                    context,
                    dupl,
                    staging_tex: None,
                    staging_dims: (0, 0),
                    last_bgra: Vec::new(),
                    last_scaled: None,
                    width,
                    height,
                    monitor_id_hint,
                    monitor_name_hint: monitor_name_hint.to_string(),
                })
            }
        }

        fn recreate(&mut self) -> Result<(), MediaError> {
            let fresh = Self::new(self.monitor_id_hint, &self.monitor_name_hint)?;
            *self = fresh;
            Ok(())
        }

        pub fn capture_scaled_rgba(
            &mut self,
            resizer: &mut Resizer,
            resolution: TargetResolution,
        ) -> Result<(u32, u32, Vec<u8>), MediaError> {
            unsafe {
                let timeout_ms = if self.last_bgra.is_empty() { 16 } else { 0 };
                let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;

                match self
                    .dupl
                    .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
                {
                    Ok(()) => {
                        let copy_res = (|| -> Result<(), MediaError> {
                            let res = resource.take().ok_or_else(|| {
                                MediaError::VideoCapture(
                                    "DXGI AcquireNextFrame returned null resource".to_string(),
                                )
                            })?;
                            let src_tex: ID3D11Texture2D = res.cast().map_err(|e| {
                                MediaError::VideoCapture(format!("ID3D11Texture2D cast: {e}"))
                            })?;

                            let mut desc = D3D11_TEXTURE2D_DESC::default();
                            src_tex.GetDesc(&mut desc);
                            let w = desc.Width;
                            let h = desc.Height;
                            if w == 0 || h == 0 {
                                return Err(MediaError::VideoCapture(
                                    "DXGI desktop texture has zero dimensions".to_string(),
                                ));
                            }

                            if self.staging_tex.is_none() || self.staging_dims != (w, h) {
                                let staging_desc = D3D11_TEXTURE2D_DESC {
                                    Width: w,
                                    Height: h,
                                    MipLevels: 1,
                                    ArraySize: 1,
                                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                                    SampleDesc: DXGI_SAMPLE_DESC {
                                        Count: 1,
                                        Quality: 0,
                                    },
                                    Usage: D3D11_USAGE_STAGING,
                                    BindFlags: 0,
                                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                                    MiscFlags: 0,
                                };
                                let mut new_tex: Option<ID3D11Texture2D> = None;
                                self.device
                                    .CreateTexture2D(&staging_desc, None, Some(&mut new_tex))
                                    .map_err(|e| {
                                        MediaError::VideoCapture(format!(
                                            "CreateTexture2D staging failed: {e}"
                                        ))
                                    })?;
                                self.staging_tex = new_tex;
                                self.staging_dims = (w, h);
                            }

                            let staging = self.staging_tex.as_ref().unwrap();
                            self.context.CopyResource(staging, &src_tex);

                            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                            self.context
                                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                                .map_err(|e| {
                                    MediaError::VideoCapture(format!(
                                        "ID3D11DeviceContext::Map failed: {e}"
                                    ))
                                })?;

                            let row_bytes = (w as usize) * 4;
                            let total_bytes = row_bytes * (h as usize);
                            if self.last_bgra.len() != total_bytes {
                                self.last_bgra.resize(total_bytes, 0);
                            }
                            let src_pitch = mapped.RowPitch as usize;
                            let src_ptr = mapped.pData.cast::<u8>();
                            if src_pitch == row_bytes {
                                std::ptr::copy_nonoverlapping(
                                    src_ptr,
                                    self.last_bgra.as_mut_ptr(),
                                    total_bytes,
                                );
                            } else {
                                for row in 0..(h as usize) {
                                    std::ptr::copy_nonoverlapping(
                                        src_ptr.add(row * src_pitch),
                                        self.last_bgra.as_mut_ptr().add(row * row_bytes),
                                        row_bytes,
                                    );
                                }
                            }

                            self.context.Unmap(staging, 0);
                            self.width = w;
                            self.height = h;
                            Ok(())
                        })();

                        let _ = self.dupl.ReleaseFrame();
                        copy_res?;

                        let (dst_w, dst_h, mut scaled) = scale_rgba_frame_with_resizer(
                            resizer,
                            &self.last_bgra,
                            self.width,
                            self.height,
                            resolution,
                        )?;
                        bgra_to_rgba_in_place(&mut scaled);
                        self.last_scaled = Some((dst_w, dst_h, scaled.clone()));
                        return Ok((dst_w, dst_h, scaled));
                    }
                    Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                        if let Some(cached) = &self.last_scaled {
                            return Ok(cached.clone());
                        }
                        if self.last_bgra.is_empty() {
                            return Err(MediaError::VideoCapture(
                                "DXGI initial frame timed out".to_string(),
                            ));
                        }
                    }
                    Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => {
                        self.recreate()?;
                        return Err(MediaError::VideoCapture(
                            "DXGI desktop duplication access lost, reinitialized".to_string(),
                        ));
                    }
                    Err(err) => {
                        return Err(MediaError::VideoCapture(format!(
                            "DXGI AcquireNextFrame error: {err}"
                        )));
                    }
                }

                let (dst_w, dst_h, mut scaled) = scale_rgba_frame_with_resizer(
                    resizer,
                    &self.last_bgra,
                    self.width,
                    self.height,
                    resolution,
                )?;
                bgra_to_rgba_in_place(&mut scaled);
                self.last_scaled = Some((dst_w, dst_h, scaled.clone()));
                Ok((dst_w, dst_h, scaled))
            }
        }
    }

    /// Persistent GDI memory DC and top-down 32-bit DIB section so window capture does not
    /// allocate/free kernel GDI bitmaps or invoke `GetDIBits` on every frame.
    struct GdiDibSurface {
        hdc_mem: HDC,
        hbitmap: HBITMAP,
        old_obj: HGDIOBJ,
        dib_ptr: *mut u8,
        width: u32,
        height: u32,
    }

    impl Default for GdiDibSurface {
        fn default() -> Self {
            Self {
                hdc_mem: HDC::default(),
                hbitmap: HBITMAP::default(),
                old_obj: HGDIOBJ::default(),
                dib_ptr: null_mut(),
                width: 0,
                height: 0,
            }
        }
    }

    impl GdiDibSurface {
        unsafe fn cleanup(&mut self) {
            if !self.hdc_mem.is_invalid() {
                if !self.old_obj.is_invalid() {
                    let _ = SelectObject(self.hdc_mem, self.old_obj);
                    self.old_obj = HGDIOBJ::default();
                }
                if !self.hbitmap.is_invalid() {
                    let _ = DeleteObject(self.hbitmap);
                    self.hbitmap = HBITMAP::default();
                }
                let _ = DeleteDC(self.hdc_mem);
                self.hdc_mem = HDC::default();
            }
            self.dib_ptr = null_mut();
            self.width = 0;
            self.height = 0;
        }

        unsafe fn ensure_size(&mut self, hdc_ref: HDC, width: u32, height: u32) -> bool {
            if !self.hdc_mem.is_invalid()
                && !self.dib_ptr.is_null()
                && self.width == width
                && self.height == height
            {
                return true;
            }
            self.cleanup();

            let hdc_mem = CreateCompatibleDC(hdc_ref);
            if hdc_mem.is_invalid() {
                return false;
            }

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32), // Top-down DIB
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut c_void = null_mut();
            let hbitmap = match CreateDIBSection(
                hdc_mem,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                HANDLE::default(),
                0,
            ) {
                Ok(hbmp) if !hbmp.is_invalid() && !bits.is_null() => hbmp,
                _ => {
                    let _ = DeleteDC(hdc_mem);
                    return false;
                }
            };

            let old_obj = SelectObject(hdc_mem, hbitmap);
            self.hdc_mem = hdc_mem;
            self.hbitmap = hbitmap;
            self.old_obj = old_obj;
            self.dib_ptr = bits.cast::<u8>();
            self.width = width;
            self.height = height;
            true
        }
    }

    impl Drop for GdiDibSurface {
        fn drop(&mut self) {
            unsafe {
                self.cleanup();
            }
        }
    }

    fn is_ignored_system_title(title: &str) -> bool {
        matches!(
            title,
            "Program Manager"
                | "Windows Input Experience"
                | "Microsoft Text Input Application"
                | "MSCTFIME UI"
                | "Default IME"
        )
    }

    unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = &mut *(lparam.0 as *mut Vec<CaptureTarget>);

        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }

        let shell_hwnd = GetShellWindow();
        if !shell_hwnd.0.is_null() && hwnd == shell_hwnd {
            return BOOL(1);
        }

        let is_minimized = IsIconic(hwnd).as_bool();
        if !is_minimized {
            let mut cloaked: u32 = 0;
            if DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                (&mut cloaked as *mut u32).cast::<c_void>(),
                size_of::<u32>() as u32,
            )
            .is_ok()
                && cloaked != 0
            {
                return BOOL(1);
            }
        }

        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let is_app_window = (ex_style & WS_EX_APPWINDOW.0) != 0;
        let is_tool_window = (ex_style & WS_EX_TOOLWINDOW.0) != 0;
        let is_no_activate = (ex_style & WS_EX_NOACTIVATE.0) != 0;

        if (is_tool_window || is_no_activate) && !is_app_window {
            return BOOL(1);
        }

        if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
            if !owner.0.is_null() && !is_app_window {
                return BOOL(1);
            }
        }

        let text_len = GetWindowTextLengthW(hwnd);
        if text_len <= 0 {
            return BOOL(1);
        }

        let mut buf = vec![0u16; (text_len as usize) + 1];
        let copied = GetWindowTextW(hwnd, &mut buf);
        if copied <= 0 {
            return BOOL(1);
        }

        let title = String::from_utf16_lossy(&buf[..copied as usize])
            .trim()
            .to_string();
        if title.is_empty() || is_ignored_system_title(&title) {
            return BOOL(1);
        }

        // Skip invisible 0x0 helper windows unless the window is minimized (e.g. Alt-Tabbed fullscreen game).
        if !is_minimized {
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_ok()
                && ((rect.right - rect.left) <= 1 || (rect.bottom - rect.top) <= 1)
            {
                return BOOL(1);
            }
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));

        out.push(CaptureTarget::Window {
            id: hwnd.0 as usize as u32,
            title,
            pid,
        });

        BOOL(1)
    }

    /// Enumerates top-level windows on Windows without calling `OpenProcess(PROCESS_ALL_ACCESS)`,
    /// ensuring fullscreen games (including minimized/Alt-Tabbed games and anti-cheat protected games)
    /// are always listed.
    pub fn list_windows_win32() -> Vec<CaptureTarget> {
        let mut windows: Vec<CaptureTarget> = Vec::new();
        unsafe {
            let lparam = LPARAM((&mut windows as *mut Vec<CaptureTarget>) as isize);
            let _ = EnumWindows(Some(enum_windows_proc), lparam);
        }
        windows
    }

    pub fn resolve_hwnd(id: u32, title_hint: &str) -> Result<HWND, MediaError> {
        let candidate = HWND(id as usize as *mut c_void);
        if unsafe { IsWindow(candidate).as_bool() } {
            return Ok(candidate);
        }

        for target in list_windows_win32() {
            if let CaptureTarget::Window {
                id: win_id, title, ..
            } = target
            {
                if win_id == id || (!title_hint.is_empty() && title == title_hint) {
                    return Ok(HWND(win_id as usize as u32 as *mut c_void));
                }
            }
        }

        Err(MediaError::SourceNotFound(format!(
            "Window id={id} ({title_hint}) closed or not found"
        )))
    }

    fn capture_primary_monitor_xcap_scaled(
        resizer: &mut Resizer,
        resolution: TargetResolution,
    ) -> Result<(u32, u32, Vec<u8>), MediaError> {
        let monitors = std::panic::catch_unwind(xcap::Monitor::all)
            .map_err(|_| MediaError::VideoCapture("xcap Monitor::all panicked".to_string()))?
            .map_err(|e| MediaError::VideoCapture(format!("Monitor::all failed: {e}")))?;
        let monitor = monitors
            .into_iter()
            .next()
            .ok_or_else(|| MediaError::VideoCapture("No monitors available".to_string()))?;
        let img = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| monitor.capture_image()))
            .map_err(|_| MediaError::VideoCapture("Monitor::capture_image panicked".to_string()))?
            .map_err(|e| MediaError::VideoCapture(format!("Monitor capture failed: {e}")))?;
        scale_rgba_frame_with_resizer(resizer, img.as_raw(), img.width(), img.height(), resolution)
    }

    /// Stateful Win32 window capturer that reuses a persistent `CreateDIBSection`, prioritizes
    /// sub-millisecond `BitBlt`, uses GPU DXGI Desktop Duplication for foreground/fullscreen games,
    /// and only falls back to `PrintWindow(PW_RENDERFULLCONTENT)` when `BitBlt` is blank.
    pub struct Win32WindowCapturer {
        id: u32,
        title: String,
        surface: GdiDibSurface,
        dxgi_fallback: Option<DxgiMonitorDuplicator>,
        print_window_cooldown: u32,
    }

    impl Win32WindowCapturer {
        pub fn new(id: u32, title: String) -> Self {
            Self {
                id,
                title,
                surface: GdiDibSurface::default(),
                dxgi_fallback: None,
                print_window_cooldown: 0,
            }
        }

        fn capture_monitor_fallback(
            &mut self,
            resizer: &mut Resizer,
            resolution: TargetResolution,
        ) -> Result<(u32, u32, Vec<u8>), MediaError> {
            if self.dxgi_fallback.is_none() {
                self.dxgi_fallback = DxgiMonitorDuplicator::new(0, "").ok();
            }
            if let Some(dxgi) = self.dxgi_fallback.as_mut() {
                if let Ok(frame) = dxgi.capture_scaled_rgba(resizer, resolution) {
                    return Ok(frame);
                }
            }
            capture_primary_monitor_xcap_scaled(resizer, resolution)
        }

        pub fn capture_scaled_rgba(
            &mut self,
            resizer: &mut Resizer,
            resolution: TargetResolution,
        ) -> Result<(u32, u32, Vec<u8>), MediaError> {
            let hwnd = resolve_hwnd(self.id, &self.title)?;
            self.id = hwnd.0 as usize as u32;

            unsafe {
                // Minimized (Alt-Tabbed) fullscreen game: use GPU DXGI desktop duplication fallback.
                if IsIconic(hwnd).as_bool() {
                    return self.capture_monitor_fallback(resizer, resolution);
                }

                let mut client_rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut client_rect);
                let client_w = (client_rect.right - client_rect.left).max(0) as u32;
                let client_h = (client_rect.bottom - client_rect.top).max(0) as u32;

                let mut win_rect = RECT::default();
                GetWindowRect(hwnd, &mut win_rect)
                    .map_err(|e| MediaError::VideoCapture(format!("GetWindowRect failed: {e}")))?;
                let win_w = (win_rect.right - win_rect.left).max(0) as u32;
                let win_h = (win_rect.bottom - win_rect.top).max(0) as u32;

                let use_client = client_w >= 2 && client_h >= 2;
                let (width, height) = if use_client {
                    (client_w, client_h)
                } else {
                    (win_w, win_h)
                };

                if width < 2 || height < 2 {
                    return self.capture_monitor_fallback(resizer, resolution);
                }

                let hdc_window = if use_client {
                    GetDC(hwnd)
                } else {
                    GetWindowDC(hwnd)
                };
                if hdc_window.is_invalid() {
                    return self.capture_monitor_fallback(resizer, resolution);
                }

                if !self.surface.ensure_size(hdc_window, width, height) {
                    let _ = ReleaseDC(hwnd, hdc_window);
                    return self.capture_monitor_fallback(resizer, resolution);
                }

                let byte_len = (width as usize) * (height as usize) * 4;
                let mut captured_valid = false;

                // 1. Fast path (~0.4ms): try BitBlt directly from the DWM window redirection surface
                // unless BitBlt recently returned an all-black surface for this window.
                if self.print_window_cooldown == 0 {
                    let blt_ok = BitBlt(
                        self.surface.hdc_mem,
                        0,
                        0,
                        width as i32,
                        height as i32,
                        hdc_window,
                        0,
                        0,
                        SRCCOPY,
                    )
                    .is_ok();
                    if blt_ok {
                        let _ = GdiFlush();
                        let raw_bgra = std::slice::from_raw_parts(self.surface.dib_ptr, byte_len);
                        if has_non_zero_sample(raw_bgra) {
                            captured_valid = true;
                        } else {
                            // BitBlt was blank (e.g. hardware flip-model swapchain); avoid retrying
                            // BitBlt on every single frame for the next ~120 frames.
                            self.print_window_cooldown = 120;
                        }
                    }
                } else {
                    self.print_window_cooldown -= 1;
                }

                // 2. If BitBlt was blank and this window is in the foreground (e.g. fullscreen or
                // borderless DirectX/Vulkan game), use GPU DXGI Desktop Duplication at 60 FPS
                // instead of blocking the game's UI thread with PrintWindow!
                if !captured_valid && GetForegroundWindow() == hwnd {
                    let _ = ReleaseDC(hwnd, hdc_window);
                    if let Ok(frame) = self.capture_monitor_fallback(resizer, resolution) {
                        return Ok(frame);
                    }
                    // Re-acquire hdc_window if DXGI fallback failed.
                    let hdc_retry = if use_client {
                        GetDC(hwnd)
                    } else {
                        GetWindowDC(hwnd)
                    };
                    if hdc_retry.is_invalid() {
                        return capture_primary_monitor_xcap_scaled(resizer, resolution);
                    }
                    let pw_flags = if use_client {
                        PRINT_WINDOW_FLAGS(PW_CLIENTONLY.0 | PW_RENDERFULLCONTENT_FLAG)
                    } else {
                        PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT_FLAG)
                    };
                    let pw_ok = PrintWindow(hwnd, self.surface.hdc_mem, pw_flags).as_bool();
                    let _ = ReleaseDC(hwnd, hdc_retry);
                    if pw_ok {
                        let _ = GdiFlush();
                        let raw_bgra = std::slice::from_raw_parts(self.surface.dib_ptr, byte_len);
                        if has_non_zero_sample(raw_bgra) {
                            let (dst_w, dst_h, mut scaled) = scale_rgba_frame_with_resizer(
                                resizer,
                                raw_bgra,
                                width,
                                height,
                                resolution,
                            )?;
                            bgra_to_rgba_in_place(&mut scaled);
                            return Ok((dst_w, dst_h, scaled));
                        }
                    }
                    return capture_primary_monitor_xcap_scaled(resizer, resolution);
                }

                // 3. Background window where BitBlt was blank: use PrintWindow(PW_RENDERFULLCONTENT).
                if !captured_valid {
                    let pw_flags = if use_client {
                        PRINT_WINDOW_FLAGS(PW_CLIENTONLY.0 | PW_RENDERFULLCONTENT_FLAG)
                    } else {
                        PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT_FLAG)
                    };
                    if PrintWindow(hwnd, self.surface.hdc_mem, pw_flags).as_bool() {
                        let _ = GdiFlush();
                        let raw_bgra = std::slice::from_raw_parts(self.surface.dib_ptr, byte_len);
                        if has_non_zero_sample(raw_bgra) {
                            captured_valid = true;
                        }
                    }
                }

                let _ = ReleaseDC(hwnd, hdc_window);

                if !captured_valid {
                    return self.capture_monitor_fallback(resizer, resolution);
                }

                // Scale directly from the persistent DIB section memory BEFORE swizzling BGRA -> RGBA,
                // eliminating both the full-res Vec allocation and full-res color channel swap.
                let raw_bgra = std::slice::from_raw_parts(self.surface.dib_ptr, byte_len);
                let (dst_w, dst_h, mut scaled) = scale_rgba_frame_with_resizer(
                    resizer,
                    raw_bgra,
                    width,
                    height,
                    resolution,
                )?;
                bgra_to_rgba_in_place(&mut scaled);
                Ok((dst_w, dst_h, scaled))
            }
        }
    }
}

enum ResolvedCaptureTarget {
    #[cfg(target_os = "windows")]
    Monitor {
        xcap_monitor: xcap::Monitor,
        dxgi: Option<win32_capture::DxgiMonitorDuplicator>,
    },
    #[cfg(not(target_os = "windows"))]
    Monitor(xcap::Monitor),
    #[cfg(not(target_os = "windows"))]
    Window(xcap::Window),
    #[cfg(target_os = "windows")]
    Win32Window(win32_capture::Win32WindowCapturer),
}

impl ResolvedCaptureTarget {
    fn resolve(target: &CaptureTarget) -> Result<Self, MediaError> {
        match target {
            CaptureTarget::Monitor { id, name } => {
                let monitors = std::panic::catch_unwind(xcap::Monitor::all)
                    .map_err(|_| {
                        MediaError::VideoCapture("xcap Monitor::all panicked".to_string())
                    })?
                    .map_err(|e| {
                        MediaError::VideoCapture(format!("xcap Monitor::all failed: {e}"))
                    })?;
                let monitor = monitors
                    .into_iter()
                    .find(|m| m.id() == *id || m.name() == name)
                    .or_else(|| {
                        std::panic::catch_unwind(xcap::Monitor::all)
                            .ok()?
                            .ok()?
                            .into_iter()
                            .next()
                    })
                    .ok_or_else(|| {
                        MediaError::SourceNotFound(format!("Monitor id={id} ({name}) not found"))
                    })?;
                #[cfg(target_os = "windows")]
                {
                    let dxgi = win32_capture::DxgiMonitorDuplicator::new(*id, name).ok();
                    Ok(Self::Monitor {
                        xcap_monitor: monitor,
                        dxgi,
                    })
                }
                #[cfg(not(target_os = "windows"))]
                {
                    Ok(Self::Monitor(monitor))
                }
            }
            #[cfg(target_os = "windows")]
            CaptureTarget::Window { id, title, .. } => {
                let hwnd = win32_capture::resolve_hwnd(*id, title)?;
                Ok(Self::Win32Window(win32_capture::Win32WindowCapturer::new(
                    hwnd.0 as usize as u32,
                    title.clone(),
                )))
            }
            #[cfg(not(target_os = "windows"))]
            CaptureTarget::Window { id, title, .. } => {
                let windows = std::panic::catch_unwind(xcap::Window::all)
                    .map_err(|_| {
                        MediaError::VideoCapture("xcap Window::all panicked".to_string())
                    })?
                    .map_err(|e| {
                        MediaError::VideoCapture(format!("xcap Window::all failed: {e}"))
                    })?;
                let window = windows
                    .into_iter()
                    .find(|w| w.id() == *id || w.title() == title)
                    .ok_or_else(|| {
                        MediaError::SourceNotFound(format!(
                            "Window id={id} ({title}) closed or not found"
                        ))
                    })?;
                Ok(Self::Window(window))
            }
        }
    }

    fn capture_scaled(
        &mut self,
        resizer: &mut Resizer,
        resolution: TargetResolution,
    ) -> Result<(u32, u32, Vec<u8>), MediaError> {
        match self {
            #[cfg(target_os = "windows")]
            Self::Monitor { xcap_monitor, dxgi } => {
                if let Some(dupl) = dxgi.as_mut() {
                    if let Ok(frame) = dupl.capture_scaled_rgba(resizer, resolution) {
                        return Ok(frame);
                    }
                }
                let img = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    xcap_monitor.capture_image()
                }))
                .map_err(|_| {
                    MediaError::VideoCapture("Monitor::capture_image panicked".to_string())
                })?
                .map_err(|e| MediaError::VideoCapture(format!("Monitor capture failed: {e}")))?;
                scale_rgba_frame_with_resizer(
                    resizer,
                    img.as_raw(),
                    img.width(),
                    img.height(),
                    resolution,
                )
            }
            #[cfg(not(target_os = "windows"))]
            Self::Monitor(monitor) => {
                let img = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    monitor.capture_image()
                }))
                .map_err(|_| {
                    MediaError::VideoCapture("Monitor::capture_image panicked".to_string())
                })?
                .map_err(|e| MediaError::VideoCapture(format!("Monitor capture failed: {e}")))?;
                scale_rgba_frame_with_resizer(
                    resizer,
                    img.as_raw(),
                    img.width(),
                    img.height(),
                    resolution,
                )
            }
            #[cfg(target_os = "windows")]
            Self::Win32Window(capturer) => capturer.capture_scaled_rgba(resizer, resolution),
            #[cfg(not(target_os = "windows"))]
            Self::Window(window) => {
                let img = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    window.capture_image()
                }))
                .map_err(|_| {
                    MediaError::VideoCapture("Window::capture_image panicked".to_string())
                })?
                .map_err(|e| MediaError::VideoCapture(format!("Window capture failed: {e}")))?;
                scale_rgba_frame_with_resizer(
                    resizer,
                    img.as_raw(),
                    img.width(),
                    img.height(),
                    resolution,
                )
            }
        }
    }
}

/// Captures a single preview snapshot for `target`, scaled to at most 720p, and returns it
/// as a 24-byte-header binary frame packet (`[u32 width, u32 height, [u8; 16] peer_uuid, i420...]`).
pub fn capture_preview_packet(target: &CaptureTarget) -> Result<Vec<u8>, MediaError> {
    let mut resolved = ResolvedCaptureTarget::resolve(target)?;
    let mut resizer = Resizer::new();
    let (width, height, rgba) = resolved.capture_scaled(&mut resizer, TargetResolution::R720p)?;
    let mut i420 = Vec::new();
    rgba_to_yuv420p_into(&rgba, width, height, &mut i420)?;
    shared_protocol::encode_yuv420p_frame_packet(uuid::Uuid::nil(), width, height, &i420)
        .map_err(|e| MediaError::VideoCapture(format!("Preview frame packet encode error: {e}")))
}

/// Native OS screen and window capturer powered by DXGI Desktop Duplication, Win32 GDI DIB, `xcap`, and `fast_image_resize`.
#[derive(Debug, Clone, Default)]
pub struct NativeVideoCapturer;

impl NativeVideoCapturer {
    pub fn new() -> Self {
        Self
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

        std::thread::Builder::new()
            .name("native-video-capture".to_string())
            .spawn(move || {
                // Resolve the monitor or window ONCE outside the frame loop to avoid
                // expensive per-frame XRandR/window-tree enumeration.
                let mut resolved = match ResolvedCaptureTarget::resolve(&target) {
                    Ok(r) => r,
                    Err(err) => {
                        warn!("Initial video capture target resolution failed: {err}");
                        return;
                    }
                };

                let mut resizer = Resizer::new();
                let start = Instant::now();
                let mut next_tick = Instant::now() + frame_interval;

                while !cancel.is_cancelled() {
                    let now = Instant::now();
                    if now < next_tick {
                        std::thread::sleep(next_tick - now);
                        if cancel.is_cancelled() {
                            break;
                        }
                    }
                    next_tick += frame_interval;
                    let now_after = Instant::now();
                    if now_after > next_tick + frame_interval {
                        next_tick = now_after + frame_interval;
                    }

                    // If the downstream encoder is currently busy with the previous frame,
                    // skip capturing this tick rather than building up a latency queue.
                    if tx.capacity() == 0 {
                        continue;
                    }

                    let scaled_res = resolved
                        .capture_scaled(&mut resizer, resolution)
                        .or_else(|_| {
                            resolved = ResolvedCaptureTarget::resolve(&target)?;
                            resolved.capture_scaled(&mut resizer, resolution)
                        });

                    match scaled_res {
                        Ok((width, height, rgba)) => {
                            let frame = RawVideoFrame {
                                width,
                                height,
                                rgba,
                                timestamp_us: start.elapsed().as_micros() as u64,
                            };
                            match tx.try_send(frame) {
                                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                                Err(mpsc::error::TrySendError::Closed(_)) => break,
                            }
                        }
                        Err(MediaError::SourceNotFound(msg)) => {
                            warn!("Capture target disappeared, stopping video capture: {msg}");
                            break;
                        }
                        Err(err) => {
                            warn!("Transient video capture error: {err}");
                        }
                    }
                }
            })
            .map_err(|e| {
                MediaError::VideoCapture(format!("Failed to spawn video capture thread: {e}"))
            })?;

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

        // Request an explicit keyframe on an already-initialized encoder
        encoder.request_keyframe();
        let annex_b_kf = encoder
            .encode_rgba(&rgba, width, height)
            .expect("encode keyframe");
        let decoded_kf = decoder
            .decode_annex_b(&annex_b_kf)
            .expect("decode keyframe")
            .expect("must produce decoded frame");
        assert_eq!((decoded_kf.0, decoded_kf.1), (width, height));

        // Dynamically change resolution (simulating window resize or fullscreen restore)
        let new_w = 640u32;
        let new_h = 360u32;
        let rgba_resized = MockVideoCapturer::generate_smpte_bars(new_w, new_h, 1);
        let annex_b_resized = encoder
            .encode_rgba(&rgba_resized, new_w, new_h)
            .expect("encode resized frame");
        let decoded_resized = decoder
            .decode_annex_b(&annex_b_resized)
            .expect("decode resized frame")
            .expect("must produce decoded resized frame");
        assert_eq!((decoded_resized.0, decoded_resized.1), (new_w, new_h));
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

    #[test]
    fn test_h264_60fps_encode_budget() {
        let width = 1280u32;
        let height = 720u32;
        let rgba = MockVideoCapturer::generate_smpte_bars(width, height, 0);
        let mut encoder = H264VideoEncoder::new(60, 8_000_000).expect("init 60fps encoder");

        // Warm up with first frame (IDR)
        let _ = encoder.encode_rgba(&rgba, width, height).expect("warmup");

        let start = Instant::now();
        let frame_count = 10;
        for i in 1..=frame_count {
            let frame = MockVideoCapturer::generate_smpte_bars(width, height, i);
            let bitstream = encoder.encode_rgba(&frame, width, height).expect("encode frame");
            assert!(!bitstream.is_empty());
        }
        let total_time = start.elapsed();
        let avg_time_per_frame = total_time / (frame_count as u32);
        println!("Average 720p encode time per frame: {avg_time_per_frame:?}");
        assert!(
            avg_time_per_frame < Duration::from_millis(16),
            "Encode time exceeded 16ms frame budget: {avg_time_per_frame:?}"
        );
    }
}
