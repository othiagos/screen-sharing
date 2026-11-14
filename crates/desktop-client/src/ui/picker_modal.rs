use crate::config::AppConfig;
use crate::media::video::{capture_preview_with_source, list_capture_sources, CaptureSourceInfo};
use crate::ui::video_surface::bgra_to_render_image;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme, IndexPath, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Context, Entity, EventEmitter, ImageSource,
    InteractiveElement, IntoElement, ObjectFit, ParentElement, Render, RenderImage, SharedString,
    StatefulInteractiveElement as _, Styled, StyledImage as _, Subscription, Window, black, div,
    hsla, img, px,
};
use shared_protocol::{
    AudioCaptureMode, BitratePreset, CaptureTarget, StreamConfig, TargetResolution,
};
use std::sync::Arc;

const AUDIO_NONE: &str = "Sem Áudio";
const AUDIO_SYSTEM: &str = "Áudio Global do Sistema";
const AUDIO_APP: &str = "Somente Áudio do Aplicativo";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioChoice {
    None,
    SystemGlobal,
    ApplicationOnly,
}

fn resolution_to_label(res: TargetResolution) -> &'static str {
    match res {
        TargetResolution::R720p => "720p",
        TargetResolution::R1080p => "1080p",
        TargetResolution::R1440p => "1440p",
        TargetResolution::Native => "Nativa",
    }
}

fn label_to_resolution(label: &str) -> TargetResolution {
    match label {
        "720p" => TargetResolution::R720p,
        "1440p" => TargetResolution::R1440p,
        "Nativa" => TargetResolution::Native,
        _ => TargetResolution::R1080p,
    }
}

fn label_to_audio_choice(label: &str) -> AudioChoice {
    if label.starts_with("Sem") {
        AudioChoice::None
    } else if label.starts_with("Somente") {
        AudioChoice::ApplicationOnly
    } else {
        AudioChoice::SystemGlobal
    }
}

#[derive(Debug, Clone)]
pub enum PickerModalEvent {
    Start {
        config: StreamConfig,
        label: String,
    },
    Cancelled,
}

impl EventEmitter<PickerModalEvent> for PickerModalState {}

pub struct PickerModalState {
    window_handle: AnyWindowHandle,
    sources: Vec<CaptureSourceInfo>,
    has_wayland: bool,
    active_tab: usize,
    pub selected_target: Option<CaptureTarget>,
    preferred_resolution: TargetResolution,
    audio_manual_override: Option<AudioChoice>,
    preview_image: Option<Arc<RenderImage>>,
    stale_images: Vec<Arc<RenderImage>>,
    preview_dims: Option<(u32, u32)>,
    preview_source_dims: Option<(u32, u32)>,
    preview_loading: bool,
    preview_note: Option<String>,
    preview_seq: u64,
    resolution_select: Entity<SelectState<Vec<String>>>,
    fps_select: Entity<SelectState<Vec<String>>>,
    bitrate_select: Entity<SelectState<Vec<String>>>,
    audio_select: Entity<SelectState<Vec<String>>>,
    _subs: Vec<Subscription>,
}

impl PickerModalState {
    pub fn new(config: &AppConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sources = list_capture_sources().unwrap_or_default();
        let has_wayland = sources
            .iter()
            .any(|s| matches!(s.target, CaptureTarget::PortalWayland));

        let resolution_select = cx.new(|cx| {
            SelectState::new(
                vec![
                    "720p".to_string(),
                    "1080p".to_string(),
                    "1440p".to_string(),
                    "Nativa".to_string(),
                ],
                Some(IndexPath::default().row(1)),
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
                Some(IndexPath::default().row(fps_idx)),
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
                Some(IndexPath::default().row(br_idx)),
                window,
                cx,
            )
        });

        let audio_select = cx.new(|cx| {
            SelectState::new(
                vec![AUDIO_NONE.to_string(), AUDIO_SYSTEM.to_string()],
                Some(IndexPath::default().row(1)),
                window,
                cx,
            )
        });

        let sub_res = cx.subscribe(
            &resolution_select,
            |this, _, ev: &SelectEvent<Vec<String>>, _| {
                if let SelectEvent::Confirm(Some(label)) = ev {
                    this.preferred_resolution = label_to_resolution(label);
                }
            },
        );
        let sub_audio = cx.subscribe(
            &audio_select,
            |this, _, ev: &SelectEvent<Vec<String>>, cx| {
                if let SelectEvent::Confirm(Some(label)) = ev {
                    this.audio_manual_override = Some(label_to_audio_choice(label));
                    cx.notify();
                }
            },
        );

        let initial_tab = if has_wayland
            && !sources
                .iter()
                .any(|s| matches!(s.target, CaptureTarget::Monitor { .. }))
        {
            2
        } else {
            0
        };

        let mut modal = Self {
            window_handle: window.window_handle(),
            sources,
            has_wayland,
            active_tab: initial_tab,
            selected_target: None,
            preferred_resolution: config.resolution,
            audio_manual_override: None,
            preview_image: None,
            stale_images: Vec::new(),
            preview_dims: None,
            preview_source_dims: None,
            preview_loading: false,
            preview_note: None,
            preview_seq: 0,
            resolution_select,
            fps_select,
            bitrate_select,
            audio_select,
            _subs: vec![sub_res, sub_audio],
        };

        let visible = modal.filtered_sources();
        modal.selected_target = visible.first().map(|s| s.target.clone());
        modal.sync_audio_options(true, window, cx);
        modal.sync_resolution_options(window, cx);
        if modal.selected_target.is_some() {
            modal.refresh_preview(cx);
        }
        modal
    }

    fn filtered_sources(&self) -> Vec<&CaptureSourceInfo> {
        self.sources
            .iter()
            .filter(|s| match self.active_tab {
                0 => matches!(s.target, CaptureTarget::Monitor { .. }),
                1 => matches!(s.target, CaptureTarget::Window { .. }),
                _ => matches!(s.target, CaptureTarget::PortalWayland),
            })
            .collect()
    }

    fn selected_source_info(&self) -> Option<&CaptureSourceInfo> {
        let target = self.selected_target.as_ref()?;
        self.sources.iter().find(|s| &s.target == target)
    }

    fn selected_source_dims(&self) -> Option<(u32, u32)> {
        self.preview_source_dims
            .or_else(|| self.selected_source_info().and_then(|s| s.source_dims))
    }

    pub fn selected_target_label(&self) -> String {
        if let Some(info) = self.selected_source_info() {
            return info.display_label.clone();
        }
        match &self.selected_target {
            Some(CaptureTarget::Monitor { name, .. }) if !name.trim().is_empty() => name.clone(),
            Some(CaptureTarget::Window { title, .. }) if !title.trim().is_empty() => title.clone(),
            Some(CaptureTarget::PortalWayland) => "Portal Wayland".to_string(),
            _ => "Transmissão de Tela".to_string(),
        }
    }

    fn sync_resolution_options(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dims = self.selected_source_dims();
        let allowed = match dims {
            Some((w, h)) => TargetResolution::supported_for_source(w, h),
            None => TargetResolution::ordered_presets().to_vec(),
        };
        let target_res = match dims {
            Some((w, h)) => self.preferred_resolution.clamp_to_source(w, h),
            None => self.preferred_resolution,
        };
        let items: Vec<String> = allowed
            .iter()
            .map(|res| resolution_to_label(*res).to_string())
            .collect();
        let idx = allowed
            .iter()
            .position(|res| *res == target_res)
            .unwrap_or(items.len().saturating_sub(1));

        self.resolution_select.update(cx, |sel, cx| {
            sel.set_items(items, window, cx);
            sel.set_selected_index(Some(IndexPath::default().row(idx)), window, cx);
        });
    }

    fn sync_audio_options(
        &mut self,
        reset_to_tab_default: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if reset_to_tab_default {
            self.audio_manual_override = None;
        }
        let allow_app_audio = self.active_tab == 1;
        let items = if allow_app_audio {
            vec![
                AUDIO_NONE.to_string(),
                AUDIO_SYSTEM.to_string(),
                AUDIO_APP.to_string(),
            ]
        } else {
            vec![AUDIO_NONE.to_string(), AUDIO_SYSTEM.to_string()]
        };

        let default_choice = if allow_app_audio {
            AudioChoice::ApplicationOnly
        } else {
            AudioChoice::SystemGlobal
        };
        let desired = match self.audio_manual_override {
            Some(choice) if allow_app_audio || choice != AudioChoice::ApplicationOnly => choice,
            _ => default_choice,
        };
        let idx = match desired {
            AudioChoice::None => 0,
            AudioChoice::SystemGlobal => 1,
            AudioChoice::ApplicationOnly => 2,
        };

        self.audio_select.update(cx, |sel, cx| {
            sel.set_items(items, window, cx);
            sel.set_selected_index(Some(IndexPath::default().row(idx)), window, cx);
        });
    }

    fn reload_targets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sources = list_capture_sources().unwrap_or_default();
        self.has_wayland = self
            .sources
            .iter()
            .any(|s| matches!(s.target, CaptureTarget::PortalWayland));
        let filtered: Vec<CaptureTarget> = self
            .filtered_sources()
            .into_iter()
            .map(|s| s.target.clone())
            .collect();
        if let Some(cur) = &self.selected_target {
            if !filtered.contains(cur) {
                self.selected_target = filtered.first().cloned();
            }
        } else {
            self.selected_target = filtered.first().cloned();
        }
        self.preview_source_dims = None;
        self.sync_audio_options(false, window, cx);
        self.sync_resolution_options(window, cx);
        self.refresh_preview(cx);
    }

    fn select_tab(&mut self, tab: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_tab == tab {
            return;
        }
        self.active_tab = tab;
        self.selected_target = self
            .filtered_sources()
            .first()
            .map(|s| s.target.clone());
        self.preview_source_dims = None;
        self.sync_audio_options(true, window, cx);
        self.sync_resolution_options(window, cx);
        self.refresh_preview(cx);
    }

    fn select_target(&mut self, target: CaptureTarget, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_target = Some(target);
        self.preview_source_dims = None;
        self.sync_resolution_options(window, cx);
        self.refresh_preview(cx);
    }

    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.selected_target.clone() else {
            if let Some(prev) = self.preview_image.take() {
                self.stale_images.push(prev);
            }
            self.preview_dims = None;
            self.preview_source_dims = None;
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
            .spawn(async move { capture_preview_with_source(&target) });

        cx.spawn(async move |this, cx| {
            let res = task.await;
            let handle_opt = this
                .read_with(cx, |modal, _| modal.window_handle)
                .ok();
            let _ = this.update(cx, |modal, cx| {
                if modal.preview_seq != seq {
                    return;
                }
                modal.preview_loading = false;
                match res {
                    Ok(cap) => {
                        if let Some(img_arc) =
                            bgra_to_render_image(cap.preview_width, cap.preview_height, cap.bgra)
                        {
                            if let Some(prev) = modal.preview_image.replace(img_arc) {
                                modal.stale_images.push(prev);
                            }
                            modal.preview_dims = Some((cap.preview_width, cap.preview_height));
                            modal.preview_source_dims = Some(cap.source_dims);
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
            if let Some(win_handle) = handle_opt {
                let _ = cx.update_window(win_handle, |_, window, cx| {
                    let _ = this.update(cx, |modal, cx| {
                        if modal.preview_seq == seq {
                            modal.sync_resolution_options(window, cx);
                        }
                    });
                });
            }
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
        let mut resolution = label_to_resolution(res_str);
        if let Some((w, h)) = self.selected_source_dims() {
            resolution = resolution.clamp_to_source(w, h);
        }

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
            .unwrap_or(AUDIO_SYSTEM);
        let audio_mode = match label_to_audio_choice(audio_str) {
            AudioChoice::None => AudioCaptureMode::None,
            AudioChoice::ApplicationOnly => match &target {
                CaptureTarget::Window { pid, .. } if *pid > 0 => {
                    AudioCaptureMode::ApplicationOnly { pid: *pid }
                }
                _ => AudioCaptureMode::SystemGlobal,
            },
            AudioChoice::SystemGlobal => AudioCaptureMode::SystemGlobal,
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
        let warning = hsla(0.11, 0.85, 0.55, 1.0);

        let filtered: Vec<CaptureSourceInfo> =
            self.filtered_sources().into_iter().cloned().collect();
        let selected = self.selected_target.clone();
        let has_wayland = self.has_wayland;
        let source_dims = self.selected_source_dims();

        let audio_is_app = self
            .audio_select
            .read(cx)
            .selected_value()
            .is_some_and(|v| label_to_audio_choice(v) == AudioChoice::ApplicationOnly);
        let missing_pid_warning = matches!(
            &selected,
            Some(CaptureTarget::Window { pid, .. }) if *pid == 0
        ) && audio_is_app;

        let hint_content = if missing_pid_warning {
            (
                "Janela sem PID detectável: será capturado o áudio global do sistema.".to_string(),
                warning,
            )
        } else if let Some((w, h)) = source_dims {
            (
                format!("Resolução real da fonte: {w}×{h} (sem upscale)"),
                muted,
            )
        } else if matches!(selected, Some(CaptureTarget::PortalWayland)) {
            (
                "Resolução real detectada ao confirmar no seletor do sistema.".to_string(),
                muted,
            )
        } else {
            (
                "Resolução limitada automaticamente ao tamanho máximo da origem.".to_string(),
                muted,
            )
        };

        let mut tabs = TabBar::new("picker-source-tabs")
            .segmented()
            .selected_index(self.active_tab)
            .on_click(cx.listener(|this, ix: &usize, window, cx| {
                this.select_tab(*ix, window, cx);
            }))
            .child(Tab::new().label("Monitores"))
            .child(Tab::new().label("Janelas"));
        if has_wayland {
            tabs = tabs.child(Tab::new().label("Portal Wayland"));
        }

        let list_items: Vec<_> = filtered
            .into_iter()
            .enumerate()
            .map(|(ix, info)| {
                let is_sel = selected.as_ref() == Some(&info.target);
                let title = info.display_label.clone();
                let badge = match &info.target {
                    CaptureTarget::Monitor { .. } => "Monitor",
                    CaptureTarget::Window { .. } => "Janela",
                    CaptureTarget::PortalWayland => "PipeWire",
                };
                let target_clone = info.target.clone();

                div()
                    .id(("picker-item", ix))
                    .w_full()
                    .min_w_0()
                    .overflow_hidden()
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
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_target(target_clone.clone(), window, cx);
                    }))
                    .child(
                        v_flex()
                            .w_full()
                            .min_w_0()
                            .overflow_hidden()
                            .gap_0p5()
                            .child(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .text_xs()
                                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                                    .text_color(fg)
                                    .truncate()
                                    .child(SharedString::from(title)),
                            )
                            .child(div().text_xs().text_color(muted).child(badge)),
                    )
            })
            .collect();

        let preview_box = div()
            .w_full()
            .h(px(176.))
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
                    .w_full()
                    .px_4()
                    .text_center()
                    .text_xs()
                    .text_color(muted)
                    .child(SharedString::from(note.to_string()))
                    .into_any_element()
            });

        let preview_header_label = match (source_dims, self.preview_dims) {
            (Some((sw, sh)), _) => format!("Preview ({sw}×{sh})"),
            (None, Some((pw, ph))) => format!("Preview ({pw}×{ph})"),
            (None, None) => "Preview ao vivo".to_string(),
        };

        v_flex()
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .gap_3()
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .justify_between()
                    .items_center()
                    .child(tabs)
                    .child(
                        Button::new("picker-refresh-list")
                            .compact()
                            .outline()
                            .label("Atualizar Lista")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reload_targets(window, cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_4()
                    .items_start()
                    .child(
                        div()
                            .id("picker-target-scroll")
                            .flex_shrink_0()
                            .w(px(270.))
                            .min_w(px(270.))
                            .max_w(px(270.))
                            .h(px(360.))
                            .p_2()
                            .rounded(cx.theme().radius)
                            .bg(secondary)
                            .border_1()
                            .border_color(border)
                            .overflow_hidden()
                            .overflow_y_scroll()
                            .child(
                                v_flex()
                                    .w_full()
                                    .min_w_0()
                                    .gap_1p5()
                                    .when(list_items.is_empty(), |el| {
                                        el.child(
                                            div()
                                                .w_full()
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
                            .min_w_0()
                            .overflow_hidden()
                            .h(px(360.))
                            .justify_between()
                            .child(
                                v_flex()
                                    .w_full()
                                    .min_w_0()
                                    .gap_2()
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .min_w_0()
                                            .h(px(24.))
                                            .justify_between()
                                            .items_center()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_xs()
                                                    .text_color(muted)
                                                    .truncate()
                                                    .child(preview_header_label),
                                            )
                                            .child(
                                                div().flex_shrink_0().child(
                                                    Button::new("picker-refresh-preview")
                                                        .compact()
                                                        .ghost()
                                                        .label("Capturar Miniatura")
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.refresh_preview(cx);
                                                        })),
                                                ),
                                            ),
                                    )
                                    .child(preview_box)
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .min_w_0()
                                            .h(px(18.))
                                            .items_center()
                                            .child(
                                                div()
                                                    .w_full()
                                                    .min_w_0()
                                                    .text_xs()
                                                    .text_color(hint_content.1)
                                                    .truncate()
                                                    .child(hint_content.0),
                                            ),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .w_full()
                                    .min_w_0()
                                    .gap_2()
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .min_w_0()
                                            .gap_2p5()
                                            .child(
                                                v_flex()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .overflow_hidden()
                                                    .gap_1()
                                                    .child(div().text_xs().text_color(muted).child("Resolução"))
                                                    .child(Select::new(&self.resolution_select)),
                                            )
                                            .child(
                                                v_flex()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .overflow_hidden()
                                                    .gap_1()
                                                    .child(div().text_xs().text_color(muted).child("Taxa (FPS)"))
                                                    .child(Select::new(&self.fps_select)),
                                            ),
                                    )
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .min_w_0()
                                            .gap_2p5()
                                            .child(
                                                v_flex()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .overflow_hidden()
                                                    .gap_1()
                                                    .child(div().text_xs().text_color(muted).child("Bitrate"))
                                                    .child(Select::new(&self.bitrate_select)),
                                            )
                                            .child(
                                                v_flex()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .overflow_hidden()
                                                    .gap_1()
                                                    .child(div().text_xs().text_color(muted).child("Captura de Áudio"))
                                                    .child(Select::new(&self.audio_select)),
                                            ),
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
            .width(px(760.))
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
                                let (cfg_opt, label) = {
                                    let st = m_start.read(cx);
                                    (st.build_stream_config(cx), st.selected_target_label())
                                };
                                if let Some(stream_cfg) = cfg_opt {
                                    m_start.update(cx, |m, cx| {
                                        m.drop_all_images(window);
                                        cx.emit(PickerModalEvent::Start {
                                            config: stream_cfg,
                                            label,
                                        });
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
