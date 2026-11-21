#![cfg(target_os = "windows")]

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;
use std::sync::Arc;

use fast_image_resize::Resizer;
use shared_protocol::{CaptureTarget, TargetResolution};
use tracing::{debug, warn};
use windows::core::{Interface, IInspectable, HRESULT};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{
    CloseHandle, BOOL, ERROR_ACCESS_DENIED, HWND, LPARAM, RECT, WAIT_TIMEOUT,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    SRCCOPY,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetShellWindow, GetWindow, GetWindowLongW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, GWL_EXSTYLE, GW_OWNER, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

use crate::media::traits::{FrameBufferPool, MediaError, SharedFrameBuffer};
use crate::media::video::scale_pixel_buffer_pooled;

/// Outcome of polling a Windows GPU/GDI capture source.
pub enum CapturePoll {
    /// New scaled BGRA frame produced.
    Frame(u32, u32, SharedFrameBuffer),
    /// Source screen/window had no compositor damage since the previous frame.
    Unchanged,
}

fn has_non_zero_sample(buf: &[u8]) -> bool {
    let step = (buf.len() / 512).max(16);
    for chunk in buf.chunks(step) {
        if chunk.len() >= 3 && (chunk[0] != 0 || chunk[1] != 0 || chunk[2] != 0) {
            return true;
        }
    }
    false
}

/// GPU DXGI Desktop Duplication monitor capturer (zero duplicate encode spam on idle ticks).
pub struct DxgiMonitorDuplicator {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dupl: IDXGIOutputDuplication,
    staging_tex: Option<ID3D11Texture2D>,
    staging_dims: (u32, u32),
    last_bgra: Vec<u8>,
    width: u32,
    height: u32,
    monitor_id_hint: u32,
    monitor_name_hint: String,
}

impl DxgiMonitorDuplicator {
    pub fn new(monitor_id_hint: u32, monitor_name_hint: &str) -> Result<Self, MediaError> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()
                .map_err(|e| MediaError::VideoCapture(format!("CreateDXGIFactory1: {e}")))?;

            let mut fallback_pair = None;
            let mut matched_pair = None;
            let mut adapter_idx = 0u32;

            while let Ok(adapter) = factory.EnumAdapters1(adapter_idx) {
                let mut output_idx = 0u32;
                while let Ok(output) = adapter.EnumOutputs(output_idx) {
                    if let Ok(desc) = output.GetDesc()
                        && desc.AttachedToDesktop.as_bool()
                    {
                        let hmon_id = desc.Monitor.0 as usize as u32;
                        let dev_name_len = desc
                            .DeviceName
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(desc.DeviceName.len());
                        let dev_name =
                            String::from_utf16_lossy(&desc.DeviceName[..dev_name_len]);

                        if fallback_pair.is_none() {
                            fallback_pair = Some((adapter.clone(), output.clone()));
                        }
                        if hmon_id == monitor_id_hint
                            || (!monitor_name_hint.is_empty()
                                && dev_name.contains(monitor_name_hint))
                        {
                            matched_pair = Some((adapter.clone(), output));
                            break;
                        }
                    }
                    output_idx += 1;
                }
                if matched_pair.is_some() {
                    break;
                }
                adapter_idx += 1;
            }

            let (adapter, output) = matched_pair.or(fallback_pair).ok_or_else(|| {
                MediaError::VideoCapture("No attached DXGI desktop outputs found".to_string())
            })?;

            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| MediaError::VideoCapture(format!("D3D11CreateDevice failed: {e}")))?;

            let device = device.ok_or_else(|| {
                MediaError::VideoCapture("D3D11CreateDevice returned null device".to_string())
            })?;
            let context = context.ok_or_else(|| {
                MediaError::VideoCapture("D3D11CreateDevice returned null context".to_string())
            })?;

            let output1: IDXGIOutput1 = output
                .cast()
                .map_err(|e| MediaError::VideoCapture(format!("IDXGIOutput1 cast: {e}")))?;
            let dupl = output1
                .DuplicateOutput(&device)
                .map_err(|e| MediaError::VideoCapture(format!("DuplicateOutput: {e}")))?;

            let dupl_desc = dupl.GetDesc();
            let width = dupl_desc.ModeDesc.Width;
            let height = dupl_desc.ModeDesc.Height;

            Ok(Self {
                device,
                context,
                dupl,
                staging_tex: None,
                staging_dims: (0, 0),
                last_bgra: Vec::new(),
                width,
                height,
                monitor_id_hint,
                monitor_name_hint: monitor_name_hint.to_string(),
            })
        }
    }

    fn recreate(&mut self) -> Result<(), MediaError> {
        *self = Self::new(self.monitor_id_hint, &self.monitor_name_hint)?;
        Ok(())
    }

    pub fn poll_scaled_frame(
        &mut self,
        resizer: &mut Resizer,
        pool: &Arc<FrameBufferPool>,
        resolution: TargetResolution,
        clamp_1080p: bool,
    ) -> Result<CapturePoll, MediaError> {
        unsafe {
            let timeout_ms = if self.last_bgra.is_empty() { 32 } else { 0 };
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;

            match self
                .dupl
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
            {
                Ok(()) => {
                    let copy_res = (|| -> Result<(), MediaError> {
                        let res = resource.take().ok_or_else(|| {
                            MediaError::VideoCapture(
                                "DXGI AcquireNextFrame returned null resource".to_string(),
                            )
                        })?;
                        let src_tex: ID3D11Texture2D = res.cast().map_err(|e| {
                            MediaError::VideoCapture(format!("ID3D11Texture2D cast: {e}"))
                        })?;
                        copy_texture_to_bgra_vec(
                            &self.device,
                            &self.context,
                            &mut self.staging_tex,
                            &mut self.staging_dims,
                            &src_tex,
                            &mut self.last_bgra,
                            &mut self.width,
                            &mut self.height,
                        )
                    })();

                    let _ = self.dupl.ReleaseFrame();
                    copy_res?;

                    let (dst_w, dst_h, scaled) = scale_pixel_buffer_pooled(
                        resizer,
                        pool,
                        &self.last_bgra,
                        self.width,
                        self.height,
                        resolution,
                        clamp_1080p,
                    )?;
                    Ok(CapturePoll::Frame(dst_w, dst_h, scaled))
                }
                Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    if self.last_bgra.is_empty() {
                        Err(MediaError::VideoCapture(
                            "DXGI initial frame timed out".to_string(),
                        ))
                    } else {
                        Ok(CapturePoll::Unchanged)
                    }
                }
                Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => {
                    self.recreate()?;
                    Ok(CapturePoll::Unchanged)
                }
                Err(err) => Err(MediaError::VideoCapture(format!(
                    "DXGI AcquireNextFrame error: {err}"
                ))),
            }
        }
    }

    pub fn source_dims(&self) -> Option<(u32, u32)> {
        (self.width > 1 && self.height > 1).then_some((self.width, self.height))
    }
}

/// Returns `true` if `pid == 0` (unknown) or the Win32 process is still running.
pub fn is_win32_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return true;
    }
    unsafe {
        match OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        ) {
            Ok(handle) => {
                let mut exit_code: u32 = 0;
                let still_running = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT
                    && GetExitCodeProcess(handle, &mut exit_code).is_ok()
                    && exit_code == 259; // STILL_ACTIVE
                let _ = CloseHandle(handle);
                still_running
            }
            Err(err) => err.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0),
        }
    }
}

/// GPU window capturer backed by `Windows.Graphics.Capture` (WGC) for tear-free game capture
/// without Alt-Tab desktop leaks, with automatic GDI DIB fallback on older Windows 10 builds.
pub struct Win32WindowCapturer {
    id: u32,
    title: String,
    pid: u32,
    wgc: Option<WgcSession>,
    wgc_attempted: bool,
    gdi_fallback: GdiDibSurface,
    last_source_dims: Option<(u32, u32)>,
}

struct WgcSession {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    winrt_device: IDirect3DDevice,
    frame_pool: Direct3D11CaptureFramePool,
    _session: GraphicsCaptureSession,
    staging_tex: Option<ID3D11Texture2D>,
    staging_dims: (u32, u32),
    last_bgra: Vec<u8>,
    width: u32,
    height: u32,
    pool_size: SizeInt32,
}

type PolledBgra<'a> = (u32, u32, &'a [u8]);

impl WgcSession {
    fn new(hwnd: HWND) -> Result<Self, MediaError> {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);

            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                None,
                windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| MediaError::VideoCapture(format!("WGC D3D11CreateDevice failed: {e}")))?;

            let device = device.ok_or_else(|| {
                MediaError::VideoCapture("WGC D3D11 device was null".to_string())
            })?;
            let context = context.ok_or_else(|| {
                MediaError::VideoCapture("WGC D3D11 context was null".to_string())
            })?;

            let dxgi_device: IDXGIDevice = device
                .cast()
                .map_err(|e| MediaError::VideoCapture(format!("WGC IDXGIDevice cast: {e}")))?;
            let inspectable: IInspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)
                .map_err(|e| {
                    MediaError::VideoCapture(format!(
                        "CreateDirect3D11DeviceFromDXGIDevice failed: {e}"
                    ))
                })?;
            let winrt_device: IDirect3DDevice = inspectable
                .cast()
                .map_err(|e| MediaError::VideoCapture(format!("IDirect3DDevice cast: {e}")))?;

            let interop: IGraphicsCaptureItemInterop =
                windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                    .map_err(|e| {
                        MediaError::VideoCapture(format!("WGC GraphicsCaptureItem factory: {e}"))
                    })?;
            let item: GraphicsCaptureItem = interop.CreateForWindow(hwnd).map_err(|e| {
                MediaError::VideoCapture(format!("WGC CreateForWindow failed: {e}"))
            })?;
            let item_size = item
                .Size()
                .map_err(|e| MediaError::VideoCapture(format!("WGC GraphicsCaptureItem::Size: {e}")))?;
            let pool_size = SizeInt32 {
                Width: item_size.Width.max(2),
                Height: item_size.Height.max(2),
            };

            let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                pool_size,
            )
            .map_err(|e| {
                MediaError::VideoCapture(format!("WGC CreateFreeThreaded failed: {e}"))
            })?;

            let session = frame_pool.CreateCaptureSession(&item).map_err(|e| {
                MediaError::VideoCapture(format!("WGC CreateCaptureSession failed: {e}"))
            })?;
            // Suppress yellow capture border on Win11 and keep game cursor visible if supported.
            let _ = session.SetIsBorderRequired(false);
            let _ = session.SetIsCursorCaptureEnabled(true);
            session.StartCapture().map_err(|e| {
                MediaError::VideoCapture(format!("WGC StartCapture failed: {e}"))
            })?;

            Ok(Self {
                device,
                context,
                winrt_device,
                frame_pool,
                _session: session,
                staging_tex: None,
                staging_dims: (0, 0),
                last_bgra: Vec::new(),
                width: 0,
                height: 0,
                pool_size,
            })
        }
    }

    fn poll_bgra(&mut self) -> Result<Option<PolledBgra<'_>>, MediaError> {
        let mut latest_frame = None;
        while let Ok(frame) = self.frame_pool.TryGetNextFrame() {
            latest_frame = Some(frame);
        }

        let Some(frame) = latest_frame else {
            return Ok(None);
        };

        let content_size = frame
            .ContentSize()
            .unwrap_or(self.pool_size);
        if content_size.Width > 1
            && content_size.Height > 1
            && (content_size.Width != self.pool_size.Width
                || content_size.Height != self.pool_size.Height)
        {
            self.pool_size = content_size;
            let _ = self.frame_pool.Recreate(
                &self.winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                content_size,
            );
        }

        let surface = frame
            .Surface()
            .map_err(|e| MediaError::VideoCapture(format!("WGC frame.Surface(): {e}")))?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(|e| {
            MediaError::VideoCapture(format!("WGC IDirect3DDxgiInterfaceAccess: {e}"))
        })?;
        let src_tex: ID3D11Texture2D = unsafe {
            access
                .GetInterface()
                .map_err(|e| MediaError::VideoCapture(format!("WGC GetInterface<ID3D11Texture2D>: {e}")))?
        };

        unsafe {
            copy_texture_to_bgra_vec(
                &self.device,
                &self.context,
                &mut self.staging_tex,
                &mut self.staging_dims,
                &src_tex,
                &mut self.last_bgra,
                &mut self.width,
                &mut self.height,
            )?;
        }

        Ok(Some((self.width, self.height, &self.last_bgra)))
    }
}

impl Drop for WgcSession {
    fn drop(&mut self) {
        let _ = self._session.Close();
        let _ = self.frame_pool.Close();
    }
}

impl Win32WindowCapturer {
    pub fn new(id: u32, title: String, pid: u32) -> Self {
        Self {
            id,
            title,
            pid,
            wgc: None,
            wgc_attempted: false,
            gdi_fallback: GdiDibSurface::default(),
            last_source_dims: None,
        }
    }

    pub fn last_source_dims(&self) -> Option<(u32, u32)> {
        self.last_source_dims
    }

    pub fn poll_scaled_frame(
        &mut self,
        resizer: &mut Resizer,
        pool: &Arc<FrameBufferPool>,
        resolution: TargetResolution,
        clamp_1080p: bool,
    ) -> Result<CapturePoll, MediaError> {
        let hwnd = resolve_hwnd(self.id, &self.title, self.pid)?;
        let resolved_id = hwnd.0 as usize as u32;
        if resolved_id != self.id {
            self.id = resolved_id;
            self.wgc = None;
            self.wgc_attempted = false;
        }
        if unsafe { IsIconic(hwnd).as_bool() } {
            // Minimized windows don't produce compositor surfaces until restored.
            return Ok(CapturePoll::Unchanged);
        }

        if !self.wgc_attempted {
            self.wgc_attempted = true;
            match WgcSession::new(hwnd) {
                Ok(session) => {
                    debug!(id = self.id, title = %self.title, "Activated Windows.Graphics.Capture for window");
                    self.wgc = Some(session);
                }
                Err(err) => {
                    warn!(id = self.id, title = %self.title, "WGC unavailable ({err}); using PrintWindow DIB fallback");
                }
            }
        }

        if let Some(wgc) = self.wgc.as_mut() {
            match wgc.poll_bgra() {
                Ok(Some((w, h, bgra))) => {
                    self.last_source_dims = Some((w, h));
                    let (dst_w, dst_h, scaled) = scale_pixel_buffer_pooled(
                        resizer,
                        pool,
                        bgra,
                        w,
                        h,
                        resolution,
                        clamp_1080p,
                    )?;
                    return Ok(CapturePoll::Frame(dst_w, dst_h, scaled));
                }
                Ok(None) => return Ok(CapturePoll::Unchanged),
                Err(err) => {
                    debug!("WGC poll failed ({err}), recreating session");
                    self.wgc = None;
                    self.wgc_attempted = false;
                }
            }
        }

        let hwnd = resolve_hwnd(self.id, &self.title, self.pid)?;
        self.capture_gdi_fallback(hwnd, resizer, pool, resolution, clamp_1080p)
    }

    fn capture_gdi_fallback(
        &mut self,
        hwnd: HWND,
        resizer: &mut Resizer,
        pool: &Arc<FrameBufferPool>,
        resolution: TargetResolution,
        clamp_1080p: bool,
    ) -> Result<CapturePoll, MediaError> {
        unsafe {
            let mut client = RECT::default();
            let mut use_window_dc = false;
            if GetClientRect(hwnd, &mut client).is_err()
                || (client.right - client.left) <= 1
                || (client.bottom - client.top) <= 1
            {
                GetWindowRect(hwnd, &mut client).map_err(|e| {
                    MediaError::VideoCapture(format!("GetWindowRect failed: {e}"))
                })?;
                use_window_dc = true;
            }
            let w = (client.right - client.left).max(2) as u32;
            let h = (client.bottom - client.top).max(2) as u32;
            self.last_source_dims = Some((w, h));

            let hdc_src = GetDC(hwnd);
            if hdc_src.is_invalid() {
                return Ok(CapturePoll::Unchanged);
            }

            if !self.gdi_fallback.ensure_size(hdc_src, w, h) {
                ReleaseDC(hwnd, hdc_src);
                return Err(MediaError::VideoCapture("GDI DIB allocation failed".to_string()));
            }

            let ok_blt = BitBlt(
                self.gdi_fallback.hdc_mem,
                0,
                0,
                w as i32,
                h as i32,
                hdc_src,
                0,
                0,
                SRCCOPY,
            )
            .is_ok();
            ReleaseDC(hwnd, hdc_src);

            let total_bytes = (w as usize) * (h as usize) * 4;
            let raw = std::slice::from_raw_parts(self.gdi_fallback.dib_ptr, total_bytes);
            if !ok_blt || !has_non_zero_sample(raw) {
                let flags = if use_window_dc {
                    PRINT_WINDOW_FLAGS(2)
                } else {
                    PRINT_WINDOW_FLAGS(3)
                };
                let _ = PrintWindow(hwnd, self.gdi_fallback.hdc_mem, flags);
            }

            let raw_after = std::slice::from_raw_parts(self.gdi_fallback.dib_ptr, total_bytes);
            let (dst_w, dst_h, scaled) = scale_pixel_buffer_pooled(
                resizer,
                pool,
                raw_after,
                w,
                h,
                resolution,
                clamp_1080p,
            )?;
            Ok(CapturePoll::Frame(dst_w, dst_h, scaled))
        }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn copy_texture_to_bgra_vec(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    staging_tex: &mut Option<ID3D11Texture2D>,
    staging_dims: &mut (u32, u32),
    src_tex: &ID3D11Texture2D,
    out_bgra: &mut Vec<u8>,
    out_w: &mut u32,
    out_h: &mut u32,
) -> Result<(), MediaError> {
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        src_tex.GetDesc(&mut desc);
        let w = desc.Width;
        let h = desc.Height;
        if w == 0 || h == 0 {
            return Err(MediaError::VideoCapture(
                "Captured D3D11 texture has zero dimensions".to_string(),
            ));
        }

        if staging_tex.is_none() || *staging_dims != (w, h) {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut new_tex: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&staging_desc, None, Some(&mut new_tex))
                .map_err(|e| MediaError::VideoCapture(format!("CreateTexture2D staging: {e}")))?;
            *staging_tex = new_tex;
            *staging_dims = (w, h);
        }

        let staging = staging_tex.as_ref().unwrap();
        context.CopyResource(staging, src_tex);

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| MediaError::VideoCapture(format!("ID3D11DeviceContext::Map: {e}")))?;

        let row_bytes = (w as usize) * 4;
        let total_bytes = row_bytes * (h as usize);
        if out_bgra.len() != total_bytes {
            out_bgra.resize(total_bytes, 0);
        }
        let src_pitch = mapped.RowPitch as usize;
        let src_ptr = mapped.pData.cast::<u8>();
        if src_pitch == row_bytes {
            std::ptr::copy_nonoverlapping(src_ptr, out_bgra.as_mut_ptr(), total_bytes);
        } else {
            for row in 0..(h as usize) {
                std::ptr::copy_nonoverlapping(
                    src_ptr.add(row * src_pitch),
                    out_bgra.as_mut_ptr().add(row * row_bytes),
                    row_bytes,
                );
            }
        }

        context.Unmap(staging, 0);
        *out_w = w;
        *out_h = h;
        Ok(())
    }
}

struct GdiDibSurface {
    hdc_mem: HDC,
    hbitmap: HBITMAP,
    old_obj: HGDIOBJ,
    dib_ptr: *mut u8,
    width: u32,
    height: u32,
}

impl Default for GdiDibSurface {
    fn default() -> Self {
        Self {
            hdc_mem: HDC::default(),
            hbitmap: HBITMAP::default(),
            old_obj: HGDIOBJ::default(),
            dib_ptr: null_mut(),
            width: 0,
            height: 0,
        }
    }
}

impl GdiDibSurface {
    unsafe fn cleanup(&mut self) {
        unsafe {
            if !self.hdc_mem.is_invalid() {
                if !self.old_obj.is_invalid() {
                    let _ = SelectObject(self.hdc_mem, self.old_obj);
                    self.old_obj = HGDIOBJ::default();
                }
                if !self.hbitmap.is_invalid() {
                    let _ = DeleteObject(self.hbitmap);
                    self.hbitmap = HBITMAP::default();
                }
                let _ = DeleteDC(self.hdc_mem);
                self.hdc_mem = HDC::default();
            }
            self.dib_ptr = null_mut();
            self.width = 0;
            self.height = 0;
        }
    }

    unsafe fn ensure_size(&mut self, hdc_ref: HDC, width: u32, height: u32) -> bool {
        unsafe {
            if !self.hdc_mem.is_invalid()
                && !self.dib_ptr.is_null()
                && self.width == width
                && self.height == height
            {
                return true;
            }
            self.cleanup();

            let hdc_mem = CreateCompatibleDC(hdc_ref);
            if hdc_mem.is_invalid() {
                return false;
            }

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut c_void = null_mut();
            let hbitmap = match CreateDIBSection(
                hdc_mem,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                windows::Win32::Foundation::HANDLE::default(),
                0,
            ) {
                Ok(hbmp) if !hbmp.is_invalid() && !bits.is_null() => hbmp,
                _ => {
                    let _ = DeleteDC(hdc_mem);
                    return false;
                }
            };

            let old_obj = SelectObject(hdc_mem, hbitmap);
            self.hdc_mem = hdc_mem;
            self.hbitmap = hbitmap;
            self.old_obj = old_obj;
            self.dib_ptr = bits.cast::<u8>();
            self.width = width;
            self.height = height;
            true
        }
    }
}

impl Drop for GdiDibSurface {
    fn drop(&mut self) {
        unsafe {
            self.cleanup();
        }
    }
}

fn is_ignored_system_title(title: &str) -> bool {
    matches!(
        title,
        "Program Manager"
            | "Windows Input Experience"
            | "Microsoft Text Input Application"
            | "MSCTFIME UI"
            | "Default IME"
    )
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        let out = &mut *(lparam.0 as *mut Vec<(CaptureTarget, Option<(u32, u32)>)>);

        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }

        let shell_hwnd = GetShellWindow();
        if !shell_hwnd.0.is_null() && hwnd == shell_hwnd {
            return BOOL(1);
        }

        let is_minimized = IsIconic(hwnd).as_bool();
        if !is_minimized {
            let mut cloaked: u32 = 0;
            if DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                (&mut cloaked as *mut u32).cast::<c_void>(),
                size_of::<u32>() as u32,
            )
            .is_ok()
                && cloaked != 0
            {
                return BOOL(1);
            }
        }

        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let is_app_window = (ex_style & WS_EX_APPWINDOW.0) != 0;
        let is_tool_window = (ex_style & WS_EX_TOOLWINDOW.0) != 0;
        let is_no_activate = (ex_style & WS_EX_NOACTIVATE.0) != 0;

        if (is_tool_window || is_no_activate) && !is_app_window {
            return BOOL(1);
        }

        if let Ok(owner) = GetWindow(hwnd, GW_OWNER)
            && !owner.0.is_null()
            && !is_app_window
        {
            return BOOL(1);
        }

        let text_len = GetWindowTextLengthW(hwnd);
        if text_len <= 0 {
            return BOOL(1);
        }

        let mut buf = vec![0u16; (text_len as usize) + 1];
        let copied = GetWindowTextW(hwnd, &mut buf);
        if copied <= 0 {
            return BOOL(1);
        }

        let title = String::from_utf16_lossy(&buf[..copied as usize])
            .trim()
            .to_string();
        if title.is_empty() || is_ignored_system_title(&title) {
            return BOOL(1);
        }

        let mut dims = None;
        if !is_minimized {
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_ok() {
                let w = rect.right - rect.left;
                let h = rect.bottom - rect.top;
                if w <= 1 || h <= 1 {
                    return BOOL(1);
                }
                dims = Some((w as u32, h as u32));
            }
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));

        out.push((
            CaptureTarget::Window {
                id: hwnd.0 as usize as u32,
                title,
                pid,
            },
            dims,
        ));

        BOOL(1)
    }
}

pub fn list_windows_win32_with_dims() -> Vec<(CaptureTarget, Option<(u32, u32)>)> {
    let mut windows: Vec<(CaptureTarget, Option<(u32, u32)>)> = Vec::new();
    unsafe {
        let lparam = LPARAM((&mut windows as *mut Vec<(CaptureTarget, Option<(u32, u32)>)>) as isize);
        let _ = EnumWindows(Some(enum_windows_proc), lparam);
    }
    windows
}

pub fn list_windows_win32() -> Vec<CaptureTarget> {
    list_windows_win32_with_dims()
        .into_iter()
        .map(|(target, _)| target)
        .collect()
}

pub fn resolve_hwnd(id: u32, title_hint: &str, pid_hint: u32) -> Result<HWND, MediaError> {
    if pid_hint > 0 && !is_win32_process_alive(pid_hint) {
        return Err(MediaError::SourceNotFound(format!(
            "Process pid={pid_hint} for window id={id} ({title_hint}) terminated"
        )));
    }

    let candidate = HWND(id as usize as *mut c_void);
    if unsafe { IsWindow(candidate).as_bool() } {
        if pid_hint == 0 {
            return Ok(candidate);
        }
        let mut actual_pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(candidate, Some(&mut actual_pid)) };
        if actual_pid == 0 || actual_pid == pid_hint {
            return Ok(candidate);
        }
    }

    for target in list_windows_win32() {
        if let CaptureTarget::Window {
            id: win_id,
            title,
            pid: win_pid,
        } = target
            && crate::media::video::matches_window_candidate(
                id,
                title_hint,
                pid_hint,
                win_id,
                &title,
                win_pid,
            )
        {
            return Ok(HWND(win_id as usize as *mut c_void));
        }
    }

    Err(MediaError::SourceNotFound(format!(
        "Window id={id} pid={pid_hint} ({title_hint}) closed or not found"
    )))
}
