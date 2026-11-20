use crate::media::DecodedVideoFrame;
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Div, ElementId, InteractiveElement, ObjectFit, ParentElement, RenderImage, SharedString,
    Stateful, Styled, StyledImage as _, Window, black, div, hsla, img, px,
};
use shared_protocol::{PeerId, ThrottleReason};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoTileKey {
    LocalPreview,
    Remote(PeerId),
}

#[derive(Debug, Clone)]
pub struct VideoTileStats {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub effective_fps: Option<u32>,
    pub target_fps: Option<u32>,
    pub backend: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub max_bitrate_kbps: Option<u32>,
    pub throttle_reason: ThrottleReason,
    pub rtt_ms: Option<u32>,
    pub warning: Option<String>,
}

pub fn format_bitrate_compact(kbps: u32) -> String {
    if kbps >= 1000 {
        format!("{:.1} Mbps", kbps as f32 / 1000.0)
    } else {
        format!("{kbps} kbps")
    }
}

pub fn is_significant_throttle(
    reason: ThrottleReason,
    bitrate_kbps: u32,
    max_bitrate_kbps: u32,
    effective_fps: u32,
    configured_fps: u32,
) -> bool {
    if reason == ThrottleReason::None {
        return false;
    }
    let fps_reduced = configured_fps > 0 && effective_fps > 0 && effective_fps < configured_fps;
    let bitrate_reduced =
        max_bitrate_kbps > 0 && bitrate_kbps < (max_bitrate_kbps.saturating_mul(70)) / 100;
    fps_reduced || bitrate_reduced
}

/// Converts a tightly-packed BGRA8 buffer into a cached GPUI `RenderImage`.
pub fn bgra_to_render_image(width: u32, height: u32, bgra: Vec<u8>) -> Option<Arc<RenderImage>> {
    if width == 0 || height == 0 {
        return None;
    }
    let expected = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
    if bgra.len() != expected {
        return None;
    }
    let buf = image::ImageBuffer::<image::Rgba<u8>, Vec<u8>>::from_raw(width, height, bgra)?;
    let frame = image::Frame::new(buf);
    Some(Arc::new(RenderImage::new(smallvec::smallvec![frame])))
}

/// Holds the current GPU-uploaded `RenderImage` for a video feed and drops stale textures.
#[derive(Default)]
pub struct VideoSlot {
    last_generation: u64,
    image: Option<Arc<RenderImage>>,
}

impl VideoSlot {
    pub fn sync(&mut self, frame: &DecodedVideoFrame, window: &mut Window) {
        if frame.generation == self.last_generation && self.image.is_some() {
            return;
        }
        if let Some(next_img) = bgra_to_render_image(frame.width, frame.height, frame.bgra.clone())
        {
            if let Some(prev) = self.image.replace(next_img) {
                let _ = window.drop_image(prev);
            }
            self.last_generation = frame.generation;
        }
    }

    pub fn clear(&mut self, window: &mut Window) {
        if let Some(prev) = self.take_image() {
            let _ = window.drop_image(prev);
        }
    }

    pub fn take_image(&mut self) -> Option<Arc<RenderImage>> {
        self.last_generation = 0;
        self.image.take()
    }

    pub fn image(&self) -> Option<Arc<RenderImage>> {
        self.image.clone()
    }
}

/// Renders a dark video tile with optional stats overlay and clean corner labels.
pub fn render_video_tile(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    image_src: Option<Arc<RenderImage>>,
    stats: Option<&VideoTileStats>,
    show_stats: bool,
    is_focused: bool,
    cx: &gpui_kit::App,
) -> Stateful<Div> {
    let border_color = if is_focused {
        cx.theme().accent
    } else {
        cx.theme().border
    };
    let muted_fg = cx.theme().muted_foreground;
    let label: SharedString = label.into();

    let mut tile = div()
        .id(id)
        .relative()
        .size_full()
        .overflow_hidden()
        .rounded(cx.theme().radius_lg)
        .bg(black())
        .border_1()
        .border_color(border_color);

    if let Some(img_arc) = image_src {
        tile = tile.child(
            img(gpui_kit::ImageSource::Render(img_arc))
                .object_fit(ObjectFit::Contain)
                .size_full(),
        );
    } else {
        tile = tile.child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted_fg)
                .child("Aguardando quadro H.264..."),
        );
    }

    tile = tile.child(
        div()
            .absolute()
            .left(px(10.))
            .bottom(px(10.))
            .px_2p5()
            .py_1()
            .rounded(cx.theme().radius)
            .bg(hsla(0., 0., 0., 0.72))
            .text_xs()
            .text_color(gpui_kit::white())
            .child(label),
    );

    if show_stats {
        if let Some(st) = stats {
            let mut lines: Vec<String> = Vec::new();
            let eff_fps = st.effective_fps.unwrap_or(st.fps);
            let show_throttle = is_significant_throttle(
                st.throttle_reason,
                st.bitrate_kbps.unwrap_or(0),
                st.max_bitrate_kbps.unwrap_or(0),
                eff_fps,
                st.target_fps.unwrap_or(eff_fps),
            );
            let fps_text = match st.target_fps {
                Some(target) if eff_fps < target && st.throttle_reason != ThrottleReason::None => {
                    format!("{}/{} fps", st.fps, target)
                }
                _ => format!("{} fps", st.fps),
            };
            lines.push(format!("{}x{} • {fps_text}", st.width, st.height));
            if let Some(backend) = &st.backend {
                lines.push(backend.clone());
            }
            if let Some(kbps) = st.bitrate_kbps {
                let br_str = match st.max_bitrate_kbps {
                    Some(max_kbps)
                        if max_kbps > 0
                            && (st.throttle_reason != ThrottleReason::None
                                || kbps < (max_kbps * 95) / 100) =>
                    {
                        format!(
                            "{} / {}",
                            format_bitrate_compact(kbps),
                            format_bitrate_compact(max_kbps)
                        )
                    }
                    _ => format_bitrate_compact(kbps),
                };
                lines.push(br_str);
            }
            if show_throttle {
                lines.push(format!("Adaptado: {}", st.throttle_reason.label()));
            }
            if let Some(rtt) = st.rtt_ms {
                lines.push(format!("RTT {rtt} ms"));
            }
            let summary = lines.join(" • ");

            let overlay = v_flex()
                .gap_1()
                .items_end()
                .child(
                    div()
                        .px_2p5()
                        .py_1()
                        .rounded(cx.theme().radius)
                        .bg(hsla(0., 0., 0., 0.76))
                        .text_xs()
                        .text_color(gpui_kit::white())
                        .child(summary),
                )
                .when_some(st.warning.clone(), |el, warn_msg| {
                    el.child(
                        h_flex()
                            .px_2p5()
                            .py_1()
                            .rounded(cx.theme().radius)
                            .bg(hsla(0.11, 0.85, 0.35, 0.85))
                            .text_xs()
                            .text_color(gpui_kit::white())
                            .child(warn_msg),
                    )
                });

            tile = tile.child(div().absolute().right(px(10.)).top(px(10.)).child(overlay));
        }
    }

    tile
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_significant_throttle_threshold() {
        // Minor reduction (10 / 12 Mbps) with full target FPS stays quiet
        assert!(!is_significant_throttle(
            ThrottleReason::Network,
            10_000,
            12_000,
            60,
            60,
        ));
        // Significant bitrate reduction (< 70% of cap) surfaces badge
        assert!(is_significant_throttle(
            ThrottleReason::Network,
            7_500,
            12_000,
            60,
            60,
        ));
        // Any network FPS fallback (30 / 60) surfaces badge
        assert!(is_significant_throttle(
            ThrottleReason::Viewer,
            11_500,
            12_000,
            30,
            60,
        ));
        // Unthrottled session never surfaces badge
        assert!(!is_significant_throttle(
            ThrottleReason::None,
            400,
            12_000,
            30,
            60,
        ));
    }
}
