use js_sys::{ArrayBuffer, Object, Reflect, Uint8Array};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use shared_protocol::{
    decode_frame_packet, CaptureTarget, IceServerConfig, PeerId, RoomId, StreamConfig,
};
use wasm_bindgen::{prelude::*, Clamped, JsCast};
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

pub async fn invoke_create_room(
    signaling_url: String,
    custom_ice_servers: Vec<IceServerConfig>,
) -> Result<RoomStatusSnapshot, String> {
    tauri_invoke(
        "create_room_session",
        &CreateRoomArgs {
            signaling_url,
            custom_ice_servers,
        },
    )
    .await
}

pub async fn invoke_join_room(
    signaling_url: String,
    room_id: RoomId,
    custom_ice_servers: Vec<IceServerConfig>,
) -> Result<RoomStatusSnapshot, String> {
    tauri_invoke(
        "join_room_session",
        &JoinRoomArgs {
            signaling_url,
            room_id,
            custom_ice_servers,
        },
    )
    .await
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

/// Fetches a raw binary frame buffer (`ArrayBuffer` / `Uint8Array`) for `peer_id` from Tauri IPC,
/// parses the 24-byte header (`[u32 width, u32 height, [u8; 16] peer_uuid]`), constructs a
/// `web_sys::ImageData`, and renders it directly onto the target `<canvas>` 2D context.
pub async fn fetch_and_render_peer_canvas(
    peer_id: PeerId,
    canvas_id: &str,
) -> Result<bool, String> {
    let js_args =
        serde_wasm_bindgen::to_value(&FetchFrameArgs { peer_id }).map_err(|e| e.to_string())?;
    let raw = tauri_invoke_raw("fetch_peer_frame", js_args).await?;

    let u8_array = if raw.is_instance_of::<ArrayBuffer>() {
        Uint8Array::new(&raw)
    } else if raw.is_instance_of::<Uint8Array>() {
        raw.unchecked_into::<Uint8Array>()
    } else {
        return Ok(false);
    };

    let packet_len = u8_array.length() as usize;
    if packet_len == 0 {
        return Ok(false);
    }

    let mut packet = vec![0u8; packet_len];
    u8_array.copy_to(&mut packet);

    let (_decoded_peer, width, height, rgba) =
        decode_frame_packet(&packet).map_err(|e| e.to_string())?;

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

    let image_data =
        ImageData::new_with_u8_clamped_array_and_sh(Clamped(rgba), width, height)
            .map_err(format_js_error)?;

    ctx.put_image_data(&image_data, 0.0, 0.0)
        .map_err(format_js_error)?;

    Ok(true)
}
