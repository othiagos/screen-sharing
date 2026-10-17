use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use shared_protocol::{
    CaptureTarget, IceServerConfig, PeerId, RoomId, StreamConfig,
};
use tauri::{ipc::Response as IpcResponse, State};
use tokio::sync::Mutex as AsyncMutex;

use crate::{
    media::{
        default_os_audio_capturer,
        traits::VideoCapturer,
        video::{capture_preview_packet, list_capture_targets, NativeVideoCapturer},
    },
    p2p::mesh::MeshPeerManager,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomStatusSnapshot {
    pub connected: bool,
    pub room_id: Option<RoomId>,
    pub local_peer_id: Option<PeerId>,
    pub remote_peers: Vec<PeerId>,
    #[serde(default)]
    pub streaming_peers: Vec<PeerId>,
    #[serde(default)]
    pub watched_peers: Vec<PeerId>,
    pub is_sharing: bool,
}

#[derive(Default)]
pub struct ClientAppState {
    mesh: AsyncMutex<Option<MeshPeerManager>>,
    is_sharing: RwLock<bool>,
}

impl ClientAppState {
    pub fn new() -> Self {
        Self::default()
    }
}

#[tauri::command]
pub async fn create_room_session(
    state: State<'_, Arc<ClientAppState>>,
    signaling_url: String,
    custom_ice_servers: Vec<IceServerConfig>,
) -> Result<RoomStatusSnapshot, String> {
    leave_room_internal(&state).await;

    let (manager, ready_rx) = MeshPeerManager::connect(&signaling_url, custom_ice_servers, true)
        .await
        .map_err(|e| e.to_string())?;

    let (room_id, local_peer_id) = manager
        .create_room(ready_rx)
        .await
        .map_err(|e| e.to_string())?;

    let remote_peers = manager.all_peer_ids();
    let streaming_peers = manager.media_store().streaming_peers();
    let watched_peers = manager.media_store().watched_peers();
    *state.mesh.lock().await = Some(manager);
    *state.is_sharing.write() = false;

    Ok(RoomStatusSnapshot {
        connected: true,
        room_id: Some(room_id),
        local_peer_id: Some(local_peer_id),
        remote_peers,
        streaming_peers,
        watched_peers,
        is_sharing: false,
    })
}

#[tauri::command]
pub async fn join_room_session(
    state: State<'_, Arc<ClientAppState>>,
    signaling_url: String,
    room_id: RoomId,
    custom_ice_servers: Vec<IceServerConfig>,
) -> Result<RoomStatusSnapshot, String> {
    leave_room_internal(&state).await;

    let (manager, ready_rx) = MeshPeerManager::connect(&signaling_url, custom_ice_servers, true)
        .await
        .map_err(|e| e.to_string())?;

    let (joined_room_id, local_peer_id) = manager
        .join_room(room_id, ready_rx)
        .await
        .map_err(|e| e.to_string())?;

    let remote_peers = manager.all_peer_ids();
    let streaming_peers = manager.media_store().streaming_peers();
    let watched_peers = manager.media_store().watched_peers();
    *state.mesh.lock().await = Some(manager);
    *state.is_sharing.write() = false;

    Ok(RoomStatusSnapshot {
        connected: true,
        room_id: Some(joined_room_id),
        local_peer_id: Some(local_peer_id),
        remote_peers,
        streaming_peers,
        watched_peers,
        is_sharing: false,
    })
}

#[tauri::command]
pub async fn leave_room_session(state: State<'_, Arc<ClientAppState>>) -> Result<(), String> {
    leave_room_internal(&state).await;
    Ok(())
}

async fn leave_room_internal(state: &Arc<ClientAppState>) {
    *state.is_sharing.write() = false;
    let prev = state.mesh.lock().await.take();
    if let Some(mgr) = prev {
        mgr.shutdown().await;
    }
}

#[tauri::command]
pub async fn get_room_status(
    state: State<'_, Arc<ClientAppState>>,
) -> Result<RoomStatusSnapshot, String> {
    let guard = state.mesh.lock().await;
    let is_sharing = *state.is_sharing.read();
    if let Some(mgr) = guard.as_ref() {
        Ok(RoomStatusSnapshot {
            connected: true,
            room_id: mgr.room_id(),
            local_peer_id: mgr.local_peer_id(),
            remote_peers: mgr.all_peer_ids(),
            streaming_peers: mgr.media_store().streaming_peers(),
            watched_peers: mgr.media_store().watched_peers(),
            is_sharing,
        })
    } else {
        Ok(RoomStatusSnapshot {
            connected: false,
            room_id: None,
            local_peer_id: None,
            remote_peers: Vec::new(),
            streaming_peers: Vec::new(),
            watched_peers: Vec::new(),
            is_sharing: false,
        })
    }
}

#[tauri::command]
pub async fn set_peer_watched(
    state: State<'_, Arc<ClientAppState>>,
    peer_id: PeerId,
    watched: bool,
) -> Result<(), String> {
    let mgr = {
        let guard = state.mesh.lock().await;
        guard.as_ref().cloned()
    };

    if let Some(mgr) = mgr {
        mgr.set_peer_watched(peer_id, watched).await;
    }
    Ok(())
}

#[tauri::command]
pub async fn list_capture_sources() -> Result<Vec<CaptureTarget>, String> {
    tokio::task::spawn_blocking(list_capture_targets)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn capture_target_preview(target: CaptureTarget) -> Result<IpcResponse, String> {
    let packet = tokio::task::spawn_blocking(move || capture_preview_packet(&target))
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    Ok(IpcResponse::new(packet))
}

#[tauri::command]
pub async fn start_screen_share(
    state: State<'_, Arc<ClientAppState>>,
    config: StreamConfig,
) -> Result<(), String> {
    let mgr = {
        let guard = state.mesh.lock().await;
        guard
            .as_ref()
            .cloned()
            .ok_or_else(|| "Not connected to a room".to_string())?
    };

    let video_capturer: Arc<dyn VideoCapturer> = Arc::new(NativeVideoCapturer::new());
    let audio_capturer = default_os_audio_capturer();

    mgr.start_sharing(config, video_capturer, audio_capturer)
        .await
        .map_err(|e| e.to_string())?;
    *state.is_sharing.write() = true;
    Ok(())
}

#[tauri::command]
pub async fn stop_screen_share(state: State<'_, Arc<ClientAppState>>) -> Result<(), String> {
    let mgr = {
        let guard = state.mesh.lock().await;
        guard.as_ref().cloned()
    };

    if let Some(mgr) = mgr {
        mgr.stop_sharing().await.map_err(|e| e.to_string())?;
    }
    *state.is_sharing.write() = false;
    Ok(())
}

/// Returns the latest decoded RGBA video frame for `peer_id` as a raw binary `ArrayBuffer`
/// prefixed with the 24-byte header `[u32 width, u32 height, [u8; 16] peer_uuid]`.
/// Uses `tauri::ipc::Response::new(Vec<u8>)` to bypass JSON serialization completely,
/// and returns an empty buffer when no new frame has been decoded since the previous poll.
#[tauri::command]
pub async fn fetch_peer_frame(
    state: State<'_, Arc<ClientAppState>>,
    peer_id: PeerId,
) -> Result<IpcResponse, String> {
    let mgr = {
        let guard = state.mesh.lock().await;
        guard.as_ref().cloned()
    };

    let Some(mgr) = mgr else {
        return Ok(IpcResponse::new(Vec::new()));
    };

    let packet = mgr
        .media_store()
        .take_new_frame_packet(peer_id)
        .unwrap_or_default();
    Ok(IpcResponse::new(packet))
}

#[tauri::command]
pub async fn fetch_local_preview_frame(
    state: State<'_, Arc<ClientAppState>>,
) -> Result<IpcResponse, String> {
    let mgr = {
        let guard = state.mesh.lock().await;
        guard.as_ref().cloned()
    };

    let Some(mgr) = mgr else {
        return Ok(IpcResponse::new(Vec::new()));
    };

    let packet = mgr
        .media_store()
        .take_new_local_preview_packet()
        .unwrap_or_default();
    Ok(IpcResponse::new(packet))
}

#[tauri::command]
pub async fn set_window_fullscreen(
    window: tauri::Window,
    fullscreen: bool,
) -> Result<(), String> {
    window.set_fullscreen(fullscreen).map_err(|e| e.to_string())
}

pub fn build_tauri_app() -> tauri::Builder<tauri::Wry> {
    let app_state = Arc::new(ClientAppState::new());
    tauri::Builder::default()
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            create_room_session,
            join_room_session,
            leave_room_session,
            get_room_status,
            set_peer_watched,
            list_capture_sources,
            capture_target_preview,
            start_screen_share,
            stop_screen_share,
            fetch_peer_frame,
            fetch_local_preview_frame,
            set_window_fullscreen,
        ])
}


