#![cfg(target_os = "linux")]

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::Cursor;
use std::rc::Rc;
use std::time::Duration;

use pipewire as pw;
use pw::{properties::properties, spa};
use shared_protocol::AudioCaptureMode;
use spa::pod::Pod;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::media::traits::{AudioCapturer, MediaError, RawAudioFrame};

/// Collects `root_pid` and all recursive child process IDs from `/proc`.
pub fn collect_process_tree_pids(root_pid: u32) -> HashSet<u32> {
    let mut pids = HashSet::new();
    if root_pid == 0 {
        return pids;
    }

    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        if !pids.insert(pid) {
            continue;
        }
        let task_dir = format!("/proc/{pid}/task");
        if let Ok(tasks) = std::fs::read_dir(task_dir) {
            for task in tasks.flatten() {
                let children_path = task.path().join("children");
                if let Ok(content) = std::fs::read_to_string(children_path) {
                    for token in content.split_whitespace() {
                        if let Ok(child_pid) = token.parse::<u32>() {
                            if !pids.contains(&child_pid) {
                                stack.push(child_pid);
                            }
                        }
                    }
                }
            }
        }
    }

    pids
}

struct CaptureCallbackState {
    sample_accum: Vec<f32>,
    tx: mpsc::Sender<RawAudioFrame>,
    channels: usize,
}

/// Linux PipeWire audio capturer supporting both Process-Isolated (`application.process.id`)
/// and System Global loopback capture.
#[derive(Debug, Clone, Default)]
pub struct PipeWireAudioCapturer;

impl PipeWireAudioCapturer {
    pub fn new() -> Self {
        Self
    }
}

impl AudioCapturer for PipeWireAudioCapturer {
    fn start_capture(
        &self,
        mode: AudioCaptureMode,
        tx: mpsc::Sender<RawAudioFrame>,
        cancel: CancellationToken,
    ) -> Result<(), MediaError> {
        if matches!(mode, AudioCaptureMode::None) {
            return Ok(());
        }

        std::thread::Builder::new()
            .name("pipewire-audio-capture".to_string())
            .spawn(move || {
                if let Err(err) = run_pipewire_capture_loop(mode, tx, cancel) {
                    warn!("PipeWire audio capture loop exited: {err}");
                }
            })
            .map_err(|e| {
                MediaError::AudioCapture(format!("Failed to spawn PipeWire thread: {e}"))
            })?;

        Ok(())
    }
}

fn run_pipewire_capture_loop(
    mode: AudioCaptureMode,
    tx: mpsc::Sender<RawAudioFrame>,
    cancel: CancellationToken,
) -> Result<(), MediaError> {
    pw::init();

    let mainloop = pw::main_loop::MainLoop::new(None)
        .map_err(|e| MediaError::AudioCapture(format!("PipeWire MainLoop init failed: {e}")))?;
    let context = pw::context::Context::new(&mainloop)
        .map_err(|e| MediaError::AudioCapture(format!("PipeWire Context init failed: {e}")))?;
    let core = context
        .connect(None)
        .map_err(|e| MediaError::AudioCapture(format!("PipeWire daemon connect failed: {e}")))?;
    let registry = core
        .get_registry()
        .map_err(|e| MediaError::AudioCapture(format!("PipeWire get_registry failed: {e}")))?;

    let target_pids: HashSet<u32> = match mode {
        AudioCaptureMode::ApplicationOnly { pid } => collect_process_tree_pids(pid),
        _ => HashSet::new(),
    };

    let matched_node_id: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));
    let matched_clone = Rc::clone(&matched_node_id);

    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != pw::types::ObjectType::Node {
                return;
            }
            if target_pids.is_empty() {
                return;
            }
            if let Some(props) = global.props {
                if let Some(pid_str) = props.get(*pw::keys::APP_PROCESS_ID) {
                    if let Ok(node_pid) = pid_str.trim().parse::<u32>() {
                        if target_pids.contains(&node_pid) {
                            info!(
                                node_id = global.id,
                                node_pid, "Matched PipeWire audio node for target process tree"
                            );
                            *matched_clone.borrow_mut() = Some(global.id);
                        }
                    }
                }
            }
        })
        .register();

    // Perform an initial roundtrip so the registry listener inspects all existing nodes first.
    let sync_seq = core
        .sync(0)
        .map_err(|e| MediaError::AudioCapture(format!("PipeWire core.sync failed: {e}")))?;
    let mainloop_sync = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == sync_seq {
                mainloop_sync.quit();
            }
        })
        .register();

    mainloop.run();

    if cancel.is_cancelled() {
        return Ok(());
    }

    let resolved_node_id = *matched_node_id.borrow();
    let props = match (mode, resolved_node_id) {
        (AudioCaptureMode::ApplicationOnly { pid }, Some(node_id)) => {
            debug!(pid, node_id, "Connecting PipeWire capture stream to application node");
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Communication",
            }
        }
        _ => {
            debug!("Connecting PipeWire capture stream to default sink monitor");
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Communication",
                *pw::keys::STREAM_CAPTURE_SINK => "true",
            }
        }
    };

    let stream = pw::stream::Stream::new(&core, "p2p-screen-share-audio", props)
        .map_err(|e| MediaError::AudioCapture(format!("PipeWire Stream::new failed: {e}")))?;

    let state = CaptureCallbackState {
        sample_accum: Vec::with_capacity(RawAudioFrame::TOTAL_SAMPLES_20MS * 2),
        tx,
        channels: RawAudioFrame::CHANNELS as usize,
    };

    let mainloop_proc = mainloop.clone();
    let cancel_proc = cancel.clone();

    let _stream_listener = stream
        .add_local_listener_with_user_data(state)
        .process(move |stream, user_data| {
            if cancel_proc.is_cancelled() {
                mainloop_proc.quit();
                return;
            }
            if let Some(mut buffer) = stream.dequeue_buffer() {
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let chunk_size = data.chunk().size() as usize;
                let chunk_offset = data.chunk().offset() as usize;

                if let Some(raw_bytes) = data.data() {
                    let end = (chunk_offset + chunk_size).min(raw_bytes.len());
                    if chunk_offset < end {
                        let valid_slice = &raw_bytes[chunk_offset..end];
                        for &sample_bytes in valid_slice.as_chunks::<4>().0 {
                            let sample = f32::from_le_bytes(sample_bytes);
                            user_data.sample_accum.push(sample);
                        }
                    }

                    while user_data.sample_accum.len() >= RawAudioFrame::TOTAL_SAMPLES_20MS {
                        let chunk: Vec<f32> = user_data
                            .sample_accum
                            .drain(..RawAudioFrame::TOTAL_SAMPLES_20MS)
                            .collect();
                        let frame = RawAudioFrame {
                            sample_rate: RawAudioFrame::SAMPLE_RATE,
                            channels: user_data.channels as u16,
                            samples: chunk,
                        };
                        if user_data.tx.try_send(frame).is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .register()
        .map_err(|e| MediaError::AudioCapture(format!("PipeWire stream listener failed: {e}")))?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(RawAudioFrame::SAMPLE_RATE);
    audio_info.set_channels(RawAudioFrame::CHANNELS as u32);

    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };

    let pod_bytes: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| MediaError::AudioCapture(format!("SPA Pod serialize failed: {e:?}")))?
    .0
    .into_inner();

    let pod = Pod::from_bytes(&pod_bytes)
        .ok_or_else(|| MediaError::AudioCapture("Invalid SPA Pod bytes".to_string()))?;
    let mut params = [pod];

    stream
        .connect(
            spa::utils::Direction::Input,
            resolved_node_id,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| MediaError::AudioCapture(format!("PipeWire stream.connect failed: {e}")))?;

    let mainloop_timer = mainloop.clone();
    let cancel_timer = cancel.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if cancel_timer.is_cancelled() {
            mainloop_timer.quit();
        }
    });
    let _ = timer
        .update_timer(
            Some(Duration::from_millis(50)),
            Some(Duration::from_millis(50)),
        )
        .into_result();

    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_process_tree_pids_current_process() {
        let current_pid = std::process::id();
        let pids = collect_process_tree_pids(current_pid);
        assert!(pids.contains(&current_pid));
    }
}
