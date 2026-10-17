use js_sys::{Array, ArrayBuffer, Object, Reflect, Uint8Array, Uint8ClampedArray};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use shared_protocol::{
    CaptureTarget, IceServerConfig, PeerId, RoomId, StreamConfig, FRAME_HEADER_LEN,
};
use wasm_bindgen::{prelude::*, JsCast};
use wasm_bindgen_futures::JsFuture;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateRoomArgs {
    signaling_url: String,
    custom_ice_servers: Vec<IceServerConfig>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JoinRoomArgs {
    signaling_url: String,
    room_id: RoomId,
    custom_ice_servers: Vec<IceServerConfig>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StartShareArgs {
    config: StreamConfig,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SetPeerWatchedArgs {
    peer_id: PeerId,
    watched: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FetchFrameArgs {
    peer_id: PeerId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CapturePreviewArgs {
    target: CaptureTarget,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SetFullscreenArgs {
    fullscreen: bool,
}

async fn tauri_invoke_raw(cmd: &str, args: JsValue) -> Result<JsValue, String> {
    let window = web_sys::window().ok_or_else(|| "No browser window available".to_string())?;

    // Support both Tauri v2 `__TAURI_INTERNALS__.invoke` and `__TAURI__.core.invoke`
    let invoke_fn = Reflect::get(&window, &JsValue::from_str("__TAURI_INTERNALS__"))
        .ok()
        .and_then(|internals| {
            if internals.is_undefined() || internals.is_null() {
                None
            } else {
                Reflect::get(&internals, &JsValue::from_str("invoke")).ok()
            }
        })
        .or_else(|| {
            let tauri = Reflect::get(&window, &JsValue::from_str("__TAURI__")).ok()?;
            let core = Reflect::get(&tauri, &JsValue::from_str("core")).ok()?;
            Reflect::get(&core, &JsValue::from_str("invoke")).ok()
        })
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .ok_or_else(|| "Tauri IPC bridge not found on window".to_string())?;

    let promise_val = invoke_fn
        .call2(&JsValue::NULL, &JsValue::from_str(cmd), &args)
        .map_err(format_js_error)?;

    let promise = js_sys::Promise::from(promise_val);
    JsFuture::from(promise)
        .await
        .map_err(format_js_error)
}

fn format_js_error(err: JsValue) -> String {
    err.as_string()
        .or_else(|| js_sys::JSON::stringify(&err).ok().and_then(|s| s.as_string()))
        .unwrap_or_else(|| "Unknown Tauri IPC error".to_string())
}

async fn tauri_invoke<A: Serialize, R: DeserializeOwned>(
    cmd: &str,
    args: &A,
) -> Result<R, String> {
    let js_args = serde_wasm_bindgen::to_value(args).map_err(|e| e.to_string())?;
    let raw = tauri_invoke_raw(cmd, js_args).await?;
    serde_wasm_bindgen::from_value(raw).map_err(|e| e.to_string())
}

fn upgrade_ws_to_wss_on_redirect(url: &str, err: &str) -> Option<String> {
    if (err.contains("301") || err.contains("302") || err.contains("307") || err.contains("308"))
        && url.starts_with("ws://")
    {
        Some(url.replacen("ws://", "wss://", 1))
    } else {
        None
    }
}

pub async fn invoke_create_room(
    signaling_url: String,
    custom_ice_servers: Vec<IceServerConfig>,
) -> Result<RoomStatusSnapshot, String> {
    match tauri_invoke(
        "create_room_session",
        &CreateRoomArgs {
            signaling_url: signaling_url.clone(),
            custom_ice_servers: custom_ice_servers.clone(),
        },
    )
    .await
    {
        Ok(res) => Ok(res),
        Err(err) => {
            if let Some(wss_url) = upgrade_ws_to_wss_on_redirect(&signaling_url, &err) {
                tauri_invoke(
                    "create_room_session",
                    &CreateRoomArgs {
                        signaling_url: wss_url,
                        custom_ice_servers,
                    },
                )
                .await
            } else {
                Err(err)
            }
        }
    }
}

pub async fn invoke_join_room(
    signaling_url: String,
    room_id: RoomId,
    custom_ice_servers: Vec<IceServerConfig>,
) -> Result<RoomStatusSnapshot, String> {
    match tauri_invoke(
        "join_room_session",
        &JoinRoomArgs {
            signaling_url: signaling_url.clone(),
            room_id: room_id.clone(),
            custom_ice_servers: custom_ice_servers.clone(),
        },
    )
    .await
    {
        Ok(res) => Ok(res),
        Err(err) => {
            if let Some(wss_url) = upgrade_ws_to_wss_on_redirect(&signaling_url, &err) {
                tauri_invoke(
                    "join_room_session",
                    &JoinRoomArgs {
                        signaling_url: wss_url,
                        room_id,
                        custom_ice_servers,
                    },
                )
                .await
            } else {
                Err(err)
            }
        }
    }
}

pub async fn invoke_leave_room() -> Result<(), String> {
    let empty = Object::new();
    let _ = tauri_invoke_raw("leave_room_session", empty.into()).await?;
    Ok(())
}

pub async fn invoke_get_room_status() -> Result<RoomStatusSnapshot, String> {
    let empty = Object::new();
    let raw = tauri_invoke_raw("get_room_status", empty.into()).await?;
    serde_wasm_bindgen::from_value(raw).map_err(|e| e.to_string())
}

pub async fn invoke_list_capture_sources() -> Result<Vec<CaptureTarget>, String> {
    let empty = Object::new();
    let raw = tauri_invoke_raw("list_capture_sources", empty.into()).await?;
    serde_wasm_bindgen::from_value(raw).map_err(|e| e.to_string())
}

pub async fn invoke_start_screen_share(config: StreamConfig) -> Result<(), String> {
    let js_args =
        serde_wasm_bindgen::to_value(&StartShareArgs { config }).map_err(|e| e.to_string())?;
    let _ = tauri_invoke_raw("start_screen_share", js_args).await?;
    Ok(())
}

pub async fn invoke_stop_screen_share() -> Result<(), String> {
    let empty = Object::new();
    let _ = tauri_invoke_raw("stop_screen_share", empty.into()).await?;
    Ok(())
}

pub async fn invoke_set_peer_watched(peer_id: PeerId, watched: bool) -> Result<(), String> {
    let js_args = serde_wasm_bindgen::to_value(&SetPeerWatchedArgs { peer_id, watched })
        .map_err(|e| e.to_string())?;
    let _ = tauri_invoke_raw("set_peer_watched", js_args).await?;
    Ok(())
}

pub async fn invoke_set_window_fullscreen(fullscreen: bool) -> Result<(), String> {
    let js_args = serde_wasm_bindgen::to_value(&SetFullscreenArgs { fullscreen })
        .map_err(|e| e.to_string())?;
    let _ = tauri_invoke_raw("set_window_fullscreen", js_args).await?;
    Ok(())
}

fn render_binary_packet_to_canvas(raw: JsValue, canvas_id: &str) -> Result<bool, String> {
    let u8_array = if raw.is_instance_of::<ArrayBuffer>() {
        Uint8Array::new(&raw)
    } else if raw.is_instance_of::<Uint8Array>() {
        raw.unchecked_into::<Uint8Array>()
    } else {
        return Ok(false);
    };

    let packet_len = u8_array.length() as usize;
    if packet_len < FRAME_HEADER_LEN {
        return Ok(false);
    }

    let mut header = [0u8; FRAME_HEADER_LEN];
    u8_array
        .subarray(0, FRAME_HEADER_LEN as u32)
        .copy_to(&mut header);

    let width = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let height = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let expected_rgba_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| "Frame dimensions overflow".to_string())?;

    if width == 0 || height == 0 || packet_len != FRAME_HEADER_LEN + expected_rgba_len {
        return Ok(false);
    }

    let window = web_sys::window().ok_or_else(|| "No window".to_string())?;
    let document = window.document().ok_or_else(|| "No document".to_string())?;
    let Some(element) = document.get_element_by_id(canvas_id) else {
        return Ok(false);
    };
    let canvas: HtmlCanvasElement = element
        .dyn_into::<HtmlCanvasElement>()
        .map_err(|_| "Target element is not an HtmlCanvasElement".to_string())?;

    if canvas.width() != width {
        canvas.set_width(width);
    }
    if canvas.height() != height {
        canvas.set_height(height);
    }

    let ctx_obj = canvas
        .get_context("2d")
        .map_err(format_js_error)?
        .ok_or_else(|| "2D context unavailable".to_string())?;
    let ctx: CanvasRenderingContext2d = ctx_obj
        .dyn_into::<CanvasRenderingContext2d>()
        .map_err(|_| "Invalid 2D rendering context".to_string())?;

    let clamped = Uint8ClampedArray::new_with_byte_offset_and_length(
        &u8_array.buffer(),
        u8_array.byte_offset() + FRAME_HEADER_LEN as u32,
        expected_rgba_len as u32,
    );

    let image_data_ctor = Reflect::get(&window, &JsValue::from_str("ImageData"))
        .map_err(format_js_error)?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| "ImageData constructor unavailable".to_string())?;
    let ctor_args = Array::of3(
        &clamped.into(),
        &JsValue::from_f64(width as f64),
        &JsValue::from_f64(height as f64),
    );
    let image_data: ImageData = Reflect::construct(&image_data_ctor, &ctor_args)
        .map_err(format_js_error)?
        .unchecked_into::<ImageData>();

    ctx.put_image_data(&image_data, 0.0, 0.0)
        .map_err(format_js_error)?;

    Ok(true)
}

/// Fetches a raw binary frame buffer (`ArrayBuffer` / `Uint8Array`) for `peer_id` from Tauri IPC,
/// parses only the 24-byte header (`[u32 width, u32 height, [u8; 16] peer_uuid]`), constructs a
/// zero-copy `Uint8ClampedArray` view over the JS `ArrayBuffer`, and renders it directly onto
/// the target `<canvas>` 2D context without copying pixel bytes through WASM linear memory.
pub async fn fetch_and_render_peer_canvas(
    peer_id: PeerId,
    canvas_id: &str,
) -> Result<bool, String> {
    let js_args =
        serde_wasm_bindgen::to_value(&FetchFrameArgs { peer_id }).map_err(|e| e.to_string())?;
    let raw = tauri_invoke_raw("fetch_peer_frame", js_args).await?;
    render_binary_packet_to_canvas(raw, canvas_id)
}

/// Captures a live preview frame for the selected `CaptureTarget` in the picker modal and renders
/// it onto the preview `<canvas>`.
pub async fn fetch_and_render_target_preview(
    target: CaptureTarget,
    canvas_id: &str,
) -> Result<bool, String> {
    let js_args = serde_wasm_bindgen::to_value(&CapturePreviewArgs { target })
        .map_err(|e| e.to_string())?;
    let raw = tauri_invoke_raw("capture_target_preview", js_args).await?;
    render_binary_packet_to_canvas(raw, canvas_id)
}

/// Fetches the latest locally captured & scaled video frame while the local user is sharing their
/// screen and renders it onto the local preview `<canvas>`.
pub async fn fetch_and_render_local_preview_canvas(canvas_id: &str) -> Result<bool, String> {
    let empty = Object::new();
    let raw = tauri_invoke_raw("fetch_local_preview_frame", empty.into()).await?;
    render_binary_packet_to_canvas(raw, canvas_id)
}
