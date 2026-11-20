use std::os::raw::{c_int, c_void};
use std::ptr::{addr_of_mut, from_mut, null, null_mut};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use fast_image_resize::{
    images::{Image, ImageRef},
    FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
};
use openh264::{
    decoder::{Decoder, DecoderConfig},
    formats::{YUVBuffer, YUVSource},
    OpenH264API,
};
#[cfg(test)]
use openh264_sys2::ENCODER_OPTION_SVC_ENCODE_PARAM_EXT;
use openh264_sys2::{
    videoFormatI420, videoFrameTypeI, videoFrameTypeIDR, EUsageType, ISVCEncoder, ISVCEncoderVtbl,
    SBitrateInfo, SEncParamExt, SFrameBSInfo, SSourcePicture, API as OpenH264SysApi,
    CAMERA_VIDEO_REAL_TIME, CONSTANT_ID, ENCODER_OPTION, ENCODER_OPTION_BITRATE,
    ENCODER_OPTION_DATAFORMAT, ENCODER_OPTION_FRAME_RATE, ENCODER_OPTION_MAX_BITRATE,
    ENCODER_OPTION_TRACE_LEVEL, RC_QUALITY_MODE, SCREEN_CONTENT_REAL_TIME, SM_FIXEDSLCNUM_SLICE,
    SM_SINGLE_SLICE, SPATIAL_LAYER_ALL, WELS_LOG_QUIET,
};
use shared_protocol::{CaptureTarget, TargetResolution};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use yuv::{
    bgra_to_yuv420, bgra_to_yuv_nv12, rgba_to_yuv420, rgba_to_yuv_nv12, yuv420_to_bgra,
    yuv420_to_rgba, BufferStoreMut, YuvBiPlanarImageMut, YuvConversionMode, YuvPlanarImage,
    YuvPlanarImageMut, YuvRange, YuvStandardMatrix,
};

#[cfg(target_os = "windows")]
use crate::media::capture_win::{
    list_windows_win32_with_dims, CapturePoll, DxgiMonitorDuplicator, Win32WindowCapturer,
};
use crate::media::traits::{
    FrameBufferPool, MediaError, RawVideoFrame, SharedFrameBuffer, VideoCapturer, VideoDecoder,
    VideoEncoder,
};

/// Scales an arbitrary RGBA buffer to `resolution` using Bilinear filtering while keeping even dimensions.
pub fn scale_rgba_frame(
    src_rgba: &[u8],
    src_width: u32,
    src_height: u32,
    resolution: TargetResolution,
) -> Result<(u32, u32, Vec<u8>), MediaError> {
    let mut resizer = Resizer::new();
    let pool = FrameBufferPool::new();
    let (w, h, buf) = scale_pixel_buffer_pooled(
        &mut resizer,
        &pool,
        src_rgba,
        src_width,
        src_height,
        resolution,
        false,
    )?;
    Ok((w, h, buf.into_vec_or_clone()))
}

/// Scales a borrowed 32bpp RGBA/BGRA buffer into a recycled pooled buffer using Bilinear filtering.
///
/// When `dst == src`, copies directly into a warm pooled buffer without resizing.
pub fn scale_pixel_buffer_pooled(
    resizer: &mut Resizer,
    pool: &Arc<FrameBufferPool>,
    src_bytes: &[u8],
    src_width: u32,
    src_height: u32,
    resolution: TargetResolution,
    clamp_1080p: bool,
) -> Result<(u32, u32, SharedFrameBuffer), MediaError> {
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
        let mut dst = pool.take_with_len(expected_len);
        dst.copy_from_slice(&src_bytes[..expected_len]);
        return Ok((dst_width, dst_height, pool.wrap(dst)));
    }

    let src_view = ImageRef::new(src_width, src_height, &src_bytes[..expected_len], PixelType::U8x4)
        .map_err(|e| MediaError::VideoCapture(format!("fast_image_resize src error: {e}")))?;

    let dst_len = (dst_width as usize) * (dst_height as usize) * 4;
    let dst_buf = pool.take_with_len(dst_len);
    let mut dst_image = Image::from_vec_u8(dst_width, dst_height, dst_buf, PixelType::U8x4)
        .map_err(|e| MediaError::VideoCapture(format!("fast_image_resize dst error: {e}")))?;
    let options = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(FilterType::Bilinear))
        .use_alpha(false);

    resizer
        .resize(&src_view, &mut dst_image, &options)
        .map_err(|e| MediaError::VideoCapture(format!("fast_image_resize resize error: {e}")))?;

    Ok((dst_width, dst_height, pool.wrap(dst_image.into_vec())))
}

/// Zero-copy pass-through when `dst == src` on an already-owned buffer; otherwise resizes into `pool`.
pub fn scale_owned_pixel_buffer_pooled(
    resizer: &mut Resizer,
    pool: &Arc<FrameBufferPool>,
    mut src_bytes: Vec<u8>,
    src_width: u32,
    src_height: u32,
    resolution: TargetResolution,
    clamp_1080p: bool,
) -> Result<(u32, u32, SharedFrameBuffer), MediaError> {
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
        src_bytes.truncate(expected_len);
        return Ok((dst_width, dst_height, pool.wrap(src_bytes)));
    }

    scale_pixel_buffer_pooled(
        resizer,
        pool,
        &src_bytes[..expected_len],
        src_width,
        src_height,
        resolution,
        clamp_1080p,
    )
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

    let src_stride = (w * 4) as u32;
    let src_slice = &src[..w * h * 4];
    let (y_plane, uv_planes) = out_buf.split_at_mut(y_len);

    if semi_planar_nv12 {
        let mut bi_planar = YuvBiPlanarImageMut {
            y_plane: BufferStoreMut::Borrowed(y_plane),
            y_stride: width,
            uv_plane: BufferStoreMut::Borrowed(uv_planes),
            uv_stride: width,
            width,
            height,
        };
        let res = if is_bgra {
            bgra_to_yuv_nv12(
                &mut bi_planar,
                src_slice,
                src_stride,
                YuvRange::Limited,
                YuvStandardMatrix::Bt601,
                YuvConversionMode::Balanced,
            )
        } else {
            rgba_to_yuv_nv12(
                &mut bi_planar,
                src_slice,
                src_stride,
                YuvRange::Limited,
                YuvStandardMatrix::Bt601,
                YuvConversionMode::Balanced,
            )
        };
        res.map_err(|e| MediaError::Codec(format!("SIMD NV12 conversion error: {e}")))?;
    } else {
        let (u_plane, v_plane) = uv_planes.split_at_mut(uv_len);
        let mut planar = YuvPlanarImageMut {
            y_plane: BufferStoreMut::Borrowed(y_plane),
            y_stride: width,
            u_plane: BufferStoreMut::Borrowed(u_plane),
            u_stride: width / 2,
            v_plane: BufferStoreMut::Borrowed(v_plane),
            v_stride: width / 2,
            width,
            height,
        };
        let res = if is_bgra {
            bgra_to_yuv420(
                &mut planar,
                src_slice,
                src_stride,
                YuvRange::Limited,
                YuvStandardMatrix::Bt601,
                YuvConversionMode::Balanced,
            )
        } else {
            rgba_to_yuv420(
                &mut planar,
                src_slice,
                src_stride,
                YuvRange::Limited,
                YuvStandardMatrix::Bt601,
                YuvConversionMode::Balanced,
            )
        };
        res.map_err(|e| MediaError::Codec(format!("SIMD I420 conversion error: {e}")))?;
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

#[allow(clippy::too_many_arguments)]
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
    let planar = YuvPlanarImage {
        y_plane,
        y_stride: strides.0 as u32,
        u_plane,
        u_stride: strides.1 as u32,
        v_plane,
        v_stride: strides.2 as u32,
        width: width as u32,
        height: height as u32,
    };
    let out_stride = (width * 4) as u32;
    let res = if bgra {
        yuv420_to_bgra(
            &planar,
            out,
            out_stride,
            YuvRange::Limited,
            YuvStandardMatrix::Bt601,
        )
    } else {
        yuv420_to_rgba(
            &planar,
            out,
            out_stride,
            YuvRange::Limited,
            YuvStandardMatrix::Bt601,
        )
    };
    if let Err(err) = res {
        warn!("SIMD YUV420->32bpp conversion error: {err}");
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

fn compute_keyframe_interval(fps: u8) -> u64 {
    let clamped = fps.clamp(1, 60);
    if clamped <= 5 {
        12
    } else {
        (u64::from(clamped) * 3).max(45)
    }
}

fn usage_type_for_fps(fps: u8) -> EUsageType {
    if fps <= 5 {
        SCREEN_CONTENT_REAL_TIME
    } else {
        CAMERA_VIDEO_REAL_TIME
    }
}

/// Direct low-level OpenH264 wrapper that initializes `SEncParamExt` with real multi-slice
/// threading (`SM_FIXEDSLCNUM_SLICE`), disables unused webcam background detection, and writes
/// contiguous Annex-B layers directly into the caller's `BytesMut`.
#[allow(non_snake_case)]
struct RawOpenH264Encoder {
    api: OpenH264API,
    encoder_ptr: *mut *const ISVCEncoderVtbl,
    initialized: bool,
    initialize_ext: unsafe extern "C" fn(*mut ISVCEncoder, *const SEncParamExt) -> c_int,
    get_default_params: unsafe extern "C" fn(*mut ISVCEncoder, *mut SEncParamExt) -> c_int,
    uninitialize: unsafe extern "C" fn(*mut ISVCEncoder) -> c_int,
    encode_frame:
        unsafe extern "C" fn(*mut ISVCEncoder, *const SSourcePicture, *mut SFrameBSInfo) -> c_int,
    force_intra_frame: unsafe extern "C" fn(*mut ISVCEncoder, bool) -> c_int,
    set_option: unsafe extern "C" fn(*mut ISVCEncoder, ENCODER_OPTION, *mut c_void) -> c_int,
    #[cfg_attr(not(test), allow(dead_code))]
    get_option: unsafe extern "C" fn(*mut ISVCEncoder, ENCODER_OPTION, *mut c_void) -> c_int,
    bit_stream_info: SFrameBSInfo,
}

unsafe impl Send for RawOpenH264Encoder {}

impl RawOpenH264Encoder {
    fn new() -> Result<Self, MediaError> {
        let api = OpenH264API::from_source();
        let mut encoder_ptr = null::<ISVCEncoderVtbl>() as *mut *const ISVCEncoderVtbl;
        let rc = unsafe { api.WelsCreateSVCEncoder(from_mut(&mut encoder_ptr)) };
        if rc != 0 || encoder_ptr.is_null() {
            return Err(MediaError::Codec(format!(
                "OpenH264 WelsCreateSVCEncoder failed (rc={rc})"
            )));
        }

        let vtbl = unsafe { &**encoder_ptr };
        let missing = || MediaError::Codec("OpenH264 ISVCEncoder vtable missing fn".to_string());
        let set_option = vtbl.SetOption.ok_or_else(missing)?;
        let mut trace_level = WELS_LOG_QUIET;
        unsafe {
            let _ = set_option(
                encoder_ptr,
                ENCODER_OPTION_TRACE_LEVEL,
                addr_of_mut!(trace_level).cast::<c_void>(),
            );
        }
        Ok(Self {
            api,
            encoder_ptr,
            initialized: false,
            initialize_ext: vtbl.InitializeExt.ok_or_else(missing)?,
            get_default_params: vtbl.GetDefaultParams.ok_or_else(missing)?,
            uninitialize: vtbl.Uninitialize.ok_or_else(missing)?,
            encode_frame: vtbl.EncodeFrame.ok_or_else(missing)?,
            force_intra_frame: vtbl.ForceIntraFrame.ok_or_else(missing)?,
            set_option,
            get_option: vtbl.GetOption.ok_or_else(missing)?,
            bit_stream_info: SFrameBSInfo::default(),
        })
    }

    fn init_for_dimensions(
        &mut self,
        width: u32,
        height: u32,
        fps: u8,
        target_bps: u32,
        max_bps: u32,
    ) -> Result<(), MediaError> {
        let greater_dim = width.max(height);
        let smaller_dim = width.min(height);
        if greater_dim > 3840 || smaller_dim > 2160 {
            return Err(MediaError::Codec(
                "OpenH264 max resolution is 3840x2160 horizontal or 2160x3840 vertical".to_string(),
            ));
        }

        unsafe {
            if self.initialized {
                let _ = (self.uninitialize)(self.encoder_ptr);
                self.initialized = false;
            }

            let mut params = SEncParamExt::default();
            let rc = (self.get_default_params)(self.encoder_ptr, &mut params);
            if rc != 0 {
                return Err(MediaError::Codec(format!(
                    "OpenH264 GetDefaultParams failed (rc={rc})"
                )));
            }

            let clamped_fps = fps.clamp(1, 60);
            let wanted_threads = std::thread::available_parallelism()
                .map(|n| n.get().clamp(2, 4) as u16)
                .unwrap_or(2);
            // OpenH264 requires enough macroblock rows per slice for SM_FIXEDSLCNUM_SLICE.
            let effective_threads = if height >= u32::from(wanted_threads) * 32 {
                wanted_threads
            } else {
                1
            };

            params.iUsageType = usage_type_for_fps(clamped_fps);
            params.iPicWidth = width as c_int;
            params.iPicHeight = height as c_int;
            params.iTargetBitrate = target_bps as c_int;
            params.iMaxBitrate = max_bps.max(target_bps) as c_int;
            params.iRCMode = RC_QUALITY_MODE;
            params.fMaxFrameRate = f32::from(clamped_fps);
            params.bEnableFrameSkip = false;
            params.bEnableDenoise = false;
            params.bEnableBackgroundDetection = false;
            if params.iUsageType == SCREEN_CONTENT_REAL_TIME {
                params.bEnableAdaptiveQuant = false;
            }
            params.eSpsPpsIdStrategy = CONSTANT_ID;
            params.iMultipleThreadIdc = effective_threads;
            if effective_threads > 1 {
                params.iLoopFilterDisableIdc = 2;
            }

            let layer = &mut params.sSpatialLayers[0];
            layer.iVideoWidth = width as c_int;
            layer.iVideoHeight = height as c_int;
            layer.fFrameRate = f32::from(clamped_fps);
            layer.iSpatialBitrate = target_bps as c_int;
            layer.iMaxSpatialBitrate = max_bps.max(target_bps) as c_int;
            if effective_threads > 1 {
                layer.sSliceArgument.uiSliceMode = SM_FIXEDSLCNUM_SLICE;
                layer.sSliceArgument.uiSliceNum = u32::from(effective_threads);
            } else {
                layer.sSliceArgument.uiSliceMode = SM_SINGLE_SLICE;
                layer.sSliceArgument.uiSliceNum = 1;
            }

            let rc = (self.initialize_ext)(self.encoder_ptr, &params);
            if rc != 0 {
                return Err(MediaError::Codec(format!(
                    "OpenH264 InitializeExt failed for {width}x{height} (rc={rc})"
                )));
            }
            self.initialized = true;

            let mut trace_level = WELS_LOG_QUIET;
            let _ = (self.set_option)(
                self.encoder_ptr,
                ENCODER_OPTION_TRACE_LEVEL,
                addr_of_mut!(trace_level).cast::<c_void>(),
            );
            let mut data_format = videoFormatI420;
            let rc = (self.set_option)(
                self.encoder_ptr,
                ENCODER_OPTION_DATAFORMAT,
                addr_of_mut!(data_format).cast::<c_void>(),
            );
            if rc != 0 {
                return Err(MediaError::Codec(format!(
                    "OpenH264 SetOption(DATAFORMAT) failed (rc={rc})"
                )));
            }
        }
        Ok(())
    }

    fn force_intra_frame(&mut self) {
        if self.initialized {
            unsafe {
                let _ = (self.force_intra_frame)(self.encoder_ptr, true);
            }
        }
    }

    unsafe fn set_option(&mut self, opt: ENCODER_OPTION, val: *mut c_void) -> c_int {
        (self.set_option)(self.encoder_ptr, opt, val)
    }

    #[cfg(test)]
    fn current_params(&self) -> Option<SEncParamExt> {
        if !self.initialized {
            return None;
        }
        let mut params = SEncParamExt::default();
        let rc = unsafe {
            (self.get_option)(
                self.encoder_ptr,
                ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
                addr_of_mut!(params).cast::<c_void>(),
            )
        };
        (rc == 0).then_some(params)
    }

    fn encode_i420_into(
        &mut self,
        (y, u, v): (&[u8], &[u8], &[u8]),
        width: u32,
        height: u32,
        ts_ms: i64,
        out_annex_b: &mut BytesMut,
    ) -> Result<bool, MediaError> {
        let w = width as c_int;
        let h = height as c_int;
        let source = SSourcePicture {
            iColorFormat: videoFormatI420,
            iStride: [w, w / 2, w / 2, 0],
            pData: [
                y.as_ptr().cast_mut(),
                u.as_ptr().cast_mut(),
                v.as_ptr().cast_mut(),
                null_mut(),
            ],
            iPicWidth: w,
            iPicHeight: h,
            uiTimeStamp: ts_ms,
        };

        let rc = unsafe { (self.encode_frame)(self.encoder_ptr, &source, &mut self.bit_stream_info) };
        if rc != 0 {
            return Err(MediaError::Codec(format!(
                "OpenH264 EncodeFrame failed (rc={rc})"
            )));
        }

        let layer_count = (self.bit_stream_info.iLayerNum.max(0) as usize)
            .min(self.bit_stream_info.sLayerInfo.len());
        let mut total_bytes = 0usize;
        let mut layer_sizes = [0usize; 128];
        for (idx, layer_size) in layer_sizes.iter_mut().enumerate().take(layer_count) {
            let layer = &self.bit_stream_info.sLayerInfo[idx];
            let nal_count = layer.iNalCount.max(0) as usize;
            if nal_count == 0 || layer.pNalLengthInByte.is_null() || layer.pBsBuf.is_null() {
                continue;
            }
            let nal_lens = unsafe { std::slice::from_raw_parts(layer.pNalLengthInByte, nal_count) };
            let bytes: usize = nal_lens.iter().map(|&n| n.max(0) as usize).sum();
            *layer_size = bytes;
            total_bytes += bytes;
        }

        out_annex_b.reserve(total_bytes);
        for (idx, &bytes) in layer_sizes.iter().enumerate().take(layer_count) {
            if bytes == 0 {
                continue;
            }
            let layer = &self.bit_stream_info.sLayerInfo[idx];
            let slice = unsafe { std::slice::from_raw_parts(layer.pBsBuf, bytes) };
            out_annex_b.extend_from_slice(slice);
        }

        let is_keyframe = self.bit_stream_info.eFrameType == videoFrameTypeIDR
            || self.bit_stream_info.eFrameType == videoFrameTypeI;
        Ok(is_keyframe)
    }
}

impl Drop for RawOpenH264Encoder {
    fn drop(&mut self) {
        unsafe {
            if self.initialized {
                let _ = (self.uninitialize)(self.encoder_ptr);
            }
            self.api.WelsDestroySVCEncoder(self.encoder_ptr);
        }
    }
}

/// Tuned low-latency `OpenH264` VBR encoder with real multi-slice threading and frame skipping disabled.
pub struct H264VideoEncoder {
    encoder: RawOpenH264Encoder,
    fps: u8,
    bitrate_bps: u32,
    max_bitrate_bps: u32,
    frame_count: u64,
    last_ts_ms: Option<u64>,
    keyframe_interval: u64,
    force_next_idr: bool,
    last_dimensions: Option<(u32, u32)>,
    yuv_scratch: Vec<u8>,
}

impl H264VideoEncoder {
    pub fn new(fps: u8, bitrate_bps: u32, max_bitrate_bps: u32) -> Result<Self, MediaError> {
        let clamped_fps = fps.clamp(1, 60);
        let clamped_bps = bitrate_bps.clamp(200_000, 30_000_000);
        let clamped_max = max_bitrate_bps.max(clamped_bps).clamp(200_000, 35_000_000);
        let encoder = RawOpenH264Encoder::new()?;

        Ok(Self {
            encoder,
            fps: clamped_fps,
            bitrate_bps: clamped_bps,
            max_bitrate_bps: clamped_max,
            frame_count: 0,
            last_ts_ms: None,
            keyframe_interval: compute_keyframe_interval(clamped_fps),
            force_next_idr: true,
            last_dimensions: None,
            yuv_scratch: Vec::new(),
        })
    }

    fn apply_bitrate_options(&mut self, prev_target: u32) {
        let mut target_info = SBitrateInfo {
            iLayer: SPATIAL_LAYER_ALL,
            iBitrate: self.bitrate_bps as i32,
        };
        let mut max_info = SBitrateInfo {
            iLayer: SPATIAL_LAYER_ALL,
            iBitrate: self.max_bitrate_bps as i32,
        };
        let (rc1, rc2) = unsafe {
            if self.bitrate_bps >= prev_target {
                let r1 = self.encoder.set_option(
                    ENCODER_OPTION_MAX_BITRATE,
                    addr_of_mut!(max_info).cast::<c_void>(),
                );
                let r2 = self.encoder.set_option(
                    ENCODER_OPTION_BITRATE,
                    addr_of_mut!(target_info).cast::<c_void>(),
                );
                (r1, r2)
            } else {
                let r1 = self.encoder.set_option(
                    ENCODER_OPTION_BITRATE,
                    addr_of_mut!(target_info).cast::<c_void>(),
                );
                let r2 = self.encoder.set_option(
                    ENCODER_OPTION_MAX_BITRATE,
                    addr_of_mut!(max_info).cast::<c_void>(),
                );
                (r1, r2)
            }
        };
        if rc1 != 0 || rc2 != 0 {
            warn!(
                rc1,
                rc2,
                target = self.bitrate_bps,
                max = self.max_bitrate_bps,
                "OpenH264 dynamic bitrate SetOption failed"
            );
        }
    }

    pub fn request_keyframe(&mut self) {
        self.force_next_idr = true;
    }

    pub fn last_yuv420p(&self) -> &[u8] {
        &self.yuv_scratch
    }

    #[cfg(test)]
    fn current_enc_params(&self) -> Option<SEncParamExt> {
        self.encoder.current_params()
    }

    fn encode_yuv_scratch_into(
        &mut self,
        width: u32,
        height: u32,
        timestamp_us: Option<u64>,
        out_annex_b: &mut BytesMut,
    ) -> Result<bool, MediaError> {
        if self.last_dimensions != Some((width, height)) {
            self.encoder.init_for_dimensions(
                width,
                height,
                self.fps,
                self.bitrate_bps,
                self.max_bitrate_bps,
            )?;
            self.last_dimensions = Some((width, height));
            self.last_ts_ms = None;
            self.force_next_idr = false;
        } else if self.force_next_idr || self.frame_count.is_multiple_of(self.keyframe_interval) {
            self.encoder.force_intra_frame();
            self.force_next_idr = false;
        }

        let w = width as usize;
        let h = height as usize;
        let y_len = w * h;
        let uv_len = y_len / 4;
        let (y_plane, uv_planes) = self.yuv_scratch.split_at(y_len);
        let (u_plane, v_plane) = uv_planes.split_at(uv_len);

        let ts_ms = match timestamp_us {
            Some(us) => {
                let raw_ms = us / 1000;
                match self.last_ts_ms {
                    Some(prev) => raw_ms.max(prev.saturating_add(1)),
                    None => raw_ms,
                }
            }
            None => (self.frame_count * 1000) / u64::from(self.fps.max(1)),
        };

        let is_keyframe = self.encoder.encode_i420_into(
            (y_plane, u_plane, v_plane),
            width,
            height,
            ts_ms as i64,
            out_annex_b,
        )?;

        self.last_ts_ms = Some(ts_ms);
        self.frame_count = self.frame_count.wrapping_add(1);
        Ok(is_keyframe)
    }

    pub fn encode_rgba(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, MediaError> {
        rgba_to_yuv420p_into(rgba, width, height, &mut self.yuv_scratch)?;
        let mut out = BytesMut::new();
        let _ = self.encode_yuv_scratch_into(width, height, None, &mut out)?;
        Ok(out.to_vec())
    }

    pub fn encode_bgra(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, MediaError> {
        bgra_to_yuv420p_into(bgra, width, height, &mut self.yuv_scratch)?;
        let mut out = BytesMut::new();
        let _ = self.encode_yuv_scratch_into(width, height, None, &mut out)?;
        Ok(out.to_vec())
    }
}

impl VideoEncoder for H264VideoEncoder {
    fn encode_frame_into(
        &mut self,
        frame: &RawVideoFrame,
        out_annex_b: &mut BytesMut,
    ) -> Result<bool, MediaError> {
        if frame.is_bgra {
            bgra_to_yuv420p_into(&frame.rgba, frame.width, frame.height, &mut self.yuv_scratch)?;
        } else {
            rgba_to_yuv420p_into(&frame.rgba, frame.width, frame.height, &mut self.yuv_scratch)?;
        }
        self.encode_yuv_scratch_into(frame.width, frame.height, Some(frame.timestamp_us), out_annex_b)
    }

    fn request_keyframe(&mut self) {
        self.force_next_idr = true;
    }

    fn set_bitrate(&mut self, target_bps: u32, max_bps: u32) -> Result<(), MediaError> {
        let clamped_target = target_bps.clamp(200_000, 30_000_000);
        let clamped_max = max_bps.max(clamped_target).clamp(200_000, 35_000_000);
        let target_diff = (clamped_target as i64 - self.bitrate_bps as i64).unsigned_abs();
        let max_diff = (clamped_max as i64 - self.max_bitrate_bps as i64).unsigned_abs();
        if target_diff * 20 <= u64::from(self.bitrate_bps.max(1))
            && max_diff * 20 <= u64::from(self.max_bitrate_bps.max(1))
        {
            return Ok(());
        }

        let prev = self.bitrate_bps;
        self.bitrate_bps = clamped_target;
        self.max_bitrate_bps = clamped_max;
        if self.last_dimensions.is_none() {
            return Ok(());
        }

        self.apply_bitrate_options(prev);
        Ok(())
    }

    fn set_target_fps(&mut self, fps: u8) -> Result<(), MediaError> {
        let clamped = fps.clamp(1, 60);
        if clamped == self.fps {
            return Ok(());
        }
        self.fps = clamped;
        self.keyframe_interval = compute_keyframe_interval(clamped);
        if self.last_dimensions.is_none() {
            return Ok(());
        }
        let mut fps_f32 = f32::from(clamped);
        let rc = unsafe {
            self.encoder.set_option(
                ENCODER_OPTION_FRAME_RATE,
                addr_of_mut!(fps_f32).cast::<c_void>(),
            )
        };
        if rc != 0 {
            warn!(rc, fps = clamped, "OpenH264 dynamic FPS SetOption failed");
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
    max_bitrate_bps: u32,
) -> Result<Box<dyn VideoEncoder>, MediaError> {
    #[cfg(target_os = "windows")]
    {
        match crate::media::codec_win::MfH264Encoder::new(fps, bitrate_bps, max_bitrate_bps) {
            Ok(hw) => return Ok(Box::new(hw)),
            Err(err) => {
                warn!("Windows HW H.264 MFT unavailable ({err}); falling back to OpenH264");
            }
        }
    }

    Ok(Box::new(H264VideoEncoder::new(
        fps,
        bitrate_bps,
        max_bitrate_bps,
    )?))
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
fn parse_linux_proc_stat_alive(stat: &str) -> bool {
    let Some(close_paren) = stat.rfind(')') else {
        return false;
    };
    let rest = stat[close_paren + 1..].trim_start();
    let Some(state_char) = rest.chars().next() else {
        return false;
    };
    !matches!(state_char, 'Z' | 'X' | 'x')
}

#[cfg(target_os = "windows")]
pub(crate) fn is_captured_process_alive(pid: u32) -> bool {
    crate::media::capture_win::is_win32_process_alive(pid)
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn is_captured_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return true;
    }
    let stat_path = format!("/proc/{pid}/stat");
    match std::fs::read_to_string(stat_path) {
        Ok(stat) => parse_linux_proc_stat_alive(&stat),
        Err(_) => false,
    }
}

pub(crate) fn matches_window_candidate(
    target_id: u32,
    target_title: &str,
    target_pid: u32,
    cand_id: u32,
    cand_title: &str,
    cand_pid: u32,
) -> bool {
    if target_pid > 0 {
        if cand_pid > 0 && cand_pid != target_pid {
            return false;
        }
        if cand_id == target_id {
            return true;
        }
        return cand_pid == target_pid && !target_title.is_empty() && cand_title == target_title;
    }
    cand_id == target_id
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
        bytes: SharedFrameBuffer,
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
            CaptureTarget::Window { id, title, pid } => {
                if *pid > 0 && !is_captured_process_alive(*pid) {
                    return Err(MediaError::SourceNotFound(format!(
                        "Process pid={pid} for window id={id} ({title}) terminated"
                    )));
                }
                #[cfg(target_os = "windows")]
                {
                    let _ = crate::media::capture_win::resolve_hwnd(*id, title, *pid)?;
                    Ok(Self::Win32Window(Win32WindowCapturer::new(
                        *id,
                        title.clone(),
                        *pid,
                    )))
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let windows = std::panic::catch_unwind(xcap::Window::all)
                        .map_err(|_| MediaError::VideoCapture("xcap Window::all panicked".to_string()))?
                        .map_err(|e| MediaError::VideoCapture(format!("Window::all failed: {e}")))?;

                    let window = windows
                        .into_iter()
                        .find(|w| {
                            let Ok(wid) = w.id() else {
                                return false;
                            };
                            let wtitle = w.title().unwrap_or_default();
                            let wpid = extract_window_pid_linux(w);
                            matches_window_candidate(*id, title, *pid, wid, &wtitle, wpid)
                        })
                        .ok_or_else(|| {
                            MediaError::SourceNotFound(format!(
                                "Window id={id} pid={pid} ({title}) not found or closed"
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
        pool: &Arc<FrameBufferPool>,
        resolution: TargetResolution,
        clamp_1080p: bool,
    ) -> Result<FrameSample, MediaError> {
        match self {
            #[cfg(target_os = "windows")]
            Self::Monitor { xcap_monitor, dxgi } => {
                if let Some(dupl) = dxgi.as_mut() {
                    match dupl.poll_scaled_frame(resizer, pool, resolution, clamp_1080p) {
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
                let (w, h, rgba) = scale_owned_pixel_buffer_pooled(
                    resizer,
                    pool,
                    img.into_raw(),
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
                let (w, h, rgba) = scale_owned_pixel_buffer_pooled(
                    resizer,
                    pool,
                    img.into_raw(),
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
                match capturer.poll_scaled_frame(resizer, pool, resolution, clamp_1080p)? {
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
                let (w, h, rgba) = scale_owned_pixel_buffer_pooled(
                    resizer,
                    pool,
                    img.into_raw(),
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
    let pool = FrameBufferPool::new();
    match resolved.poll_scaled(&mut resizer, &pool, TargetResolution::R720p, true)? {
        FrameSample::New {
            width,
            height,
            source_width,
            source_height,
            bytes,
            is_bgra,
        } => {
            let mut bgra = bytes.into_vec_or_clone();
            if !is_bgra {
                rgba_to_bgra_in_place(&mut bgra);
            }
            Ok(PreviewCapture {
                preview_width: width,
                preview_height: height,
                source_dims: (source_width, source_height),
                bgra,
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
                let pool = FrameBufferPool::new();
                let start = Instant::now();
                let mut next_tick = Instant::now() + frame_interval;
                let mut last_frame: Option<(u32, u32, SharedFrameBuffer, bool)> = None;
                let mut last_sent_at = Instant::now() - Duration::from_secs(1);
                let mut consecutive_window_errors: u32 = 0;
                let mut first_window_error_at: Option<Instant> = None;

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

                    if let CaptureTarget::Window { pid, id, title } = &target {
                        if *pid > 0 && !is_captured_process_alive(*pid) {
                            warn!(
                                pid,
                                id,
                                title = %title,
                                "Captured process exited, stopping video capture"
                            );
                            break;
                        }
                    }

                    if tx.capacity() == 0 {
                        continue;
                    }

                    let poll_res = resolved
                        .poll_scaled(&mut resizer, &pool, resolution, false)
                        .or_else(|err| {
                            if matches!(err, MediaError::SourceNotFound(_)) {
                                return Err(err);
                            }
                            resolved = ResolvedCaptureTarget::resolve(&target)?;
                            resolved.poll_scaled(&mut resizer, &pool, resolution, false)
                        });

                    match poll_res {
                        Ok(FrameSample::New {
                            width,
                            height,
                            bytes,
                            is_bgra,
                            ..
                        }) => {
                            consecutive_window_errors = 0;
                            first_window_error_at = None;
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
                            consecutive_window_errors = 0;
                            first_window_error_at = None;
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
                            if matches!(target, CaptureTarget::Window { .. }) {
                                consecutive_window_errors = consecutive_window_errors.saturating_add(1);
                                let started = *first_window_error_at.get_or_insert_with(Instant::now);
                                if consecutive_window_errors >= 5
                                    && started.elapsed() >= Duration::from_millis(400)
                                {
                                    warn!(
                                        "Captured window failed repeatedly ({consecutive_window_errors} errors), stopping video capture: {err}"
                                    );
                                    break;
                                }
                            }
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

        let mut encoder = H264VideoEncoder::new(60, 6_000_000, 7_000_000).expect("init H264 encoder");
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

    #[test]
    fn test_h264_in_place_bitrate_and_fps_updates() {
        let width = 320u32;
        let height = 240u32;
        let mut bgra = MockVideoCapturer::generate_smpte_bars(width, height, 1);
        rgba_to_bgra_in_place(&mut bgra);

        let mut encoder = H264VideoEncoder::new(60, 6_166_000, 7_000_000).expect("init H264 encoder");
        let mut decoder = H264VideoDecoder::new().expect("init H264 decoder");

        // Frame 0: initial IDR
        let f0 = encoder.encode_bgra(&bgra, width, height).expect("encode f0");
        assert!(is_h264_keyframe(&f0));
        assert!(decoder.decode_annex_b_bgra(&f0).expect("dec f0").is_some());

        // Adjust VBR range and target FPS down in-place without recreating or forcing IDR
        encoder.set_bitrate(2_800_000, 4_500_000).expect("set lower bitrate");
        encoder.set_target_fps(30).expect("set lower fps");

        let f1 = encoder.encode_bgra(&bgra, width, height).expect("encode f1");
        assert!(!f1.is_empty());
        assert!(
            !is_h264_keyframe(&f1),
            "In-place bitrate/FPS update should keep P-frame continuity without IDR burst"
        );
        assert!(decoder.decode_annex_b_bgra(&f1).expect("dec f1").is_some());

        // Raise bitrate back up
        encoder.set_bitrate(7_000_000, 7_000_000).expect("raise bitrate");
        let f2 = encoder.encode_bgra(&bgra, width, height).expect("encode f2");
        assert!(!is_h264_keyframe(&f2));
        assert!(decoder.decode_annex_b_bgra(&f2).expect("dec f2").is_some());

        // Verify 5 FPS reading mode uses ScreenContentRealTime + 12-frame GOP while 15/30/60 stay fast
        assert_eq!(usage_type_for_fps(5), SCREEN_CONTENT_REAL_TIME);
        assert_eq!(usage_type_for_fps(15), CAMERA_VIDEO_REAL_TIME);
        assert_eq!(usage_type_for_fps(30), CAMERA_VIDEO_REAL_TIME);
        assert_eq!(usage_type_for_fps(60), CAMERA_VIDEO_REAL_TIME);
        let mut reader_enc = H264VideoEncoder::new(5, 800_000, 900_000).expect("init 5fps encoder");
        assert_eq!(reader_enc.keyframe_interval, 12);
        let rf0 = reader_enc.encode_bgra(&bgra, width, height).expect("encode 5fps f0");
        assert!(is_h264_keyframe(&rf0));
        assert!(decoder.decode_annex_b_bgra(&rf0).expect("dec 5fps f0").is_some());
    }

    #[test]
    fn test_openh264_multislice_and_bgd_config() {
        let width = 1280u32;
        let height = 720u32;
        let bgra = vec![128u8; (width * height * 4) as usize];
        let expected_threads = std::thread::available_parallelism()
            .map(|n| n.get().clamp(2, 4) as u16)
            .unwrap_or(2);

        let mut enc60 = H264VideoEncoder::new(60, 3_500_000, 4_000_000).expect("init 60fps");
        let mut out = BytesMut::new();
        let frame = RawVideoFrame {
            width,
            height,
            rgba: bgra.clone().into(),
            is_bgra: true,
            timestamp_us: 0,
        };
        let is_kf0 = enc60.encode_frame_into(&frame, &mut out).expect("encode f0");
        assert!(is_kf0);
        assert!(is_h264_keyframe(&out));

        let params = enc60.current_enc_params().expect("read active SEncParamExt");
        assert_eq!(params.iUsageType, CAMERA_VIDEO_REAL_TIME);
        assert!(!params.bEnableBackgroundDetection);
        assert_eq!(params.iMultipleThreadIdc, expected_threads);
        assert_eq!(
            params.sSpatialLayers[0].sSliceArgument.uiSliceMode,
            SM_FIXEDSLCNUM_SLICE
        );
        assert_eq!(
            params.sSpatialLayers[0].sSliceArgument.uiSliceNum,
            u32::from(expected_threads)
        );

        // Next frame should be a P-frame (O(1) keyframe flag agrees with Annex-B scan)
        out.clear();
        let is_kf1 = enc60.encode_frame_into(&frame, &mut out).expect("encode f1");
        assert!(!is_kf1);
        assert!(!is_h264_keyframe(&out));

        // 5 FPS reading mode keeps ScreenContentRealTime with multi-slice enabled too
        let mut enc5 = H264VideoEncoder::new(5, 800_000, 900_000).expect("init 5fps");
        out.clear();
        let _ = enc5.encode_frame_into(&frame, &mut out).expect("encode 5fps");
        let params5 = enc5.current_enc_params().expect("read 5fps SEncParamExt");
        assert_eq!(params5.iUsageType, SCREEN_CONTENT_REAL_TIME);
        assert!(!params5.bEnableBackgroundDetection);
        assert_eq!(params5.iMultipleThreadIdc, expected_threads);
    }

    #[test]
    fn test_simd_yuv_roundtrip_and_parity() {
        let w = 640u32;
        let h = 360u32;
        let rgba = MockVideoCapturer::generate_smpte_bars(w, h, 3);
        let mut bgra = rgba.clone();
        rgba_to_bgra_in_place(&mut bgra);

        let mut i420_from_rgba = Vec::new();
        let mut i420_from_bgra = Vec::new();
        rgba_to_yuv420p_into(&rgba, w, h, &mut i420_from_rgba).expect("rgba->i420");
        bgra_to_yuv420p_into(&bgra, w, h, &mut i420_from_bgra).expect("bgra->i420");
        assert_eq!(i420_from_rgba, i420_from_bgra);

        let mut nv12_from_rgba = Vec::new();
        let mut nv12_from_bgra = Vec::new();
        rgba_to_nv12_into(&rgba, w, h, &mut nv12_from_rgba).expect("rgba->nv12");
        bgra_to_nv12_into(&bgra, w, h, &mut nv12_from_bgra).expect("bgra->nv12");
        assert_eq!(nv12_from_rgba, nv12_from_bgra);

        // Round-trip BGRA -> I420 -> BGRA stays close on SMPTE bar interiors
        let restored = yuv420p_planar_to_bgra(&i420_from_bgra, w, h).expect("i420->bgra");
        assert_eq!(restored.len(), bgra.len());
        let sample_px = ((h / 2) * w + (w / 14)) as usize * 4;
        for ch in 0..3 {
            let diff = (i32::from(restored[sample_px + ch]) - i32::from(bgra[sample_px + ch])).abs();
            assert!(diff <= 6, "channel {ch} diff too high: {diff}");
        }
        assert_eq!(restored[sample_px + 3], 255);
    }

    #[test]
    #[ignore]
    fn bench_frame_generation_1080p() {
        use shared_protocol::{VideoFrameHeader, VIDEO_FLAG_H264, VIDEO_FLAG_KEYFRAME, VIDEO_HEADER_LEN};

        let w = 1920u32;
        let h = 1080u32;
        let iters = 120usize;

        // Pre-generate 2 moving 1080p BGRA frames + 1440p source for scale bench
        let mut frames_1080p: Vec<Vec<u8>> = (0..4u64)
            .map(|i| {
                let mut f = MockVideoCapturer::generate_smpte_bars(w, h, i * 17);
                rgba_to_bgra_in_place(&mut f);
                f
            })
            .collect();
        let mut src_1440p = MockVideoCapturer::generate_smpte_bars(2560, 1440, 5);
        rgba_to_bgra_in_place(&mut src_1440p);

        // 1. BGRA -> I420
        let mut i420 = Vec::new();
        bgra_to_yuv420p_into(&frames_1080p[0], w, h, &mut i420).unwrap();
        let t0 = Instant::now();
        for i in 0..iters {
            bgra_to_yuv420p_into(&frames_1080p[i % frames_1080p.len()], w, h, &mut i420).unwrap();
        }
        let yuv_i420_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;

        // 2. BGRA -> NV12
        let mut nv12 = Vec::new();
        bgra_to_nv12_into(&frames_1080p[0], w, h, &mut nv12).unwrap();
        let t0 = Instant::now();
        for i in 0..iters {
            bgra_to_nv12_into(&frames_1080p[i % frames_1080p.len()], w, h, &mut nv12).unwrap();
        }
        let yuv_nv12_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;

        // 3. I420 -> BGRA (viewer side)
        let mut bgra_out = vec![0u8; (w * h * 4) as usize];
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let t0 = Instant::now();
        for _ in 0..iters {
            yuv420p_to_32bpp(
                &i420[..y_len],
                &i420[y_len..y_len + uv_len],
                &i420[y_len + uv_len..y_len + uv_len * 2],
                w as usize,
                h as usize,
                (w as usize, (w / 2) as usize, (w / 2) as usize),
                &mut bgra_out,
                true,
            );
        }
        let i420_to_bgra_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;

        // 4. Pooled passthrough & 1440p -> 1080p resize
        let mut resizer = Resizer::new();
        let pool = FrameBufferPool::new();
        let t0 = Instant::now();
        for i in 0..iters {
            let (_, _, buf) = scale_pixel_buffer_pooled(
                &mut resizer,
                &pool,
                &frames_1080p[i % frames_1080p.len()],
                w,
                h,
                TargetResolution::R1080p,
                false,
            )
            .unwrap();
            drop(buf);
        }
        let passthrough_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;

        let t0 = Instant::now();
        for _ in 0..iters {
            let (_, _, buf) = scale_pixel_buffer_pooled(
                &mut resizer,
                &pool,
                &src_1440p,
                2560,
                1440,
                TargetResolution::R1080p,
                false,
            )
            .unwrap();
            drop(buf);
        }
        let scale_1440_to_1080_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;

        // 5. Full pipeline: BGRA -> I420 -> OpenH264 1080p60 (moving) -> QUIC packet
        let mut enc60 = H264VideoEncoder::new(60, 6_166_000, 7_000_000).unwrap();
        let mut pkt_buf = BytesMut::with_capacity(256 * 1024);
        let shared_frames: Vec<RawVideoFrame> = frames_1080p
            .drain(..)
            .enumerate()
            .map(|(idx, data)| RawVideoFrame {
                width: w,
                height: h,
                rgba: data.into(),
                is_bgra: true,
                timestamp_us: idx as u64 * 16_666,
            })
            .collect();
        // Warmup
        pkt_buf.resize(VIDEO_HEADER_LEN, 0);
        let _ = enc60.encode_frame_into(&shared_frames[0], &mut pkt_buf).unwrap();
        pkt_buf.clear();

        let sender_id = uuid::Uuid::new_v4();
        let t0 = Instant::now();
        for i in 0..iters {
            let f = &shared_frames[i % shared_frames.len()];
            pkt_buf.resize(VIDEO_HEADER_LEN, 0);
            let is_kf = enc60.encode_frame_into(f, &mut pkt_buf).unwrap();
            let hdr = VideoFrameHeader {
                sender_id,
                frame_seq: i as u32,
                timestamp_us: i as u64 * 16_666,
                width: w as u16,
                height: h as u16,
                flags: VIDEO_FLAG_H264 | if is_kf { VIDEO_FLAG_KEYFRAME } else { 0 },
            };
            hdr.write_prefix(&mut pkt_buf[..VIDEO_HEADER_LEN]).unwrap();
            let _pkt = pkt_buf.split().freeze();
        }
        let full_60_moving_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;

        // 6. Full pipeline 1080p60 static
        let t0 = Instant::now();
        for i in 0..iters {
            pkt_buf.resize(VIDEO_HEADER_LEN, 0);
            let is_kf = enc60.encode_frame_into(&shared_frames[0], &mut pkt_buf).unwrap();
            let hdr = VideoFrameHeader {
                sender_id,
                frame_seq: (iters + i) as u32,
                timestamp_us: (iters + i) as u64 * 16_666,
                width: w as u16,
                height: h as u16,
                flags: VIDEO_FLAG_H264 | if is_kf { VIDEO_FLAG_KEYFRAME } else { 0 },
            };
            hdr.write_prefix(&mut pkt_buf[..VIDEO_HEADER_LEN]).unwrap();
            let _pkt = pkt_buf.split().freeze();
        }
        let full_60_static_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;

        // 7. Full pipeline 1080p5 (Texto Nitido)
        let mut enc5 = H264VideoEncoder::new(5, 1_533_000, 1_800_000).unwrap();
        let iters_5 = 30usize;
        let t0 = Instant::now();
        for i in 0..iters_5 {
            let f = &shared_frames[i % shared_frames.len()];
            pkt_buf.resize(VIDEO_HEADER_LEN, 0);
            let _ = enc5.encode_frame_into(f, &mut pkt_buf).unwrap();
            pkt_buf.clear();
        }
        let full_5_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters_5 as f64;

        let active_threads = enc60
            .current_enc_params()
            .map_or(0, |p| p.iMultipleThreadIdc);

        println!("=== 1080p Frame Generation Benchmark ({iters} frames, OpenH264 threads={active_threads}) ===");
        println!("  BGRA -> I420 (SIMD)           : {yuv_i420_ms:.2} ms/frame");
        println!("  BGRA -> NV12 (SIMD)           : {yuv_nv12_ms:.2} ms/frame");
        println!("  I420 -> BGRA (SIMD viewer)    : {i420_to_bgra_ms:.2} ms/frame");
        println!("  1080p pooled passthrough      : {passthrough_ms:.2} ms/frame");
        println!("  1440p -> 1080p Bilinear pool  : {scale_1440_to_1080_ms:.2} ms/frame");
        println!("  Full 1080p60 moving -> packet : {full_60_moving_ms:.2} ms/frame ({:.1} FPS max)", 1000.0 / full_60_moving_ms);
        println!("  Full 1080p60 static -> packet : {full_60_static_ms:.2} ms/frame ({:.1} FPS max)", 1000.0 / full_60_static_ms);
        println!("  Full 1080p5 Texto Nitido      : {full_5_ms:.2} ms/frame ({:.1} FPS max)", 1000.0 / full_5_ms);
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

    #[test]
    fn test_window_candidate_strict_pid_and_id_matching() {
        // Exact ID + matching PID passes even if title changed
        assert!(matches_window_candidate(
            100,
            "Game - Lobby",
            4242,
            100,
            "Game - In Match",
            4242
        ));
        // Recreated window within the SAME PID matches exact title
        assert!(matches_window_candidate(
            100,
            "Game",
            4242,
            200,
            "Game",
            4242
        ));
        // Never hijack another process's window just because title matches
        assert!(!matches_window_candidate(
            100,
            "Game",
            4242,
            200,
            "Game",
            9999
        ));
        // Even if OS recycled ID, reject when PID differs
        assert!(!matches_window_candidate(
            100,
            "Game",
            4242,
            100,
            "Game",
            9999
        ));
        // Unknown PID only matches exact window ID, never arbitrary title fallback
        assert!(matches_window_candidate(100, "Docs", 0, 100, "Docs", 0));
        assert!(!matches_window_candidate(100, "Docs", 0, 200, "Docs", 0));
    }

    #[test]
    fn test_process_liveness_checks() {
        assert!(is_captured_process_alive(0));
        assert!(is_captured_process_alive(std::process::id()));
        assert!(!is_captured_process_alive(u32::MAX - 1));

        #[cfg(not(target_os = "windows"))]
        {
            assert!(parse_linux_proc_stat_alive("1234 (my (app) name) S 1 1 1 0 -1 0"));
            assert!(parse_linux_proc_stat_alive("1234 (game) R 1 1 1 0 -1 0"));
            assert!(!parse_linux_proc_stat_alive("1234 (my (app) name) Z 1 1 1 0 -1 0"));
            assert!(!parse_linux_proc_stat_alive("1234 (game) X 1 1 1 0 -1 0"));
            assert!(!parse_linux_proc_stat_alive("malformed"));
        }
    }
}
