use crate::i18n::throttle_reason_label;
use crate::media::DecodedVideoFrame;
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Div, ElementId, InteractiveElement, ObjectFit, ParentElement, RenderImage, SharedString,
    Stateful, Styled, StyledImage as _, Window, black, div, hsla, img, px,
};
use rust_i18n::t;
use shared_protocol::{PeerId, ThrottleReason, TEXT_FOCUS_FPS};
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

pub fn format_friendly_resolution(width: u32, height: u32) -> String {
    match (width, height) {
        (1280, 720) => "720p".to_string(),
        (1920, 1080) => "1080p".to_string(),
        (2560, 1440) => "1440p".to_string(),
        (3840, 2160) => "4K".to_string(),
        _ => format!("{width}×{height}"),
    }
}

pub fn friendly_throttle_message(reason: ThrottleReason) -> Option<String> {
    match reason {
        ThrottleReason::None => None,
        ThrottleReason::Network => Some(t!("warnings.throttle_network").into_owned()),
        ThrottleReason::Viewer => Some(t!("warnings.throttle_viewer").into_owned()),
        ThrottleReason::Cpu => Some(t!("warnings.throttle_cpu").into_owned()),
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

pub use crate::media::pipeline::bgra_to_render_image;

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
        if let Some(next_img) = frame.image.clone() {
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

#[derive(Debug, Clone, Copy, Default)]
pub struct VideoTileOptions {
    pub show_label: bool,
    pub show_stats: bool,
    pub is_focused: bool,
}

/// Renders a dark video tile with optional stats overlay and clean corner labels.
pub fn render_video_tile(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    image_src: Option<Arc<RenderImage>>,
    stats: Option<&VideoTileStats>,
    options: VideoTileOptions,
    cx: &gpui_kit::App,
) -> Stateful<Div> {
    let border_color = if options.is_focused {
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
                .child(t!("stage.loading_stream").into_owned()),
        );
    }

    if options.show_label {
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
    }

    if options.show_stats
        && let Some(st) = stats {
            let mut lines: Vec<String> = Vec::new();
            let eff_fps = st.effective_fps.unwrap_or(st.fps);
            let show_throttle = is_significant_throttle(
                st.throttle_reason,
                st.bitrate_kbps.unwrap_or(0),
                st.max_bitrate_kbps.unwrap_or(0),
                eff_fps,
                st.target_fps.unwrap_or(eff_fps),
            );
            let fps_text = if st
                .target_fps
                .is_some_and(|target| target <= u32::from(TEXT_FOCUS_FPS))
            {
                t!("modes.sharp_text").into_owned()
            } else {
                match st.target_fps {
                    Some(target)
                        if eff_fps < target && st.throttle_reason != ThrottleReason::None =>
                    {
                        format!("{}/{} fps", st.fps, target)
                    }
                    _ => format!("{} fps", st.fps),
                }
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
                lines.push(
                    t!(
                        "details.adapted",
                        reason = throttle_reason_label(st.throttle_reason).as_str()
                    )
                    .into_owned(),
                );
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

    #[test]
    fn test_format_friendly_resolution() {
        assert_eq!(format_friendly_resolution(1280, 720), "720p");
        assert_eq!(format_friendly_resolution(1920, 1080), "1080p");
        assert_eq!(format_friendly_resolution(2560, 1440), "1440p");
        assert_eq!(format_friendly_resolution(3840, 2160), "4K");
        assert_eq!(format_friendly_resolution(1366, 768), "1366×768");
        assert_eq!(format_friendly_resolution(1920, 1040), "1920×1040");
    }

    #[test]
    fn test_friendly_throttle_message() {
        assert_eq!(friendly_throttle_message(ThrottleReason::None), None);
        for reason in [
            ThrottleReason::Network,
            ThrottleReason::Viewer,
            ThrottleReason::Cpu,
        ] {
            let msg = friendly_throttle_message(reason).expect("throttled reason has message");
            assert!(!msg.contains("Bitrate"));
            assert!(!msg.contains("FPS"));
        }
    }
}
