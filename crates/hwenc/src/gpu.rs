//! Screen capture that stays on the GPU (Windows).
//!
//! Windows Graphics Capture delivers each new desktop frame as a D3D11
//! texture. The D3D11 video processor converts it to NV12 (and scales it) on
//! the GPU, and the encoder MFT reads the NV12 texture through a DXGI device
//! manager. No frame is copied to system memory, and a frame is taken as soon
//! as the desktop changed instead of on a fixed timer.

use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HMODULE, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory1,
};
use windows::Win32::Graphics::Gdi::{
    DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICEW, EnumDisplayDevicesW, EnumDisplayMonitors,
    GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};
use windows::core::{BOOL, Interface, PCWSTR};

use crate::mf::{Encoder, Session};
use crate::{Config, Control, EncodedFrame, Error, ScreenRect};

/// Converted frames waiting for or held by the encoder. The encoder reads
/// one or two at a time, so a ring of this size is never overwritten while
/// it still needs a texture.
const RING: usize = 6;
/// Without desktop changes, the last frame is sent again this often, so a
/// viewer that joins (or lost packets) gets a picture.
const REPEAT: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(1);

/// A monitor as libwebrtc numbers it: the index of an active display device.
struct Monitor {
    handle: HMONITOR,
    rect: RECT,
}

/// Finds the monitor of display device `index` (`None`: the first active one).
fn monitor(index: Option<u32>) -> Result<Monitor, Error> {
    let mut device = 0;
    loop {
        let mut info = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a correctly sized DISPLAY_DEVICEW out parameter.
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), device, &mut info, 0) }.as_bool() {
            return Err(Error::Platform("screen not found".into()));
        }
        let active = info.StateFlags.contains(DISPLAY_DEVICE_ACTIVE);
        if active && index.is_none_or(|i| i == device) {
            return find_monitor(&info.DeviceName);
        }
        if index.is_some_and(|i| i == device) {
            return Err(Error::Platform("screen is not active".into()));
        }
        device += 1;
    }
}

fn find_monitor(device_name: &[u16; 32]) -> Result<Monitor, Error> {
    struct Search<'a> {
        name: &'a [u16; 32],
        found: Option<Monitor>,
    }
    unsafe extern "system" fn visit(handle: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `Search` passed to EnumDisplayMonitors below,
        // which outlives the enumeration.
        let search = unsafe { &mut *(data.0 as *mut Search) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: MONITORINFOEXW starts with MONITORINFO and cbSize says
        // which one it is.
        let ok = unsafe { GetMonitorInfoW(handle, &mut info as *mut _ as *mut MONITORINFO) };
        if ok.as_bool() && info.szDevice == *search.name {
            search.found = Some(Monitor {
                handle,
                rect: info.monitorInfo.rcMonitor,
            });
            return false.into();
        }
        true.into()
    }
    let mut search = Search {
        name: device_name,
        found: None,
    };
    // SAFETY: the callback only runs during this call and gets `search`.
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(visit),
            LPARAM(&mut search as *mut Search as isize),
        );
    }
    search
        .found
        .ok_or_else(|| Error::Platform("monitor not found".into()))
}

/// The area of screen `index` in physical pixels, relative to the top-left
/// corner of the virtual desktop (as libwebrtc reports it).
pub(crate) fn screen_rect(index: Option<u32>) -> Result<ScreenRect, Error> {
    let rect = monitor(index)?.rect;
    // SAFETY: plain metric queries.
    let (left, top) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    };
    Ok(ScreenRect {
        x: rect.left - left,
        y: rect.top - top,
        width: (rect.right - rect.left) as u32,
        height: (rect.bottom - rect.top) as u32,
    })
}

/// The DXGI adapter that drives `monitor`, so capture, conversion and
/// encoding share one GPU.
fn adapter_for(monitor: HMONITOR) -> Option<IDXGIAdapter> {
    // SAFETY: COM calls on valid interfaces; out structs are locals.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                if output.GetDesc().is_ok_and(|d| d.Monitor == monitor) {
                    return adapter.cast().ok();
                }
                o += 1;
            }
            a += 1;
        }
        None
    }
}

/// D3D11 device with the video processor that turns BGRA into NV12.
struct Converter {
    device: ID3D11Device,
    video: ID3D11VideoDevice,
    context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    ring: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    /// Ring slot of the newest converted frame.
    last: Option<usize>,
}

impl Converter {
    fn new(
        adapter: Option<&IDXGIAdapter>,
        input: (u32, u32),
        config: &Config,
    ) -> Result<Self, Error> {
        // SAFETY: COM calls on valid interfaces; descriptors are locals that
        // outlive the calls.
        unsafe {
            let mut device = None;
            let mut immediate = None;
            let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
            let driver = if adapter.is_some() {
                D3D_DRIVER_TYPE_UNKNOWN
            } else {
                windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE
            };
            D3D11CreateDevice(
                adapter,
                driver,
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut immediate),
            )?;
            let device: ID3D11Device = device.ok_or(Error::NoEncoder)?;
            let immediate: ID3D11DeviceContext = immediate.ok_or(Error::NoEncoder)?;
            // The encoder MFT uses the device from its own threads.
            let _ = immediate
                .cast::<ID3D11Multithread>()?
                .SetMultithreadProtected(true);

            let video: ID3D11VideoDevice = device.cast()?;
            let context: ID3D11VideoContext = immediate.cast()?;
            let rate = DXGI_RATIONAL {
                Numerator: config.fps,
                Denominator: 1,
            };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: input.0,
                InputHeight: input.1,
                OutputFrameRate: rate,
                OutputWidth: config.width,
                OutputHeight: config.height,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = video.CreateVideoProcessorEnumerator(&desc)?;
            let processor = video.CreateVideoProcessor(&enumerator, 0)?;
            context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            // Full range RGB in; BT.601 limited range out, like the libyuv
            // conversion of the software path (and what receivers assume).
            let rgb_full = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let yuv_601_studio = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 1 << 4 };
            context.VideoProcessorSetStreamColorSpace(&processor, 0, &rgb_full);
            context.VideoProcessorSetOutputColorSpace(&processor, &yuv_601_studio);

            let texture = D3D11_TEXTURE2D_DESC {
                Width: config.width,
                Height: config.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let view = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut ring = Vec::with_capacity(RING);
            for _ in 0..RING {
                let mut tex = None;
                device.CreateTexture2D(&texture, None, Some(&mut tex))?;
                let tex: ID3D11Texture2D = tex.ok_or(Error::NoEncoder)?;
                let mut out = None;
                video.CreateVideoProcessorOutputView(&tex, &enumerator, &view, Some(&mut out))?;
                ring.push((tex, out.ok_or(Error::NoEncoder)?));
            }
            Ok(Self {
                device,
                video,
                context,
                enumerator,
                processor,
                ring,
                next: 0,
                last: None,
            })
        }
    }

    /// Converts a captured BGRA texture into the next ring slot.
    fn convert(&mut self, frame: &ID3D11Texture2D) -> Result<&ID3D11Texture2D, Error> {
        let slot = self.next;
        self.next = (self.next + 1) % RING;
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        // SAFETY: COM calls on valid interfaces. The stream struct holds the
        // input view in ManuallyDrop; it is taken back out after the call so
        // it is released exactly once.
        unsafe {
            let mut input = None;
            self.video.CreateVideoProcessorInputView(
                frame,
                &self.enumerator,
                &desc,
                Some(&mut input),
            )?;
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(input),
                ..Default::default()
            };
            let streams = [stream];
            let result =
                self.context
                    .VideoProcessorBlt(&self.processor, &self.ring[slot].1, 0, &streams);
            let [stream] = streams;
            drop(ManuallyDrop::into_inner(stream.pInputSurface));
            result?;
        }
        self.last = Some(slot);
        Ok(&self.ring[slot].0)
    }

    fn last(&self) -> Option<&ID3D11Texture2D> {
        self.last.map(|slot| &self.ring[slot].0)
    }
}

/// Windows Graphics Capture of one monitor into the converter's device.
struct Capture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    size: (u32, u32),
}

impl Capture {
    fn new(device: &ID3D11Device, monitor: HMONITOR) -> Result<Self, Error> {
        // SAFETY: COM/WinRT calls on valid interfaces.
        unsafe {
            let interop =
                windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            let item: GraphicsCaptureItem = interop.CreateForMonitor(monitor)?;
            let size = item.Size()?;
            let winrt: IDirect3DDevice =
                CreateDirect3D11DeviceFromDXGIDevice(&device.cast::<IDXGIDevice>()?)?.cast()?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            )?;
            let session = pool.CreateCaptureSession(&item)?;
            let _ = session.SetIsCursorCaptureEnabled(true);
            // Our own share border shows what is captured (Windows 11 only).
            let _ = session.SetIsBorderRequired(false);
            session.StartCapture()?;
            Ok(Self {
                pool,
                session,
                size: (size.Width as u32, size.Height as u32),
            })
        }
    }

    /// The newest captured frame, dropping older ones.
    fn latest(&self) -> Option<Direct3D11CaptureFrame> {
        let mut newest = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            if let Some(old) = newest.replace(frame) {
                let _ = old.Close();
            }
        }
        newest
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn texture(frame: &Direct3D11CaptureFrame) -> Result<ID3D11Texture2D, Error> {
    let surface = frame.Surface()?;
    // SAFETY: COM call on a valid interface.
    unsafe {
        Ok(surface
            .cast::<IDirect3DDxgiInterfaceAccess>()?
            .GetInterface()?)
    }
}

/// Everything the capture loop needs besides the encoder.
struct Prepared {
    converter: Converter,
    capture: Capture,
    manager: IMFDXGIDeviceManager,
}

fn prepare(screen: Option<u32>, config: &Config) -> Result<Prepared, Error> {
    let monitor = monitor(screen)?;
    let adapter = adapter_for(monitor.handle);
    let input = (
        (monitor.rect.right - monitor.rect.left) as u32,
        (monitor.rect.bottom - monitor.rect.top) as u32,
    );
    let converter = Converter::new(adapter.as_ref(), input, config)?;
    let capture = Capture::new(&converter.device, monitor.handle)?;
    if capture.size != input {
        return Err(Error::Platform(format!(
            "capture size {:?} differs from the monitor size {input:?}",
            capture.size
        )));
    }
    let mut token = 0;
    let mut manager = None;
    // SAFETY: out parameters are locals; COM call on a valid interface.
    let manager = unsafe {
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.ok_or(Error::NoEncoder)?;
        manager.ResetDevice(&converter.device, token)?;
        manager
    };
    Ok(Prepared {
        converter,
        capture,
        manager,
    })
}

/// Encoder thread entry point for a screen share.
pub(crate) fn run(
    screen: Option<u32>,
    config: Config,
    stop: Receiver<()>,
    control: Arc<Control>,
    ready: Sender<Result<String, Error>>,
    mut on_frame: impl FnMut(EncodedFrame),
) {
    let session = match Session::start() {
        Ok(session) => session,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let started = prepare(screen, &config).and_then(|prepared| {
        let encoder = Encoder::new(config, Some(&prepared.manager))?;
        Ok((prepared, encoder))
    });
    let (mut prepared, mut encoder) = match started {
        Ok(started) => started,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    tracing::info!(encoder = %encoder.name, ?config, "GPU screen capture started");
    let _ = ready.send(Ok(encoder.name.clone()));
    let result = capture_loop(
        &mut prepared,
        &mut encoder,
        &config,
        &stop,
        &control,
        &mut on_frame,
    );
    if let Err(e) = result {
        tracing::error!("GPU screen capture failed: {e}");
    }
    encoder.shutdown();
    drop(prepared);
    drop(session);
}

fn capture_loop(
    prepared: &mut Prepared,
    encoder: &mut Encoder,
    config: &Config,
    stop: &Receiver<()>,
    control: &Control,
    on_frame: &mut impl FnMut(EncodedFrame),
) -> Result<(), Error> {
    let epoch = Instant::now();
    let interval = Duration::from_secs(1) / config.fps.max(1);
    // Allow a frame slightly early: desktop frames arrive on the display's
    // refresh grid, which rarely divides evenly into the share rate.
    let min_gap = interval.mul_f32(0.8);
    let mut last_sent: Option<Instant> = None;
    let mut pending: Option<Direct3D11CaptureFrame> = None;
    loop {
        match stop.try_recv() {
            Err(TryRecvError::Empty) => {}
            _ => return Ok(()),
        }
        encoder.drain_events(on_frame)?;
        if let Some(frame) = prepared.capture.latest()
            && let Some(old) = pending.replace(frame)
        {
            let _ = old.Close();
        }
        let now = Instant::now();
        let due = last_sent.is_none_or(|t| now - t >= min_gap);
        if encoder.credits > 0 && due {
            let timestamp_us = epoch.elapsed().as_micros() as i64;
            if let Some(frame) = pending.take() {
                let converted = prepared.converter.convert(&texture(&frame)?);
                let _ = frame.Close();
                encoder.apply_control(control);
                encoder.process_texture(converted?, timestamp_us)?;
                encoder.credits -= 1;
                last_sent = Some(now);
            } else if last_sent.is_some_and(|t| now - t >= REPEAT)
                || control.keyframe.load(std::sync::atomic::Ordering::Relaxed)
            {
                // Nothing changed on screen; repeat the last picture.
                if let Some(last) = prepared.converter.last() {
                    encoder.apply_control(control);
                    encoder.process_texture(last, timestamp_us)?;
                    encoder.credits -= 1;
                    last_sent = Some(now);
                }
            }
        }
        std::thread::sleep(POLL);
    }
}
