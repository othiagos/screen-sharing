pub mod handler;
pub mod state;

use std::net::SocketAddr;

use anyhow::Result;
use axum::{routing::get, Router};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::handler::ws_handler;
use crate::state::SignalingState;

pub fn build_router(state: SignalingState) -> Router {
    Router::new()
        .route("/", get(|| async { "P2P Signaling Server OK" }))
        .route("/health", get(|| async { "OK" }))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

/// Spawns the signaling server on the requested address (use `127.0.0.1:0` for an ephemeral port).
/// Returns the bound `SocketAddr`, the shared `SignalingState`, and the server's `JoinHandle`.
pub async fn spawn_server(
    bind_addr: SocketAddr,
) -> Result<(SocketAddr, SignalingState, JoinHandle<()>)> {
    let state = SignalingState::new();
    let app = build_router(state.clone());
    let listener = TcpListener::bind(bind_addr).await?;
    let local_addr = listener.local_addr()?;

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Ok((local_addr, state, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use shared_protocol::{ClientSignal, PeerId, ServerSignal};
    use std::time::Duration;
    use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

    async fn send_signal<S>(sink: &mut S, signal: &ClientSignal)
    where
        S: futures_util::Sink<WsMessage> + Unpin,
        S::Error: std::fmt::Debug,
    {
        let payload = serde_json::to_string(signal).expect("serialize ClientSignal");
        sink.send(WsMessage::Text(payload))
            .await
            .expect("send ws text");
    }

    async fn recv_signal<St>(stream: &mut St) -> ServerSignal
    where
        St: futures_util::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("timed out waiting for ServerSignal")
                .expect("stream ended")
                .expect("ws message ok");
            if let WsMessage::Text(text) = msg {
                return serde_json::from_str::<ServerSignal>(&text)
                    .expect("deserialize ServerSignal");
            }
        }
    }

    #[tokio::test]
    async fn test_three_websocket_clients_room_lifecycle_and_cleanup() {
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("valid socket addr");
        let (bound_addr, state, server_handle) =
            spawn_server(addr).await.expect("spawn signaling server");
        let ws_url = format!("ws://{bound_addr}/ws");

        // Connect 3 WebSocket clients: Peer A, Peer B, Peer C
        let (ws_a, _) = connect_async(&ws_url).await.expect("connect Peer A");
        let (mut sink_a, mut stream_a) = ws_a.split();

        let (ws_b, _) = connect_async(&ws_url).await.expect("connect Peer B");
        let (mut sink_b, mut stream_b) = ws_b.split();

        let (ws_c, _) = connect_async(&ws_url).await.expect("connect Peer C");
        let (mut sink_c, mut stream_c) = ws_c.split();

        // 1. Peer A creates a room
        send_signal(&mut sink_a, &ClientSignal::CreateRoom).await;
        let (room_id, peer_a_id): (String, PeerId) = match recv_signal(&mut stream_a).await {
            ServerSignal::RoomCreated {
                room_id,
                local_peer_id,
            } => (room_id, local_peer_id),
            other => panic!("Expected RoomCreated, got {other:?}"),
        };
        assert_eq!(state.active_room_count(), 1);

        // 2. Peer B joins the room
        send_signal(
            &mut sink_b,
            &ClientSignal::JoinRoom {
                room_id: room_id.clone(),
            },
        )
        .await;

        let peer_b_id = match recv_signal(&mut stream_b).await {
            ServerSignal::RoomJoined {
                room_id: joined_room,
                local_peer_id,
                existing_peers,
            } => {
                assert_eq!(joined_room, room_id);
                assert_eq!(existing_peers, vec![peer_a_id]);
                local_peer_id
            }
            other => panic!("Expected RoomJoined for B, got {other:?}"),
        };

        // Peer A must receive PeerJoined for Peer B
        match recv_signal(&mut stream_a).await {
            ServerSignal::PeerJoined { peer_id } => assert_eq!(peer_id, peer_b_id),
            other => panic!("Expected PeerJoined(B) on A, got {other:?}"),
        }

        // 3. Peer C joins the room
        send_signal(
            &mut sink_c,
            &ClientSignal::JoinRoom {
                room_id: room_id.clone(),
            },
        )
        .await;

        let peer_c_id = match recv_signal(&mut stream_c).await {
            ServerSignal::RoomJoined {
                room_id: joined_room,
                local_peer_id,
                existing_peers,
            } => {
                assert_eq!(joined_room, room_id);
                assert_eq!(existing_peers.len(), 2);
                assert!(existing_peers.contains(&peer_a_id));
                assert!(existing_peers.contains(&peer_b_id));
                local_peer_id
            }
            other => panic!("Expected RoomJoined for C, got {other:?}"),
        };

        // Both A and B must receive PeerJoined(C)
        match recv_signal(&mut stream_a).await {
            ServerSignal::PeerJoined { peer_id } => assert_eq!(peer_id, peer_c_id),
            other => panic!("Expected PeerJoined(C) on A, got {other:?}"),
        }
        match recv_signal(&mut stream_b).await {
            ServerSignal::PeerJoined { peer_id } => assert_eq!(peer_id, peer_c_id),
            other => panic!("Expected PeerJoined(C) on B, got {other:?}"),
        }

        // 4. Exchange SDP Offers, Answers, and ICE Candidates strictly routed to target PeerId
        send_signal(
            &mut sink_c,
            &ClientSignal::SdpOffer {
                target: peer_a_id,
                sdp: "mock-offer-c-to-a".to_string(),
            },
        )
        .await;

        match recv_signal(&mut stream_a).await {
            ServerSignal::IncomingOffer { from, sdp } => {
                assert_eq!(from, peer_c_id);
                assert_eq!(sdp, "mock-offer-c-to-a");
            }
            other => panic!("Expected IncomingOffer on A, got {other:?}"),
        }

        send_signal(
            &mut sink_a,
            &ClientSignal::SdpAnswer {
                target: peer_c_id,
                sdp: "mock-answer-a-to-c".to_string(),
            },
        )
        .await;

        match recv_signal(&mut stream_c).await {
            ServerSignal::IncomingAnswer { from, sdp } => {
                assert_eq!(from, peer_a_id);
                assert_eq!(sdp, "mock-answer-a-to-c");
            }
            other => panic!("Expected IncomingAnswer on C, got {other:?}"),
        }

        send_signal(
            &mut sink_b,
            &ClientSignal::IceCandidate {
                target: peer_c_id,
                candidate: "candidate:mock-b-to-c".to_string(),
                sdp_mid: Some("0".to_string()),
                sdp_mline_index: Some(0),
            },
        )
        .await;

        match recv_signal(&mut stream_c).await {
            ServerSignal::IncomingIceCandidate {
                from,
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => {
                assert_eq!(from, peer_b_id);
                assert_eq!(candidate, "candidate:mock-b-to-c");
                assert_eq!(sdp_mid.as_deref(), Some("0"));
                assert_eq!(sdp_mline_index, Some(0));
            }
            other => panic!("Expected IncomingIceCandidate on C, got {other:?}"),
        }

        // 5. Disconnect all 3 clients and verify PeerLeft broadcasts & room purge
        let _ = sink_c.close().await;
        drop(sink_c);
        drop(stream_c);

        match recv_signal(&mut stream_a).await {
            ServerSignal::PeerLeft { peer_id } => assert_eq!(peer_id, peer_c_id),
            other => panic!("Expected PeerLeft(C) on A, got {other:?}"),
        }
        match recv_signal(&mut stream_b).await {
            ServerSignal::PeerLeft { peer_id } => assert_eq!(peer_id, peer_c_id),
            other => panic!("Expected PeerLeft(C) on B, got {other:?}"),
        }

        let _ = sink_b.close().await;
        drop(sink_b);
        drop(stream_b);

        match recv_signal(&mut stream_a).await {
            ServerSignal::PeerLeft { peer_id } => assert_eq!(peer_id, peer_b_id),
            other => panic!("Expected PeerLeft(B) on A, got {other:?}"),
        }

        let _ = sink_a.close().await;
        drop(sink_a);
        drop(stream_a);

        // Wait briefly for server disconnect cleanup task to purge the empty room
        for _ in 0..50 {
            if state.active_room_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            state.active_room_count(),
            0,
            "Empty room must be purged after all peers disconnect"
        );

        server_handle.abort();
    }
}
