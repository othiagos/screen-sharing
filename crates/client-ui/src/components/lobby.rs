use leptos::*;
use shared_protocol::IceServerConfig;
use wasm_bindgen_futures::spawn_local;

use crate::ipc::{invoke_create_room, invoke_join_room, RoomStatusSnapshot};

#[component]
pub fn LobbyView(
    set_room_status: WriteSignal<RoomStatusSnapshot>,
    set_error_msg: WriteSignal<Option<String>>,
) -> impl IntoView {
    let (signaling_url, set_signaling_url) =
        create_signal("ws://127.0.0.1:9001/ws".to_string());
    let (room_id_input, set_room_id_input) = create_signal(String::new());
    let (show_turn_panel, set_show_turn_panel) = create_signal(false);
    let (turn_url, set_turn_url) = create_signal(String::new());
    let (turn_username, set_turn_username) = create_signal(String::new());
    let (turn_credential, set_turn_credential) = create_signal(String::new());
    let (busy, set_busy) = create_signal(false);

    let build_turn_servers = move || -> Vec<IceServerConfig> {
        let url = turn_url.get().trim().to_string();
        if url.is_empty() {
            return Vec::new();
        }
        let user = turn_username.get().trim().to_string();
        let cred = turn_credential.get().trim().to_string();
        vec![IceServerConfig {
            urls: vec![url],
            username: if user.is_empty() { None } else { Some(user) },
            credential: if cred.is_empty() { None } else { Some(cred) },
        }]
    };

    let on_create_room = move |_| {
        if busy.get() {
            return;
        }
        set_busy.set(true);
        set_error_msg.set(None);
        let url = signaling_url.get();
        let custom_ice = build_turn_servers();

        spawn_local(async move {
            match invoke_create_room(url, custom_ice).await {
                Ok(snapshot) => set_room_status.set(snapshot),
                Err(err) => set_error_msg.set(Some(err)),
            }
            set_busy.set(false);
        });
    };

    let on_join_room = move |_| {
        if busy.get() {
            return;
        }
        let target_room = room_id_input.get().trim().to_string();
        if target_room.is_empty() {
            set_error_msg.set(Some("Please enter a Room ID to join.".to_string()));
            return;
        }
        set_busy.set(true);
        set_error_msg.set(None);
        let url = signaling_url.get();
        let custom_ice = build_turn_servers();

        spawn_local(async move {
            match invoke_join_room(url, target_room, custom_ice).await {
                Ok(snapshot) => set_room_status.set(snapshot),
                Err(err) => set_error_msg.set(Some(err)),
            }
            set_busy.set(false);
        });
    };

    view! {
        <div class="panel">
            <h2>"P2P Screen Sharing Lobby"</h2>
            <div class="row">
                <label>"Signaling Server URL:"</label>
                <input
                    type="text"
                    prop:value=signaling_url
                    on:input=move |ev| set_signaling_url.set(event_target_value(&ev))
                    placeholder="ws://127.0.0.1:9001/ws"
                />
                <button on:click=on_create_room disabled=busy>
                    "Create Room"
                </button>
            </div>

            <div class="row">
                <label>"Room ID:"</label>
                <input
                    type="text"
                    prop:value=room_id_input
                    on:input=move |ev| set_room_id_input.set(event_target_value(&ev))
                    placeholder="Enter Room ID"
                />
                <button on:click=on_join_room disabled=busy>
                    "Join Room"
                </button>
            </div>

            <div class="row">
                <button on:click=move |_| set_show_turn_panel.update(|v| *v = !*v)>
                    {move || if show_turn_panel.get() {
                        "Hide Custom TURN Configuration"
                    } else {
                        "Configure Custom TURN Server (Optional)"
                    }}
                </button>
            </div>

            <Show when=move || show_turn_panel.get()>
                <div class="panel" style="margin-top: 1rem;">
                    <h3>"Custom TURN Server Fallback"</h3>
                    <div class="row">
                        <input
                            type="text"
                            prop:value=turn_url
                            on:input=move |ev| set_turn_url.set(event_target_value(&ev))
                            placeholder="turn:turn.example.com:3478"
                        />
                        <input
                            type="text"
                            prop:value=turn_username
                            on:input=move |ev| set_turn_username.set(event_target_value(&ev))
                            placeholder="Username"
                        />
                        <input
                            type="password"
                            prop:value=turn_credential
                            on:input=move |ev| set_turn_credential.set(event_target_value(&ev))
                            placeholder="Credential / Password"
                        />
                    </div>
                </div>
            </Show>
        </div>
    }
}
