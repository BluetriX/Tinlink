//! Hardware H.264 encoding for screen sharing.
//!
//! The LiveKit SDK only ships hardware encoders on Linux. On Windows this
//! crate drives the GPU vendor's Media Foundation encoder (NVENC, AMF or
//! Quick Sync) and hands out Annex B access units, which the SDK forwards
//! unchanged through a pre-encoded video source.

pub mod nal;

#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod mf;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::thread::JoinHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Frame size in pixels; both must be even (NV12).
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}

/// A monitor's area in physical pixels, relative to the top-left corner of
/// the virtual desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// The area of screen `index` (numbered like libwebrtc's screen sources:
/// the index of an active display device; `None`: the first one).
#[cfg(windows)]
pub fn screen_rect(index: Option<u32>) -> Result<ScreenRect, Error> {
    gpu::screen_rect(index)
}

#[cfg(not(windows))]
pub fn screen_rect(_index: Option<u32>) -> Result<ScreenRect, Error> {
    Err(Error::Unsupported)
}

/// One encoded access unit in Annex B format.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub timestamp_us: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no hardware H.264 encoder found")]
    NoEncoder,
    #[error("hardware encoding is not supported on this platform")]
    Unsupported,
    #[error("frame size {0}x{1} must be even and non-zero")]
    InvalidSize(u32, u32),
    #[error("{0}")]
    Platform(String),
}

/// Frames pass between the threads through this.
pub(crate) struct InputFrame {
    pub nv12: Vec<u8>,
    pub timestamp_us: i64,
}

/// Requests from the caller, applied by the encoder thread before the next frame.
#[derive(Default)]
pub(crate) struct Control {
    pub keyframe: AtomicBool,
    /// Requested bitrate, 0 when unchanged.
    pub bitrate_bps: AtomicU32,
    pub dropped: AtomicU64,
}

/// A running hardware encoder. Frames go in with [`HardwareEncoder::push`];
/// encoded frames are delivered to the callback on the encoder's own thread
/// as soon as the GPU emits them.
pub struct HardwareEncoder {
    name: String,
    input: Option<SyncSender<InputFrame>>,
    /// Only for screen capture encoders; dropped to stop them.
    stop: Option<SyncSender<()>>,
    control: Arc<Control>,
    thread: Option<JoinHandle<()>>,
}

impl HardwareEncoder {
    #[cfg(windows)]
    pub fn start(
        config: Config,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self, Error> {
        if config.width == 0
            || config.height == 0
            || !config.width.is_multiple_of(2)
            || !config.height.is_multiple_of(2)
        {
            return Err(Error::InvalidSize(config.width, config.height));
        }
        // One slot: a frame that arrives while the encoder is busy replaces
        // nothing and is dropped, so latency never builds up in a queue.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let control = Arc::new(Control::default());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = {
            let control = control.clone();
            std::thread::Builder::new()
                .name("hw-encoder".into())
                .spawn(move || mf::run(config, rx, control, ready_tx, on_frame))
                .map_err(|e| Error::Platform(e.to_string()))?
        };
        let name = match ready_rx.recv() {
            Ok(Ok(name)) => name,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => return Err(Error::Platform("encoder thread exited".into())),
        };
        Ok(Self {
            name,
            input: Some(tx),
            stop: None,
            control,
            thread: Some(thread),
        })
    }

    /// Captures screen `screen` (see [`screen_rect`]) and encodes it at
    /// `config`'s size, all on the GPU: no frame passes through system
    /// memory, and frames are taken as soon as the desktop changes.
    /// [`HardwareEncoder::push`] is not used with such an encoder.
    #[cfg(windows)]
    pub fn start_screen(
        screen: Option<u32>,
        config: Config,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self, Error> {
        if config.width == 0
            || config.height == 0
            || !config.width.is_multiple_of(2)
            || !config.height.is_multiple_of(2)
        {
            return Err(Error::InvalidSize(config.width, config.height));
        }
        // Nothing is sent on this channel; dropping it stops the thread.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let control = Arc::new(Control::default());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = {
            let control = control.clone();
            std::thread::Builder::new()
                .name("gpu-screen".into())
                .spawn(move || gpu::run(screen, config, rx, control, ready_tx, on_frame))
                .map_err(|e| Error::Platform(e.to_string()))?
        };
        let name = match ready_rx.recv() {
            Ok(Ok(name)) => name,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => return Err(Error::Platform("capture thread exited".into())),
        };
        Ok(Self {
            name,
            input: None,
            stop: Some(tx),
            control,
            thread: Some(thread),
        })
    }

    #[cfg(not(windows))]
    pub fn start_screen(
        _screen: Option<u32>,
        _config: Config,
        _on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self, Error> {
        Err(Error::Unsupported)
    }

    #[cfg(not(windows))]
    pub fn start(
        _config: Config,
        _on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self, Error> {
        Err(Error::Unsupported)
    }

    /// The encoder's friendly name, e.g. "NVIDIA H.264 Encoder MFT".
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Queues an NV12 frame (`width * height * 3 / 2` bytes, stride = width).
    /// Returns false if the encoder was still busy and the frame was dropped.
    pub fn push(&self, nv12: Vec<u8>, timestamp_us: i64) -> bool {
        let Some(input) = &self.input else {
            return false;
        };
        match input.try_send(InputFrame { nv12, timestamp_us }) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.control.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// The next encoded frame will be a keyframe (e.g. after a PLI).
    pub fn request_keyframe(&self) {
        self.control.keyframe.store(true, Ordering::Relaxed);
    }

    pub fn set_bitrate(&self, bitrate_bps: u32) {
        self.control
            .bitrate_bps
            .store(bitrate_bps.max(1), Ordering::Relaxed);
    }

    /// Frames dropped because the encoder was busy.
    pub fn dropped_frames(&self) -> u64 {
        self.control.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for HardwareEncoder {
    fn drop(&mut self) {
        // Closing the channel ends the encoder thread.
        self.input = None;
        self.stop = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
