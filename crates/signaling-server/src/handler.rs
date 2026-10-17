use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use shared_protocol::{ClientSignal, ServerSignal};
use tokio::sync::mpsc;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::state::SignalingState;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<SignalingState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: SignalingState) {
    let peer_id = Uuid::new_v4();
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerSignal>();

    debug!(%peer_id, "WebSocket peer connected");

    let send_task = tokio::spawn(async move {
        while let Some(signal) = rx.recv().await {
            match serde_json::to_string(&signal) {
                Ok(text) => {
                    if ws_sender.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
                Err(err) => {
                    warn!("Failed to serialize ServerSignal: {err}");
                }
            }
        }
    });

    while let Some(msg_res) = ws_receiver.next().await {
        let msg = match msg_res {
            Ok(m) => m,
            Err(err) => {
                debug!(%peer_id, "WebSocket read error: {err}");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                match serde_json::from_str::<ClientSignal>(&text) {
                    Ok(client_signal) => {
                        handle_client_signal(peer_id, client_signal, &tx, &state);
                    }
                    Err(err) => {
                        let _ = tx.send(ServerSignal::Error {
                            message: format!("Invalid signaling payload: {err}"),
                        });
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    state.leave_current_room(peer_id);
    send_task.abort();
    debug!(%peer_id, "WebSocket peer disconnected and cleaned up");
}

fn handle_client_signal(
    peer_id: Uuid,
    signal: ClientSignal,
    tx: &mpsc::UnboundedSender<ServerSignal>,
    state: &SignalingState,
) {
    match signal {
        ClientSignal::CreateRoom => {
            state.create_room(peer_id, tx.clone());
        }
        ClientSignal::JoinRoom { room_id } => {
            let trimmed = room_id.trim().to_string();
            if trimmed.is_empty() {
                let _ = tx.send(ServerSignal::Error {
                    message: "Room ID cannot be empty".to_string(),
                });
                return;
            }
            state.join_room(trimmed, peer_id, tx.clone());
        }
        ClientSignal::LeaveRoom => {
            state.leave_current_room(peer_id);
        }
        ClientSignal::SdpOffer { target, sdp } => {
            if let Err(err) = state.route_to_peer(
                peer_id,
                target,
                ServerSignal::IncomingOffer { from: peer_id, sdp },
            ) {
                let _ = tx.send(ServerSignal::Error { message: err });
            }
        }
        ClientSignal::SdpAnswer { target, sdp } => {
            if let Err(err) = state.route_to_peer(
                peer_id,
                target,
                ServerSignal::IncomingAnswer { from: peer_id, sdp },
            ) {
                let _ = tx.send(ServerSignal::Error { message: err });
            }
        }
        ClientSignal::IceCandidate {
            target,
            candidate,
            sdp_mid,
            sdp_mline_index,
        } => {
            if let Err(err) = state.route_to_peer(
                peer_id,
                target,
                ServerSignal::IncomingIceCandidate {
                    from: peer_id,
                    candidate,
                    sdp_mid,
                    sdp_mline_index,
                },
            ) {
                let _ = tx.send(ServerSignal::Error { message: err });
            }
        }
    }
}
