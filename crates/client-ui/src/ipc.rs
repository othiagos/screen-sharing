use std::cell::RefCell;
use std::collections::HashMap;

use js_sys::{Array, ArrayBuffer, Float32Array, Object, Reflect, Uint8Array, Uint8ClampedArray};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use shared_protocol::{
    CaptureTarget, IceServerConfig, PeerId, RoomId, StreamConfig, FRAME_HEADER_LEN,
};
use wasm_bindgen::{prelude::*, JsCast};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    CanvasRenderingContext2d, HtmlCanvasElement, ImageData, WebGlBuffer, WebGlProgram,
    WebGlRenderingContext as GL, WebGlShader, WebGlTexture, WebGlUniformLocation,
};

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

/// Hardware-accelerated WebGL video renderer that uploads planar I420 (`Y`, `U`, `V` luminance
/// textures) or `RGBA` textures directly from JS `Uint8Array` views and converts BT.601 YUV420p
/// to RGB in a GPU fragment shader.
struct WebGlVideoRenderer {
    canvas: HtmlCanvasElement,
    gl: GL,
    program: WebGlProgram,
    vbo: WebGlBuffer,
    tex_y: WebGlTexture,
    tex_u: WebGlTexture,
    tex_v: WebGlTexture,
    u_mode: WebGlUniformLocation,
    a_pos: u32,
    a_tex: u32,
}

thread_local! {
    static WEBGL_RENDERERS: RefCell<HashMap<String, WebGlVideoRenderer>> = RefCell::new(HashMap::new());
}

fn compile_shader(gl: &GL, shader_type: u32, source: &str) -> Result<WebGlShader, String> {
    let shader = gl
        .create_shader(shader_type)
        .ok_or_else(|| "Failed to create WebGL shader".to_string())?;
    gl.shader_source(&shader, source);
    gl.compile_shader(&shader);
    if gl
        .get_shader_parameter(&shader, GL::COMPILE_STATUS)
        .as_bool()
        .unwrap_or(false)
    {
        Ok(shader)
    } else {
        let log = gl.get_shader_info_log(&shader).unwrap_or_default();
        gl.delete_shader(Some(&shader));
        Err(format!("WebGL shader compile error: {log}"))
    }
}

fn create_planar_texture(gl: &GL) -> Result<WebGlTexture, String> {
    let tex = gl
        .create_texture()
        .ok_or_else(|| "Failed to create WebGL texture".to_string())?;
    gl.bind_texture(GL::TEXTURE_2D, Some(&tex));
    gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_WRAP_S, GL::CLAMP_TO_EDGE as i32);
    gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_WRAP_T, GL::CLAMP_TO_EDGE as i32);
    gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_MIN_FILTER, GL::LINEAR as i32);
    gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_MAG_FILTER, GL::LINEAR as i32);
    Ok(tex)
}

impl WebGlVideoRenderer {
    fn new(canvas: &HtmlCanvasElement) -> Result<Self, String> {
        let ctx_obj = canvas
            .get_context("webgl")
            .or_else(|_| canvas.get_context("experimental-webgl"))
            .map_err(format_js_error)?
            .ok_or_else(|| "WebGL context unavailable".to_string())?;
        let gl: GL = ctx_obj
            .dyn_into::<GL>()
            .map_err(|_| "Invalid WebGL rendering context".to_string())?;

        let vs_src = r#"
            attribute vec2 a_pos;
            attribute vec2 a_tex;
            varying vec2 v_tex;
            void main() {
                gl_Position = vec4(a_pos, 0.0, 1.0);
                v_tex = a_tex;
            }
        "#;
        let fs_src = r#"
            precision mediump float;
            varying vec2 v_tex;
            uniform sampler2D u_tex_y;
            uniform sampler2D u_tex_u;
            uniform sampler2D u_tex_v;
            uniform int u_mode;
            void main() {
                if (u_mode == 1) {
                    gl_FragColor = texture2D(u_tex_y, v_tex);
                } else {
                    float y = texture2D(u_tex_y, v_tex).r;
                    float u = texture2D(u_tex_u, v_tex).r - 0.5;
                    float v = texture2D(u_tex_v, v_tex).r - 0.5;
                    float y_adj = 1.16438356 * (y - 0.0625);
                    float r = y_adj + 1.59602678 * v;
                    float g = y_adj - 0.39176229 * u - 0.81296764 * v;
                    float b = y_adj + 2.01723214 * u;
                    gl_FragColor = vec4(clamp(vec3(r, g, b), 0.0, 1.0), 1.0);
                }
            }
        "#;

        let vs = compile_shader(&gl, GL::VERTEX_SHADER, vs_src)?;
        let fs = compile_shader(&gl, GL::FRAGMENT_SHADER, fs_src)?;
        let program = gl
            .create_program()
            .ok_or_else(|| "Failed to create WebGL program".to_string())?;
        gl.attach_shader(&program, &vs);
        gl.attach_shader(&program, &fs);
        gl.link_program(&program);
        gl.delete_shader(Some(&vs));
        gl.delete_shader(Some(&fs));

        if !gl
            .get_program_parameter(&program, GL::LINK_STATUS)
            .as_bool()
            .unwrap_or(false)
        {
            let log = gl.get_program_info_log(&program).unwrap_or_default();
            return Err(format!("WebGL program link error: {log}"));
        }

        gl.use_program(Some(&program));

        // Interleaved [x, y, u, v] full-screen triangle strip with top-left UV origin
        let vertices: [f32; 16] = [
            -1.0, -1.0, 0.0, 1.0, // bottom-left
            1.0, -1.0, 1.0, 1.0, // bottom-right
            -1.0, 1.0, 0.0, 0.0, // top-left
            1.0, 1.0, 1.0, 0.0, // top-right
        ];
        let vbo = gl
            .create_buffer()
            .ok_or_else(|| "Failed to create WebGL VBO".to_string())?;
        gl.bind_buffer(GL::ARRAY_BUFFER, Some(&vbo));
        let vert_array = Float32Array::from(&vertices[..]);
        gl.buffer_data_with_array_buffer_view(GL::ARRAY_BUFFER, vert_array.as_ref(), GL::STATIC_DRAW);

        let a_pos = gl.get_attrib_location(&program, "a_pos") as u32;
        let a_tex = gl.get_attrib_location(&program, "a_tex") as u32;
        gl.enable_vertex_attrib_array(a_pos);
        gl.enable_vertex_attrib_array(a_tex);
        gl.vertex_attrib_pointer_with_i32(a_pos, 2, GL::FLOAT, false, 16, 0);
        gl.vertex_attrib_pointer_with_i32(a_tex, 2, GL::FLOAT, false, 16, 8);

        let tex_y = create_planar_texture(&gl)?;
        let tex_u = create_planar_texture(&gl)?;
        let tex_v = create_planar_texture(&gl)?;

        if let Some(loc_y) = gl.get_uniform_location(&program, "u_tex_y") {
            gl.uniform1i(Some(&loc_y), 0);
        }
        if let Some(loc_u) = gl.get_uniform_location(&program, "u_tex_u") {
            gl.uniform1i(Some(&loc_u), 1);
        }
        if let Some(loc_v) = gl.get_uniform_location(&program, "u_tex_v") {
            gl.uniform1i(Some(&loc_v), 2);
        }
        let u_mode = gl
            .get_uniform_location(&program, "u_mode")
            .ok_or_else(|| "Missing u_mode uniform".to_string())?;

        gl.pixel_storei(GL::UNPACK_ALIGNMENT, 1);

        Ok(Self {
            canvas: canvas.clone(),
            gl,
            program,
            vbo,
            tex_y,
            tex_u,
            tex_v,
            u_mode,
            a_pos,
            a_tex,
        })
    }

    fn draw_frame(
        &self,
        u8_array: &Uint8Array,
        width: u32,
        height: u32,
        is_i420: bool,
    ) -> Result<(), String> {
        let gl = &self.gl;
        gl.viewport(0, 0, width as i32, height as i32);
        gl.use_program(Some(&self.program));
        gl.bind_buffer(GL::ARRAY_BUFFER, Some(&self.vbo));
        gl.vertex_attrib_pointer_with_i32(self.a_pos, 2, GL::FLOAT, false, 16, 0);
        gl.vertex_attrib_pointer_with_i32(self.a_tex, 2, GL::FLOAT, false, 16, 8);
        gl.pixel_storei(GL::UNPACK_ALIGNMENT, 1);

        if is_i420 {
            let y_len = width * height;
            let uv_len = y_len / 4;
            let y_start = FRAME_HEADER_LEN as u32;
            let u_start = y_start + y_len;
            let v_start = u_start + uv_len;

            let y_view = u8_array.subarray(y_start, u_start);
            let u_view = u8_array.subarray(u_start, v_start);
            let v_view = u8_array.subarray(v_start, v_start + uv_len);

            gl.uniform1i(Some(&self.u_mode), 0);

            gl.active_texture(GL::TEXTURE0);
            gl.bind_texture(GL::TEXTURE_2D, Some(&self.tex_y));
            gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_array_buffer_view(
                GL::TEXTURE_2D,
                0,
                GL::LUMINANCE as i32,
                width as i32,
                height as i32,
                0,
                GL::LUMINANCE,
                GL::UNSIGNED_BYTE,
                Some(y_view.as_ref()),
            )
            .map_err(format_js_error)?;

            let half_w = (width / 2) as i32;
            let half_h = (height / 2) as i32;

            gl.active_texture(GL::TEXTURE1);
            gl.bind_texture(GL::TEXTURE_2D, Some(&self.tex_u));
            gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_array_buffer_view(
                GL::TEXTURE_2D,
                0,
                GL::LUMINANCE as i32,
                half_w,
                half_h,
                0,
                GL::LUMINANCE,
                GL::UNSIGNED_BYTE,
                Some(u_view.as_ref()),
            )
            .map_err(format_js_error)?;

            gl.active_texture(GL::TEXTURE2);
            gl.bind_texture(GL::TEXTURE_2D, Some(&self.tex_v));
            gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_array_buffer_view(
                GL::TEXTURE_2D,
                0,
                GL::LUMINANCE as i32,
                half_w,
                half_h,
                0,
                GL::LUMINANCE,
                GL::UNSIGNED_BYTE,
                Some(v_view.as_ref()),
            )
            .map_err(format_js_error)?;
        } else {
            let rgba_len = width * height * 4;
            let start = FRAME_HEADER_LEN as u32;
            let rgba_view = u8_array.subarray(start, start + rgba_len);

            gl.uniform1i(Some(&self.u_mode), 1);
            gl.active_texture(GL::TEXTURE0);
            gl.bind_texture(GL::TEXTURE_2D, Some(&self.tex_y));
            gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_array_buffer_view(
                GL::TEXTURE_2D,
                0,
                GL::RGBA as i32,
                width as i32,
                height as i32,
                0,
                GL::RGBA,
                GL::UNSIGNED_BYTE,
                Some(rgba_view.as_ref()),
            )
            .map_err(format_js_error)?;
        }

        gl.draw_arrays(GL::TRIANGLE_STRIP, 0, 4);
        Ok(())
    }
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
    let px = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| "Frame dimensions overflow".to_string())?;
    let expected_i420_len = px + (px / 2);
    let expected_rgba_len = px.saturating_mul(4);

    let is_i420 = packet_len == FRAME_HEADER_LEN + expected_i420_len && expected_i420_len > 0;
    let is_rgba = packet_len == FRAME_HEADER_LEN + expected_rgba_len && expected_rgba_len > 0;

    if width == 0 || height == 0 || (!is_i420 && !is_rgba) {
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

    // 1. Hardware-Accelerated GPU WebGL Renderer (supports both planar I420 and RGBA)
    let webgl_res = WEBGL_RENDERERS.with(|map_cell| -> Result<(), String> {
        let mut map = map_cell.borrow_mut();
        let needs_reinit = match map.get(canvas_id) {
            Some(existing) => existing.canvas != canvas,
            None => true,
        };
        if needs_reinit {
            let renderer = WebGlVideoRenderer::new(&canvas)?;
            map.insert(canvas_id.to_string(), renderer);
        }
        map.get(canvas_id)
            .unwrap()
            .draw_frame(&u8_array, width, height, is_i420)
    });

    if webgl_res.is_ok() {
        return Ok(true);
    }

    // 2. Fallback to 2D Canvas if WebGL context creation is unavailable
    let ctx_obj = canvas
        .get_context("2d")
        .map_err(format_js_error)?
        .ok_or_else(|| "2D context unavailable".to_string())?;
    let ctx: CanvasRenderingContext2d = ctx_obj
        .dyn_into::<CanvasRenderingContext2d>()
        .map_err(|_| "Invalid 2D rendering context".to_string())?;

    let clamped = if is_rgba {
        Uint8ClampedArray::new_with_byte_offset_and_length(
            &u8_array.buffer(),
            u8_array.byte_offset() + FRAME_HEADER_LEN as u32,
            expected_rgba_len as u32,
        )
    } else {
        let mut i420 = vec![0u8; expected_i420_len];
        u8_array
            .subarray(
                FRAME_HEADER_LEN as u32,
                (FRAME_HEADER_LEN + expected_i420_len) as u32,
            )
            .copy_to(&mut i420);
        let w = width as usize;
        let h = height as usize;
        let y_plane = &i420[..px];
        let u_plane = &i420[px..px + px / 4];
        let v_plane = &i420[px + px / 4..];
        let mut rgba = vec![255u8; expected_rgba_len];
        let half_w = w / 2;
        for y in 0..h {
            for x in 0..w {
                let y_val = i32::from(y_plane[y * w + x]);
                let uv_idx = (y / 2) * half_w + (x / 2);
                let u_val = i32::from(u_plane[uv_idx]) - 128;
                let v_val = i32::from(v_plane[uv_idx]) - 128;
                let dst = (y * w + x) * 4;
                rgba[dst] = (y_val + ((359 * v_val + 128) >> 8)).clamp(0, 255) as u8;
                rgba[dst + 1] =
                    (y_val - ((88 * u_val + 183 * v_val + 128) >> 8)).clamp(0, 255) as u8;
                rgba[dst + 2] = (y_val + ((454 * u_val + 128) >> 8)).clamp(0, 255) as u8;
            }
        }
        let arr = Uint8ClampedArray::new_with_length(expected_rgba_len as u32);
        arr.copy_from(&rgba);
        arr
    };

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

/// Fetches a raw binary frame buffer (`ArrayBuffer` / `Uint8Array`) for `peer_id` from Tauri IPC
/// and renders it onto the target `<canvas>` using GPU-accelerated WebGL shaders (with 2D fallback).
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
