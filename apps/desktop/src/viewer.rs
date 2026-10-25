//! Renders a remote screen share into a native window.
//!
//! Frames never pass through the WebView: the decoded frame is scaled and
//! converted to RGB with libyuv and blitted with softbuffer. Only the newest
//! frame is kept, so a slow repaint drops frames instead of adding latency.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use livekit::prelude::RemoteVideoTrack;
use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::prelude::VideoBuffer;
use livekit::webrtc::video_stream::native::NativeVideoStream;
use tauri::AppHandle;
use tauri::window::{Window, WindowBuilder};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Closes its window when dropped.
pub struct Viewer {
    stop: Arc<AtomicBool>,
    window: Window,
}

impl Viewer {
    pub fn open(app: &AppHandle, title: &str, track: RemoteVideoTrack) -> Result<Self> {
        let label = format!("screen-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let window = WindowBuilder::new(app, label)
            .title(title)
            .inner_size(1280.0, 720.0)
            .build()
            .context("cannot create viewer window")?;
        let stop = Arc::new(AtomicBool::new(false));
        {
            let window = window.clone();
            let stop = stop.clone();
            let rt = tokio::runtime::Handle::current();
            std::thread::Builder::new()
                .name("screen-viewer".into())
                .spawn(move || {
                    if let Err(e) = render_loop(&window, track, &stop, &rt) {
                        tracing::debug!("viewer stopped: {e:#}");
                    }
                })?;
        }
        Ok(Self { stop, window })
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.window.close();
    }
}

fn render_loop(
    window: &Window,
    track: RemoteVideoTrack,
    stop: &AtomicBool,
    rt: &tokio::runtime::Handle,
) -> Result<()> {
    let context = softbuffer::Context::new(window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut surface =
        softbuffer::Surface::new(&context, window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
    // The default queue holds one frame: always render the newest decoded one.
    let mut stream = NativeVideoStream::new(track.rtc_track());
    let mut scratch: Vec<u32> = Vec::new();

    while let Some(frame) = rt.block_on(stream.next()) {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Fails once the user closed the window.
        let size = window.inner_size()?;
        let (Some(win_w), Some(win_h)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            continue; // minimized
        };
        let mut i420 = frame.buffer.to_i420();
        let (fit_w, fit_h) = fit((i420.width(), i420.height()), (win_w.get(), win_h.get()));
        if fit_w == 0 || fit_h == 0 {
            continue;
        }
        let scaled = if (fit_w, fit_h) == (i420.width(), i420.height()) {
            i420
        } else {
            i420.scale(fit_w as i32, fit_h as i32)
        };

        scratch.resize((fit_w * fit_h) as usize, 0);
        let (sy, su, sv) = scaled.strides();
        let (y, u, v) = scaled.data();
        // libyuv "ARGB" is 0xAARRGGBB per u32, which softbuffer expects.
        yuv_helper::i420_to_argb(
            y,
            sy,
            u,
            su,
            v,
            sv,
            bytemuck::cast_slice_mut(&mut scratch),
            fit_w * 4,
            fit_w as i32,
            fit_h as i32,
        );

        surface
            .resize(win_w, win_h)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut buffer = surface.buffer_mut().map_err(|e| anyhow::anyhow!("{e}"))?;
        buffer.fill(0);
        let (x0, y0) = (
            ((win_w.get() - fit_w) / 2) as usize,
            ((win_h.get() - fit_h) / 2) as usize,
        );
        let stride = win_w.get() as usize;
        for (row, src) in scratch.chunks_exact(fit_w as usize).enumerate() {
            let start = (y0 + row) * stride + x0;
            buffer[start..start + src.len()].copy_from_slice(src);
        }
        buffer.present().map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(())
}

/// Largest size with the frame's aspect ratio that fits into the window.
pub(crate) fn fit((fw, fh): (u32, u32), (ww, wh): (u32, u32)) -> (u32, u32) {
    if fw == 0 || fh == 0 {
        return (0, 0);
    }
    let (fw64, fh64, ww64, wh64) = (fw as u64, fh as u64, ww as u64, wh as u64);
    if fw64 * wh64 > ww64 * fh64 {
        // Window is taller than the frame: full width, letterbox top/bottom.
        (ww, (fh64 * ww64 / fw64) as u32)
    } else {
        ((fw64 * wh64 / fh64) as u32, wh)
    }
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn fit_keeps_aspect_ratio() {
        assert_eq!(fit((1920, 1080), (1920, 1080)), (1920, 1080));
        assert_eq!(fit((1920, 1080), (1280, 1024)), (1280, 720));
        assert_eq!(fit((1920, 1080), (800, 300)), (533, 300));
        assert_eq!(fit((0, 0), (800, 300)), (0, 0));
    }
}
