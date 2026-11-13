use crate::ui::app_view::{AppView, StageLayout};
use crate::ui::video_surface::{VideoTileKey, VideoTileStats, render_video_tile};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::slider::Slider;
use gpui_kit::component::{ActiveTheme, Disableable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    ClipboardItem, Context, InteractiveElement, IntoElement, ParentElement, RenderImage,
    SharedString, StatefulInteractiveElement as _, Styled, Window, black, div, hsla, px,
};
use shared_protocol::PeerId;
use std::sync::Arc;
use std::time::Duration;

fn sync_video_slots(app: &mut AppView, window: &mut Window) {
    let stale_keys: Vec<PeerId> = app
        .video_slots
        .keys()
        .copied()
        .filter(|id| !app.subscribed_peers.contains(id))
        .collect();
    for peer_id in stale_keys {
        if let Some(mut slot) = app.video_slots.remove(&peer_id) {
            slot.clear(window);
        }
    }

    for peer_id in app.subscribed_peers.iter().copied() {
        if let Some(frame) = app.video_store.latest_peer_frame(peer_id) {
            app.video_slots
                .entry(peer_id)
                .or_default()
                .sync(&frame, window);
        }
    }

    if app.local_broadcast.is_some() {
        if let Some(local_frame) = app.video_store.latest_local_preview() {
            app.local_preview_slot.sync(&local_frame, window);
        }
    } else {
        app.local_preview_slot.clear(window);
    }
}

fn tile_info(
    app: &AppView,
    key: VideoTileKey,
) -> (
    SharedString,
    Option<Arc<RenderImage>>,
    Option<VideoTileStats>,
) {
    match key {
        VideoTileKey::LocalPreview => {
            let label = SharedString::from("Sua Transmissão (Preview Local)");
            let img_arc = app.local_preview_slot.image();
            let stats = app.broadcast_stats.as_ref().map(|bs| VideoTileStats {
                width: bs.width,
                height: bs.height,
                fps: bs.encoded_fps,
                backend: Some(bs.backend.clone()),
                bitrate_kbps: Some(bs.target_bitrate_kbps),
                rtt_ms: app.rtt_ms,
                warning: bs.warning.clone(),
            });
            (label, img_arc, stats)
        }
        VideoTileKey::Remote(peer_id) => {
            let nick = app
                .peers
                .iter()
                .find(|p| p.peer_id == peer_id)
                .map(|p| p.nickname.clone())
                .unwrap_or_else(|| format!("Peer {}", &peer_id.to_string()[..6]));
            let img_arc = app.video_slots.get(&peer_id).and_then(|s| s.image());
            let frame_opt = app.video_store.latest_peer_frame(peer_id);
            let meta_opt = app
                .peers
                .iter()
                .find(|p| p.peer_id == peer_id)
                .and_then(|p| p.active_stream.as_ref());

            let stats = frame_opt
                .as_ref()
                .map(|f| VideoTileStats {
                    width: f.width,
                    height: f.height,
                    fps: f.fps_estimate,
                    backend: meta_opt.map(|m| m.codec_name.clone()),
                    bitrate_kbps: None,
                    rtt_ms: app.rtt_ms,
                    warning: None,
                })
                .or_else(|| {
                    meta_opt.map(|m| VideoTileStats {
                        width: m.width,
                        height: m.height,
                        fps: u32::from(m.fps),
                        backend: Some(m.codec_name.clone()),
                        bitrate_kbps: None,
                        rtt_ms: app.rtt_ms,
                        warning: None,
                    })
                });
            (SharedString::from(nick), img_arc, stats)
        }
    }
}

fn build_clickable_tile(
    app: &AppView,
    key: VideoTileKey,
    tile_id_prefix: &'static str,
    focused: bool,
    multi_tile: bool,
    cx: &mut Context<AppView>,
) -> impl IntoElement {
    let (label, img_arc, stats) = tile_info(app, key);
    let el_id = match key {
        VideoTileKey::LocalPreview => (tile_id_prefix, 0usize),
        VideoTileKey::Remote(id) => (tile_id_prefix, id.as_u128() as usize),
    };
    render_video_tile(
        el_id,
        label,
        img_arc,
        stats.as_ref(),
        app.show_stats,
        focused,
        cx,
    )
    .cursor_pointer()
    .on_click(cx.listener(move |this, ev: &gpui_kit::ClickEvent, _, cx| {
        if ev.click_count() >= 2 {
            this.stage_layout = StageLayout::Focus(key);
            this.is_fullscreen = !this.is_fullscreen;
            cx.notify();
        } else if multi_tile {
            this.stage_layout = StageLayout::Focus(key);
            cx.notify();
        }
    }))
}

pub fn render_stage(
    app: &mut AppView,
    window: &mut Window,
    cx: &mut Context<AppView>,
) -> impl IntoElement {
    sync_video_slots(app, window);

    let bg = cx.theme().background;
    let secondary = cx.theme().secondary;
    let border = cx.theme().border;
    let fg = cx.theme().foreground;
    let muted = cx.theme().muted_foreground;
    let accent = cx.theme().accent;
    let danger = cx.theme().danger;

    let mut stage_keys: Vec<VideoTileKey> = app
        .subscribed_peers
        .iter()
        .copied()
        .map(VideoTileKey::Remote)
        .collect();
    let has_remote_tiles = !stage_keys.is_empty();
    let has_local_broadcast = app.local_broadcast.is_some();

    if has_local_broadcast && !has_remote_tiles {
        stage_keys.push(VideoTileKey::LocalPreview);
    }

    if app.is_fullscreen {
        let mut fs_keys = stage_keys.clone();
        if has_local_broadcast && !fs_keys.contains(&VideoTileKey::LocalPreview) {
            fs_keys.push(VideoTileKey::LocalPreview);
        }
        let target_key = match app.stage_layout {
            StageLayout::Focus(k) if fs_keys.contains(&k) => k,
            _ => fs_keys.first().copied().unwrap_or(VideoTileKey::LocalPreview),
        };
        let tile = build_clickable_tile(app, target_key, "fs-tile", false, false, cx);

        return div()
            .size_full()
            .relative()
            .bg(black())
            .child(tile)
            .child(
                div()
                    .absolute()
                    .right(px(14.))
                    .bottom(px(14.))
                    .px_3()
                    .py_1p5()
                    .rounded(cx.theme().radius)
                    .bg(hsla(0., 0., 0., 0.72))
                    .text_xs()
                    .text_color(gpui_kit::white())
                    .child("ESC ou duplo clique para sair da tela cheia"),
            )
            .into_any_element();
    }

    let copied_recently = app
        .last_clipboard_feedback
        .is_some_and(|t| t.elapsed() < Duration::from_secs(2));
    let copy_label = if copied_recently {
        "Copiado!"
    } else {
        "Copiar ID"
    };

    let (rtt_text, rtt_color) = match app.rtt_ms {
        Some(ms) if ms <= 45 => (format!("{ms} ms"), hsla(0.38, 0.7, 0.45, 1.0)),
        Some(ms) if ms <= 120 => (format!("{ms} ms"), hsla(0.12, 0.85, 0.55, 1.0)),
        Some(ms) => (format!("{ms} ms"), danger),
        None => ("-- ms".to_string(), muted),
    };

    let is_focus_layout = matches!(app.stage_layout, StageLayout::Focus(_));
    let first_available_key = stage_keys
        .first()
        .copied()
        .or_else(|| has_local_broadcast.then_some(VideoTileKey::LocalPreview));

    let top_bar = h_flex()
        .h(px(54.))
        .px_4()
        .justify_between()
        .items_center()
        .bg(secondary)
        .border_b_1()
        .border_color(border)
        .child(
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui_kit::FontWeight::BOLD)
                        .text_color(fg)
                        .child(format!("Sala: {}", app.room_id)),
                )
                .child(
                    Button::new("btn-copy-room")
                        .compact()
                        .outline()
                        .label(copy_label)
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(this.room_id.clone()));
                            this.last_clipboard_feedback = Some(std::time::Instant::now());
                            cx.notify();
                        })),
                )
                .child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_1p5()
                        .items_center()
                        .rounded(cx.theme().radius)
                        .bg(bg)
                        .border_1()
                        .border_color(border)
                        .child(div().w(px(8.)).h(px(8.)).rounded_full().bg(rtt_color))
                        .child(div().text_xs().text_color(fg).child(rtt_text)),
                ),
        )
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Button::new("btn-toggle-layout")
                        .compact()
                        .outline()
                        .label(if is_focus_layout {
                            "Modo Grid"
                        } else {
                            "Modo Foco"
                        })
                        .disabled(first_available_key.is_none())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.stage_layout = match this.stage_layout {
                                StageLayout::Grid => {
                                    if let Some(k) = first_available_key {
                                        StageLayout::Focus(k)
                                    } else {
                                        StageLayout::Grid
                                    }
                                }
                                StageLayout::Focus(_) => StageLayout::Grid,
                            };
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("btn-fullscreen")
                        .compact()
                        .outline()
                        .label("Tela Cheia")
                        .disabled(first_available_key.is_none())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.is_fullscreen = true;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("btn-stats-toggle")
                        .compact()
                        .outline()
                        .label(if app.show_stats {
                            "Ocultar Stats"
                        } else {
                            "Stats (Ctrl+Shift+S)"
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_stats = !this.show_stats;
                            cx.notify();
                        })),
                )
                .child(if has_local_broadcast {
                    Button::new("btn-stop-share")
                        .danger()
                        .label("Parar Transmissão")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.stop_local_broadcast(window, cx);
                        }))
                        .into_any_element()
                } else {
                    Button::new("btn-open-share")
                        .primary()
                        .label("Compartilhar Tela")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_capture_picker(window, cx);
                        }))
                        .into_any_element()
                })
                .child(
                    Button::new("btn-leave-room")
                        .ghost()
                        .label("Sair da Sala")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.leave_room(window, cx);
                        })),
                ),
        );

    let active_warning = app.active_warning();

    let peer_cards: Vec<_> = app
        .peers
        .iter()
        .enumerate()
        .map(|(ix, peer)| {
            let peer_id = peer.peer_id;
            let is_local = app.local_peer_id == Some(peer_id);
            let is_live = peer.active_stream.is_some();
            let is_sub = app.subscribed_peers.contains(&peer_id);
            let stream_desc = peer
                .active_stream
                .as_ref()
                .map(|m| format!("{}x{} @ {}fps • {}", m.width, m.height, m.fps, m.codec_name));
            let audio_state = app.peer_sliders.get(&peer_id).cloned();

            v_flex()
                .id(("peer-card", ix))
                .p_3()
                .gap_2()
                .rounded(cx.theme().radius)
                .bg(bg)
                .border_1()
                .border_color(if is_sub { accent } else { border })
                .child(
                    h_flex()
                        .justify_between()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .text_color(fg)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(if is_local {
                                    format!("{} (Você)", peer.nickname)
                                } else {
                                    peer.nickname.clone()
                                }),
                        )
                        .when(is_live, |el| {
                            el.child(
                                div()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded(cx.theme().radius)
                                    .bg(danger)
                                    .text_xs()
                                    .font_weight(gpui_kit::FontWeight::BOLD)
                                    .text_color(gpui_kit::white())
                                    .child("AO VIVO"),
                            )
                        }),
                )
                .when_some(stream_desc, |el, desc| {
                    el.child(div().text_xs().text_color(muted).child(desc))
                })
                .when(!is_local && (is_live || is_sub), |el| {
                    el.child(
                        h_flex().gap_2().child(if is_sub {
                            Button::new(("watch-btn", ix))
                                .compact()
                                .outline()
                                .label("Parar de Assistir")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.toggle_peer_subscription(peer_id, false, window, cx);
                                }))
                                .into_any_element()
                        } else {
                            Button::new(("watch-btn", ix))
                                .compact()
                                .primary()
                                .label("Assistir")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.toggle_peer_subscription(peer_id, true, window, cx);
                                }))
                                .into_any_element()
                        }),
                    )
                })
                .when(!is_local, |el| {
                    el.when_some(audio_state, |row, audio| {
                        row.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Button::new(("mute-btn", ix))
                                        .compact()
                                        .outline()
                                        .label(if audio.muted { "Mudo" } else { "Som" })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_peer_mute(peer_id, cx);
                                        })),
                                )
                                .child(div().flex_1().child(Slider::new(&audio.slider).horizontal()))
                                .child(
                                    div()
                                        .w(px(40.))
                                        .text_xs()
                                        .text_color(muted)
                                        .child(format!("{}%", audio.volume_pct)),
                                ),
                        )
                    })
                })
        })
        .collect();

    let sidebar = v_flex()
        .w(px(295.))
        .h_full()
        .bg(secondary)
        .border_r_1()
        .border_color(border)
        .p_3()
        .gap_3()
        .child(
            div()
                .text_xs()
                .font_weight(gpui_kit::FontWeight::BOLD)
                .text_color(muted)
                .child(format!("PARTICIPANTES ({})", app.peers.len())),
        )
        .child(
            div()
                .id("sidebar-peers-scroll")
                .flex_1()
                .overflow_y_scroll()
                .child(v_flex().gap_2().children(peer_cards)),
        );

    let main_video_area = if stage_keys.is_empty() && !has_local_broadcast {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .w(px(440.))
                    .p_6()
                    .gap_2()
                    .items_center()
                    .rounded(cx.theme().radius_lg)
                    .bg(secondary)
                    .border_1()
                    .border_color(border)
                    .child(
                        div()
                            .text_base()
                            .font_weight(gpui_kit::FontWeight::BOLD)
                            .text_color(fg)
                            .child("Nenhuma transmissão selecionada"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("Clique em \"Compartilhar Tela\" na barra superior ou em \"Assistir\" ao lado de um participante AO VIVO."),
                    ),
            )
            .into_any_element()
    } else {
        let mut all_focusable = stage_keys.clone();
        if has_local_broadcast && !all_focusable.contains(&VideoTileKey::LocalPreview) {
            all_focusable.push(VideoTileKey::LocalPreview);
        }

        let content = match app.stage_layout {
            StageLayout::Focus(focused_key) if all_focusable.contains(&focused_key) => {
                let others: Vec<VideoTileKey> = all_focusable
                    .iter()
                    .copied()
                    .filter(|k| *k != focused_key)
                    .collect();
                let main_tile = build_clickable_tile(
                    app,
                    focused_key,
                    "focus-main",
                    true,
                    false,
                    cx,
                );

                v_flex()
                    .size_full()
                    .gap_3()
                    .child(div().flex_1().w_full().min_h_0().child(main_tile))
                    .when(!others.is_empty(), |el| {
                        let thumbs: Vec<_> = others
                            .into_iter()
                            .enumerate()
                            .map(|(ix, key)| {
                                div().w(px(220.)).h_full().child(build_clickable_tile(
                                    app,
                                    key,
                                    match ix {
                                        0 => "thumb-0",
                                        1 => "thumb-1",
                                        2 => "thumb-2",
                                        _ => "thumb-n",
                                    },
                                    false,
                                    true,
                                    cx,
                                ))
                            })
                            .collect();
                        el.child(h_flex().h(px(124.)).gap_2p5().children(thumbs))
                    })
                    .into_any_element()
            }
            _ => {
                let multi = stage_keys.len() > 1;
                let grid = if stage_keys.len() <= 1 {
                    let key = stage_keys[0];
                    build_clickable_tile(app, key, "grid-single", false, false, cx)
                        .into_any_element()
                } else {
                    let mut rows = v_flex().size_full().gap_3();
                    for chunk in stage_keys.chunks(2) {
                        let mut row = h_flex().flex_1().min_h_0().w_full().gap_3();
                        for key in chunk {
                            row = row.child(
                                div().flex_1().h_full().min_w_0().child(build_clickable_tile(
                                    app,
                                    *key,
                                    "grid-cell",
                                    false,
                                    multi,
                                    cx,
                                )),
                            );
                        }
                        rows = rows.child(row);
                    }
                    rows.into_any_element()
                };

                if has_local_broadcast && has_remote_tiles {
                    let pip_tile = build_clickable_tile(
                        app,
                        VideoTileKey::LocalPreview,
                        "local-pip",
                        false,
                        true,
                        cx,
                    );
                    div()
                        .relative()
                        .size_full()
                        .child(grid)
                        .child(
                            div()
                                .absolute()
                                .right(px(16.))
                                .bottom(px(16.))
                                .w(px(260.))
                                .h(px(148.))
                                .child(pip_tile),
                        )
                        .into_any_element()
                } else {
                    grid
                }
            }
        };

        content
    };

    v_flex()
        .size_full()
        .bg(bg)
        .child(top_bar)
        .when_some(active_warning, |el, warn_msg| {
            el.child(
                h_flex()
                    .px_4()
                    .py_2()
                    .justify_between()
                    .items_center()
                    .bg(hsla(0.11, 0.85, 0.35, 0.25))
                    .border_b_1()
                    .border_color(hsla(0.11, 0.85, 0.45, 0.8))
                    .child(div().text_xs().text_color(fg).child(warn_msg.clone()))
                    .child(
                        Button::new("btn-dismiss-warn")
                            .compact()
                            .ghost()
                            .label("OK")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.dismiss_warning(warn_msg.clone(), cx);
                            })),
                    ),
            )
        })
        .child(
            h_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(sidebar)
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .p_3()
                        .child(main_video_area),
                ),
        )
        .into_any_element()
}
