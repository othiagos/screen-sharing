#![cfg(target_os = "linux")]

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions};
use ashpd::desktop::PersistMode;
use fast_image_resize::Resizer;
use pipewire as pw;
use pw::{properties::properties, spa};
use shared_protocol::TargetResolution;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::Pod;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::media::traits::{MediaError, RawVideoFrame};
use crate::media::video::{scale_bgra_frame_with_resizer, scale_rgba_frame_with_resizer};

/// Opens an XDG Desktop Portal ScreenCast session and streams PipeWire video frames into `tx`.
pub fn start_wayland_portal_capture(
    resolution: TargetResolution,
    fps: u8,
    tx: mpsc::Sender<RawVideoFrame>,
    cancel: CancellationToken,
) -> Result<(), MediaError> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| MediaError::VideoCapture("Tokio runtime unavailable for Wayland portal".to_string()))?;

    let (pw_fd, node_id) = handle.block_on(open_portal_screencast())?;

    std::thread::Builder::new()
        .name("pipewire-video-capture".to_string())
        .spawn(move || {
            if let Err(err) = run_pipewire_video_loop(pw_fd, node_id, resolution, fps, tx, cancel) {
                warn!("PipeWire video stream exited: {err}");
            }
        })
        .map_err(|e| MediaError::VideoCapture(format!("Failed spawning PipeWire video thread: {e}")))?;

    Ok(())
}

async fn open_portal_screencast() -> Result<(OwnedFd, u32), MediaError> {
    let proxy = Screencast::new()
        .await
        .map_err(|e| MediaError::VideoCapture(format!("XDG Screencast portal unavailable: {e}")))?;
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(|e| MediaError::VideoCapture(format!("Portal create_session failed: {e}")))?;

    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_sources(SourceType::MONITOR | SourceType::WINDOW)
                .set_multiple(false)
                .set_cursor_mode(CursorMode::EMBEDDED)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await
        .map_err(|e| MediaError::VideoCapture(format!("Portal select_sources failed: {e}")))?;

    let response = proxy
        .start(&session, None, StartCastOptions::default())
        .await
        .map_err(|e| MediaError::VideoCapture(format!("Portal start failed: {e}")))?
        .response()
        .map_err(|e| MediaError::VideoCapture(format!("Portal selection canceled or failed: {e}")))?;

    let stream = response
        .streams()
        .first()
        .ok_or_else(|| MediaError::VideoCapture("Portal returned no PipeWire streams".to_string()))?;
    let node_id = stream.pipe_wire_node_id();
    let pw_fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(|e| MediaError::VideoCapture(format!("Portal open_pipe_wire_remote failed: {e}")))?;

    info!(node_id, "Acquired Wayland/PipeWire screencast stream");
    Ok((pw_fd, node_id))
}

struct PwVideoState {
    format: Option<VideoInfoRaw>,
    resizer: Resizer,
    resolution: TargetResolution,
    min_interval: Duration,
    last_sent: Instant,
    start: Instant,
    tx: mpsc::Sender<RawVideoFrame>,
    cancel: CancellationToken,
    mainloop: pw::main_loop::MainLoopRc,
}

fn run_pipewire_video_loop(
    pw_fd: OwnedFd,
    node_id: u32,
    resolution: TargetResolution,
    fps: u8,
    tx: mpsc::Sender<RawVideoFrame>,
    cancel: CancellationToken,
) -> Result<(), MediaError> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| MediaError::VideoCapture(format!("PipeWire MainLoop failed: {e}")))?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None)
        .map_err(|e| MediaError::VideoCapture(format!("PipeWire Context failed: {e}")))?;
    let core = context
        .connect_fd(pw_fd, None)
        .map_err(|e| MediaError::VideoCapture(format!("PipeWire connect_fd failed: {e}")))?;

    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
    };

    let stream = pw::stream::StreamBox::new(&core, "screen-share-video", props)
        .map_err(|e| MediaError::VideoCapture(format!("PipeWire video Stream::new: {e}")))?;

    let state = Rc::new(RefCell::new(PwVideoState {
        format: None,
        resizer: Resizer::new(),
        resolution,
        min_interval: Duration::from_micros(1_000_000 / u64::from(fps.clamp(1, 60))),
        last_sent: Instant::now() - Duration::from_secs(1),
        start: Instant::now(),
        tx,
        cancel: cancel.clone(),
        mainloop: mainloop.clone(),
    }));

    let state_param = Rc::clone(&state);
    let state_proc = Rc::clone(&state);
    let _listener = stream
        .add_local_listener_with_user_data(())
        .param_changed(move |_, _, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = VideoInfoRaw::new();
            if info.parse(param).is_ok() {
                debug!(
                    fmt = ?info.format(),
                    w = info.size().width,
                    h = info.size().height,
                    "Negotiated PipeWire portal video format"
                );
                state_param.borrow_mut().format = Some(info);
            }
        })
        .process(move |stream, _| {
            let mut st = state_proc.borrow_mut();
            if st.cancel.is_cancelled() {
                st.mainloop.quit();
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(info) = st.format else { return };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let chunk = datas[0].chunk();
            let size = chunk.size() as usize;
            let stride = chunk.stride() as usize;
            let Some(slice) = datas[0].data() else {
                return;
            };
            if size == 0 || slice.len() < size {
                return;
            }

            let now = Instant::now();
            if now.duration_since(st.last_sent) < st.min_interval || st.tx.capacity() == 0 {
                return;
            }

            let w = info.size().width;
            let h = info.size().height;
            let row_bytes = (w as usize) * 4;
            if w < 2 || h < 2 || size < row_bytes * (h as usize) {
                return;
            }

            let mut packed = Vec::with_capacity(row_bytes * (h as usize));
            if stride == 0 || stride == row_bytes {
                packed.extend_from_slice(&slice[..row_bytes * (h as usize)]);
            } else {
                for row in 0..(h as usize) {
                    let off = row * stride;
                    if off + row_bytes <= slice.len() {
                        packed.extend_from_slice(&slice[off..off + row_bytes]);
                    }
                }
            }

            let res_choice = st.resolution;
            let is_bgra = matches!(info.format(), VideoFormat::BGRA | VideoFormat::BGRx);
            let scaled = if is_bgra {
                scale_bgra_frame_with_resizer(&mut st.resizer, &packed, w, h, res_choice, false)
                    .map(|(dw, dh, b)| (dw, dh, b, true))
            } else {
                scale_rgba_frame_with_resizer(&mut st.resizer, &packed, w, h, res_choice, false)
                    .map(|(dw, dh, r)| (dw, dh, r, false))
            };

            if let Ok((width, height, bytes, is_bgra)) = scaled {
                st.last_sent = now;
                let frame = RawVideoFrame {
                    width,
                    height,
                    rgba: bytes,
                    is_bgra,
                    timestamp_us: st.start.elapsed().as_micros() as u64,
                };
                if matches!(
                    st.tx.try_send(frame),
                    Err(mpsc::error::TrySendError::Closed(_))
                ) {
                    st.mainloop.quit();
                }
            }
        })
        .register()
        .map_err(|e| MediaError::VideoCapture(format!("PipeWire listener register failed: {e}")))?;

    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRA,
            VideoFormat::BGRA,
            VideoFormat::BGRx,
            VideoFormat::RGBA,
            VideoFormat::RGBx
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 64,
                height: 64
            },
            spa::utils::Rectangle {
                width: 7680,
                height: 4320
            }
        ),
    );
    let pod_bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| MediaError::VideoCapture(format!("PipeWire Pod serialize: {e:?}")))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&pod_bytes)
        .ok_or_else(|| MediaError::VideoCapture("Invalid PipeWire video format pod".to_string()))?];

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| MediaError::VideoCapture(format!("PipeWire video stream.connect: {e}")))?;

    let mainloop_timer = mainloop.clone();
    let cancel_timer = cancel.clone();
    let _timer = mainloop.loop_().add_timer(move |_| {
        if cancel_timer.is_cancelled() {
            mainloop_timer.quit();
        }
    });
    let _ = _timer.update_timer(
        Some(Duration::from_millis(50)),
        Some(Duration::from_millis(50)),
    );

    mainloop.run();
    Ok(())
}
