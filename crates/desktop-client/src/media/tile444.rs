use bytes::{BufMut, BytesMut};
use jpeg_encoder::{ColorType, Encoder as JpegEncoder, SamplingFactor};
use qoi::Channels as QoiChannels;
use shared_protocol::VIDEO_FLAG_TILE_444;
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::media::traits::{MediaError, RawVideoFrame, VideoEncoder};

pub const TILE_SIZE: usize = 64;
pub const TILE444_KEYFRAME_INTERVAL: u64 = 12;
pub const DEFAULT_JPEG_444_QUALITY: u8 = 94;
pub const MIN_JPEG_444_QUALITY: u8 = 82;

const T444_MAGIC: [u8; 4] = *b"T444";
const T444_HEADER_LEN: usize = 12;
const TILE_RECORD_HEADER_LEN: usize = 10;

const FRAME_FLAG_KEYFRAME: u8 = 0x01;

const TILE_ENC_SOLID_RGB: u8 = 0;
const TILE_ENC_QOI_RGB: u8 = 1;
const TILE_ENC_JPEG_444: u8 = 2;

/// Dedicated 4:4:4 dirty-tile screen codec for "Texto Nítido" (5 FPS).
///
/// Splits frames into 64x64 tiles with zero chroma subsampling:
/// - Solid tiles -> 3 bytes (`SolidRgb`, 100% lossless)
/// - Text / UI / code tiles -> `QoiRgb` (100% pixel-exact lossless 4:4:4)
/// - High-entropy photographic/gradient tiles -> `Jpeg444` (`SamplingFactor::R_4_4_4`, YUV 4:4:4)
pub struct Tile444VideoEncoder {
    prev_pixels: Vec<u8>,
    prev_dims: Option<(u32, u32)>,
    prev_is_bgra: bool,
    tile_rgb_scratch: Vec<u8>,
    qoi_scratch: Vec<u8>,
    jpeg_scratch: Vec<u8>,
    frame_count: u64,
    force_next_keyframe: bool,
    nominal_max_bps: u32,
    jpeg_quality: u8,
}

#[derive(Clone, Copy)]
struct TileRect {
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
}

impl Tile444VideoEncoder {
    pub fn new(initial_bps: u32, max_bps: u32) -> Self {
        let nominal_max = max_bps.max(initial_bps).max(600_000);
        Self {
            prev_pixels: Vec::new(),
            prev_dims: None,
            prev_is_bgra: false,
            tile_rgb_scratch: vec![0u8; TILE_SIZE * TILE_SIZE * 3],
            qoi_scratch: vec![0u8; qoi::encode_max_len(TILE_SIZE as u32, TILE_SIZE as u32, 3)],
            jpeg_scratch: Vec::with_capacity(TILE_SIZE * TILE_SIZE * 3),
            frame_count: 0,
            force_next_keyframe: true,
            nominal_max_bps: nominal_max,
            jpeg_quality: DEFAULT_JPEG_444_QUALITY,
        }
    }

    pub fn jpeg_quality(&self) -> u8 {
        self.jpeg_quality
    }

    fn compute_jpeg_quality(target_bps: u32, nominal_max_bps: u32) -> u8 {
        let denom = nominal_max_bps.max(1) as f32;
        let ratio = (target_bps as f32 / denom).clamp(0.0, 1.0);
        if ratio >= 0.80 {
            DEFAULT_JPEG_444_QUALITY
        } else if ratio <= 0.30 {
            MIN_JPEG_444_QUALITY
        } else {
            let t = (ratio - 0.30) / 0.50;
            let span = f32::from(DEFAULT_JPEG_444_QUALITY - MIN_JPEG_444_QUALITY);
            (f32::from(MIN_JPEG_444_QUALITY) + t * span).round() as u8
        }
    }

    fn is_tile_dirty(src: &[u8], prev: &[u8], stride: usize, rect: TileRect) -> bool {
        let row_bytes = rect.w * 4;
        for row in 0..rect.h {
            let off = (rect.y0 + row) * stride + rect.x0 * 4;
            if src[off..off + row_bytes] != prev[off..off + row_bytes] {
                return true;
            }
        }
        false
    }

    /// Extracts tile pixels into tightly-packed RGB in `dst_rgb` and returns `Some([r, g, b])`
    /// when every pixel in the tile shares the exact same RGB color.
    fn extract_tile_rgb(
        src: &[u8],
        stride: usize,
        rect: TileRect,
        is_bgra: bool,
        dst_rgb: &mut [u8],
    ) -> Option<[u8; 3]> {
        let first_off = rect.y0 * stride + rect.x0 * 4;
        let (r_idx, b_idx) = if is_bgra { (2, 0) } else { (0, 2) };
        let first_rgb = [
            src[first_off + r_idx],
            src[first_off + 1],
            src[first_off + b_idx],
        ];
        let mut is_solid = true;

        let mut dst_pos = 0usize;
        for row in 0..rect.h {
            let row_off = (rect.y0 + row) * stride + rect.x0 * 4;
            let row_slice = &src[row_off..row_off + rect.w * 4];
            let (pixels, _) = row_slice.as_chunks::<4>();
            for px in pixels {
                let r = px[r_idx];
                let g = px[1];
                let b = px[b_idx];
                dst_rgb[dst_pos] = r;
                dst_rgb[dst_pos + 1] = g;
                dst_rgb[dst_pos + 2] = b;
                dst_pos += 3;
                if is_solid && (r != first_rgb[0] || g != first_rgb[1] || b != first_rgb[2]) {
                    is_solid = false;
                }
            }
        }

        is_solid.then_some(first_rgb)
    }
}

impl VideoEncoder for Tile444VideoEncoder {
    fn encode_frame_into(
        &mut self,
        frame: &RawVideoFrame,
        out_payload: &mut BytesMut,
    ) -> Result<bool, MediaError> {
        let w = frame.width as usize;
        let h = frame.height as usize;
        if w < 2 || h < 2 || w > usize::from(u16::MAX) || h > usize::from(u16::MAX) {
            return Err(MediaError::Codec(format!(
                "Tile444 unsupported frame dimensions: {}x{}",
                frame.width, frame.height
            )));
        }
        let expected_len = w
            .checked_mul(h)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| MediaError::Codec("Tile444 frame byte size overflow".to_string()))?;
        let src = frame.rgba.as_slice();
        if src.len() < expected_len {
            return Err(MediaError::Codec(format!(
                "Tile444 source buffer too small for {}x{}: expected {expected_len}, got {}",
                frame.width,
                frame.height,
                src.len()
            )));
        }
        let src = &src[..expected_len];

        let dims_changed =
            self.prev_dims != Some((frame.width, frame.height)) || self.prev_is_bgra != frame.is_bgra;
        let is_keyframe = dims_changed
            || self.force_next_keyframe
            || self.frame_count.is_multiple_of(TILE444_KEYFRAME_INTERVAL);

        let cols = w.div_ceil(TILE_SIZE);
        let rows = h.div_ceil(TILE_SIZE);
        let stride = w * 4;

        let header_off = out_payload.len();
        out_payload.resize(header_off + T444_HEADER_LEN, 0);

        let mut encoded_tiles: u16 = 0;

        for ty in 0..rows {
            let y0 = ty * TILE_SIZE;
            let th = (h - y0).min(TILE_SIZE);
            for tx in 0..cols {
                let x0 = tx * TILE_SIZE;
                let tw = (w - x0).min(TILE_SIZE);
                let rect = TileRect {
                    x0,
                    y0,
                    w: tw,
                    h: th,
                };

                if !is_keyframe && !Self::is_tile_dirty(src, &self.prev_pixels, stride, rect) {
                    continue;
                }

                let rgb_len = tw * th * 3;
                let solid = Self::extract_tile_rgb(
                    src,
                    stride,
                    rect,
                    frame.is_bgra,
                    &mut self.tile_rgb_scratch[..rgb_len],
                );

                if let Some(rgb) = solid {
                    out_payload.put_u16_le(tx as u16);
                    out_payload.put_u16_le(ty as u16);
                    out_payload.put_u8(TILE_ENC_SOLID_RGB);
                    out_payload.put_u8(0);
                    out_payload.put_u32_le(3);
                    out_payload.put_slice(&rgb);
                    encoded_tiles = encoded_tiles.saturating_add(1);
                    continue;
                }

                let rgb_slice = &self.tile_rgb_scratch[..rgb_len];
                let qoi_len = qoi::encode_to_buf(
                    &mut self.qoi_scratch,
                    rgb_slice,
                    tw as u32,
                    th as u32,
                )
                .map_err(|e| MediaError::Codec(format!("Tile444 QOI encode failed: {e}")))?;

                // Keep 100% lossless QOI for text/UI/code tiles (<= 50% of raw RGB).
                let lossless_limit = rgb_len / 2;
                let mut use_jpeg = false;

                if qoi_len > lossless_limit {
                    self.jpeg_scratch.clear();
                    let mut jenc = JpegEncoder::new(&mut self.jpeg_scratch, self.jpeg_quality);
                    jenc.set_sampling_factor(SamplingFactor::R_4_4_4);
                    if jenc
                        .encode(rgb_slice, tw as u16, th as u16, ColorType::Rgb)
                        .is_ok()
                        && !self.jpeg_scratch.is_empty()
                        && self.jpeg_scratch.len() < qoi_len
                    {
                        use_jpeg = true;
                    }
                }

                out_payload.put_u16_le(tx as u16);
                out_payload.put_u16_le(ty as u16);
                if use_jpeg {
                    out_payload.put_u8(TILE_ENC_JPEG_444);
                    out_payload.put_u8(0);
                    out_payload.put_u32_le(self.jpeg_scratch.len() as u32);
                    out_payload.put_slice(&self.jpeg_scratch);
                } else {
                    out_payload.put_u8(TILE_ENC_QOI_RGB);
                    out_payload.put_u8(0);
                    out_payload.put_u32_le(qoi_len as u32);
                    out_payload.put_slice(&self.qoi_scratch[..qoi_len]);
                }
                encoded_tiles = encoded_tiles.saturating_add(1);
            }
        }

        // Write 12-byte T444 frame header in-place
        let hdr = &mut out_payload[header_off..header_off + T444_HEADER_LEN];
        hdr[0..4].copy_from_slice(&T444_MAGIC);
        hdr[4] = if is_keyframe { FRAME_FLAG_KEYFRAME } else { 0 };
        hdr[5] = TILE_SIZE as u8;
        hdr[6..8].copy_from_slice(&(frame.width as u16).to_le_bytes());
        hdr[8..10].copy_from_slice(&(frame.height as u16).to_le_bytes());
        hdr[10..12].copy_from_slice(&encoded_tiles.to_le_bytes());

        if self.prev_pixels.len() != expected_len {
            self.prev_pixels.resize(expected_len, 0);
        }
        self.prev_pixels.copy_from_slice(src);
        self.prev_dims = Some((frame.width, frame.height));
        self.prev_is_bgra = frame.is_bgra;
        self.force_next_keyframe = false;
        self.frame_count = self.frame_count.wrapping_add(1);

        Ok(is_keyframe)
    }

    fn request_keyframe(&mut self) {
        self.force_next_keyframe = true;
    }

    fn set_bitrate(&mut self, target_bps: u32, max_bps: u32) -> Result<(), MediaError> {
        if max_bps > self.nominal_max_bps {
            self.nominal_max_bps = max_bps;
        }
        self.jpeg_quality = Self::compute_jpeg_quality(target_bps, self.nominal_max_bps);
        Ok(())
    }

    fn codec_flag(&self) -> u8 {
        VIDEO_FLAG_TILE_444
    }

    fn backend_name(&self) -> &'static str {
        "Tiles-4:4:4"
    }
}

/// Stateful viewer decoder for `T444` (4:4:4 lossless + YUV 4:4:4 JPEG dirty-tile frames).
pub struct Tile444VideoDecoder {
    canvas_bgra: Vec<u8>,
    width: u32,
    height: u32,
    has_keyframe: bool,
    qoi_rgb_scratch: Vec<u8>,
    jpeg_bgra_scratch: Vec<u8>,
}

impl Default for Tile444VideoDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Tile444VideoDecoder {
    pub fn new() -> Self {
        Self {
            canvas_bgra: Vec::new(),
            width: 0,
            height: 0,
            has_keyframe: false,
            qoi_rgb_scratch: vec![0u8; TILE_SIZE * TILE_SIZE * 3],
            jpeg_bgra_scratch: vec![0u8; TILE_SIZE * TILE_SIZE * 4],
        }
    }

    /// Decodes a `T444` packet into the persistent BGRA canvas.
    ///
    /// Returns `Ok(None)` for unchanged static deltas (`tile_count == 0`) to avoid redundant
    /// GPU texture uploads. Returns `Err` on delta-before-keyframe or malformed payload so the
    /// receiver automatically requests a fresh keyframe.
    pub fn decode_frame_bgra(
        &mut self,
        payload: &[u8],
    ) -> Result<Option<(u32, u32, Vec<u8>)>, MediaError> {
        if payload.len() < T444_HEADER_LEN {
            return Err(MediaError::Codec(format!(
                "Tile444 payload too short: {} bytes",
                payload.len()
            )));
        }
        if payload[0..4] != T444_MAGIC {
            return Err(MediaError::Codec("Invalid Tile444 magic header".to_string()));
        }

        let is_keyframe = (payload[4] & FRAME_FLAG_KEYFRAME) != 0;
        let tile_size = usize::from(payload[5]);
        let width = u32::from(u16::from_le_bytes([payload[6], payload[7]]));
        let height = u32::from(u16::from_le_bytes([payload[8], payload[9]]));
        let tile_count = usize::from(u16::from_le_bytes([payload[10], payload[11]]));

        if tile_size == 0 || tile_size > TILE_SIZE || width < 2 || height < 2 {
            return Err(MediaError::Codec(format!(
                "Invalid Tile444 frame geometry: {width}x{height} tile_size={tile_size}"
            )));
        }

        if !is_keyframe && (!self.has_keyframe || self.width != width || self.height != height) {
            return Err(MediaError::Codec(
                "Tile444 delta received before matching keyframe".to_string(),
            ));
        }

        let w = width as usize;
        let h = height as usize;
        let canvas_len = w
            .checked_mul(h)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| MediaError::Codec("Tile444 canvas size overflow".to_string()))?;

        if is_keyframe {
            if self.canvas_bgra.len() != canvas_len {
                self.canvas_bgra.resize(canvas_len, 255);
            }
            self.width = width;
            self.height = height;
            self.has_keyframe = true;
        } else if tile_count == 0 {
            return Ok(None);
        }

        let cols = w.div_ceil(tile_size);
        let rows = h.div_ceil(tile_size);
        let stride = w * 4;
        let mut cursor = T444_HEADER_LEN;

        for _ in 0..tile_count {
            if cursor + TILE_RECORD_HEADER_LEN > payload.len() {
                return Err(MediaError::Codec("Truncated Tile444 tile record header".to_string()));
            }
            let tx = usize::from(u16::from_le_bytes([payload[cursor], payload[cursor + 1]]));
            let ty = usize::from(u16::from_le_bytes([payload[cursor + 2], payload[cursor + 3]]));
            let encoding = payload[cursor + 4];
            let data_len = u32::from_le_bytes([
                payload[cursor + 6],
                payload[cursor + 7],
                payload[cursor + 8],
                payload[cursor + 9],
            ]) as usize;
            cursor += TILE_RECORD_HEADER_LEN;

            if tx >= cols || ty >= rows {
                return Err(MediaError::Codec(format!(
                    "Tile444 coordinates ({tx},{ty}) out of bounds ({cols}x{rows})"
                )));
            }
            if cursor.checked_add(data_len).is_none_or(|end| end > payload.len()) {
                return Err(MediaError::Codec("Truncated Tile444 tile payload".to_string()));
            }
            let data = &payload[cursor..cursor + data_len];
            cursor += data_len;

            let x0 = tx * tile_size;
            let y0 = ty * tile_size;
            let tw = (w - x0).min(tile_size);
            let th = (h - y0).min(tile_size);

            match encoding {
                TILE_ENC_SOLID_RGB => {
                    if data.len() != 3 {
                        return Err(MediaError::Codec(format!(
                            "Invalid SolidRgb tile length: {}",
                            data.len()
                        )));
                    }
                    let bgra = [data[2], data[1], data[0], 255];
                    for row in 0..th {
                        let off = (y0 + row) * stride + x0 * 4;
                        let (dst_px, _) = self.canvas_bgra[off..off + tw * 4].as_chunks_mut::<4>();
                        for px in dst_px {
                            *px = bgra;
                        }
                    }
                }
                TILE_ENC_QOI_RGB => {
                    let rgb_len = tw * th * 3;
                    let qoi_hdr = qoi::decode_to_buf(&mut self.qoi_rgb_scratch[..rgb_len], data)
                        .map_err(|e| MediaError::Codec(format!("Tile444 QOI decode error: {e}")))?;
                    if qoi_hdr.width as usize != tw
                        || qoi_hdr.height as usize != th
                        || qoi_hdr.channels != QoiChannels::Rgb
                    {
                        return Err(MediaError::Codec(format!(
                            "Tile444 QOI geometry mismatch: got {}x{} {:?}, expected {tw}x{th} Rgb",
                            qoi_hdr.width, qoi_hdr.height, qoi_hdr.channels
                        )));
                    }
                    let mut src_pos = 0usize;
                    for row in 0..th {
                        let off = (y0 + row) * stride + x0 * 4;
                        let (dst_px, _) = self.canvas_bgra[off..off + tw * 4].as_chunks_mut::<4>();
                        for px in dst_px {
                            px[0] = self.qoi_rgb_scratch[src_pos + 2];
                            px[1] = self.qoi_rgb_scratch[src_pos + 1];
                            px[2] = self.qoi_rgb_scratch[src_pos];
                            px[3] = 255;
                            src_pos += 3;
                        }
                    }
                }
                TILE_ENC_JPEG_444 => {
                    let options =
                        DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::BGRA);
                    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(data), options);
                    decoder
                        .decode_headers()
                        .map_err(|e| MediaError::Codec(format!("Tile444 JPEG header error: {e}")))?;
                    let info = decoder.info().ok_or_else(|| {
                        MediaError::Codec("Tile444 JPEG missing image info".to_string())
                    })?;
                    if usize::from(info.width) != tw || usize::from(info.height) != th {
                        return Err(MediaError::Codec(format!(
                            "Tile444 JPEG geometry mismatch: got {}x{}, expected {tw}x{th}",
                            info.width, info.height
                        )));
                    }
                    let bgra_len = tw * th * 4;
                    decoder
                        .decode_into(&mut self.jpeg_bgra_scratch[..bgra_len])
                        .map_err(|e| MediaError::Codec(format!("Tile444 JPEG decode error: {e}")))?;
                    let row_bytes = tw * 4;
                    for row in 0..th {
                        let s_off = row * row_bytes;
                        let d_off = (y0 + row) * stride + x0 * 4;
                        self.canvas_bgra[d_off..d_off + row_bytes]
                            .copy_from_slice(&self.jpeg_bgra_scratch[s_off..s_off + row_bytes]);
                    }
                }
                other => {
                    return Err(MediaError::Codec(format!(
                        "Unknown Tile444 tile encoding: {other}"
                    )));
                }
            }
        }

        Ok(Some((width, height, self.canvas_bgra.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn make_frame(width: u32, height: u32, bgra: Vec<u8>) -> RawVideoFrame {
        RawVideoFrame {
            width,
            height,
            rgba: bgra.into(),
            is_bgra: true,
            timestamp_us: 0,
        }
    }

    /// Generates an IDE-like BGRA frame with solid dark background and razor-sharp 1px colored text strokes.
    fn generate_ide_text_frame(width: u32, height: u32, cursor_col: u32) -> Vec<u8> {
        let w = width as usize;
        let h = height as usize;
        let mut bgra = vec![0u8; w * h * 4];
        // Dark editor background (#1E1E24 in BGRA)
        let (bgra_px, _) = bgra.as_chunks_mut::<4>();
        for px in bgra_px {
            *px = [0x24, 0x1E, 0x1E, 0xFF];
        }
        // Colored 1px syntax strokes (red, cyan, yellow, green) where 4:2:0 normally smears chroma
        let palette: [[u8; 4]; 4] = [
            [0x33, 0x33, 0xFF, 0xFF], // Pure red text
            [0xFF, 0xCC, 0x33, 0xFF], // Cyan keyword
            [0x44, 0xEE, 0xFF, 0xFF], // Yellow string
            [0x66, 0xEE, 0x66, 0xFF], // Green comment
        ];
        for line in (16..h.saturating_sub(16)).step_by(18) {
            let color = palette[(line / 18) % palette.len()];
            let max_x = (w / 2).min(w.saturating_sub(20));
            for x in (20..max_x).step_by(3) {
                for dy in 0..10 {
                    if (x + dy) % 2 == 0 {
                        let off = ((line + dy) * w + x) * 4;
                        bgra[off..off + 4].copy_from_slice(&color);
                    }
                }
            }
        }
        // Blinking cursor bar at (cursor_col, 20)
        let cx = (cursor_col as usize).min(w.saturating_sub(4));
        for dy in 0..14 {
            let off = ((20 + dy) * w + cx) * 4;
            bgra[off..off + 4].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        }
        bgra
    }

    /// Generates high-entropy photographic noise + smooth colorful gradients to exercise YUV 4:4:4 JPEG fallback.
    fn generate_photo_frame(width: u32, height: u32) -> Vec<u8> {
        let w = width as usize;
        let h = height as usize;
        let mut bgra = vec![0u8; w * h * 4];
        let mut state: u32 = 0x1234_5678;
        for y in 0..h {
            for x in 0..w {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = ((state >> 24) as i32) - 128;
                let r = (((x * 255) / w.max(1)) as i32 + (noise / 3)).clamp(0, 255) as u8;
                let g = (((y * 255) / h.max(1)) as i32 + (noise / 4)).clamp(0, 255) as u8;
                let b = ((((x ^ y) * 3) & 0xFF) as i32 + (noise / 3)).clamp(0, 255) as u8;
                let off = (y * w + x) * 4;
                bgra[off] = b;
                bgra[off + 1] = g;
                bgra[off + 2] = r;
                bgra[off + 3] = 255;
            }
        }
        bgra
    }

    fn count_tile_encodings(payload: &[u8]) -> (usize, usize, usize) {
        let tile_count = usize::from(u16::from_le_bytes([payload[10], payload[11]]));
        let mut cursor = T444_HEADER_LEN;
        let (mut solid, mut qoi, mut jpeg) = (0, 0, 0);
        for _ in 0..tile_count {
            let enc = payload[cursor + 4];
            let len = u32::from_le_bytes([
                payload[cursor + 6],
                payload[cursor + 7],
                payload[cursor + 8],
                payload[cursor + 9],
            ]) as usize;
            cursor += TILE_RECORD_HEADER_LEN + len;
            match enc {
                TILE_ENC_SOLID_RGB => solid += 1,
                TILE_ENC_QOI_RGB => qoi += 1,
                TILE_ENC_JPEG_444 => jpeg += 1,
                _ => {}
            }
        }
        (solid, qoi, jpeg)
    }

    #[test]
    fn test_tile444_lossless_text_and_solid_tiles_exact_pixels() {
        let w = 640u32;
        let h = 360u32;
        let bgra = generate_ide_text_frame(w, h, 80);
        let frame = make_frame(w, h, bgra.clone());

        let mut enc = Tile444VideoEncoder::new(800_000, 900_000);
        let mut dec = Tile444VideoDecoder::new();
        let mut payload = BytesMut::new();

        let is_kf = enc.encode_frame_into(&frame, &mut payload).expect("encode text kf");
        assert!(is_kf);
        assert_eq!(enc.codec_flag(), VIDEO_FLAG_TILE_444);

        let (solid, qoi_tiles, jpeg_tiles) = count_tile_encodings(&payload);
        assert!(solid > 0, "Expected solid background tiles");
        assert!(qoi_tiles > 0, "Expected QOI lossless text tiles");
        assert_eq!(jpeg_tiles, 0, "Pure text/IDE screen should remain 100% lossless");

        let (dw, dh, decoded) = dec
            .decode_frame_bgra(&payload)
            .expect("decode text kf")
            .expect("frame output");
        assert_eq!((dw, dh), (w, h));
        assert_eq!(decoded, bgra, "Text/IDE tiles must decode bit-for-bit identical");
    }

    #[test]
    fn test_tile444_photographic_tile_uses_yuv444_fallback_and_congestion_scaling() {
        let w = 128u32;
        let h = 128u32;
        let bgra = generate_photo_frame(w, h);
        let frame = make_frame(w, h, bgra.clone());

        let mut enc = Tile444VideoEncoder::new(1_533_000, 1_800_000);
        assert_eq!(enc.jpeg_quality(), DEFAULT_JPEG_444_QUALITY);

        let mut payload = BytesMut::new();
        let is_kf = enc.encode_frame_into(&frame, &mut payload).expect("encode photo");
        assert!(is_kf);
        let (_, _, jpeg_tiles) = count_tile_encodings(&payload);
        assert!(jpeg_tiles > 0, "High-entropy photo tiles should use Jpeg444 fallback");

        let mut dec = Tile444VideoDecoder::new();
        let (_, _, decoded) = dec
            .decode_frame_bgra(&payload)
            .expect("decode photo")
            .expect("decoded frame");
        assert_eq!(decoded.len(), bgra.len());

        // Verify high fidelity (low MAE) with full 4:4:4 chroma retention
        let (src_px, _) = bgra.as_chunks::<4>();
        let (dec_px, _) = decoded.as_chunks::<4>();
        let total_err: u64 = src_px
            .iter()
            .zip(dec_px.iter())
            .map(|(a, b)| {
                (i32::from(a[0]) - i32::from(b[0])).unsigned_abs() as u64
                    + (i32::from(a[1]) - i32::from(b[1])).unsigned_abs() as u64
                    + (i32::from(a[2]) - i32::from(b[2])).unsigned_abs() as u64
            })
            .sum();
        let mae = total_err as f64 / ((w * h * 3) as f64);
        assert!(mae < 8.0, "Expected near-lossless YUV 4:4:4 MAE < 8.0, got {mae:.2}");

        // Congestion scales JPEG 4:4:4 quality smoothly between 94 and 82, never below 82
        enc.set_bitrate(1_533_000, 1_800_000).unwrap();
        assert_eq!(enc.jpeg_quality(), 94);
        enc.set_bitrate(990_000, 1_800_000).unwrap();
        assert!(enc.jpeg_quality() < 94 && enc.jpeg_quality() > 82);
        enc.set_bitrate(400_000, 1_800_000).unwrap();
        assert_eq!(enc.jpeg_quality(), MIN_JPEG_444_QUALITY);
        // Recovers back to 94 when congestion clears
        enc.set_bitrate(1_600_000, 1_800_000).unwrap();
        assert_eq!(enc.jpeg_quality(), DEFAULT_JPEG_444_QUALITY);
    }

    #[test]
    fn test_tile444_delta_dirty_tiles_and_periodic_keyframe_12() {
        let w = 640u32;
        let h = 360u32;
        let bgra0 = generate_ide_text_frame(w, h, 80);
        let bgra1 = generate_ide_text_frame(w, h, 88); // Small cursor move inside 1 tile
        let f0 = make_frame(w, h, bgra0.clone());
        let f1 = make_frame(w, h, bgra1.clone());

        let mut enc = Tile444VideoEncoder::new(800_000, 900_000);
        let mut dec = Tile444VideoDecoder::new();
        let mut buf = BytesMut::new();

        // Frame 0: initial keyframe
        assert!(enc.encode_frame_into(&f0, &mut buf).unwrap());
        let (_, _, dec0) = dec.decode_frame_bgra(&buf).unwrap().unwrap();
        assert_eq!(dec0, bgra0);

        // Frame 1: identical static frame -> 0 dirty tiles, 12-byte payload, decoder skips re-emit
        buf.clear();
        assert!(!enc.encode_frame_into(&f0, &mut buf).unwrap());
        assert_eq!(buf.len(), T444_HEADER_LEN);
        assert!(dec.decode_frame_bgra(&buf).unwrap().is_none());

        // Frame 2: small cursor move -> only 1 dirty tile encoded, exact lossless reconstruction
        buf.clear();
        assert!(!enc.encode_frame_into(&f1, &mut buf).unwrap());
        let tile_count = u16::from_le_bytes([buf[10], buf[11]]);
        assert_eq!(tile_count, 1, "Only the cursor tile should be dirty");
        let (_, _, dec2) = dec.decode_frame_bgra(&buf).unwrap().unwrap();
        assert_eq!(dec2, bgra1);

        // Frames 3..11: deltas; Frame 12: automatic periodic keyframe
        for idx in 3..12u64 {
            buf.clear();
            let kf = enc.encode_frame_into(&f1, &mut buf).unwrap();
            assert!(!kf, "Frame {idx} should be delta");
        }
        buf.clear();
        assert!(
            enc.encode_frame_into(&f1, &mut buf).unwrap(),
            "Frame 12 must emit periodic keyframe"
        );
    }

    #[test]
    fn test_tile444_non_multiple_of_64_resolutions_and_resize() {
        let mut enc = Tile444VideoEncoder::new(1_533_000, 1_800_000);
        let mut dec = Tile444VideoDecoder::new();
        let mut buf = BytesMut::new();

        for (w, h) in [(1366u32, 768u32), (1920, 1080), (640, 360)] {
            let bgra = generate_ide_text_frame(w, h, 42);
            let frame = make_frame(w, h, bgra.clone());
            buf.clear();
            let is_kf = enc.encode_frame_into(&frame, &mut buf).unwrap();
            assert!(is_kf, "Resize to {w}x{h} must trigger keyframe");
            let (dw, dh, decoded) = dec.decode_frame_bgra(&buf).unwrap().unwrap();
            assert_eq!((dw, dh), (w, h));
            assert_eq!(decoded, bgra);
        }
    }

    #[test]
    fn test_tile444_rejects_delta_before_keyframe_and_corrupt_payload() {
        let w = 256u32;
        let h = 256u32;
        let f0 = make_frame(w, h, generate_ide_text_frame(w, h, 40));
        let f1 = make_frame(w, h, generate_ide_text_frame(w, h, 50));

        let mut enc = Tile444VideoEncoder::new(800_000, 900_000);
        let mut buf = BytesMut::new();
        let _ = enc.encode_frame_into(&f0, &mut buf).unwrap();
        buf.clear();
        assert!(!enc.encode_frame_into(&f1, &mut buf).unwrap());

        let mut fresh_dec = Tile444VideoDecoder::new();
        assert!(fresh_dec.decode_frame_bgra(&buf).is_err());
        assert!(fresh_dec.decode_frame_bgra(b"BAD!").is_err());
        assert!(fresh_dec.decode_frame_bgra(&buf[..buf.len().saturating_sub(3)]).is_err());
    }

    #[test]
    #[ignore]
    fn bench_tile444_1080p() {
        let w = 1920u32;
        let h = 1080u32;
        let text0 = make_frame(w, h, generate_ide_text_frame(w, h, 100));
        let text1 = make_frame(w, h, generate_ide_text_frame(w, h, 108));
        let photo = make_frame(w, h, generate_photo_frame(w, h));

        let mut enc = Tile444VideoEncoder::new(1_533_000, 1_800_000);
        let mut dec = Tile444VideoDecoder::new();
        let mut buf = BytesMut::with_capacity(512 * 1024);

        // 1. IDE text keyframe
        let iters = 30;
        let t0 = Instant::now();
        let mut text_kf_bytes = 0usize;
        for _ in 0..iters {
            enc.request_keyframe();
            buf.clear();
            let _ = enc.encode_frame_into(&text0, &mut buf).unwrap();
            text_kf_bytes = buf.len();
        }
        let text_kf_enc_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;
        let t0 = Instant::now();
        for _ in 0..iters {
            let _ = dec.decode_frame_bgra(&buf).unwrap();
        }
        let text_kf_dec_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;

        // 2. Typing delta (small region change)
        let mut delta_bytes = 0usize;
        let t0 = Instant::now();
        for i in 0..iters {
            buf.clear();
            let f = if i % 2 == 0 { &text1 } else { &text0 };
            let _ = enc.encode_frame_into(f, &mut buf).unwrap();
            if buf[4] == 0 {
                delta_bytes = buf.len();
            }
        }
        let delta_enc_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;

        // 3. Static delta (0 changed tiles)
        enc.request_keyframe();
        buf.clear();
        let _ = enc.encode_frame_into(&text0, &mut buf).unwrap();
        let t0 = Instant::now();
        let mut static_bytes = 0usize;
        for _ in 0..10 {
            buf.clear();
            let _ = enc.encode_frame_into(&text0, &mut buf).unwrap();
            static_bytes = buf.len();
        }
        let static_enc_ms = t0.elapsed().as_secs_f64() * 1000.0 / 10.0;

        // 4. Photographic keyframe (YUV 4:4:4 JPEG Q=94 fallback)
        let iters_p = 15;
        let t0 = Instant::now();
        let mut photo_kf_bytes = 0usize;
        for _ in 0..iters_p {
            enc.request_keyframe();
            buf.clear();
            let _ = enc.encode_frame_into(&photo, &mut buf).unwrap();
            photo_kf_bytes = buf.len();
        }
        let photo_kf_enc_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters_p as f64;
        let t0 = Instant::now();
        for _ in 0..iters_p {
            let _ = dec.decode_frame_bgra(&buf).unwrap();
        }
        let photo_kf_dec_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters_p as f64;

        println!("=== 1080p Tile444 (Texto Nitido) Benchmark ===");
        println!(
            "  IDE Text Keyframe (100% lossless) : enc {text_kf_enc_ms:.2} ms | dec {text_kf_dec_ms:.2} ms | {:.1} KB",
            text_kf_bytes as f64 / 1024.0
        );
        println!(
            "  Typing Delta (dirty tile)         : enc {delta_enc_ms:.2} ms | {:.1} KB",
            delta_bytes as f64 / 1024.0
        );
        println!(
            "  Static Screen Delta (0 tiles)     : enc {static_enc_ms:.2} ms | {static_bytes} B"
        );
        println!(
            "  Photo Keyframe (YUV 4:4:4 Q=94)   : enc {photo_kf_enc_ms:.2} ms | dec {photo_kf_dec_ms:.2} ms | {:.1} KB",
            photo_kf_bytes as f64 / 1024.0
        );
    }
}
