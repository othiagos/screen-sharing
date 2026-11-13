use crate::config::AppConfig;
use crate::media::video::{capture_preview_bgra, list_capture_targets};
use crate::ui::video_surface::bgra_to_render_image;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, ImageSource, InteractiveElement, IntoElement,
    ObjectFit, ParentElement, Render, RenderImage, SharedString, StatefulInteractiveElement as _,
    Styled, StyledImage as _, Window, black, div, hsla, img, px,
};
use shared_protocol::{
    AudioCaptureMode, BitratePreset, CaptureTarget, StreamConfig, TargetResolution,
};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum PickerModalEvent {
    Start(StreamConfig),
    Cancelled,
}

impl EventEmitter<PickerModalEvent> for PickerModalState {}

pub struct PickerModalState {
    targets: Vec<CaptureTarget>,
    has_wayland: bool,
    active_tab: usize,
    pub selected_target: Option<CaptureTarget>,
    preview_image: Option<Arc<RenderImage>>,
    stale_images: Vec<Arc<RenderImage>>,
    preview_dims: Option<(u32, u32)>,
    preview_loading: bool,
    preview_note: Option<String>,
    preview_seq: u64,
    resolution_select: Entity<SelectState<Vec<String>>>,
    fps_select: Entity<SelectState<Vec<String>>>,
    bitrate_select: Entity<SelectState<Vec<String>>>,
    audio_select: Entity<SelectState<Vec<String>>>,
}

impl PickerModalState {
    pub fn new(config: &AppConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let targets = list_capture_targets().unwrap_or_default();
        let has_wayland = targets
            .iter()
            .any(|t| matches!(t, CaptureTarget::PortalWayland));

        let res_items = vec![
            "720p".to_string(),
            "1080p".to_string(),
            "1440p".to_string(),
            "Nativa".to_string(),
        ];
        let res_idx = match config.resolution {
            TargetResolution::R720p => 0,
            TargetResolution::R1080p => 1,
            TargetResolution::R1440p => 2,
            TargetResolution::Native => 3,
        };
        let resolution_select = cx.new(|cx| {
            SelectState::new(
                res_items,
                Some(gpui_kit::component::IndexPath::default().row(res_idx)),
                window,
                cx,
            )
        });

        let fps_items = vec![
            "15 FPS".to_string(),
            "30 FPS".to_string(),
            "60 FPS".to_string(),
        ];
        let fps_idx = match config.fps {
            0..=22 => 0,
            23..=45 => 1,
            _ => 2,
        };
        let fps_select = cx.new(|cx| {
            SelectState::new(
                fps_items,
                Some(gpui_kit::component::IndexPath::default().row(fps_idx)),
                window,
                cx,
            )
        });

        let bitrate_items = vec![
            "Auto (Adaptativo)".to_string(),
            "6 Mbps".to_string(),
            "12 Mbps".to_string(),
            "20 Mbps".to_string(),
        ];
        let br_idx = match config.bitrate {
            BitratePreset::Auto => 0,
            BitratePreset::Mbps6 => 1,
            BitratePreset::Mbps12 => 2,
            BitratePreset::Mbps20 => 3,
        };
        let bitrate_select = cx.new(|cx| {
            SelectState::new(
                bitrate_items,
                Some(gpui_kit::component::IndexPath::default().row(br_idx)),
                window,
                cx,
            )
        });

        let audio_items = vec![
            "Sem Áudio".to_string(),
            "Áudio Global do Sistema".to_string(),
            "Somente Áudio do Aplicativo".to_string(),
        ];
        let audio_select = cx.new(|cx| {
            SelectState::new(
                audio_items,
                Some(gpui_kit::component::IndexPath::default().row(1)),
                window,
                cx,
            )
        });

        let initial_tab = if has_wayland
            && !targets
                .iter()
                .any(|t| matches!(t, CaptureTarget::Monitor { .. }))
        {
            2
        } else {
            0
        };

        let mut modal = Self {
            targets,
            has_wayland,
            active_tab: initial_tab,
            selected_target: None,
            preview_image: None,
            stale_images: Vec::new(),
            preview_dims: None,
            preview_loading: false,
            preview_note: None,
            preview_seq: 0,
            resolution_select,
            fps_select,
            bitrate_select,
            audio_select,
        };

        let visible = modal.filtered_targets();
        modal.selected_target = visible.first().cloned();
        if modal.selected_target.is_some() {
            modal.refresh_preview(cx);
        }
        modal
    }

    fn filtered_targets(&self) -> Vec<CaptureTarget> {
        self.targets
            .iter()
            .filter(|t| match self.active_tab {
                0 => matches!(t, CaptureTarget::Monitor { .. }),
                1 => matches!(t, CaptureTarget::Window { .. }),
                _ => matches!(t, CaptureTarget::PortalWayland),
            })
            .cloned()
            .collect()
    }

    fn reload_targets(&mut self, cx: &mut Context<Self>) {
        self.targets = list_capture_targets().unwrap_or_default();
        self.has_wayland = self
            .targets
            .iter()
            .any(|t| matches!(t, CaptureTarget::PortalWayland));
        let filtered = self.filtered_targets();
        if let Some(cur) = &self.selected_target {
            if !filtered.contains(cur) {
                self.selected_target = filtered.first().cloned();
            }
        } else {
            self.selected_target = filtered.first().cloned();
        }
        self.refresh_preview(cx);
    }

    fn select_tab(&mut self, tab: usize, cx: &mut Context<Self>) {
        self.active_tab = tab;
        let filtered = self.filtered_targets();
        self.selected_target = filtered.first().cloned();
        self.refresh_preview(cx);
    }

    fn select_target(&mut self, target: CaptureTarget, cx: &mut Context<Self>) {
        self.selected_target = Some(target);
        self.refresh_preview(cx);
    }

    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.selected_target.clone() else {
            if let Some(prev) = self.preview_image.take() {
                self.stale_images.push(prev);
            }
            self.preview_dims = None;
            self.preview_loading = false;
            self.preview_note = Some("Selecione uma origem na lista à esquerda.".to_string());
            cx.notify();
            return;
        };

        self.preview_seq = self.preview_seq.wrapping_add(1);
        let seq = self.preview_seq;
        self.preview_loading = true;
        self.preview_note = None;
        cx.notify();

        let task = cx
            .background_executor()
            .spawn(async move { capture_preview_bgra(&target) });

        cx.spawn(async move |this, cx| {
            let res = task.await;
            let _ = this.update(cx, |modal, cx| {
                if modal.preview_seq != seq {
                    return;
                }
                modal.preview_loading = false;
                match res {
                    Ok((w, h, bgra)) => {
                        if let Some(img_arc) = bgra_to_render_image(w, h, bgra) {
                            if let Some(prev) = modal.preview_image.replace(img_arc) {
                                modal.stale_images.push(prev);
                            }
                            modal.preview_dims = Some((w, h));
                        }
                    }
                    Err(err) => {
                        if let Some(prev) = modal.preview_image.take() {
                            modal.stale_images.push(prev);
                        }
                        modal.preview_dims = None;
                        modal.preview_note = Some(err.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn drop_all_images(&mut self, window: &mut Window) {
        if let Some(img_arc) = self.preview_image.take() {
            let _ = window.drop_image(img_arc);
        }
        for stale in self.stale_images.drain(..) {
            let _ = window.drop_image(stale);
        }
    }

    pub fn build_stream_config(&self, cx: &gpui_kit::App) -> Option<StreamConfig> {
        let target = self.selected_target.clone()?;
        let res_str = self
            .resolution_select
            .read(cx)
            .selected_value()
            .map(String::as_str)
            .unwrap_or("1080p");
        let resolution = match res_str {
            "720p" => TargetResolution::R720p,
            "1440p" => TargetResolution::R1440p,
            "Nativa" => TargetResolution::Native,
            _ => TargetResolution::R1080p,
        };

        let fps_str = self
            .fps_select
            .read(cx)
            .selected_value()
            .map(String::as_str)
            .unwrap_or("60 FPS");
        let fps = if fps_str.starts_with("15") {
            15
        } else if fps_str.starts_with("30") {
            30
        } else {
            60
        };

        let br_str = self
            .bitrate_select
            .read(cx)
            .selected_value()
            .map(String::as_str)
            .unwrap_or("Auto (Adaptativo)");
        let bitrate = if br_str.starts_with("6") {
            BitratePreset::Mbps6
        } else if br_str.starts_with("12") {
            BitratePreset::Mbps12
        } else if br_str.starts_with("20") {
            BitratePreset::Mbps20
        } else {
            BitratePreset::Auto
        };

        let audio_str = self
            .audio_select
            .read(cx)
            .selected_value()
            .map(String::as_str)
            .unwrap_or("Áudio Global do Sistema");
        let audio_mode = if audio_str.starts_with("Sem") {
            AudioCaptureMode::None
        } else if audio_str.starts_with("Somente") {
            match &target {
                CaptureTarget::Window { pid, .. } if *pid > 0 => {
                    AudioCaptureMode::ApplicationOnly { pid: *pid }
                }
                _ => AudioCaptureMode::SystemGlobal,
            }
        } else {
            AudioCaptureMode::SystemGlobal
        };

        Some(StreamConfig {
            target,
            resolution,
            fps,
            bitrate,
            audio_mode,
        })
    }
}

impl Render for PickerModalState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for stale in self.stale_images.drain(..) {
            let _ = window.drop_image(stale);
        }

        let border = cx.theme().border;
        let bg = cx.theme().background;
        let secondary = cx.theme().secondary;
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let accent = cx.theme().accent;

        let filtered = self.filtered_targets();
        let selected = self.selected_target.clone();
        let has_wayland = self.has_wayland;

        let mut tabs = TabBar::new("picker-source-tabs")
            .segmented()
            .selected_index(self.active_tab)
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.select_tab(*ix, cx);
            }))
            .child(Tab::new().label("Monitores"))
            .child(Tab::new().label("Janelas"));
        if has_wayland {
            tabs = tabs.child(Tab::new().label("Portal Wayland"));
        }

        let list_items: Vec<_> = filtered
            .into_iter()
            .enumerate()
            .map(|(ix, target)| {
                let is_sel = selected.as_ref() == Some(&target);
                let (title, badge) = match &target {
                    CaptureTarget::Monitor { name, .. } => {
                        let label = if name.trim().is_empty() {
                            format!("Monitor #{}", ix + 1)
                        } else {
                            name.clone()
                        };
                        (label, "Monitor".to_string())
                    }
                    CaptureTarget::Window { title, pid, .. } => {
                        let badge = if *pid > 0 {
                            format!("PID {pid}")
                        } else {
                            "Janela".to_string()
                        };
                        (title.clone(), badge)
                    }
                    CaptureTarget::PortalWayland => (
                        "Selecionar Tela/Janela pelo Portal Wayland".to_string(),
                        "PipeWire".to_string(),
                    ),
                };

                div()
                    .id(("picker-item", ix))
                    .p_2()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .bg(if is_sel {
                        hsla(0.58, 0.7, 0.45, 0.22)
                    } else {
                        bg
                    })
                    .border_1()
                    .border_color(if is_sel { accent } else { border })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_target(target.clone(), cx);
                    }))
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                                    .text_color(fg)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(SharedString::from(title)),
                            )
                            .child(div().text_xs().text_color(muted).child(badge)),
                    )
            })
            .collect();

        let preview_box = div()
            .w_full()
            .h(px(186.))
            .rounded(cx.theme().radius)
            .overflow_hidden()
            .bg(black())
            .border_1()
            .border_color(border)
            .flex()
            .items_center()
            .justify_center()
            .child(if let Some(img_arc) = self.preview_image.clone() {
                img(ImageSource::Render(img_arc))
                    .object_fit(ObjectFit::Contain)
                    .size_full()
                    .into_any_element()
            } else {
                let note = if self.preview_loading {
                    "Capturando miniatura..."
                } else {
                    self.preview_note
                        .as_deref()
                        .unwrap_or("Preview indisponível para este item")
                };
                div()
                    .px_4()
                    .text_xs()
                    .text_color(muted)
                    .child(SharedString::from(note.to_string()))
                    .into_any_element()
            });

        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(tabs)
                    .child(
                        Button::new("picker-refresh-list")
                            .compact()
                            .outline()
                            .label("Atualizar Lista")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reload_targets(cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_4()
                    .items_start()
                    .child(
                        div()
                            .id("picker-target-scroll")
                            .w(px(270.))
                            .h(px(330.))
                            .p_2()
                            .rounded(cx.theme().radius)
                            .bg(secondary)
                            .border_1()
                            .border_color(border)
                            .overflow_y_scroll()
                            .child(
                                v_flex()
                                    .gap_1p5()
                                    .when(list_items.is_empty(), |el| {
                                        el.child(
                                            div()
                                                .p_3()
                                                .text_xs()
                                                .text_color(muted)
                                                .child("Nenhuma janela/monitor encontrado nesta aba."),
                                        )
                                    })
                                    .children(list_items),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_2p5()
                            .child(
                                h_flex()
                                    .justify_between()
                                    .items_center()
                                    .child(div().text_xs().text_color(muted).child(
                                        match self.preview_dims {
                                            Some((w, h)) => format!("Preview ({w}x{h})"),
                                            None => "Preview ao vivo".to_string(),
                                        },
                                    ))
                                    .child(
                                        Button::new("picker-refresh-preview")
                                            .compact()
                                            .ghost()
                                            .label("Capturar Miniatura")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.refresh_preview(cx);
                                            })),
                                    ),
                            )
                            .child(preview_box)
                            .child(
                                h_flex()
                                    .gap_2p5()
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .gap_1()
                                            .child(div().text_xs().text_color(muted).child("Resolução"))
                                            .child(Select::new(&self.resolution_select)),
                                    )
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .gap_1()
                                            .child(div().text_xs().text_color(muted).child("Taxa (FPS)"))
                                            .child(Select::new(&self.fps_select)),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_2p5()
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .gap_1()
                                            .child(div().text_xs().text_color(muted).child("Bitrate"))
                                            .child(Select::new(&self.bitrate_select)),
                                    )
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .gap_1()
                                            .child(div().text_xs().text_color(muted).child("Captura de Áudio"))
                                            .child(Select::new(&self.audio_select)),
                                    ),
                            ),
                    ),
            )
    }
}

/// Opens the Capture Picker Modal Dialog and returns the created modal entity handle.
pub fn open_picker_dialog(
    config: &AppConfig,
    window: &mut Window,
    cx: &mut gpui_kit::App,
) -> Entity<PickerModalState> {
    let modal = cx.new(|cx| PickerModalState::new(config, window, cx));
    let modal_body = modal.clone();
    let modal_cancel = modal.clone();
    let modal_start = modal.clone();
    let modal_close = modal.clone();

    window.open_dialog(cx, move |dialog, _, _| {
        let m_cancel = modal_cancel.clone();
        let m_start = modal_start.clone();
        let m_close = modal_close.clone();

        dialog
            .title("Compartilhar Tela e Áudio")
            .width(px(740.))
            .centered()
            .overlay_closable(false)
            .on_close(move |_, window, cx| {
                m_close.update(cx, |m, cx| {
                    m.drop_all_images(window);
                    cx.emit(PickerModalEvent::Cancelled);
                });
            })
            .child(modal_body.clone())
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("picker-btn-cancel")
                            .ghost()
                            .label("Cancelar")
                            .on_click(move |_, window, cx| {
                                m_cancel.update(cx, |m, cx| {
                                    m.drop_all_images(window);
                                    cx.emit(PickerModalEvent::Cancelled);
                                });
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("picker-btn-start")
                            .primary()
                            .label("Iniciar Transmissão")
                            .on_click(move |_, window, cx| {
                                let cfg_opt = m_start.read(cx).build_stream_config(cx);
                                if let Some(stream_cfg) = cfg_opt {
                                    m_start.update(cx, |m, cx| {
                                        m.drop_all_images(window);
                                        cx.emit(PickerModalEvent::Start(stream_cfg));
                                    });
                                    window.close_dialog(cx);
                                } else {
                                    m_start.update(cx, |m, cx| {
                                        m.preview_note = Some(
                                            "Selecione um monitor, janela ou portal antes de iniciar."
                                                .to_string(),
                                        );
                                        cx.notify();
                                    });
                                }
                            }),
                    ),
            )
    });

    modal
}
