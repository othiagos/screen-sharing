use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use quinn::{Connection, RecvStream, SendStream};
use shared_protocol::{
    decode_control_payload, encode_control_frame, AudioPacketHeader, ClientControl,
    ControlErrorCode, PeerId, ServerControl, VideoFrameHeader, AUDIO_HEADER_LEN,
    MAX_AUDIO_PACKET_BYTES, MAX_CONTROL_FRAME_BYTES, MAX_VIDEO_FRAME_BYTES, MEDIA_KIND_AUDIO,
    MEDIA_KIND_VIDEO, PROTOCOL_VERSION, VIDEO_HEADER_LEN,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use uuid::Uuid;

use crate::state::{ConnectedPeer, OutboundMedia, SfuState};

pub async fn handle_quic_connection(conn: Connection, state: Arc<SfuState>) {
    let remote = conn.remote_address();
    let (mut ctrl_send, mut ctrl_recv) = match conn.accept_bi().await {
        Ok(streams) => streams,
        Err(err) => {
            debug!(%remote, "Failed accepting control stream: {err}");
            return;
        }
    };

    let hello = match read_control_message(&mut ctrl_recv).await {
        Ok(Some(msg)) => msg,
        Ok(None) => return,
        Err(err) => {
            debug!(%remote, "Failed reading initial Hello frame: {err}");
            return;
        }
    };

    let (nickname, auth_token) = match hello {
        ClientControl::Hello {
            protocol_version,
            nickname,
            auth_token,
        } => {
            if protocol_version != PROTOCOL_VERSION {
                reject_with_error(
                    &conn,
                    &mut ctrl_send,
                    ControlErrorCode::ProtocolMismatch,
                    format!(
                        "Protocol version mismatch: server={PROTOCOL_VERSION}, client={protocol_version}"
                    ),
                )
                .await;
                return;
            }
            (nickname, auth_token)
        }
        _ => {
            reject_with_error(
                &conn,
                &mut ctrl_send,
                ControlErrorCode::BadRequest,
                "Expected Hello as first control message".to_string(),
            )
            .await;
            return;
        }
    };

    if !state.verify_auth_token(auth_token.as_deref()) {
        reject_with_error(
            &conn,
            &mut ctrl_send,
            ControlErrorCode::Unauthorized,
            "Invalid or missing server auth token".to_string(),
        )
        .await;
        return;
    }

    let clean_nick = {
        let trimmed = nickname.trim();
        if trimmed.is_empty() {
            "Player".to_string()
        } else {
            trimmed.chars().take(32).collect()
        }
    };

    let peer_id = Uuid::new_v4();
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerControl>();
    let (peer_raw, mut video_rx, mut audio_rx) =
        ConnectedPeer::new(peer_id, clean_nick.clone(), ctrl_tx);
    let peer = Arc::new(peer_raw);
    state.register_peer(Arc::clone(&peer));

    if write_control_message(&mut ctrl_send, &ServerControl::Welcome { local_peer_id: peer_id })
        .await
        .is_err()
    {
        state.remove_peer(peer_id);
        return;
    }

    info!(%peer_id, nickname = %clean_nick, %remote, "QUIC peer authenticated");

    let cancel = CancellationToken::new();

    // 1. Control writer task
    let cancel_ctrl_w = cancel.clone();
    let ctrl_writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_ctrl_w.cancelled() => break,
                maybe_msg = ctrl_rx.recv() => {
                    let Some(msg) = maybe_msg else { break };
                    if write_control_message(&mut ctrl_send, &msg).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = ctrl_send.finish();
    });

    // 2. Outbound video UniStream sender
    let conn_vid_w = conn.clone();
    let cancel_vid_w = cancel.clone();
    let video_writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_vid_w.cancelled() => break,
                maybe_item = video_rx.recv() => {
                    let Some(OutboundMedia::Video { packet, .. }) = maybe_item else { break };
                    let Ok(mut stream) = conn_vid_w.open_uni().await else { break };
                    if stream.write_all(&packet).await.is_err() {
                        break;
                    }
                    let _ = stream.finish();
                }
            }
        }
    });

    // 3. Outbound audio Datagram (with UniStream fallback) sender
    let conn_aud_w = conn.clone();
    let cancel_aud_w = cancel.clone();
    let audio_writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_aud_w.cancelled() => break,
                maybe_item = audio_rx.recv() => {
                    let Some(OutboundMedia::Audio { packet, .. }) = maybe_item else { break };
                    match conn_aud_w.send_datagram(packet.clone()) {
                        Ok(()) => {}
                        Err(_) => {
                            if let Ok(mut stream) = conn_aud_w.open_uni().await {
                                let _ = stream.write_all(&packet).await;
                                let _ = stream.finish();
                            }
                        }
                    }
                }
            }
        }
    });

    // 4. Inbound UniStreams reader (video frames + fallback audio packets)
    let conn_uni_r = conn.clone();
    let state_uni_r = Arc::clone(&state);
    let cancel_uni_r = cancel.clone();
    let uni_reader = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_uni_r.cancelled() => break,
                accept_res = conn_uni_r.accept_uni() => {
                    let Ok(mut recv) = accept_res else { break };
                    let state_inner = Arc::clone(&state_uni_r);
                    tokio::spawn(async move {
                        let max_bytes = MAX_VIDEO_FRAME_BYTES + VIDEO_HEADER_LEN;
                        let Ok(raw) = recv.read_to_end(max_bytes).await else { return };
                        if raw.is_empty() {
                            return;
                        }
                        match raw[0] {
                            MEDIA_KIND_VIDEO => {
                                if let Ok((hdr, _)) = VideoFrameHeader::decode(&raw) {
                                    if hdr.sender_id == peer_id {
                                        state_inner.forward_video(
                                            peer_id,
                                            hdr.is_keyframe(),
                                            Bytes::from(raw),
                                        );
                                    }
                                }
                            }
                            MEDIA_KIND_AUDIO
                                if raw.len() <= MAX_AUDIO_PACKET_BYTES + AUDIO_HEADER_LEN =>
                            {
                                if let Ok((hdr, _)) = AudioPacketHeader::decode(&raw) {
                                    if hdr.sender_id == peer_id {
                                        state_inner.forward_audio(peer_id, Bytes::from(raw));
                                    }
                                }
                            }
                            _ => {}
                        }
                    });
                }
            }
        }
    });

    // 5. Inbound Datagram reader (Opus audio packets)
    let conn_dg_r = conn.clone();
    let state_dg_r = Arc::clone(&state);
    let cancel_dg_r = cancel.clone();
    let dg_reader = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_dg_r.cancelled() => break,
                dg_res = conn_dg_r.read_datagram() => {
                    let Ok(bytes) = dg_res else { break };
                    if let Ok((hdr, _)) = AudioPacketHeader::decode(&bytes) {
                        if hdr.sender_id == peer_id {
                            state_dg_r.forward_audio(peer_id, bytes);
                        }
                    }
                }
            }
        }
    });

    // 6. Control reader loop
    loop {
        match read_control_message(&mut ctrl_recv).await {
            Ok(Some(msg)) => handle_client_control(peer_id, msg, &peer, &state),
            Ok(None) => break,
            Err(err) => {
                debug!(%peer_id, "Control stream ended/errored: {err}");
                break;
            }
        }
    }

    cancel.cancel();
    ctrl_writer.abort();
    video_writer.abort();
    audio_writer.abort();
    uni_reader.abort();
    dg_reader.abort();
    state.remove_peer(peer_id);
    info!(%peer_id, "QUIC peer disconnected and cleaned up");
}

fn handle_client_control(
    peer_id: PeerId,
    msg: ClientControl,
    peer: &Arc<ConnectedPeer>,
    state: &Arc<SfuState>,
) {
    match msg {
        ClientControl::Hello { .. } => {}
        ClientControl::CreateRoom { password } => {
            if let Err((code, message)) = state.create_room(peer_id, password.as_deref()) {
                peer.send_control(ServerControl::Error { code, message });
            }
        }
        ClientControl::JoinRoom { room_id, password } => {
            if let Err((code, message)) = state.join_room(&room_id, peer_id, password.as_deref()) {
                peer.send_control(ServerControl::Error { code, message });
            }
        }
        ClientControl::LeaveRoom => {
            state.leave_current_room(peer_id);
        }
        ClientControl::StartBroadcast { metadata } => {
            state.set_broadcast(peer_id, Some(metadata));
        }
        ClientControl::StopBroadcast => {
            state.set_broadcast(peer_id, None);
        }
        ClientControl::SetSubscription {
            broadcaster_id,
            video,
            audio,
        } => {
            state.set_subscription(peer_id, broadcaster_id, video, audio);
        }
        ClientControl::RequestKeyframe { broadcaster_id } => {
            state.request_keyframe(peer_id, broadcaster_id);
        }
        ClientControl::Ping { client_time_ms } => {
            peer.send_control(ServerControl::Pong { client_time_ms });
        }
    }
}

async fn read_control_message(recv: &mut RecvStream) -> Result<Option<ClientControl>> {
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_CONTROL_FRAME_BYTES {
        anyhow::bail!("Control frame too large: {len} > {MAX_CONTROL_FRAME_BYTES}");
    }
    let mut payload = vec![0u8; len];
    recv.read_exact(&mut payload).await?;
    let msg: ClientControl = decode_control_payload(&payload)?;
    Ok(Some(msg))
}

async fn write_control_message(send: &mut SendStream, msg: &ServerControl) -> Result<()> {
    let framed = encode_control_frame(msg)?;
    send.write_all(&framed).await?;
    Ok(())
}

async fn reject_with_error(
    conn: &Connection,
    ctrl_send: &mut SendStream,
    code: ControlErrorCode,
    message: String,
) {
    let _ = write_control_message(ctrl_send, &ServerControl::Error { code, message }).await;
    let _ = ctrl_send.finish();
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), conn.closed()).await;
}
