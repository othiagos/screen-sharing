#![cfg(target_os = "windows")]

use std::ptr::null_mut;
use std::sync::mpsc as std_mpsc;

use tracing::{debug, info};
use windows::core::{Interface, VARIANT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base, ICodecAPI, IMFActivate,
    IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform,
    MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFShutdown,
    MFStartup, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12, CODECAPI_AVEncCommonLowLatency,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncCommonRealTime, CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, METransformHaveOutput, METransformNeedInput,
    MFSampleExtension_CleanPoint, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ASYNCMFT,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_OUTPUT_DATA_BUFFER, MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NONE, MF_EVENT_TYPE,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFMediaType_Video, MFSTARTUP_NOSOCKET,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

use crate::media::traits::{MediaError, RawVideoFrame, VideoEncoder};
use crate::media::video::{bgra_to_nv12_into, rgba_to_nv12_into};

/// Windows Media Foundation Hardware H.264 encoder (NVENC / AMD AMF / Intel QSV).
///
/// Wraps all COM / D3D11 / Media Foundation interfaces inside a dedicated COM MTA
/// worker thread so `MfH264Encoder` is strictly `Send + 'static` and `CoInitializeEx` /
/// `MFStartup` always pair on the same thread as `MFShutdown` / `CoUninitialize`.
pub struct MfH264Encoder {
    tx: Option<std_mpsc::SyncSender<MfCommand>>,
    worker: Option<std::thread::JoinHandle<()>>,
    nv12_buf: Vec<u8>,
    pending_keyframe: bool,
}

type EncodeReply = (Result<Vec<u8>, MediaError>, Vec<u8>);

enum MfCommand {
    Encode {
        width: u32,
        height: u32,
        nv12_buf: Vec<u8>,
        force_keyframe: bool,
        reply: std_mpsc::SyncSender<EncodeReply>,
    },
    SetBitrate {
        bitrate_bps: u32,
        reply: std_mpsc::SyncSender<Result<(), MediaError>>,
    },
}

struct ComMfGuard {
    com_initialized: bool,
    mf_started: bool,
}

impl ComMfGuard {
    unsafe fn enter() -> Result<Self, MediaError> {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut guard = Self {
            com_initialized: hr.is_ok(),
            mf_started: false,
        };
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
            .map_err(|e| MediaError::Codec(format!("MFStartup failed: {e}")))?;
        guard.mf_started = true;
        Ok(guard)
    }
}

impl Drop for ComMfGuard {
    fn drop(&mut self) {
        unsafe {
            if self.mf_started {
                let _ = MFShutdown();
            }
            if self.com_initialized {
                CoUninitialize();
            }
        }
    }
}

impl MfH264Encoder {
    pub fn new(fps: u8, bitrate_bps: u32) -> Result<Self, MediaError> {
        let (init_tx, init_rx) = std_mpsc::sync_channel::<Result<(), MediaError>>(1);
        let (cmd_tx, cmd_rx) = std_mpsc::sync_channel::<MfCommand>(1);

        let worker = std::thread::Builder::new()
            .name("mf-h264-worker".to_string())
            .spawn(move || run_mf_worker(fps, bitrate_bps, init_tx, cmd_rx))
            .map_err(|e| MediaError::Codec(format!("Failed to spawn MF H.264 worker thread: {e}")))?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx: Some(cmd_tx),
                worker: Some(worker),
                nv12_buf: Vec::new(),
                pending_keyframe: false,
            }),
            Ok(Err(err)) => {
                drop(cmd_tx);
                let _ = worker.join();
                Err(err)
            }
            Err(_) => {
                drop(cmd_tx);
                let _ = worker.join();
                Err(MediaError::Codec(
                    "MF H.264 worker thread terminated during initialization".to_string(),
                ))
            }
        }
    }

    fn send_command<R>(
        &self,
        build: impl FnOnce(std_mpsc::SyncSender<R>) -> MfCommand,
    ) -> Result<R, MediaError> {
        let tx = self.tx.as_ref().ok_or_else(|| {
            MediaError::Codec("Media Foundation encoder worker is stopped".to_string())
        })?;
        let (reply_tx, reply_rx) = std_mpsc::sync_channel(1);
        tx.send(build(reply_tx))
            .map_err(|_| MediaError::Codec("Media Foundation encoder worker exited".to_string()))?;
        reply_rx.recv().map_err(|_| {
            MediaError::Codec("Media Foundation encoder worker dropped reply".to_string())
        })
    }
}

fn run_mf_worker(
    fps: u8,
    bitrate_bps: u32,
    init_tx: std_mpsc::SyncSender<Result<(), MediaError>>,
    cmd_rx: std_mpsc::Receiver<MfCommand>,
) {
    let _guard = match unsafe { ComMfGuard::enter() } {
        Ok(guard) => guard,
        Err(err) => {
            let _ = init_tx.send(Err(err));
            return;
        }
    };

    let mut inner = match MfWorkerInner::new(fps, bitrate_bps) {
        Ok(inner) => inner,
        Err(err) => {
            let _ = init_tx.send(Err(err));
            return;
        }
    };

    let _ = init_tx.send(Ok(()));

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            MfCommand::Encode {
                width,
                height,
                nv12_buf,
                force_keyframe,
                reply,
            } => {
                if force_keyframe {
                    inner.force_next_idr = true;
                }
                let res = inner.encode_nv12(width, height, &nv12_buf);
                let _ = reply.send((res, nv12_buf));
            }
            MfCommand::SetBitrate { bitrate_bps, reply } => {
                let res = inner.set_bitrate(bitrate_bps);
                let _ = reply.send(res);
            }
        }
    }
    // `inner` drops before `_guard`, releasing all COM objects prior to MFShutdown/CoUninitialize.
}

struct MfWorkerInner {
    transform: IMFTransform,
    event_gen: Option<IMFMediaEventGenerator>,
    codec_api: Option<ICodecAPI>,
    _dxgi_mgr: IMFDXGIDeviceManager,
    _d3d_device: ID3D11Device,
    fps: u8,
    bitrate_bps: u32,
    configured_dims: Option<(u32, u32)>,
    force_next_idr: bool,
    frame_index: u64,
}

impl MfWorkerInner {
    fn new(fps: u8, bitrate_bps: u32) -> Result<Self, MediaError> {
        unsafe {
            let mut d3d_device: Option<ID3D11Device> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d_device),
                None,
                None,
            )
            .map_err(|e| MediaError::Codec(format!("MF D3D11CreateDevice failed: {e}")))?;

            let d3d_device = d3d_device.ok_or_else(|| {
                MediaError::Codec("MF D3D11CreateDevice returned null device".to_string())
            })?;

            let mut reset_token = 0u32;
            let mut dxgi_mgr: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut reset_token, &mut dxgi_mgr).map_err(|e| {
                MediaError::Codec(format!("MFCreateDXGIDeviceManager failed: {e}"))
            })?;
            let dxgi_mgr = dxgi_mgr.ok_or_else(|| {
                MediaError::Codec("Null IMFDXGIDeviceManager".to_string())
            })?;
            dxgi_mgr
                .ResetDevice(&d3d_device, reset_token)
                .map_err(|e| MediaError::Codec(format!("IMFDXGIDeviceManager::ResetDevice: {e}")))?;

            let out_type = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;
            let mut activates_ptr: *mut Option<IMFActivate> = null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                None,
                Some(&out_type),
                &mut activates_ptr,
                &mut count,
            )
            .map_err(|e| MediaError::Codec(format!("MFTEnumEx H.264 HW failed: {e}")))?;

            if count == 0 || activates_ptr.is_null() {
                if !activates_ptr.is_null() {
                    CoTaskMemFree(Some(activates_ptr.cast()));
                }
                return Err(MediaError::Codec(
                    "No Windows Hardware H.264 MFT encoder available".to_string(),
                ));
            }

            let activates = std::slice::from_raw_parts_mut(activates_ptr, count as usize);
            let mut activated: Option<IMFTransform> = None;
            for act in activates.iter().flatten() {
                if let Ok(mft) = act.ActivateObject::<IMFTransform>() {
                    activated = Some(mft);
                    break;
                }
            }
            for act_slot in activates.iter_mut() {
                drop(act_slot.take());
            }
            CoTaskMemFree(Some(activates_ptr.cast()));

            let transform = activated.ok_or_else(|| {
                MediaError::Codec("Failed activating any Hardware H.264 MFT".to_string())
            })?;

            // Unlock Async MFT if required (NVENC / AMF / QSV drivers are commonly async MFTs).
            let mut is_async = false;
            if let Ok(attrs) = transform.GetAttributes() {
                if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
                    attrs
                        .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                        .map_err(|e| {
                            MediaError::Codec(format!("MF_TRANSFORM_ASYNC_UNLOCK failed: {e}"))
                        })?;
                    is_async = true;
                }
            }

            // Set DirectX device manager for hardware-accelerated MFT
            let _ = transform.ProcessMessage(
                windows::Win32::Media::MediaFoundation::MFT_MESSAGE_SET_D3D_MANAGER,
                windows::core::Interface::as_raw(&dxgi_mgr) as usize,
            );

            let event_gen = if is_async {
                transform.cast::<IMFMediaEventGenerator>().ok()
            } else {
                None
            };
            let codec_api = transform.cast::<ICodecAPI>().ok();

            info!(is_async, "Initialized Windows Media Foundation Hardware H.264 MFT");

            Ok(Self {
                transform,
                event_gen,
                codec_api,
                _dxgi_mgr: dxgi_mgr,
                _d3d_device: d3d_device,
                fps: fps.clamp(1, 60),
                bitrate_bps: bitrate_bps.clamp(500_000, 35_000_000),
                configured_dims: None,
                force_next_idr: true,
                frame_index: 0,
            })
        }
    }

    unsafe fn configure_streams(&mut self, width: u32, height: u32) -> Result<(), MediaError> {
        let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);

        if let Some(api) = &self.codec_api {
            let _ = api.SetValue(
                &CODECAPI_AVEncCommonRateControlMode,
                &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
            );
            let _ = api.SetValue(
                &CODECAPI_AVEncCommonMeanBitRate,
                &VARIANT::from(self.bitrate_bps),
            );
            let _ = api.SetValue(&CODECAPI_AVEncCommonLowLatency, &VARIANT::from(true));
            let _ = api.SetValue(&CODECAPI_AVEncCommonRealTime, &VARIANT::from(true));
            let _ = api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
            let gop = u32::from(self.fps) * 3;
            let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(gop));
        }

        let packed_size = (u64::from(width) << 32) | u64::from(height);
        let packed_fps = (u64::from(self.fps) << 32) | 1u64;
        let packed_par = (1u64 << 32) | 1u64;

        let out_type: IMFMediaType = MFCreateMediaType()
            .map_err(|e| MediaError::Codec(format!("MFCreateMediaType output: {e}")))?;
        out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok();
        out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).ok();
        out_type.SetUINT32(&MF_MT_AVG_BITRATE, self.bitrate_bps).ok();
        out_type.SetUINT64(&MF_MT_FRAME_SIZE, packed_size).ok();
        out_type.SetUINT64(&MF_MT_FRAME_RATE, packed_fps).ok();
        out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, packed_par).ok();
        out_type.SetUINT32(&MF_MT_INTERLACE_MODE, 2).ok(); // Progressive
        out_type
            .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)
            .ok();

        self.transform
            .SetOutputType(0, &out_type, 0)
            .map_err(|e| MediaError::Codec(format!("MFT SetOutputType(H264) failed: {e}")))?;

        let in_type: IMFMediaType = MFCreateMediaType()
            .map_err(|e| MediaError::Codec(format!("MFCreateMediaType input: {e}")))?;
        in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok();
        in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).ok();
        in_type.SetUINT64(&MF_MT_FRAME_SIZE, packed_size).ok();
        in_type.SetUINT64(&MF_MT_FRAME_RATE, packed_fps).ok();
        in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, packed_par).ok();
        in_type.SetUINT32(&MF_MT_INTERLACE_MODE, 2).ok();

        self.transform
            .SetInputType(0, &in_type, 0)
            .map_err(|e| MediaError::Codec(format!("MFT SetInputType(NV12) failed: {e}")))?;

        let _ = self
            .transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
        let _ = self
            .transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);

        self.configured_dims = Some((width, height));
        self.force_next_idr = true;
        debug!(width, height, fps = self.fps, bitrate = self.bitrate_bps, "Configured MF H.264 streams");
        Ok(())
    }

    fn encode_nv12(&mut self, width: u32, height: u32, nv12_buf: &[u8]) -> Result<Vec<u8>, MediaError> {
        let nv12_len = u32::try_from(nv12_buf.len())
            .map_err(|_| MediaError::Codec("NV12 frame buffer exceeds 4 GiB".to_string()))?;

        unsafe {
            if self.configured_dims != Some((width, height)) {
                self.configure_streams(width, height)?;
            }

            if self.force_next_idr {
                self.force_next_idr = false;
                if let Some(api) = &self.codec_api {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                }
            }

            let buffer = MFCreateMemoryBuffer(nv12_len)
                .map_err(|e| MediaError::Codec(format!("MFCreateMemoryBuffer: {e}")))?;
            let mut ptr: *mut u8 = null_mut();
            buffer
                .Lock(&mut ptr, None, None)
                .map_err(|e| MediaError::Codec(format!("IMFMediaBuffer::Lock: {e}")))?;
            std::ptr::copy_nonoverlapping(nv12_buf.as_ptr(), ptr, nv12_buf.len());
            let _ = buffer.Unlock();
            let _ = buffer.SetCurrentLength(nv12_len);

            let sample: IMFSample = MFCreateSample()
                .map_err(|e| MediaError::Codec(format!("MFCreateSample: {e}")))?;
            let _ = sample.AddBuffer(&buffer);
            let dur_100ns = 10_000_000i64 / i64::from(self.fps.max(1));
            let pts_100ns = (self.frame_index as i64) * dur_100ns;
            let _ = sample.SetSampleTime(pts_100ns);
            let _ = sample.SetSampleDuration(dur_100ns);
            self.frame_index = self.frame_index.wrapping_add(1);

            if let Some(gen) = &self.event_gen {
                // Async MFT loop: deliver input once METransformNeedInput arrives, then read output.
                let mut sent_input = false;
                let mut annex_b = Vec::new();
                for _ in 0..16 {
                    let ev = gen.GetEvent(MF_EVENT_FLAG_NONE).map_err(|e| {
                        MediaError::Codec(format!("MFT GetEvent failed: {e}"))
                    })?;
                    let ev_type = MF_EVENT_TYPE(ev.GetType().unwrap_or(0) as i32);
                    if ev_type == METransformNeedInput && !sent_input {
                        self.transform.ProcessInput(0, &sample, 0).map_err(|e| {
                            MediaError::Codec(format!("Async MFT ProcessInput: {e}"))
                        })?;
                        sent_input = true;
                    } else if ev_type == METransformHaveOutput {
                        extract_mft_output(&self.transform, &mut annex_b)?;
                        if sent_input && !annex_b.is_empty() {
                            return Ok(annex_b);
                        }
                    }
                }
                Ok(annex_b)
            } else {
                self.transform
                    .ProcessInput(0, &sample, 0)
                    .map_err(|e| MediaError::Codec(format!("Sync MFT ProcessInput: {e}")))?;
                let mut annex_b = Vec::new();
                let _ = extract_mft_output(&self.transform, &mut annex_b);
                Ok(annex_b)
            }
        }
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), MediaError> {
        let clamped = bitrate_bps.clamp(500_000, 35_000_000);
        self.bitrate_bps = clamped;
        if let Some(api) = &self.codec_api {
            unsafe {
                let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(clamped));
            }
        }
        Ok(())
    }
}

impl VideoEncoder for MfH264Encoder {
    fn encode_frame(&mut self, frame: &RawVideoFrame) -> Result<Vec<u8>, MediaError> {
        let width = frame.width & !1;
        let height = frame.height & !1;
        if width < 2 || height < 2 {
            return Err(MediaError::Codec("Video frame too small for MFT".to_string()));
        }

        if frame.is_bgra {
            bgra_to_nv12_into(&frame.rgba, width, height, &mut self.nv12_buf)?;
        } else {
            rgba_to_nv12_into(&frame.rgba, width, height, &mut self.nv12_buf)?;
        }

        let nv12_buf = std::mem::take(&mut self.nv12_buf);
        let force_keyframe = std::mem::take(&mut self.pending_keyframe);
        let (res, returned_buf) = self.send_command(|reply| MfCommand::Encode {
            width,
            height,
            nv12_buf,
            force_keyframe,
            reply,
        })?;
        self.nv12_buf = returned_buf;
        res
    }

    fn request_keyframe(&mut self) {
        self.pending_keyframe = true;
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), MediaError> {
        self.send_command(|reply| MfCommand::SetBitrate { bitrate_bps, reply })?
    }

    fn backend_name(&self) -> &'static str {
        "MediaFoundation-HW"
    }
}

unsafe fn extract_mft_output(
    transform: &IMFTransform,
    out_bytes: &mut Vec<u8>,
) -> Result<(), MediaError> {
    let stream_info = transform
        .GetOutputStreamInfo(0)
        .map_err(|e| MediaError::Codec(format!("GetOutputStreamInfo: {e}")))?;
    let provides_samples = (stream_info.dwFlags & 0x0000_0100) != 0; // MFT_OUTPUT_STREAM_PROVIDES_SAMPLES

    let mut out_sample: Option<IMFSample> = None;
    if !provides_samples {
        let s = MFCreateSample()
            .map_err(|e| MediaError::Codec(format!("MFCreateSample out: {e}")))?;
        let b = MFCreateMemoryBuffer(stream_info.cbSize.max(512 * 1024))
            .map_err(|e| MediaError::Codec(format!("MFCreateMemoryBuffer out: {e}")))?;
        let _ = s.AddBuffer(&b);
        out_sample = Some(s);
    }

    let mut out_buf = [MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: std::mem::ManuallyDrop::new(out_sample),
        dwStatus: 0,
        pEvents: std::mem::ManuallyDrop::new(None),
    }];
    let mut status = 0u32;
    let res = transform.ProcessOutput(0, &mut out_buf, &mut status);
    let sample_opt = std::mem::ManuallyDrop::take(&mut out_buf[0].pSample);
    let _ = std::mem::ManuallyDrop::take(&mut out_buf[0].pEvents);
    res.map_err(|e| MediaError::Codec(format!("MFT ProcessOutput: {e}")))?;

    if let Some(sample) = sample_opt {
        let _is_clean = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
        if let Ok(contiguous) = sample.ConvertToContiguousBuffer() {
            let mut ptr: *mut u8 = null_mut();
            let mut cur_len = 0u32;
            if contiguous.Lock(&mut ptr, None, Some(&mut cur_len)).is_ok() && !ptr.is_null() {
                let slice = std::slice::from_raw_parts(ptr, cur_len as usize);
                out_bytes.extend_from_slice(slice);
                let _ = contiguous.Unlock();
            }
        }
    }
    Ok(())
}

impl Drop for MfH264Encoder {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}
