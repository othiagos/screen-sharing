use leptos::*;
use shared_protocol::{AudioCaptureMode, CaptureTarget, StreamConfig, TargetResolution};
use wasm_bindgen_futures::spawn_local;

use crate::ipc::{invoke_list_capture_sources, invoke_start_screen_share};

#[component]
pub fn CapturePickerModal(
    show: ReadSignal<bool>,
    set_show: WriteSignal<bool>,
    set_is_sharing: WriteSignal<bool>,
    set_error_msg: WriteSignal<Option<String>>,
) -> impl IntoView {
    let (sources, set_sources) = create_signal::<Vec<CaptureTarget>>(Vec::new());
    let (selected_idx, set_selected_idx) = create_signal(0usize);
    let (resolution_choice, set_resolution_choice) = create_signal("720p".to_string());
    let (fps_choice, set_fps_choice) = create_signal("30".to_string());
    let (audio_choice, set_audio_choice) = create_signal("system".to_string());

    let refresh_sources = move || {
        spawn_local(async move {
            match invoke_list_capture_sources().await {
                Ok(list) => {
                    set_selected_idx.set(0);
                    set_sources.set(list);
                }
                Err(err) => set_error_msg.set(Some(err)),
            }
        });
    };

    create_effect(move |_| {
        if show.get() {
            refresh_sources();
        }
    });

    let on_start_share = move |_| {
        let current_sources = sources.get();
        let target = current_sources
            .get(selected_idx.get())
            .cloned()
            .unwrap_or(CaptureTarget::Monitor {
                id: 0,
                name: "Primary Monitor".to_string(),
            });

        let resolution = match resolution_choice.get().as_str() {
            "1080p" => TargetResolution::R1080p,
            "native" => TargetResolution::Native,
            _ => TargetResolution::R720p,
        };

        let fps: u8 = match fps_choice.get().as_str() {
            "15" => 15,
            "60" => 60,
            _ => 30,
        };

        let target_pid = match &target {
            CaptureTarget::Window { pid, .. } => *pid,
            CaptureTarget::Monitor { .. } => 0,
        };

        let audio_mode = match audio_choice.get().as_str() {
            "mute" => AudioCaptureMode::None,
            "app" => AudioCaptureMode::ApplicationOnly { pid: target_pid },
            _ => AudioCaptureMode::SystemGlobal,
        };

        let config = StreamConfig {
            target,
            resolution,
            fps,
            audio_mode,
        };

        spawn_local(async move {
            match invoke_start_screen_share(config).await {
                Ok(()) => {
                    set_is_sharing.set(true);
                    set_show.set(false);
                }
                Err(err) => set_error_msg.set(Some(err)),
            }
        });
    };

    view! {
        <Show when=move || show.get()>
            <div class="panel" style="margin-top: 1.25rem; background: #0f172a; border: 1px solid var(--accent);">
                <div class="row" style="justify-content: space-between; margin-top: 0;">
                    <h3>"Select Screen / Window & Stream Settings"</h3>
                    <button
                        class="choice-btn"
                        on:click=move |_| refresh_sources()
                    >
                        "Refresh Sources"
                    </button>
                </div>

                <div style="margin-top: 1rem;">
                    <label style="font-size: 0.9rem; color: var(--text-muted); font-weight: 600;">
                        "1. Choose Monitor or Window to Share:"
                    </label>
                    <div class="source-list">
                        {move || {
                            let current_sel = selected_idx.get();
                            sources
                                .get()
                                .into_iter()
                                .enumerate()
                                .map(|(idx, src)| {
                                    let (badge, title) = match src {
                                        CaptureTarget::Monitor { id, name } => {
                                            ("MONITOR", format!("#{id}: {name}"))
                                        }
                                        CaptureTarget::Window { title, pid, .. } => {
                                            ("WINDOW", format!("{title} (PID {pid})"))
                                        }
                                    };
                                    let is_active = current_sel == idx;
                                    let item_class = if is_active {
                                        "source-item active"
                                    } else {
                                        "source-item"
                                    };
                                    view! {
                                        <button
                                            type="button"
                                            class=item_class
                                            on:click=move |_| set_selected_idx.set(idx)
                                        >
                                            <span style="overflow: hidden; text-overflow: ellipsis; white-space: nowrap;">
                                                {title}
                                            </span>
                                            <span class="source-badge">{badge}</span>
                                        </button>
                                    }
                                })
                                .collect_view()
                        }}
                    </div>
                </div>

                <div class="row" style="margin-top: 1.1rem; gap: 1.5rem; align-items: flex-start;">
                    <div>
                        <label style="display: block; font-size: 0.85rem; color: var(--text-muted); font-weight: 600; margin-bottom: 0.4rem;">
                            "2. Resolution:"
                        </label>
                        <div class="row" style="margin-top: 0; gap: 0.45rem;">
                            {[
                                ("720p", "720p (1280x720)"),
                                ("1080p", "1080p (1920x1080)"),
                                ("native", "Native"),
                            ]
                            .into_iter()
                            .map(|(val, label)| {
                                let v = val.to_string();
                                let v_click = val.to_string();
                                view! {
                                    <button
                                        type="button"
                                        class=move || {
                                            if resolution_choice.get() == v {
                                                "choice-btn active"
                                            } else {
                                                "choice-btn"
                                            }
                                        }
                                        on:click=move |_| set_resolution_choice.set(v_click.clone())
                                    >
                                        {label}
                                    </button>
                                }
                            })
                            .collect_view()}
                        </div>
                    </div>

                    <div>
                        <label style="display: block; font-size: 0.85rem; color: var(--text-muted); font-weight: 600; margin-bottom: 0.4rem;">
                            "3. Frame Rate:"
                        </label>
                        <div class="row" style="margin-top: 0; gap: 0.45rem;">
                            {[("15", "15 FPS"), ("30", "30 FPS"), ("60", "60 FPS")]
                                .into_iter()
                                .map(|(val, label)| {
                                    let v = val.to_string();
                                    let v_click = val.to_string();
                                    view! {
                                        <button
                                            type="button"
                                            class=move || {
                                                if fps_choice.get() == v {
                                                    "choice-btn active"
                                                } else {
                                                    "choice-btn"
                                                }
                                            }
                                            on:click=move |_| set_fps_choice.set(v_click.clone())
                                        >
                                            {label}
                                        </button>
                                    }
                                })
                                .collect_view()}
                        </div>
                    </div>

                    <div>
                        <label style="display: block; font-size: 0.85rem; color: var(--text-muted); font-weight: 600; margin-bottom: 0.4rem;">
                            "4. Audio Mode:"
                        </label>
                        <div class="row" style="margin-top: 0; gap: 0.45rem;">
                            {[
                                ("system", "System Audio"),
                                ("app", "Window Audio Only (PID)"),
                                ("mute", "Mute (No Audio)"),
                            ]
                            .into_iter()
                            .map(|(val, label)| {
                                let v = val.to_string();
                                let v_click = val.to_string();
                                view! {
                                    <button
                                        type="button"
                                        class=move || {
                                            if audio_choice.get() == v {
                                                "choice-btn active"
                                            } else {
                                                "choice-btn"
                                            }
                                        }
                                        on:click=move |_| set_audio_choice.set(v_click.clone())
                                    >
                                        {label}
                                    </button>
                                }
                            })
                            .collect_view()}
                        </div>
                    </div>
                </div>

                <div class="row" style="margin-top: 1.25rem; justify-content: flex-end;">
                    <button on:click=on_start_share>"Start Streaming"</button>
                    <button class="danger" on:click=move |_| set_show.set(false)>
                        "Cancel"
                    </button>
                </div>
            </div>
        </Show>
    }
}
