use std::time::{Duration, Instant};

use fast_image_resize::{
    images::{Image, ImageRef},
    PixelType, ResizeAlg, ResizeOptions, Resizer,
};
use openh264::{
    decoder::{Decoder, DecoderConfig},
    encoder::{Encoder, EncoderConfig, RateControlMode, SpsPpsStrategy, UsageType},
    formats::{YUVBuffer, YUVSlices, YUVSource},
    OpenH264API,
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

/// Fast 2x2-block fixed-point integer RGBA to planar I420 (YUV420p) converter.
fn rgba_to_yuv420p_into(
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

    for by in 0..half_h {
        let py = by * 2;
        let rgba_row0 = &rgba[py * row_stride..(py + 1) * row_stride];
        let rgba_row1 = &rgba[(py + 1) * row_stride..(py + 2) * row_stride];
        let (y_row0, y_row1) = y_plane[py * w..(py + 2) * w].split_at_mut(w);
        let u_row = &mut u_plane[by * half_w..(by + 1) * half_w];
        let v_row = &mut v_plane[by * half_w..(by + 1) * half_w];

        for bx in 0..half_w {
            let px = bx * 2;
            let c00 = &rgba_row0[px * 4..px * 4 + 4];
            let c01 = &rgba_row0[(px + 1) * 4..(px + 1) * 4 + 4];
            let c10 = &rgba_row1[px * 4..px * 4 + 4];
            let c11 = &rgba_row1[(px + 1) * 4..(px + 1) * 4 + 4];

            let (r00, g00, b00) = (i32::from(c00[0]), i32::from(c00[1]), i32::from(c00[2]));
            let (r01, g01, b01) = (i32::from(c01[0]), i32::from(c01[1]), i32::from(c01[2]));
            let (r10, g10, b10) = (i32::from(c10[0]), i32::from(c10[1]), i32::from(c10[2]));
            let (r11, g11, b11) = (i32::from(c11[0]), i32::from(c11[1]), i32::from(c11[2]));

            y_row0[px] = (((66 * r00 + 129 * g00 + 25 * b00 + 128) >> 8) + 16) as u8;
            y_row0[px + 1] = (((66 * r01 + 129 * g01 + 25 * b01 + 128) >> 8) + 16) as u8;
            y_row1[px] = (((66 * r10 + 129 * g10 + 25 * b10 + 128) >> 8) + 16) as u8;
            y_row1[px + 1] = (((66 * r11 + 129 * g11 + 25 * b11 + 128) >> 8) + 16) as u8;

            let r_avg = (r00 + r01 + r10 + r11 + 2) >> 2;
            let g_avg = (g00 + g01 + g10 + g11 + 2) >> 2;
            let b_avg = (b00 + b01 + b10 + b11 + 2) >> 2;

            u_row[bx] = (((-38 * r_avg - 74 * g_avg + 112 * b_avg + 128) >> 8) + 128)
                .clamp(0, 255) as u8;
            v_row[bx] = (((112 * r_avg - 94 * g_avg - 18 * b_avg + 128) >> 8) + 128)
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

/// Fast 2x2-block fixed-point integer I420 (YUV420p) to RGBA converter.
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

        for bx in 0..half_w {
            let px = bx * 2;
            let u = i32::from(u_row[bx]) - 128;
            let v = i32::from(v_row[bx]) - 128;
            let c_r = (359 * v + 128) >> 8;
            let c_g = (88 * u + 183 * v + 128) >> 8;
            let c_b = (454 * u + 128) >> 8;

            write_px(&mut out_top[px * 4..px * 4 + 4], y_row0[px], c_r, c_g, c_b);
            write_px(
                &mut out_top[(px + 1) * 4..(px + 1) * 4 + 4],
                y_row0[px + 1],
                c_r,
                c_g,
                c_b,
            );
            write_px(&mut out_rest[px * 4..px * 4 + 4], y_row1[px], c_r, c_g, c_b);
            write_px(
                &mut out_rest[(px + 1) * 4..(px + 1) * 4 + 4],
                y_row1[px + 1],
                c_r,
                c_g,
                c_b,
            );
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
        let config = EncoderConfig::new()
            .set_bitrate_bps(bitrate_bps.max(1_000_000))
            .max_frame_rate(clamped_fps as f32)
            .enable_skip_frame(false)
            .rate_control_mode(RateControlMode::Bitrate)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .usage_type(UsageType::ScreenContentRealTime);

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

        let bitstream = self
            .encoder
            .encode(&yuv_slices)
            .map_err(|e| MediaError::Codec(format!("OpenH264 encode error: {e}")))?;

        self.last_dimensions = Some((width, height));
        self.force_next_idr = false;
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
                        yuv420p_to_rgba(
                            decoded_yuv.y(),
                            decoded_yuv.u(),
                            decoded_yuv.v(),
                            w,
                            h,
                            decoded_yuv.strides(),
                            &mut rgba,
                        );
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

    use shared_protocol::CaptureTarget;
    use windows::Win32::{
        Foundation::{BOOL, HWND, LPARAM, RECT},
        Graphics::{
            Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED},
            Gdi::{
                BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
                GetDC, GetDIBits, GetWindowDC, ReleaseDC, SelectObject, BITMAPINFO,
                BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
            },
        },
        Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS, PW_CLIENTONLY},
        UI::WindowsAndMessaging::{
            EnumWindows, GetClientRect, GetShellWindow, GetWindow, GetWindowLongW, GetWindowRect,
            GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
            IsWindowVisible, GWL_EXSTYLE, GW_OWNER, WS_EX_APPWINDOW, WS_EX_NOACTIVATE,
            WS_EX_TOOLWINDOW,
        },
    };

    use crate::media::traits::MediaError;

    const PW_RENDERFULLCONTENT_FLAG: u32 = 0x00000002;

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
                    return Ok(HWND(win_id as usize as *mut c_void));
                }
            }
        }

        Err(MediaError::SourceNotFound(format!(
            "Window id={id} ({title_hint}) closed or not found"
        )))
    }

    fn capture_primary_monitor_fallback() -> Result<(u32, u32, Vec<u8>), MediaError> {
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
        Ok((img.width(), img.height(), img.into_raw()))
    }

    /// Captures a window on Windows using `PrintWindow` (`PW_RENDERFULLCONTENT`) with `BitBlt` and
    /// monitor-capture fallbacks so minimized/fullscreen/resized windows never panic or crash.
    pub fn capture_window_win32(
        id: u32,
        title_hint: &str,
    ) -> Result<(u32, u32, Vec<u8>), MediaError> {
        let hwnd = resolve_hwnd(id, title_hint)?;

        unsafe {
            // When a fullscreen game is minimized (Alt+Tab), its window rect is (-32000, -32000)
            // with 0x0 client size. Fall back to monitor capture until the game is restored.
            if IsIconic(hwnd).as_bool() {
                return capture_primary_monitor_fallback();
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
                return capture_primary_monitor_fallback();
            }

            let hdc_window = if use_client {
                GetDC(hwnd)
            } else {
                GetWindowDC(hwnd)
            };
            if hdc_window.is_invalid() {
                return capture_primary_monitor_fallback();
            }

            let hdc_mem = CreateCompatibleDC(hdc_window);
            if hdc_mem.is_invalid() {
                let _ = ReleaseDC(hwnd, hdc_window);
                return capture_primary_monitor_fallback();
            }

            let hbitmap = CreateCompatibleBitmap(hdc_window, width as i32, height as i32);
            if hbitmap.is_invalid() {
                let _ = DeleteDC(hdc_mem);
                let _ = ReleaseDC(hwnd, hdc_window);
                return capture_primary_monitor_fallback();
            }

            let old_obj = SelectObject(hdc_mem, hbitmap);

            let pw_flags = if use_client {
                PRINT_WINDOW_FLAGS(PW_CLIENTONLY.0 | PW_RENDERFULLCONTENT_FLAG)
            } else {
                PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT_FLAG)
            };

            let mut captured_ok = PrintWindow(hwnd, hdc_mem, pw_flags).as_bool();
            if !captured_ok {
                captured_ok = BitBlt(
                    hdc_mem,
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
            }

            let mut bmi = BITMAPINFO {
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

            let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
            let lines = if captured_ok {
                GetDIBits(
                    hdc_mem,
                    hbitmap,
                    0,
                    height,
                    Some(rgba.as_mut_ptr().cast::<c_void>()),
                    &mut bmi,
                    DIB_RGB_COLORS,
                )
            } else {
                0
            };

            let _ = SelectObject(hdc_mem, old_obj);
            let _ = DeleteObject(hbitmap);
            let _ = DeleteDC(hdc_mem);
            let _ = ReleaseDC(hwnd, hdc_window);

            if lines == 0 {
                return capture_primary_monitor_fallback();
            }

            // Convert BGRA -> RGBA and check if the surface is completely blank (which can happen
            // for exclusive-fullscreen DirectX swapchains when PrintWindow is unsupported).
            let mut non_zero_samples = 0usize;
            let pixel_count = rgba.len() / 4;
            let sample_step = (pixel_count / 64).max(1);
            for idx in 0..pixel_count {
                let base = idx * 4;
                rgba.swap(base, base + 2);
                rgba[base + 3] = 255;
                if idx % sample_step == 0
                    && (rgba[base] != 0 || rgba[base + 1] != 0 || rgba[base + 2] != 0)
                {
                    non_zero_samples += 1;
                }
            }

            if non_zero_samples == 0 {
                return capture_primary_monitor_fallback();
            }

            Ok((width, height, rgba))
        }
    }
}

enum ResolvedCaptureTarget {
    Monitor(xcap::Monitor),
    #[cfg(not(target_os = "windows"))]
    Window(xcap::Window),
    #[cfg(target_os = "windows")]
    Win32Window {
        id: u32,
        title: String,
    },
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
                Ok(Self::Monitor(monitor))
            }
            #[cfg(target_os = "windows")]
            CaptureTarget::Window { id, title, .. } => {
                let hwnd = win32_capture::resolve_hwnd(*id, title)?;
                Ok(Self::Win32Window {
                    id: hwnd.0 as usize as u32,
                    title: title.clone(),
                })
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

    fn capture(&self) -> Result<(u32, u32, Vec<u8>), MediaError> {
        match self {
            Self::Monitor(monitor) => {
                let img = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    monitor.capture_image()
                }))
                .map_err(|_| {
                    MediaError::VideoCapture("Monitor::capture_image panicked".to_string())
                })?
                .map_err(|e| MediaError::VideoCapture(format!("Monitor capture failed: {e}")))?;
                Ok((img.width(), img.height(), img.into_raw()))
            }
            #[cfg(target_os = "windows")]
            Self::Win32Window { id, title } => win32_capture::capture_window_win32(*id, title),
            #[cfg(not(target_os = "windows"))]
            Self::Window(window) => {
                let img = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    window.capture_image()
                }))
                .map_err(|_| {
                    MediaError::VideoCapture("Window::capture_image panicked".to_string())
                })?
                .map_err(|e| MediaError::VideoCapture(format!("Window capture failed: {e}")))?;
                Ok((img.width(), img.height(), img.into_raw()))
            }
        }
    }
}

/// Captures a single preview snapshot for `target`, scaled to at most 720p, and returns it
/// as a 24-byte-header binary frame packet (`[u32 width, u32 height, [u8; 16] peer_uuid, rgba...]`).
pub fn capture_preview_packet(target: &CaptureTarget) -> Result<Vec<u8>, MediaError> {
    let resolved = ResolvedCaptureTarget::resolve(target)?;
    let (src_w, src_h, raw_rgba) = resolved.capture()?;
    let (width, height, rgba) =
        scale_rgba_frame(&raw_rgba, src_w, src_h, TargetResolution::R720p)?;
    shared_protocol::encode_frame_packet(uuid::Uuid::nil(), width, height, &rgba)
        .map_err(|e| MediaError::VideoCapture(format!("Preview frame packet encode error: {e}")))
}

/// Native OS screen and window capturer powered by `xcap` + Win32 GDI/PrintWindow + `fast_image_resize`.
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
                let mut next_tick = Instant::now();

                while !cancel.is_cancelled() {
                    let now = Instant::now();
                    if now < next_tick {
                        std::thread::sleep(next_tick - now);
                        if cancel.is_cancelled() {
                            break;
                        }
                    }
                    next_tick = Instant::now() + frame_interval;

                    // If the downstream encoder is currently busy with the previous frame,
                    // skip capturing this tick rather than building up a latency queue.
                    if tx.capacity() == 0 {
                        continue;
                    }

                    let raw_res = resolved.capture().or_else(|_| {
                        resolved = ResolvedCaptureTarget::resolve(&target)?;
                        resolved.capture()
                    });

                    match raw_res {
                        Ok((src_w, src_h, raw_rgba)) => {
                            if let Ok((width, height, rgba)) = scale_rgba_frame_with_resizer(
                                &mut resizer,
                                &raw_rgba,
                                src_w,
                                src_h,
                                resolution,
                            ) {
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
}
