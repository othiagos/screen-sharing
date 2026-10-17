use leptos::*;

use crate::{
    components::{lobby::LobbyView, stage::StageView},
    ipc::RoomStatusSnapshot,
};

#[component]
pub fn App() -> impl IntoView {
    let (room_status, set_room_status) = create_signal(RoomStatusSnapshot {
        connected: false,
        room_id: None,
        local_peer_id: None,
        remote_peers: Vec::new(),
        streaming_peers: Vec::new(),
        watched_peers: Vec::new(),
        is_sharing: false,
    });
    let (error_msg, set_error_msg) = create_signal::<Option<String>>(None);
    let is_connected = create_memo(move |_| room_status.with(|s| s.connected));

    view! {
        <main class="app-shell">
            <Show when=move || error_msg.get().is_some()>
                <div class="error-banner">
                    {move || error_msg.get().unwrap_or_default()}
                </div>
            </Show>

            <Show
                when=move || is_connected.get()
                fallback=move || {
                    view! {
                        <LobbyView
                            set_room_status=set_room_status
                            set_error_msg=set_error_msg
                        />
                    }
                }
            >
                <StageView
                    room_status=room_status
                    set_room_status=set_room_status
                    set_error_msg=set_error_msg
                />
            </Show>
        </main>
    }
}

