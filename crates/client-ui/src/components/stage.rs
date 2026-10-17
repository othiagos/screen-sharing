use std::{cell::Cell, rc::Rc, time::Duration};

use leptos::{leptos_dom::helpers::set_interval_with_handle, *};
use shared_protocol::PeerId;
use wasm_bindgen_futures::spawn_local;

use crate::{
    components::picker::CapturePickerModal,
    ipc::{
        fetch_and_render_local_preview_canvas, fetch_and_render_peer_canvas,
        invoke_get_room_status, invoke_leave_room, invoke_set_peer_watched,
        invoke_set_window_fullscreen, invoke_stop_screen_share, RoomStatusSnapshot,
    },
};

#[component]
pub fn StageView(
    room_status: ReadSignal<RoomStatusSnapshot>,
    set_room_status: WriteSignal<RoomStatusSnapshot>,
    set_error_msg: WriteSignal<Option<String>>,
) -> impl IntoView {
    let (show_picker, set_show_picker) = create_signal(false);
    let (is_sharing, set_is_sharing) = create_signal(room_status.with_untracked(|s| s.is_sharing));
    let (show_local_preview, set_show_local_preview) = create_signal(true);
    let (fullscreen_peer, set_fullscreen_peer) = create_signal::<Option<PeerId>>(None);

    let keydown_handle = window_event_listener(ev::keydown, move |ev| {
        if ev.key() == "Escape" && fullscreen_peer.get_untracked().is_some() {
            set_fullscreen_peer.set(None);
            spawn_local(async move {
                let _ = invoke_set_window_fullscreen(false).await;
            });
        }
    });
    on_cleanup(move || keydown_handle.remove());

    // 1. Lightweight Room Status Heartbeat (every 1000ms) with cleanup and in-flight guard.
    // Only triggers a reactive update if the room state actually changed.
    let status_in_flight = Rc::new(Cell::new(false));
    if let Ok(status_handle) = set_interval_with_handle(
        move || {
            if !room_status.with_untracked(|s| s.connected) || status_in_flight.get() {
                return;
            }
            status_in_flight.set(true);
            let in_flight = Rc::clone(&status_in_flight);
            spawn_local(async move {
                if let Ok(snapshot) = invoke_get_room_status().await {
                    if is_sharing.get_untracked() != snapshot.is_sharing {
                        set_is_sharing.set(snapshot.is_sharing);
                    }
                    if let Some(fs_peer) = fullscreen_peer.get_untracked() {
                        let valid_fs = if fs_peer.is_nil() {
                            snapshot.is_sharing && show_local_preview.get_untracked()
                        } else {
                            snapshot.watched_peers.contains(&fs_peer)
                        };
                        if !valid_fs {
                            set_fullscreen_peer.set(None);
                            let _ = invoke_set_window_fullscreen(false).await;
                        }
                    }
                    let changed = room_status.with_untracked(|cur| cur != &snapshot);
                    if changed {
                        set_room_status.set(snapshot);
                    }
                }
                in_flight.set(false);
            });
        },
        Duration::from_millis(1000),
    ) {
        on_cleanup(move || status_handle.clear());
    }

    // 2. On-Demand 60 FPS Video Frame Renderer (every 14ms) — completely idle (0 IPC calls)
    // unless the user is watching at least one peer OR previewing their own active screen share.
    let frame_in_flight = Rc::new(Cell::new(false));
    if let Ok(frame_handle) = set_interval_with_handle(
        move || {
            if frame_in_flight.get() {
                return;
            }
            let watched = room_status.with_untracked(|s| {
                if s.connected {
                    s.watched_peers.clone()
                } else {
                    Vec::new()
                }
            });
            let render_local =
                is_sharing.get_untracked() && show_local_preview.get_untracked();
            if watched.is_empty() && !render_local {
                return;
            }

            frame_in_flight.set(true);
            let in_flight = Rc::clone(&frame_in_flight);
            spawn_local(async move {
                if render_local {
                    let _ = fetch_and_render_local_preview_canvas("local-preview-canvas").await;
                }
                for peer_id in watched {
                    let canvas_id = format!("peer-canvas-{peer_id}");
                    let _ = fetch_and_render_peer_canvas(peer_id, &canvas_id).await;
                }
                in_flight.set(false);
            });
        },
        Duration::from_millis(14),
    ) {
        on_cleanup(move || frame_handle.clear());
    }

    let on_toggle_fullscreen = move |peer_id: PeerId| {
        let next = if fullscreen_peer.get_untracked() == Some(peer_id) {
            None
        } else {
            Some(peer_id)
        };
        set_fullscreen_peer.set(next);
        spawn_local(async move {
            let _ = invoke_set_window_fullscreen(next.is_some()).await;
        });
    };

    let on_toggle_watch = move |peer_id: PeerId, watch: bool| {
        if !watch && fullscreen_peer.get_untracked() == Some(peer_id) {
            set_fullscreen_peer.set(None);
            spawn_local(async move {
                let _ = invoke_set_window_fullscreen(false).await;
            });
        }
        spawn_local(async move {
            if let Err(err) = invoke_set_peer_watched(peer_id, watch).await {
                set_error_msg.set(Some(err));
                return;
            }
            if let Ok(snapshot) = invoke_get_room_status().await {
                set_room_status.set(snapshot);
            }
        });
    };

    let on_stop_share = move |_| {
        if fullscreen_peer.get_untracked() == Some(PeerId::nil()) {
            set_fullscreen_peer.set(None);
            spawn_local(async move {
                let _ = invoke_set_window_fullscreen(false).await;
            });
        }
        spawn_local(async move {
            match invoke_stop_screen_share().await {
                Ok(()) => set_is_sharing.set(false),
                Err(err) => set_error_msg.set(Some(err)),
            }
        });
    };

    let on_leave_room = move |_| {
        if fullscreen_peer.get_untracked().is_some() {
            set_fullscreen_peer.set(None);
            spawn_local(async move {
                let _ = invoke_set_window_fullscreen(false).await;
            });
        }
        spawn_local(async move {
            let _ = invoke_leave_room().await;
            set_room_status.set(RoomStatusSnapshot {
                connected: false,
                room_id: None,
                local_peer_id: None,
                remote_peers: Vec::new(),
                streaming_peers: Vec::new(),
                watched_peers: Vec::new(),
                is_sharing: false,
            });
        });
    };

    view! {
        <div class="panel">
            <div class="row" style="justify-content: space-between; margin-top: 0;">
                <div>
                    <h2>
                        "Room: "
                        {move || room_status.with(|s| s.room_id.clone().unwrap_or_else(|| "Unknown".to_string()))}
                    </h2>
                    <p style="color: var(--text-muted); font-size: 0.85rem; margin-top: 0.25rem;">
                        "Your Peer ID: "
                        {move || {
                            room_status
                                .with(|s| s.local_peer_id.map(|id| id.to_string()).unwrap_or_default())
                        }}
                    </p>
                </div>

                <div class="row" style="margin-top: 0;">
                    {move || {
                        if is_sharing.get() {
                            view! {
                                <button
                                    class="choice-btn"
                                    on:click=move |_| {
                                        let next = !show_local_preview.get_untracked();
                                        if !next && fullscreen_peer.get_untracked() == Some(PeerId::nil()) {
                                            set_fullscreen_peer.set(None);
                                            spawn_local(async move {
                                                let _ = invoke_set_window_fullscreen(false).await;
                                            });
                                        }
                                        set_show_local_preview.set(next);
                                    }
                                >
                                    {move || {
                                        if show_local_preview.get() {
                                            "Hide Local Preview"
                                        } else {
                                            "Show Local Preview"
                                        }
                                    }}
                                </button>
                                <button class="danger" on:click=on_stop_share>
                                    "Stop Sharing"
                                </button>
                            }
                            .into_view()
                        } else {
                            view! {
                                <button on:click=move |_| set_show_picker.set(true)>
                                    "Share Screen"
                                </button>
                            }
                            .into_view()
                        }
                    }}
                    <button class="danger" on:click=on_leave_room>
                        "Leave Room"
                    </button>
                </div>
            </div>

            <CapturePickerModal
                show=show_picker
                set_show=set_show_picker
                set_is_sharing=set_is_sharing
                set_error_msg=set_error_msg
            />

            <div class="panel" style="margin-top: 1.25rem; margin-bottom: 0; background: #0f172a;">
                <h3 style="font-size: 1rem; margin-bottom: 0.75rem;">
                    "Participants in Room ("
                    {move || room_status.with(|s| s.remote_peers.len() + 1)}
                    ")"
                </h3>

                {move || {
                    let peers = room_status.with(|s| s.remote_peers.clone());
                    let streaming = room_status.with(|s| s.streaming_peers.clone());
                    let watched = room_status.with(|s| s.watched_peers.clone());

                    if peers.is_empty() {
                        view! {
                            <p style="color: var(--text-muted); font-size: 0.9rem;">
                                "No other participants in the room yet. Share the Room ID above so others can join!"
                            </p>
                        }
                        .into_view()
                    } else {
                        view! {
                            <div style="display: flex; flex-direction: column; gap: 0.6rem;">
                                {peers
                                    .into_iter()
                                    .map(|peer_id| {
                                        let is_live = streaming.contains(&peer_id);
                                        let is_watching = watched.contains(&peer_id);
                                        let short_id = peer_id.to_string();

                                        view! {
                                            <div
                                                class="row"
                                                style="justify-content: space-between; margin-top: 0; padding: 0.65rem 0.9rem; background: var(--bg-panel); border-radius: 0.5rem; border: 1px solid #334155;"
                                            >
                                                <div class="row" style="margin-top: 0;">
                                                    <span style="font-weight: 600;">
                                                        {format!("Peer {short_id}")}
                                                    </span>
                                                    {if is_live {
                                                        view! {
                                                            <span style="background: var(--danger); color: #fff; font-size: 0.75rem; font-weight: 700; padding: 0.15rem 0.5rem; border-radius: 0.35rem; letter-spacing: 0.04em;">
                                                                "LIVE"
                                                            </span>
                                                        }
                                                        .into_view()
                                                    } else {
                                                        view! {
                                                            <span style="color: var(--text-muted); font-size: 0.85rem;">
                                                                "In Room (Not Sharing)"
                                                            </span>
                                                        }
                                                        .into_view()
                                                    }}
                                                </div>

                                                <div>
                                                    {if is_live {
                                                        if is_watching {
                                                            view! {
                                                                <button
                                                                    class="danger"
                                                                    on:click=move |_| on_toggle_watch(peer_id, false)
                                                                >
                                                                    "Stop Watching"
                                                                </button>
                                                            }
                                                            .into_view()
                                                        } else {
                                                            view! {
                                                                <button
                                                                    on:click=move |_| on_toggle_watch(peer_id, true)
                                                                >
                                                                    "Watch Stream"
                                                                </button>
                                                            }
                                                            .into_view()
                                                        }
                                                    } else {
                                                        view! { <span></span> }.into_view()
                                                    }}
                                                </div>
                                            </div>
                                        }
                                    })
                                    .collect_view()}
                            </div>
                        }
                        .into_view()
                    }
                }}
            </div>

            {move || {
                let watched = room_status.with(|s| s.watched_peers.clone());
                let sharing_preview = is_sharing.get() && show_local_preview.get();
                if watched.is_empty() && !sharing_preview {
                    view! {
                        <div style="margin-top: 1.25rem; padding: 2rem; text-align: center; color: var(--text-muted); border: 1px dashed #334155; border-radius: 0.75rem;">
                            "You are not watching any stream right now. When a participant goes "
                            <strong style="color: var(--text-primary);">"LIVE"</strong>
                            ", click "
                            <strong style="color: var(--accent);">"Watch Stream"</strong>
                            " above to view their screen and audio — or click "
                            <strong style="color: var(--accent);">"Share Screen"</strong>
                            " to broadcast your own."
                        </div>
                    }
                    .into_view()
                } else {
                    let local_nil = PeerId::nil();
                    view! {
                        <div class="stage-grid">
                            <Show when=move || is_sharing.get() && show_local_preview.get()>
                                <div
                                    class="peer-card"
                                    class:fullscreen-card=move || fullscreen_peer.get() == Some(local_nil)
                                >
                                    <div class="peer-header" style="align-items: center;">
                                        <div class="row" style="margin-top: 0; gap: 0.5rem;">
                                            <span style="color: var(--text-primary); font-weight: 600;">
                                                "Your Shared Screen (Preview)"
                                            </span>
                                            <span style="background: var(--accent); color: #fff; font-size: 0.7rem; font-weight: 700; padding: 0.1rem 0.45rem; border-radius: 0.3rem;">
                                                "SHARING"
                                            </span>
                                        </div>
                                        <div class="row" style="margin-top: 0; gap: 0.5rem;">
                                            <button
                                                class="choice-btn"
                                                style="padding: 0.3rem 0.65rem; font-size: 0.8rem;"
                                                on:click=move |_| on_toggle_fullscreen(local_nil)
                                            >
                                                {move || {
                                                    if fullscreen_peer.get() == Some(local_nil) {
                                                        "Exit Fullscreen"
                                                    } else {
                                                        "Fullscreen"
                                                    }
                                                }}
                                            </button>
                                            <button
                                                class="choice-btn"
                                                style="padding: 0.3rem 0.65rem; font-size: 0.8rem;"
                                                on:click=move |_| {
                                                    if fullscreen_peer.get_untracked() == Some(local_nil) {
                                                        set_fullscreen_peer.set(None);
                                                        spawn_local(async move {
                                                            let _ = invoke_set_window_fullscreen(false).await;
                                                        });
                                                    }
                                                    set_show_local_preview.set(false);
                                                }
                                            >
                                                "Hide Preview"
                                            </button>
                                        </div>
                                    </div>
                                    <canvas
                                        id="local-preview-canvas"
                                        width="1280"
                                        height="720"
                                        title="Double-click to toggle fullscreen"
                                        on:dblclick=move |_| on_toggle_fullscreen(local_nil)
                                    ></canvas>
                                </div>
                            </Show>

                            <For
                                each=move || room_status.with(|s| s.watched_peers.clone())
                                key=|peer_id: &PeerId| *peer_id
                                children=move |peer_id: PeerId| {
                                    let canvas_id = format!("peer-canvas-{peer_id}");
                                    let peer_label = format!("Peer {peer_id}");
                                    view! {
                                        <div
                                            class="peer-card"
                                            class:fullscreen-card=move || fullscreen_peer.get() == Some(peer_id)
                                        >
                                            <div class="peer-header" style="align-items: center;">
                                                <div class="row" style="margin-top: 0; gap: 0.5rem;">
                                                    <span style="color: var(--text-primary); font-weight: 600;">
                                                        {peer_label}
                                                    </span>
                                                    <span style="background: var(--danger); color: #fff; font-size: 0.7rem; font-weight: 700; padding: 0.1rem 0.45rem; border-radius: 0.3rem;">
                                                        "LIVE"
                                                    </span>
                                                </div>
                                                <div class="row" style="margin-top: 0; gap: 0.5rem;">
                                                    <button
                                                        class="choice-btn"
                                                        style="padding: 0.3rem 0.65rem; font-size: 0.8rem;"
                                                        on:click=move |_| on_toggle_fullscreen(peer_id)
                                                    >
                                                        {move || {
                                                            if fullscreen_peer.get() == Some(peer_id) {
                                                                "Exit Fullscreen"
                                                            } else {
                                                                "Fullscreen"
                                                            }
                                                        }}
                                                    </button>
                                                    <button
                                                        class="danger"
                                                        style="padding: 0.3rem 0.65rem; font-size: 0.8rem;"
                                                        on:click=move |_| on_toggle_watch(peer_id, false)
                                                    >
                                                        "Stop Watching"
                                                    </button>
                                                </div>
                                            </div>
                                            <canvas
                                                id=canvas_id
                                                width="1280"
                                                height="720"
                                                title="Double-click to toggle fullscreen"
                                                on:dblclick=move |_| on_toggle_fullscreen(peer_id)
                                            ></canvas>
                                        </div>
                                    }
                                }
                            />
                        </div>
                    }
                    .into_view()
                }
            }}
        </div>
    }
}
