use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use desktop_client::{
    media::mock::{MockAudioCapturer, MockVideoCapturer},
    p2p::mesh::MeshPeerManager,
};
use shared_protocol::{
     decode_frame_packet, AudioCaptureMode, CaptureTarget, StreamConfig, TargetResolution,
};
use signaling_server::spawn_server;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_three_peer_full_mesh_serverless_renegotiation_and_media() {
    // 1. Spawn signaling-server on an ephemeral local port (127.0.0.1:0)
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("valid SocketAddr");
    let (bound_addr, _server_state, server_handle) =
        spawn_server(bind_addr).await.expect("spawn signaling server");
    let ws_url = format!("ws://{bound_addr}/ws");

    // 2. Instantiate 3 headless client instances (Peer A, Peer B, Peer C)
    let (peer_a, ready_a) = MeshPeerManager::connect(&ws_url, Vec::new(), false)
        .await
        .expect("connect Peer A");
    let (room_id, peer_a_id) = peer_a.create_room(ready_a).await.expect("Peer A create room");

    let (peer_b, ready_b) = MeshPeerManager::connect(&ws_url, Vec::new(), false)
        .await
        .expect("connect Peer B");
    let (_, peer_b_id) = peer_b
        .join_room(room_id.clone(), ready_b)
        .await
        .expect("Peer B join room");

    let (peer_c, ready_c) = MeshPeerManager::connect(&ws_url, Vec::new(), false)
        .await
        .expect("connect Peer C");
    let (_, peer_c_id) = peer_c
        .join_room(room_id.clone(), ready_c)
        .await
        .expect("Peer C join room");

    assert_ne!(peer_a_id, peer_b_id);
    assert_ne!(peer_b_id, peer_c_id);

    // 3. Wait for all mesh RTCPeerConnections to reach Connected and "mesh-control" DCs to open
    let mesh_timeout = Duration::from_secs(20);
    peer_a
        .wait_for_mesh_ready(2, mesh_timeout)
        .await
        .expect("Peer A mesh ready with 2 peers");
    peer_b
        .wait_for_mesh_ready(2, mesh_timeout)
        .await
        .expect("Peer B mesh ready with 2 peers");
    peer_c
        .wait_for_mesh_ready(2, mesh_timeout)
        .await
        .expect("Peer C mesh ready with 2 peers");

    // 4. Abort / Shut down the signaling-server task completely!
    server_handle.abort();
    let _ = server_handle.await;

    // Give a brief moment for WebSocket disconnects to propagate so we prove signaling is 100% dead
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 5. Start streaming from Peer A using renegotiation over "mesh-control"
    let stream_cfg = StreamConfig {
        target: CaptureTarget::Monitor {
            id: 1,
            name: "Synthetic SMPTE Monitor".to_string(),
        },
        resolution: TargetResolution::R720p,
        fps: 30,
        audio_mode: AudioCaptureMode::SystemGlobal,
    };

    let mock_video = Arc::new(MockVideoCapturer::new(320, 240));
    let mock_audio = Arc::new(MockAudioCapturer::new());

    peer_a
        .start_sharing(stream_cfg, mock_video, mock_audio)
        .await
        .expect("Peer A start_sharing over mesh-control while signaling server is dead");

    // 6. Assert that Peer B and Peer C each receive and decode at least 30 valid video frames
    // and 50 valid Opus audio packets from Peer A while the signaling server is dead.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let b_video = peer_b.media_store().decoded_video_frames(peer_a_id);
        let b_audio = peer_b.media_store().decoded_audio_packets(peer_a_id);
        let c_video = peer_c.media_store().decoded_video_frames(peer_a_id);
        let c_audio = peer_c.media_store().decoded_audio_packets(peer_a_id);

        if b_video >= 30 && b_audio >= 50 && c_video >= 30 && c_audio >= 50 {
            break;
        }

        if tokio::time::Instant::now() >= deadline {
            panic!(
                "Timed out waiting for serverless P2P media! \
                 Peer B received (video={b_video}/30, audio={b_audio}/50), \
                 Peer C received (video={c_video}/30, audio={c_audio}/50)"
            );
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Verify binary frame packet formatting for WASM <canvas> rendering
    let frame_packet_b = peer_b
        .media_store()
        .latest_frame_packet(peer_a_id)
        .expect("Peer B must have latest decoded frame packet from Peer A");
    let (decoded_peer, width, height, rgba) =
        decode_frame_packet(&frame_packet_b).expect("valid 24-byte frame packet");
    assert_eq!(decoded_peer, peer_a_id);
    assert_eq!((width, height), (320, 240));
    assert_eq!(rgba.len(), 320 * 240 * 4);

    // Cleanly shut down all 3 peers
    peer_a.shutdown().await;
    peer_b.shutdown().await;
    peer_c.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_late_joiner_discovers_and_receives_existing_stream() {
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("valid SocketAddr");
    let (bound_addr, _server_state, server_handle) =
        spawn_server(bind_addr).await.expect("spawn signaling server");
    let ws_url = format!("ws://{bound_addr}/ws");

    // 1. Peer A creates a room and starts sharing BEFORE anyone else joins!
    let (peer_a, ready_a) = MeshPeerManager::connect(&ws_url, Vec::new(), true)
        .await
        .expect("connect Peer A");
    let (room_id, peer_a_id) = peer_a.create_room(ready_a).await.expect("Peer A create room");

    let stream_cfg = StreamConfig {
        target: CaptureTarget::Monitor {
            id: 1,
            name: "Synthetic Monitor".to_string(),
        },
        resolution: TargetResolution::R720p,
        fps: 30,
        audio_mode: AudioCaptureMode::SystemGlobal,
    };
    peer_a
        .start_sharing(
            stream_cfg,
            Arc::new(MockVideoCapturer::new(320, 240)),
            Arc::new(MockAudioCapturer::new()),
        )
        .await
        .expect("Peer A starts sharing while alone in room");

    // 2. Peer B joins the room LATER (with on-demand watching enabled, just like the GUI)
    let (peer_b, ready_b) = MeshPeerManager::connect(&ws_url, Vec::new(), true)
        .await
        .expect("connect Peer B");
    let (_, _peer_b_id) = peer_b
        .join_room(room_id, ready_b)
        .await
        .expect("Peer B join room");

    // 3. Verify that Peer B automatically detects Peer A in `streaming_peers()` (so "Watch Stream" appears!)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if peer_b.media_store().streaming_peers().contains(&peer_a_id) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("Timed out waiting for late-joiner Peer B to see Peer A in streaming_peers()");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 4. Now Peer B clicks "Watch Stream" on Peer A (`set_peer_watched(peer_a_id, true)`)
    peer_b.set_peer_watched(peer_a_id, true).await;

    // 5. Verify Peer B decodes video frames from Peer A
    let watch_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if peer_b.media_store().decoded_video_frames(peer_a_id) >= 5 {
            break;
        }
        if tokio::time::Instant::now() >= watch_deadline {
            panic!("Timed out waiting for late-joiner Peer B to decode frames after clicking Watch Stream");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    peer_a.shutdown().await;
    peer_b.shutdown().await;
    server_handle.abort();
}

