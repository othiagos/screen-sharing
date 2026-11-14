use std::time::{Duration, Instant};

use fast_image_resize::{
    images::{Image, ImageRef},
    FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
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
use tracing::{info, warn};

#[cfg(target_os = "windows")]
use crate::media::capture_win::{
    list_windows_win32_with_dims, CapturePoll, DxgiMonitorDuplicator, Win32WindowCapturer,
};
use crate::media::traits::{
    MediaError, RawVideoFrame, VideoCapturer, VideoDecoder, VideoEncoder,
};

/// Scales an arbitrary RGBA buffer to `resolution` using Bilinear filtering while keeping even dimensions.
pub fn scale_rgba_frame(
    src_rgba: &[u8],
    src_width: u32,
    src_height: u32,
    resolution: TargetResolution,
) -> Result<(u32, u32, Vec<u8>), MediaError> {
    let mut resizer = Resizer::new();
    scale_rgba_frame_with_resizer(&mut resizer, src_rgba, src_width, src_height, resolution, false)
}

pub fn scale_rgba_frame_with_resizer(
    resizer: &mut Resizer,
    src_rgba: &[u8],
    src_width: u32,
    src_height: u32,
    resolution: TargetResolution,
    clamp_1080p: bool,
) -> Result<(u32, u32, Vec<u8>), MediaError> {
    scale_pixel_buffer_with_resizer(
        resizer,
        src_rgba,
        src_width,
        src_height,
        resolution,
        clamp_1080p,
    )
}

pub fn scale_bgra_frame_with_resizer(
    resizer: &mut Resizer,
    src_bgra: &[u8],
    src_width: u32,
    src_height: u32,
    resolution: TargetResolution,
    clamp_1080p: bool,
) -> Result<(u32, u32, Vec<u8>), MediaError> {
    scale_pixel_buffer_with_resizer(
        resizer,
        src_bgra,
        src_width,
        src_height,
        resolution,
        clamp_1080p,
    )
}

fn scale_pixel_buffer_with_resizer(
    resizer: &mut Resizer,
    src_bytes: &[u8],
    src_width: u32,
    src_height: u32,
    resolution: TargetResolution,
    clamp_1080p: bool,
) -> Result<(u32, u32, Vec<u8>), MediaError> {
    let expected_len = (src_width as usize)
        .checked_mul(src_height as usize)
        .and_then(|px| px.checked_mul(4))
        .unwrap_or(0);
    if src_width == 0 || src_height == 0 || src_bytes.len() < expected_len {
        return Err(MediaError::VideoCapture(format!(
            "Invalid source 32bpp buffer for {src_width}x{src_height}: got {} bytes",
            src_bytes.len()
        )));
    }

    let (dst_width, dst_height) = resolution.compute_dimensions(src_width, src_height, clamp_1080p);
    if dst_width == src_width && dst_height == src_height {
        return Ok((dst_width, dst_height, src_bytes[..expected_len].to_vec()));
    }

    let src_view = ImageRef::new(src_width, src_height, &src_bytes[..expected_len], PixelType::U8x4)
        .map_err(|e| MediaError::VideoCapture(format!("fast_image_resize src error: {e}")))?;

    let mut dst_image = Image::new(dst_width, dst_height, PixelType::U8x4);
    let options = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(FilterType::Bilinear))
        .use_alpha(false);

    resizer
        .resize(&src_view, &mut dst_image, &options)
        .map_err(|e| MediaError::VideoCapture(format!("fast_image_resize resize error: {e}")))?;

    Ok((dst_width, dst_height, dst_image.into_vec()))
}

#[inline(always)]
fn validate_even_yuv_dims(buf: &[u8], width: u32, height: u32) -> Result<(usize, usize), MediaError> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(MediaError::Codec(format!(
            "YUV420 requires non-zero even dimensions, got {width}x{height}"
        )));
    }
    let w = width as usize;
    let h = height as usize;
    let expected = w * h * 4;
    if buf.len() < expected {
        return Err(MediaError::Codec(format!(
            "Pixel buffer too small for {width}x{height}: expected {expected}, got {}",
            buf.len()
        )));
    }
    Ok((w, h))
}

#[inline(always)]
fn rgb_to_y(r: i32, g: i32, b: i32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
}

#[inline(always)]
fn rgb_to_uv(r: i32, g: i32, b: i32) -> (u8, u8) {
    let u = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
    let v = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
    (u, v)
}

#[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
fn convert_32bpp_to_yuv_420(
    src: &[u8],
    width: u32,
    height: u32,
    is_bgra: bool,
    semi_planar_nv12: bool,
    out_buf: &mut Vec<u8>,
) -> Result<(), MediaError> {
    let (w, h) = validate_even_yuv_dims(src, width, height)?;
    let y_len = w * h;
    let uv_len = y_len / 4;
    let total = y_len + uv_len * 2;
    if out_buf.len() != total {
        out_buf.resize(total, 0);
    }

    let (y_plane, uv_planes) = out_buf.split_at_mut(y_len);
    let half_w = w / 2;
    let half_h = h / 2;
    let row_stride = w * 4;
    let r_idx = if is_bgra { 2 } else { 0 };
    let b_idx = if is_bgra { 0 } else { 2 };

    for (px_row, y_row) in src.chunks_exact(row_stride).zip(y_plane.chunks_exact_mut(w)) {
        for (px, y_out) in px_row.chunks_exact(4).zip(y_row.iter_mut()) {
            let r = i32::from(px[r_idx]);
            let g = i32::from(px[1]);
            let b = i32::from(px[b_idx]);
            *y_out = rgb_to_y(r, g, b);
        }
    }

    if semi_planar_nv12 {
        for by in 0..half_h {
            let py = by * 2;
            let row0 = &src[py * row_stride..(py + 1) * row_stride];
            let row1 = &src[(py + 1) * row_stride..(py + 2) * row_stride];
            let uv_row = &mut uv_planes[by * w..(by + 1) * w];

            for ((c0, c1), uv_pair) in row0
                .chunks_exact(8)
                .zip(row1.chunks_exact(8))
                .zip(uv_row.chunks_exact_mut(2))
            {
                let r_avg = (i32::from(c0[r_idx])
                    + i32::from(c0[4 + r_idx])
                    + i32::from(c1[r_idx])
                    + i32::from(c1[4 + r_idx])
                    + 2)
                    >> 2;
                let g_avg = (i32::from(c0[1])
                    + i32::from(c0[5])
                    + i32::from(c1[1])
                    + i32::from(c1[5])
                    + 2)
                    >> 2;
                let b_avg = (i32::from(c0[b_idx])
                    + i32::from(c0[4 + b_idx])
                    + i32::from(c1[b_idx])
                    + i32::from(c1[4 + b_idx])
                    + 2)
                    >> 2;

                let (u, v) = rgb_to_uv(r_avg, g_avg, b_avg);
                uv_pair[0] = u;
                uv_pair[1] = v;
            }
        }
    } else {
        let (u_plane, v_plane) = uv_planes.split_at_mut(uv_len);
        for by in 0..half_h {
            let py = by * 2;
            let row0 = &src[py * row_stride..(py + 1) * row_stride];
            let row1 = &src[(py + 1) * row_stride..(py + 2) * row_stride];
            let u_row = &mut u_plane[by * half_w..(by + 1) * half_w];
            let v_row = &mut v_plane[by * half_w..(by + 1) * half_w];

            for (((c0, c1), u_out), v_out) in row0
                .chunks_exact(8)
                .zip(row1.chunks_exact(8))
                .zip(u_row.iter_mut())
                .zip(v_row.iter_mut())
            {
                let r_avg = (i32::from(c0[r_idx])
                    + i32::from(c0[4 + r_idx])
                    + i32::from(c1[r_idx])
                    + i32::from(c1[4 + r_idx])
                    + 2)
                    >> 2;
                let g_avg = (i32::from(c0[1])
                    + i32::from(c0[5])
                    + i32::from(c1[1])
                    + i32::from(c1[5])
                    + 2)
                    >> 2;
                let b_avg = (i32::from(c0[b_idx])
                    + i32::from(c0[4 + b_idx])
                    + i32::from(c1[b_idx])
                    + i32::from(c1[4 + b_idx])
                    + 2)
                    >> 2;

                let (u, v) = rgb_to_uv(r_avg, g_avg, b_avg);
                *u_out = u;
                *v_out = v;
            }
        }
    }

    Ok(())
}

pub fn rgba_to_yuv420p_into(
    rgba: &[u8],
    width: u32,
    height: u32,
    yuv_buf: &mut Vec<u8>,
) -> Result<(), MediaError> {
    convert_32bpp_to_yuv_420(rgba, width, height, false, false, yuv_buf)
}

pub fn bgra_to_yuv420p_into(
    bgra: &[u8],
    width: u32,
    height: u32,
    yuv_buf: &mut Vec<u8>,
) -> Result<(), MediaError> {
    convert_32bpp_to_yuv_420(bgra, width, height, true, false, yuv_buf)
}

pub fn rgba_to_nv12_into(
    rgba: &[u8],
    width: u32,
    height: u32,
    nv12_buf: &mut Vec<u8>,
) -> Result<(), MediaError> {
    convert_32bpp_to_yuv_420(rgba, width, height, false, true, nv12_buf)
}

pub fn bgra_to_nv12_into(
    bgra: &[u8],
    width: u32,
    height: u32,
    nv12_buf: &mut Vec<u8>,
) -> Result<(), MediaError> {
    convert_32bpp_to_yuv_420(bgra, width, height, true, true, nv12_buf)
}

pub fn rgba_to_yuv420p(rgba: &[u8], width: u32, height: u32) -> Result<YUVBuffer, MediaError> {
    let mut yuv_buf = Vec::new();
    rgba_to_yuv420p_into(rgba, width, height, &mut yuv_buf)?;
    Ok(YUVBuffer::from_vec(yuv_buf, width as usize, height as usize))
}

/// Converts a tightly-packed RGBA buffer to BGRA in place (used for GPUI `RenderImage` on Linux/mock sources).
pub fn rgba_to_bgra_in_place(buf: &mut [u8]) {
    let (chunks, _) = buf.as_chunks_mut::<4>();
    for px in chunks {
        px.swap(0, 2);
    }
}

pub fn yuv420p_planar_to_rgba(
    yuv: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, MediaError> {
    yuv420p_planar_to_32bpp(yuv, width, height, false)
}

pub fn yuv420p_planar_to_bgra(
    yuv: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, MediaError> {
    yuv420p_planar_to_32bpp(yuv, width, height, true)
}

fn yuv420p_planar_to_32bpp(
    yuv: &[u8],
    width: u32,
    height: u32,
    bgra: bool,
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
    let mut out = vec![0u8; y_len * 4];
    yuv420p_to_32bpp(
        &yuv[..y_len],
        &yuv[y_len..y_len + uv_len],
        &yuv[y_len + uv_len..y_len + uv_len * 2],
        w,
        h,
        (w, w / 2, w / 2),
        &mut out,
        bgra,
    );
    Ok(out)
}

#[allow(unknown_lints, clippy::chunks_exact_to_as_chunks, clippy::too_many_arguments)]
fn yuv420p_to_32bpp(
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    width: usize,
    height: usize,
    strides: (usize, usize, usize),
    out: &mut [u8],
    bgra: bool,
) {
    #[inline(always)]
    fn write_px(dst: &mut [u8], y_val: u8, c_r: i32, c_g: i32, c_b: i32, bgra: bool) {
        let y = i32::from(y_val);
        let r = (y + c_r).clamp(0, 255) as u8;
        let g = (y - c_g).clamp(0, 255) as u8;
        let b = (y + c_b).clamp(0, 255) as u8;
        if bgra {
            dst[0] = b;
            dst[1] = g;
            dst[2] = r;
        } else {
            dst[0] = r;
            dst[1] = g;
            dst[2] = b;
        }
        dst[3] = 255;
    }

    let half_w = width / 2;
    let half_h = height / 2;
    let out_stride = width * 4;

    for by in 0..half_h {
        let py = by * 2;
        let y_row0 = &y_plane[py * strides.0..py * strides.0 + width];
        let y_row1 = &y_plane[(py + 1) * strides.0..(py + 1) * strides.0 + width];
        let u_row = &u_plane[by * strides.1..by * strides.1 + half_w];
        let v_row = &v_plane[by * strides.2..by * strides.2 + half_w];
        let (out_top, out_rest) =
            out[py * out_stride..(py + 2) * out_stride].split_at_mut(out_stride);

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

            write_px(&mut top_px[0..4], y0[0], c_r, c_g, c_b, bgra);
            write_px(&mut top_px[4..8], y0[1], c_r, c_g, c_b, bgra);
            write_px(&mut bot_px[0..4], y1[0], c_r, c_g, c_b, bgra);
            write_px(&mut bot_px[4..8], y1[1], c_r, c_g, c_b, bgra);
        }
    }
}

/// Scans an H.264 Annex-B bitstream for an IDR keyframe NAL unit (type 5).
pub fn is_h264_keyframe(annex_b: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < annex_b.len() {
        if annex_b[i] == 0 && annex_b[i + 1] == 0 {
            let nal_offset = if annex_b[i + 2] == 1 {
                Some(i + 3)
            } else if i + 4 < annex_b.len() && annex_b[i + 2] == 0 && annex_b[i + 3] == 1 {
                Some(i + 4)
            } else {
                None
            };
            if let Some(off) = nal_offset {
                if off < annex_b.len() && (annex_b[off] & 0x1F) == 5 {
                    return true;
                }
                i = off;
                continue;
            }
        }
        i += 1;
    }
    false
}

/// Tuned low-latency `OpenH264` software encoder with frame skipping disabled to prevent stutters.
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
            .set_bitrate_bps(bitrate_bps.clamp(500_000, 30_000_000))
            .max_frame_rate(clamped_fps as f32)
            .enable_skip_frame(false)
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
            keyframe_interval: ((clamped_fps as u64) * 3).max(45),
            force_next_idr: true,
            last_dimensions: None,
            yuv_scratch: Vec::new(),
        })
    }

    pub fn request_keyframe(&mut self) {
        self.force_next_idr = true;
    }

    pub fn last_yuv420p(&self) -> &[u8] {
        &self.yuv_scratch
    }

    fn encode_yuv_scratch(&mut self, width: u32, height: u32) -> Result<Vec<u8>, MediaError> {
        if self.last_dimensions.is_some_and(|dim| dim != (width, height)) {
            self.encoder = Self::build_encoder(self.fps, self.bitrate_bps)?;
            self.last_dimensions = None;
            self.force_next_idr = true;
        }

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

    pub fn encode_rgba(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, MediaError> {
        rgba_to_yuv420p_into(rgba, width, height, &mut self.yuv_scratch)?;
        self.encode_yuv_scratch(width, height)
    }

    pub fn encode_bgra(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, MediaError> {
        bgra_to_yuv420p_into(bgra, width, height, &mut self.yuv_scratch)?;
        self.encode_yuv_scratch(width, height)
    }
}

impl VideoEncoder for H264VideoEncoder {
    fn encode_frame(&mut self, frame: &RawVideoFrame) -> Result<Vec<u8>, MediaError> {
        if frame.is_bgra {
            self.encode_bgra(&frame.rgba, frame.width, frame.height)
        } else {
            self.encode_rgba(&frame.rgba, frame.width, frame.height)
        }
    }

    fn request_keyframe(&mut self) {
        self.force_next_idr = true;
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), MediaError> {
        let clamped = bitrate_bps.clamp(500_000, 30_000_000);
        let diff = (clamped as i64 - self.bitrate_bps as i64).unsigned_abs();
        if diff * 6 > u64::from(self.bitrate_bps.max(1)) {
            self.bitrate_bps = clamped;
            self.encoder = Self::build_encoder(self.fps, self.bitrate_bps)?;
            self.last_dimensions = None;
            self.force_next_idr = true;
        }
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "OpenH264-CPU"
    }
}

/// Creates the best available H.264 encoder for the current platform (Hardware MFT on Windows, OpenH264 fallback).
pub fn create_best_video_encoder(
    fps: u8,
    bitrate_bps: u32,
) -> Result<Box<dyn VideoEncoder>, MediaError> {
    #[cfg(target_os = "windows")]
    {
        match crate::media::codec_win::MfH264Encoder::new(fps, bitrate_bps) {
            Ok(hw) => return Ok(Box::new(hw)),
            Err(err) => {
                warn!("Windows HW H.264 MFT unavailable ({err}); falling back to OpenH264");
            }
        }
    }

    Ok(Box::new(H264VideoEncoder::new(fps, bitrate_bps)?))
}

/// Low-latency H.264 Annex-B decoder outputting tightly-packed RGBA or BGRA buffers.
pub struct H264VideoDecoder {
    decoder: Decoder,
}

impl H264VideoDecoder {
    pub fn new() -> Result<Self, MediaError> {
        let cfg = DecoderConfig::new();
        let decoder = Decoder::with_api_config(OpenH264API::from_source(), cfg)
            .map_err(|e| MediaError::Codec(format!("OpenH264 decoder init error: {e}")))?;
        Ok(Self { decoder })
    }

    pub fn decode_annex_b_yuv(
        &mut self,
        annex_b: &[u8],
    ) -> Result<Option<(u32, u32, Vec<u8>)>, MediaError> {
        let maybe_yuv = self
            .decoder
            .decode(annex_b)
            .map_err(|e| MediaError::Codec(format!("OpenH264 decode error: {e}")))?;
        let Some(yuv) = maybe_yuv else {
            return Ok(None);
        };
        let (width, height) = yuv.dimensions();
        if width < 2 || height < 2 {
            return Ok(None);
        }
        let y_len = width * height;
        let uv_len = y_len / 4;
        let mut out = vec![0u8; y_len + uv_len * 2];
        let (y_stride, u_stride, v_stride) = yuv.strides();
        let (y_dst, uv_dst) = out.split_at_mut(y_len);
        let (u_dst, v_dst) = uv_dst.split_at_mut(uv_len);

        copy_strided_plane(yuv.y(), y_stride, y_dst, width, height);
        copy_strided_plane(yuv.u(), u_stride, u_dst, width / 2, height / 2);
        copy_strided_plane(yuv.v(), v_stride, v_dst, width / 2, height / 2);
        Ok(Some((width as u32, height as u32, out)))
    }

    pub fn decode_annex_b(
        &mut self,
        annex_b: &[u8],
    ) -> Result<Option<(u32, u32, Vec<u8>)>, MediaError> {
        self.decode_annex_b_32bpp(annex_b, false)
    }

    pub fn decode_annex_b_bgra(
        &mut self,
        annex_b: &[u8],
    ) -> Result<Option<(u32, u32, Vec<u8>)>, MediaError> {
        self.decode_annex_b_32bpp(annex_b, true)
    }

    fn decode_annex_b_32bpp(
        &mut self,
        annex_b: &[u8],
        bgra: bool,
    ) -> Result<Option<(u32, u32, Vec<u8>)>, MediaError> {
        let maybe_yuv = self
            .decoder
            .decode(annex_b)
            .map_err(|e| MediaError::Codec(format!("OpenH264 decode error: {e}")))?;
        let Some(yuv) = maybe_yuv else {
            return Ok(None);
        };
        let (width, height) = yuv.dimensions();
        if width < 2 || height < 2 {
            return Ok(None);
        }
        let mut out = vec![0u8; width * height * 4];
        yuv420p_to_32bpp(
            yuv.y(),
            yuv.u(),
            yuv.v(),
            width,
            height,
            yuv.strides(),
            &mut out,
            bgra,
        );
        Ok(Some((width as u32, height as u32, out)))
    }
}

impl VideoDecoder for H264VideoDecoder {
    fn decode_annex_b_bgra(
        &mut self,
        annex_b: &[u8],
    ) -> Result<Option<(u32, u32, Vec<u8>)>, MediaError> {
        H264VideoDecoder::decode_annex_b_bgra(self, annex_b)
    }

    fn backend_name(&self) -> &'static str {
        "OpenH264-CPU"
    }
}

fn copy_strided_plane(
    src: &[u8],
    src_stride: usize,
    dst: &mut [u8],
    row_width: usize,
    rows: usize,
) {
    if src_stride == row_width && src.len() >= row_width * rows {
        dst.copy_from_slice(&src[..row_width * rows]);
        return;
    }
    for row in 0..rows {
        let s_off = row * src_stride;
        let d_off = row * row_width;
        if s_off + row_width <= src.len() {
            dst[d_off..d_off + row_width].copy_from_slice(&src[s_off..s_off + row_width]);
        }
    }
}

/// UI-facing metadata about an enumerable capture target (human label and source pixel size).
#[derive(Debug, Clone)]
pub struct CaptureSourceInfo {
    pub target: CaptureTarget,
    pub display_label: String,
    pub source_dims: Option<(u32, u32)>,
}

/// Captured thumbnail paired with true source dimensions.
#[derive(Debug, Clone)]
pub struct PreviewCapture {
    pub preview_width: u32,
    pub preview_height: u32,
    pub source_dims: (u32, u32),
    pub bgra: Vec<u8>,
}

fn is_technical_or_generic_monitor_name(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.starts_with(r"\\.\") {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("unknown monitor") || lower == "generic pnp monitor"
}

/// Formats a user-facing monitor label without technical OS device paths (`\\.\DISPLAY1`).
pub fn format_monitor_display_label(
    index_1_based: usize,
    os_name: &str,
    friendly_name: Option<&str>,
) -> String {
    let candidate = friendly_name
        .map(str::trim)
        .filter(|s| !is_technical_or_generic_monitor_name(s))
        .or_else(|| {
            let trimmed = os_name.trim();
            (!is_technical_or_generic_monitor_name(trimmed)).then_some(trimmed)
        });

    match candidate {
        Some(name) => format!("Monitor {index_1_based} — {name}"),
        None => format!("Monitor {index_1_based}"),
    }
}

fn query_monitor_physical_dims(monitor: &xcap::Monitor) -> Option<(u32, u32)> {
    let w = monitor.width().ok().filter(|&v| v > 1)?;
    let h = monitor.height().ok().filter(|&v| v > 1)?;
    #[cfg(target_os = "windows")]
    {
        Some((w, h))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let scale = monitor
            .scale_factor()
            .ok()
            .filter(|s| s.is_finite() && *s > 0.1)
            .unwrap_or(1.0);
        let phys_w = ((w as f32) * scale).round() as u32;
        let phys_h = ((h as f32) * scale).round() as u32;
        (phys_w > 1 && phys_h > 1).then_some((phys_w, phys_h))
    }
}

fn query_window_dims(window: &xcap::Window) -> Option<(u32, u32)> {
    if window.is_minimized().unwrap_or(false) {
        return None;
    }
    let w = window.width().ok().filter(|&v| v > 1)?;
    let h = window.height().ok().filter(|&v| v > 1)?;
    Some((w, h))
}

/// Enumerates monitors, application windows, and Wayland portal (when applicable) with UI metadata.
pub fn list_capture_sources() -> Result<Vec<CaptureSourceInfo>, MediaError> {
    let mut sources = Vec::new();

    #[cfg(target_os = "linux")]
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        sources.push(CaptureSourceInfo {
            target: CaptureTarget::PortalWayland,
            display_label: "Selecionar Tela/Janela pelo Portal Wayland".to_string(),
            source_dims: None,
        });
    }

    match std::panic::catch_unwind(xcap::Monitor::all) {
        Ok(Ok(monitors)) => {
            for (idx, monitor) in monitors.into_iter().enumerate() {
                let Ok(id) = monitor.id() else {
                    continue;
                };
                let os_name = monitor.name().unwrap_or_else(|_| format!("Monitor {id}"));
                let friendly = monitor.friendly_name().ok();
                let display_label =
                    format_monitor_display_label(idx + 1, &os_name, friendly.as_deref());
                let source_dims = query_monitor_physical_dims(&monitor);
                sources.push(CaptureSourceInfo {
                    target: CaptureTarget::Monitor { id, name: os_name },
                    display_label,
                    source_dims,
                });
            }
        }
        Ok(Err(err)) => warn!("Monitor enumeration warning: {err}"),
        Err(_) => warn!("Monitor enumeration panicked inside xcap"),
    }

    #[cfg(target_os = "windows")]
    {
        let mut seen_ids = std::collections::HashSet::new();
        for (win_target, dims) in list_windows_win32_with_dims() {
            let display_label = match &win_target {
                CaptureTarget::Window { id, title, .. } => {
                    seen_ids.insert(*id);
                    title.clone()
                }
                _ => continue,
            };
            sources.push(CaptureSourceInfo {
                target: win_target,
                display_label,
                source_dims: dims,
            });
        }
        if let Ok(Ok(windows)) = std::panic::catch_unwind(xcap::Window::all) {
            for window in windows {
                let Ok(id) = window.id() else {
                    continue;
                };
                let Ok(title) = window.title() else {
                    continue;
                };
                let trimmed = title.trim().to_string();
                if seen_ids.contains(&id) || trimmed.is_empty() {
                    continue;
                }
                let source_dims = query_window_dims(&window);
                sources.push(CaptureSourceInfo {
                    target: CaptureTarget::Window {
                        id,
                        title: trimmed.clone(),
                        pid: window.pid().unwrap_or(0),
                    },
                    display_label: trimmed,
                    source_dims,
                });
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        match std::panic::catch_unwind(xcap::Window::all) {
            Ok(Ok(windows)) => {
                for window in windows {
                    let Ok(title) = window.title() else {
                        continue;
                    };
                    let trimmed = title.trim().to_string();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let Ok(id) = window.id() else {
                        continue;
                    };
                    let pid = extract_window_pid_linux(&window);
                    let source_dims = query_window_dims(&window);
                    sources.push(CaptureSourceInfo {
                        target: CaptureTarget::Window {
                            id,
                            title: trimmed.clone(),
                            pid,
                        },
                        display_label: trimmed,
                        source_dims,
                    });
                }
            }
            Ok(Err(err)) => warn!("Window enumeration warning: {err}"),
            Err(_) => warn!("Window enumeration panicked inside xcap"),
        }
    }

    Ok(sources)
}

/// Enumerates monitors, application windows, and Wayland portal (when applicable).
pub fn list_capture_targets() -> Result<Vec<CaptureTarget>, MediaError> {
    Ok(list_capture_sources()?
        .into_iter()
        .map(|s| s.target)
        .collect())
}

#[cfg(not(target_os = "windows"))]
fn extract_window_pid_linux(window: &xcap::Window) -> u32 {
    if let Some(pid) = window.pid().ok().filter(|&pid| pid != 0) {
        return pid;
    }
    let app_name = window.app_name().unwrap_or_default().trim().to_lowercase();
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

enum ResolvedCaptureTarget {
    #[cfg(target_os = "windows")]
    Monitor {
        xcap_monitor: xcap::Monitor,
        dxgi: Option<DxgiMonitorDuplicator>,
    },
    #[cfg(not(target_os = "windows"))]
    Monitor(xcap::Monitor),
    #[cfg(target_os = "windows")]
    Win32Window(Win32WindowCapturer),
    #[cfg(not(target_os = "windows"))]
    Window(xcap::Window),
}

enum FrameSample {
    New {
        width: u32,
        height: u32,
        source_width: u32,
        source_height: u32,
        bytes: Vec<u8>,
        is_bgra: bool,
    },
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Unchanged,
}

impl ResolvedCaptureTarget {
    fn resolve(target: &CaptureTarget) -> Result<Self, MediaError> {
        match target {
            CaptureTarget::PortalWayland => Err(MediaError::VideoCapture(
                "PortalWayland uses async PipeWire portal stream".to_string(),
            )),
            CaptureTarget::Monitor { id, name } => {
                let monitors = std::panic::catch_unwind(xcap::Monitor::all)
                    .map_err(|_| MediaError::VideoCapture("xcap Monitor::all panicked".to_string()))?
                    .map_err(|e| MediaError::VideoCapture(format!("Monitor::all failed: {e}")))?;

                let monitor = monitors
                    .into_iter()
                    .find(|m| {
                        m.id().is_ok_and(|mid| mid == *id)
                            || (!name.is_empty()
                                && m.name().as_deref().is_ok_and(|mname| mname == name.as_str()))
                    })
                    .ok_or_else(|| {
                        MediaError::SourceNotFound(format!("Monitor id={id} ({name}) not found"))
                    })?;

                #[cfg(target_os = "windows")]
                {
                    let dxgi = match DxgiMonitorDuplicator::new(*id, name) {
                        Ok(d) => Some(d),
                        Err(err) => {
                            warn!("DXGI Desktop Duplication unavailable ({err}), using xcap monitor fallback");
                            None
                        }
                    };
                    Ok(Self::Monitor {
                        xcap_monitor: monitor,
                        dxgi,
                    })
                }
                #[cfg(not(target_os = "windows"))]
                Ok(Self::Monitor(monitor))
            }
            CaptureTarget::Window { id, title, .. } => {
                #[cfg(target_os = "windows")]
                {
                    let _ = crate::media::capture_win::resolve_hwnd(*id, title)?;
                    Ok(Self::Win32Window(Win32WindowCapturer::new(*id, title.clone())))
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let windows = std::panic::catch_unwind(xcap::Window::all)
                        .map_err(|_| MediaError::VideoCapture("xcap Window::all panicked".to_string()))?
                        .map_err(|e| MediaError::VideoCapture(format!("Window::all failed: {e}")))?;

                    let window = windows
                        .into_iter()
                        .find(|w| {
                            w.id().is_ok_and(|wid| wid == *id)
                                || (!title.is_empty()
                                    && w.title()
                                        .as_deref()
                                        .is_ok_and(|wtitle| wtitle == title.as_str()))
                        })
                        .ok_or_else(|| {
                            MediaError::SourceNotFound(format!(
                                "Window id={id} ({title}) not found or closed"
                            ))
                        })?;
                    Ok(Self::Window(window))
                }
            }
        }
    }

    fn poll_scaled(
        &mut self,
        resizer: &mut Resizer,
        resolution: TargetResolution,
        clamp_1080p: bool,
    ) -> Result<FrameSample, MediaError> {
        match self {
            #[cfg(target_os = "windows")]
            Self::Monitor { xcap_monitor, dxgi } => {
                if let Some(dupl) = dxgi.as_mut() {
                    match dupl.poll_scaled_frame(resizer, resolution, clamp_1080p) {
                        Ok(CapturePoll::Frame(width, height, bytes)) => {
                            let (source_width, source_height) =
                                dupl.source_dims().unwrap_or((width, height));
                            return Ok(FrameSample::New {
                                width,
                                height,
                                source_width,
                                source_height,
                                bytes,
                                is_bgra: true,
                            });
                        }
                        Ok(CapturePoll::Unchanged) => return Ok(FrameSample::Unchanged),
                        Err(err) => {
                            tracing::debug!("DXGI capture poll error ({err}); falling back to xcap");
                        }
                    }
                }
                let img = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    xcap_monitor.capture_image()
                }))
                .map_err(|_| MediaError::VideoCapture("Monitor::capture_image panicked".to_string()))?
                .map_err(|e| MediaError::VideoCapture(format!("Monitor capture failed: {e}")))?;
                let (source_width, source_height) = (img.width(), img.height());
                let (w, h, rgba) = scale_rgba_frame_with_resizer(
                    resizer,
                    img.as_raw(),
                    source_width,
                    source_height,
                    resolution,
                    clamp_1080p,
                )?;
                Ok(FrameSample::New {
                    width: w,
                    height: h,
                    source_width,
                    source_height,
                    bytes: rgba,
                    is_bgra: false,
                })
            }
            #[cfg(not(target_os = "windows"))]
            Self::Monitor(monitor) => {
                let img = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    monitor.capture_image()
                }))
                .map_err(|_| MediaError::VideoCapture("Monitor::capture_image panicked".to_string()))?
                .map_err(|e| MediaError::VideoCapture(format!("Monitor capture failed: {e}")))?;
                let (source_width, source_height) = (img.width(), img.height());
                let (w, h, rgba) = scale_rgba_frame_with_resizer(
                    resizer,
                    img.as_raw(),
                    source_width,
                    source_height,
                    resolution,
                    clamp_1080p,
                )?;
                Ok(FrameSample::New {
                    width: w,
                    height: h,
                    source_width,
                    source_height,
                    bytes: rgba,
                    is_bgra: false,
                })
            }
            #[cfg(target_os = "windows")]
            Self::Win32Window(capturer) => {
                match capturer.poll_scaled_frame(resizer, resolution, clamp_1080p)? {
                    CapturePoll::Frame(width, height, bytes) => {
                        let (source_width, source_height) =
                            capturer.last_source_dims().unwrap_or((width, height));
                        Ok(FrameSample::New {
                            width,
                            height,
                            source_width,
                            source_height,
                            bytes,
                            is_bgra: true,
                        })
                    }
                    CapturePoll::Unchanged => Ok(FrameSample::Unchanged),
                }
            }
            #[cfg(not(target_os = "windows"))]
            Self::Window(window) => {
                let img = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    window.capture_image()
                }))
                .map_err(|_| MediaError::VideoCapture("Window::capture_image panicked".to_string()))?
                .map_err(|e| MediaError::VideoCapture(format!("Window capture failed: {e}")))?;
                let (source_width, source_height) = (img.width(), img.height());
                let (w, h, rgba) = scale_rgba_frame_with_resizer(
                    resizer,
                    img.as_raw(),
                    source_width,
                    source_height,
                    resolution,
                    clamp_1080p,
                )?;
                Ok(FrameSample::New {
                    width: w,
                    height: h,
                    source_width,
                    source_height,
                    bytes: rgba,
                    is_bgra: false,
                })
            }
        }
    }
}

/// Captures a single 720p BGRA thumbnail alongside the original source resolution.
pub fn capture_preview_with_source(target: &CaptureTarget) -> Result<PreviewCapture, MediaError> {
    if matches!(target, CaptureTarget::PortalWayland) {
        return Err(MediaError::VideoCapture(
            "Preview ao vivo do Wayland abre após confirmar o seletor do sistema".to_string(),
        ));
    }
    let mut resolved = ResolvedCaptureTarget::resolve(target)?;
    let mut resizer = Resizer::new();
    match resolved.poll_scaled(&mut resizer, TargetResolution::R720p, true)? {
        FrameSample::New {
            width,
            height,
            source_width,
            source_height,
            mut bytes,
            is_bgra,
        } => {
            if !is_bgra {
                rgba_to_bgra_in_place(&mut bytes);
            }
            Ok(PreviewCapture {
                preview_width: width,
                preview_height: height,
                source_dims: (source_width, source_height),
                bgra: bytes,
            })
        }
        FrameSample::Unchanged => Err(MediaError::VideoCapture(
            "Janela minimizada ou sem quadro disponível para preview".to_string(),
        )),
    }
}

/// Captures a single 720p BGRA thumbnail directly usable by GPUI `RenderImage`.
pub fn capture_preview_bgra(target: &CaptureTarget) -> Result<(u32, u32, Vec<u8>), MediaError> {
    let cap = capture_preview_with_source(target)?;
    Ok((cap.preview_width, cap.preview_height, cap.bgra))
}

/// Native OS screen/window capturer (DXGI/WGC on Windows, Portal PipeWire + xcap on Linux).
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
        #[cfg(target_os = "linux")]
        if matches!(target, CaptureTarget::PortalWayland) {
            return crate::media::capture_lin::start_wayland_portal_capture(
                resolution,
                fps,
                tx,
                cancel,
            );
        }

        let clamped_fps = fps.clamp(1, 60) as u64;
        let frame_interval = Duration::from_micros(1_000_000 / clamped_fps);

        std::thread::Builder::new()
            .name("native-video-capture".to_string())
            .spawn(move || {
                let mut resolved = match ResolvedCaptureTarget::resolve(&target) {
                    Ok(r) => r,
                    Err(err) => {
                        warn!("Initial video capture target resolution failed: {err}");
                        return;
                    }
                };
                info!(?target, fps = clamped_fps, ?resolution, "Started native video capture");

                let mut resizer = Resizer::new();
                let start = Instant::now();
                let mut next_tick = Instant::now() + frame_interval;
                let mut last_frame: Option<(u32, u32, Vec<u8>, bool)> = None;
                let mut last_sent_at = Instant::now() - Duration::from_secs(1);

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

                    if tx.capacity() == 0 {
                        continue;
                    }

                    let poll_res = resolved
                        .poll_scaled(&mut resizer, resolution, false)
                        .or_else(|_| {
                            resolved = ResolvedCaptureTarget::resolve(&target)?;
                            resolved.poll_scaled(&mut resizer, resolution, false)
                        });

                    match poll_res {
                        Ok(FrameSample::New {
                            width,
                            height,
                            bytes,
                            is_bgra,
                            ..
                        }) => {
                            last_sent_at = Instant::now();
                            last_frame = Some((width, height, bytes.clone(), is_bgra));
                            let frame = RawVideoFrame {
                                width,
                                height,
                                rgba: bytes,
                                is_bgra,
                                timestamp_us: start.elapsed().as_micros() as u64,
                            };
                            if matches!(
                                tx.try_send(frame),
                                Err(mpsc::error::TrySendError::Closed(_))
                            ) {
                                break;
                            }
                        }
                        Ok(FrameSample::Unchanged) => {
                            // Avoid cloning/re-encoding identical frames 60x/s; emit 2Hz keepalive max.
                            if last_sent_at.elapsed() >= Duration::from_millis(500) {
                                if let Some((w, h, ref bytes, is_bgra)) = last_frame {
                                    last_sent_at = Instant::now();
                                    let frame = RawVideoFrame {
                                        width: w,
                                        height: h,
                                        rgba: bytes.clone(),
                                        is_bgra,
                                        timestamp_us: start.elapsed().as_micros() as u64,
                                    };
                                    if matches!(
                                        tx.try_send(frame),
                                        Err(mpsc::error::TrySendError::Closed(_))
                                    ) {
                                        break;
                                    }
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
            .map_err(|e| MediaError::VideoCapture(format!("Failed to spawn capture thread: {e}")))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::mock::MockVideoCapturer;

    #[test]
    fn test_frame_scaler_bilinear_resolutions() {
        let src_w = 1365u32;
        let src_h = 767u32;
        let src_rgba = vec![200u8; (src_w * src_h * 4) as usize];

        let (w720, h720, buf720) =
            scale_rgba_frame(&src_rgba, src_w, src_h, TargetResolution::R720p)
                .expect("scale to 720p");
        assert_eq!((w720 % 2, h720 % 2), (0, 0));
        assert!(w720 <= 1280 && h720 <= 720);
        assert_eq!(buf720.len(), (w720 * h720 * 4) as usize);

        let (wnat, hnat, bufnat) =
            scale_rgba_frame(&src_rgba, src_w, src_h, TargetResolution::Native)
                .expect("scale to native even");
        assert_eq!((wnat, hnat), (1364, 766));
        assert_eq!(bufnat.len(), (wnat * hnat * 4) as usize);
    }

    #[test]
    fn test_nv12_and_bgra_conversions() {
        let w = 320u32;
        let h = 240u32;
        let rgba = MockVideoCapturer::generate_smpte_bars(w, h, 0);
        let mut nv12 = Vec::new();
        rgba_to_nv12_into(&rgba, w, h, &mut nv12).expect("rgba to nv12");
        assert_eq!(nv12.len(), (w * h * 3 / 2) as usize);

        let mut bgra = rgba.clone();
        rgba_to_bgra_in_place(&mut bgra);
        assert_eq!(bgra[0], rgba[2]);
        assert_eq!(bgra[2], rgba[0]);

        let mut nv12_b = Vec::new();
        bgra_to_nv12_into(&bgra, w, h, &mut nv12_b).expect("bgra to nv12");
        assert_eq!(nv12, nv12_b);
    }

    #[test]
    fn test_h264_encode_decode_bgra_and_keyframe_detection() {
        let width = 320u32;
        let height = 240u32;
        let mut bgra = MockVideoCapturer::generate_smpte_bars(width, height, 0);
        rgba_to_bgra_in_place(&mut bgra);

        let mut encoder = H264VideoEncoder::new(60, 6_000_000).expect("init H264 encoder");
        let mut decoder = H264VideoDecoder::new().expect("init H264 decoder");

        let annex_b = encoder
            .encode_bgra(&bgra, width, height)
            .expect("encode BGRA");
        assert!(!annex_b.is_empty());
        assert!(is_h264_keyframe(&annex_b));

        let decoded = decoder
            .decode_annex_b_bgra(&annex_b)
            .expect("decode BGRA")
            .expect("frame produced");
        assert_eq!((decoded.0, decoded.1), (width, height));
        assert_eq!(decoded.2.len(), (width * height * 4) as usize);

        // Request an explicit keyframe
        encoder.request_keyframe();
        let kf = encoder.encode_bgra(&bgra, width, height).expect("encode kf");
        assert!(is_h264_keyframe(&kf));

        // Resize mid-stream
        let new_w = 640u32;
        let new_h = 360u32;
        let mut resized = MockVideoCapturer::generate_smpte_bars(new_w, new_h, 2);
        rgba_to_bgra_in_place(&mut resized);
        let kf2 = encoder
            .encode_bgra(&resized, new_w, new_h)
            .expect("encode resized");
        let dec2 = decoder
            .decode_annex_b_bgra(&kf2)
            .expect("decode resized")
            .expect("resized frame");
        assert_eq!((dec2.0, dec2.1), (new_w, new_h));
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
        for _ in 0..5 {
            let frame = rx.recv().await.expect("receive frame");
            assert_eq!((frame.width, frame.height), (160, 120));
        }
        cancel.cancel();
        assert!(start.elapsed() >= Duration::from_millis(80));
    }

    #[test]
    fn test_format_monitor_display_label() {
        assert_eq!(
            format_monitor_display_label(1, r"\\.\DISPLAY1", Some("DELL U2720Q")),
            "Monitor 1 — DELL U2720Q"
        );
        assert_eq!(
            format_monitor_display_label(2, r"\\.\DISPLAY2", Some("Unknown Monitor 65537")),
            "Monitor 2"
        );
        assert_eq!(
            format_monitor_display_label(1, r"\\.\DISPLAY1", Some("Generic PnP Monitor")),
            "Monitor 1"
        );
        assert_eq!(
            format_monitor_display_label(1, "HDMI-1", Some("HDMI-1")),
            "Monitor 1 — HDMI-1"
        );
        assert_eq!(
            format_monitor_display_label(2, "", None),
            "Monitor 2"
        );
    }
}
