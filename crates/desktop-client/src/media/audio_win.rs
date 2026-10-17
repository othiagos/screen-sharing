#![cfg(target_os = "windows")]

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use shared_protocol::AudioCaptureMode;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use windows::{
    core::{implement, IUnknown, Interface, Result as WinResult, GUID, PCWSTR, PROPVARIANT},
    Win32::{
        Foundation::{CloseHandle, E_FAIL, HANDLE, S_OK, WAIT_OBJECT_0},
        Media::{
            Audio::{
                eConsole, eRender, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
                IActivateAudioInterfaceCompletionHandler,
                IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
                IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
                AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
                AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
                AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
                PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
            },
            Multimedia::WAVE_FORMAT_IEEE_FLOAT,
        },
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoUninitialize,
                CLSCTX_ALL, COINIT_MULTITHREADED,
            },
            Threading::{CreateEventW, WaitForSingleObject},
            Variant::VT_BLOB,
        },
    },
};

use crate::media::traits::{AudioCapturer, MediaError, RawAudioFrame};

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationCompletionHandler {
    tx: std_mpsc::SyncSender<WinResult<IAudioClient>>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationCompletionHandler_Impl {
    fn ActivateCompleted(
        &self,
        activate_operation: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> WinResult<()> {
        let result = (|| {
            let op = activate_operation.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            let mut activate_result = S_OK;
            let mut activated_interface: Option<IUnknown> = None;
            unsafe {
                op.GetActivateResult(&mut activate_result, &mut activated_interface)?;
            }
            activate_result.ok()?;
            let unknown =
                activated_interface.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            unknown.cast::<IAudioClient>()
        })();

        let _ = self.tx.send(result);
        Ok(())
    }
}

/// Windows 10 (Build 2004+) & Windows 11 WASAPI Process-Isolated and System Global audio capturer.
#[derive(Debug, Clone, Default)]
pub struct WindowsAudioCapturer;

impl WindowsAudioCapturer {
    pub fn new() -> Self {
        Self
    }
}

impl AudioCapturer for WindowsAudioCapturer {
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
            .name("wasapi-audio-capture".to_string())
            .spawn(move || {
                if let Err(err) = run_wasapi_capture_loop(mode, tx, cancel) {
                    warn!("WASAPI audio capture loop exited: {err}");
                }
            })
            .map_err(|e| {
                MediaError::AudioCapture(format!("Failed to spawn WASAPI capture thread: {e}"))
            })?;

        Ok(())
    }
}

fn explicit_48khz_stereo_float_format() -> WAVEFORMATEX {
    let channels = RawAudioFrame::CHANNELS;
    let sample_rate = RawAudioFrame::SAMPLE_RATE;
    let bits_per_sample = 32u16;
    let block_align = channels * (bits_per_sample / 8);
    let avg_bytes_per_sec = sample_rate * (block_align as u32);

    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
        nChannels: channels,
        nSamplesPerSec: sample_rate,
        nAvgBytesPerSec: avg_bytes_per_sec,
        nBlockAlign: block_align,
        wBitsPerSample: bits_per_sample,
        cbSize: 0,
    }
}

fn activate_process_loopback_client(pid: u32) -> Result<IAudioClient, MediaError> {
    let mut activation_params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: windows::Win32::Media::Audio::AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };

    let mut prop_variant = PROPVARIANT::default();
    unsafe {
        let raw_vt = (&mut prop_variant as *mut PROPVARIANT).cast::<u16>();
        *raw_vt = VT_BLOB.0;
        let blob_size_ptr = (raw_vt as *mut u8).add(8).cast::<u32>();
        *blob_size_ptr = size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32;
        let blob_data_ptr = (raw_vt as *mut u8).add(16).cast::<*mut c_void>();
        *blob_data_ptr = (&mut activation_params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast();
    }

    let (tx, rx) = std_mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler =
        ActivationCompletionHandler { tx }.into();

    let _operation = unsafe {
        ActivateAudioInterfaceAsync(
            PCWSTR(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK.as_ptr()),
            &IAudioClient::IID as *const GUID,
            Some(&prop_variant),
            &handler,
        )
    }
    .map_err(|e| MediaError::AudioCapture(format!("ActivateAudioInterfaceAsync failed: {e}")))?;

    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|e| MediaError::AudioCapture(format!("WASAPI activation timeout: {e}")))?
        .map_err(|e| MediaError::AudioCapture(format!("WASAPI activation error: {e}")))
}

fn activate_system_loopback_client() -> Result<IAudioClient, MediaError> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| MediaError::AudioCapture(format!("CoCreateInstance failed: {e}")))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| MediaError::AudioCapture(format!("GetDefaultAudioEndpoint failed: {e}")))?;
        device
            .Activate::<IAudioClient>(CLSCTX_ALL, None)
            .map_err(|e| MediaError::AudioCapture(format!("Device Activate failed: {e}")))
    }
}

fn run_wasapi_capture_loop(
    mode: AudioCaptureMode,
    tx: mpsc::Sender<RawAudioFrame>,
    cancel: CancellationToken,
) -> Result<(), MediaError> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let result = (|| -> Result<(), MediaError> {
        let audio_client = match mode {
            AudioCaptureMode::ApplicationOnly { pid } => {
                debug!(pid, "Activating Windows WASAPI Process Loopback");
                activate_process_loopback_client(pid)?
            }
            AudioCaptureMode::SystemGlobal => {
                debug!("Activating Windows WASAPI System Global Loopback");
                activate_system_loopback_client()?
            }
            AudioCaptureMode::None => return Ok(()),
        };

        // Note: VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK returns E_NOTIMPL on GetMixFormat().
        // Always pass an explicit 48kHz 2-channel float WAVEFORMATEX with AUTOCONVERTPCM.
        let wave_format = explicit_48khz_stereo_float_format();
        let stream_flags = AUDCLNT_STREAMFLAGS_LOOPBACK
            | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

        unsafe {
            audio_client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    stream_flags,
                    200_000, // 20ms in 100-ns units
                    0,
                    &wave_format,
                    None,
                )
                .map_err(|e| MediaError::AudioCapture(format!("IAudioClient::Initialize failed: {e}")))?;
        }

        let event_handle: HANDLE = unsafe {
            CreateEventW(None, false, false, None)
                .map_err(|e| MediaError::AudioCapture(format!("CreateEventW failed: {e}")))?
        };

        unsafe {
            audio_client
                .SetEventHandle(event_handle)
                .map_err(|e| MediaError::AudioCapture(format!("SetEventHandle failed: {e}")))?;
        }

        let capture_client: IAudioCaptureClient = unsafe {
            audio_client
                .GetService::<IAudioCaptureClient>()
                .map_err(|e| MediaError::AudioCapture(format!("GetService<IAudioCaptureClient> failed: {e}")))?
        };

        unsafe {
            audio_client
                .Start()
                .map_err(|e| MediaError::AudioCapture(format!("IAudioClient::Start failed: {e}")))?;
        }

        let mut sample_accum = Vec::with_capacity(RawAudioFrame::TOTAL_SAMPLES_20MS * 2);

        while !cancel.is_cancelled() {
            let wait_res = unsafe { WaitForSingleObject(event_handle, 50) };
            if wait_res != WAIT_OBJECT_0 {
                continue;
            }

            loop {
                let packet_frames = unsafe {
                    capture_client
                        .GetNextPacketSize()
                        .unwrap_or(0)
                };
                if packet_frames == 0 {
                    break;
                }

                let mut data_ptr: *mut u8 = null_mut();
                let mut frames_to_read = 0u32;
                let mut flags = 0u32;

                let get_buf_res = unsafe {
                    capture_client.GetBuffer(
                        &mut data_ptr,
                        &mut frames_to_read,
                        &mut flags,
                        None,
                        None,
                    )
                };
                if get_buf_res.is_err() {
                    break;
                }

                let total_samples = (frames_to_read as usize) * (RawAudioFrame::CHANNELS as usize);
                if (flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)) != 0 || data_ptr.is_null() {
                    sample_accum.extend(std::iter::repeat_n(0.0f32, total_samples));
                } else {
                    let float_slice = unsafe {
                        std::slice::from_raw_parts(data_ptr.cast::<f32>(), total_samples)
                    };
                    sample_accum.extend_from_slice(float_slice);
                }

                unsafe {
                    let _ = capture_client.ReleaseBuffer(frames_to_read);
                }

                while sample_accum.len() >= RawAudioFrame::TOTAL_SAMPLES_20MS {
                    let chunk: Vec<f32> = sample_accum
                        .drain(..RawAudioFrame::TOTAL_SAMPLES_20MS)
                        .collect();
                    let frame = RawAudioFrame {
                        sample_rate: RawAudioFrame::SAMPLE_RATE,
                        channels: RawAudioFrame::CHANNELS,
                        samples: chunk,
                    };
                    if tx.blocking_send(frame).is_err() {
                        break;
                    }
                }
            }
        }

        unsafe {
            let _ = audio_client.Stop();
            let _ = CloseHandle(event_handle);
        }
        Ok(())
    })();

    unsafe {
        CoUninitialize();
    }
    result
}
