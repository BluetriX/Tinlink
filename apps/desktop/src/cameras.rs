//! Shows every webcam of the room in one native grid window.
//!
//! Like the screen viewer, frames never pass through the WebView. One task
//! per camera scales its newest frame to the size of its grid cell and
//! converts it to RGB; a render thread composes the cells and blits them with
//! softbuffer. The speaker's cell gets a green frame.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use livekit::prelude::*;
use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::prelude::{RtcVideoTrack, VideoBuffer};
use livekit::webrtc::video_stream::native::NativeVideoStream;
use tauri::AppHandle;
use tauri::window::{Window, WindowBuilder};
use tokio::task::JoinHandle;

use crate::viewer::{fit, input_loop};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Space between cells and around the grid, in pixels.
const GAP: u32 = 6;
const SPEAKING: u32 = 0x0043_b581;
const BORDER: u32 = 3;
/// Repaint at least this often, so the speaker frame follows without new video.
const REPAINT: Duration = Duration::from_millis(100);

/// Columns and rows for `count` cells: as square as possible, wider than tall.
pub(crate) fn grid(count: usize) -> (u32, u32) {
    let count = count.max(1) as u32;
    let cols = count.isqrt() + u32::from(count.isqrt().pow(2) < count);
    (cols, count.div_ceil(cols))
}

/// The cell rectangles `(x, y, width, height)` for `count` cells in a window.
pub(crate) fn cells(count: usize, (width, height): (u32, u32)) -> Vec<(u32, u32, u32, u32)> {
    let (cols, rows) = grid(count);
    let cell_w = width.saturating_sub(GAP * (cols + 1)) / cols;
    let cell_h = height.saturating_sub(GAP * (rows + 1)) / rows;
    // Rows that are not full are centred.
    (0..count as u32)
        .map(|i| {
            let (row, col) = (i / cols, i % cols);
            let in_row = (count as u32 - row * cols).min(cols);
            let x0 = (width.saturating_sub(in_row * cell_w + (in_row - 1) * GAP)) / 2;
            let y0 = (height.saturating_sub(rows * cell_h + (rows - 1) * GAP)) / 2;
            (
                x0 + col * (cell_w + GAP),
                y0 + row * (cell_h + GAP),
                cell_w,
                cell_h,
            )
        })
        .collect()
}

struct Image {
    argb: Vec<u32>,
    width: u32,
    height: u32,
}

struct Tile {
    sid: TrackSid,
    identity: String,
    name: String,
    /// The cell size the image should have, set by the render thread.
    target: (u32, u32),
    image: Option<Image>,
    feeder: JoinHandle<()>,
}

#[derive(Default)]
struct Shared {
    tiles: Mutex<Vec<Tile>>,
    changed: Condvar,
}

struct Open {
    window: Window,
    stop: Arc<AtomicBool>,
}

/// The camera grid of one room; dropping it closes the window.
pub struct CameraGrid {
    app: AppHandle,
    room: Arc<Room>,
    shared: Arc<Shared>,
    open: Mutex<Option<Open>>,
}

impl CameraGrid {
    pub fn new(app: AppHandle, room: Arc<Room>) -> Self {
        Self {
            app,
            room,
            shared: Arc::default(),
            open: Mutex::new(None),
        }
    }

    /// Adds a camera and opens the window if it is not open.
    pub fn add(
        &self,
        sid: TrackSid,
        identity: String,
        name: String,
        track: RtcVideoTrack,
        mirror: bool,
    ) {
        {
            let mut tiles = self.shared.tiles.lock().unwrap();
            if tiles.iter().any(|t| t.sid == sid) {
                return;
            }
            let feeder = tokio::spawn(feed(self.shared.clone(), sid.clone(), track, mirror));
            let tile = Tile {
                sid,
                identity,
                name,
                target: (0, 0),
                image: None,
                feeder,
            };
            // Own camera first, the others in the order they started.
            if mirror {
                tiles.insert(0, tile);
            } else {
                tiles.push(tile);
            }
        }
        self.shared.changed.notify_all();
        if let Err(e) = self.show() {
            tracing::error!("cannot open camera window: {e:#}");
        }
        self.update_title();
    }

    pub fn remove(&self, sid: &TrackSid) {
        let empty = {
            let mut tiles = self.shared.tiles.lock().unwrap();
            tiles.retain(|t| {
                let keep = &t.sid != sid;
                if !keep {
                    t.feeder.abort();
                }
                keep
            });
            tiles.is_empty()
        };
        self.shared.changed.notify_all();
        if empty {
            self.close();
        } else {
            self.update_title();
        }
    }

    pub fn has_cameras(&self) -> bool {
        !self.shared.tiles.lock().unwrap().is_empty()
    }

    /// Opens the window again, or brings it to the front.
    pub fn show(&self) -> Result<()> {
        if !self.has_cameras() {
            anyhow::bail!("nobody has a camera on");
        }
        let mut open = self.open.lock().unwrap();
        if let Some(o) = open.as_ref()
            && o.window.is_visible().is_ok()
            && !o.stop.load(Ordering::Relaxed)
        {
            let _ = o.window.unminimize();
            let _ = o.window.set_focus();
            return Ok(());
        }
        let label = format!("cameras-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let window = WindowBuilder::new(&self.app, label)
            .title("Cameras")
            .inner_size(960.0, 560.0)
            .build()
            .context("cannot create camera window")?;
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (window, stop, shared, room) = (
                window.clone(),
                stop.clone(),
                self.shared.clone(),
                self.room.clone(),
            );
            std::thread::Builder::new()
                .name("camera-grid".into())
                .spawn(move || {
                    if let Err(e) = render_loop(&window, &shared, &room, &stop) {
                        tracing::debug!("camera grid stopped: {e:#}");
                    }
                    // Closed by the user: stop converting frames for it.
                    stop.store(true, Ordering::Relaxed);
                    for tile in shared.tiles.lock().unwrap().iter_mut() {
                        tile.target = (0, 0);
                    }
                })?;
        }
        {
            let (window, stop) = (window.clone(), stop.clone());
            std::thread::Builder::new()
                .name("camera-grid-input".into())
                .spawn(move || input_loop(&window, &stop))?;
        }
        *open = Some(Open { window, stop });
        Ok(())
    }

    fn update_title(&self) {
        let names: Vec<String> = self
            .shared
            .tiles
            .lock()
            .unwrap()
            .iter()
            .map(|t| t.name.clone())
            .collect();
        if let Some(o) = self.open.lock().unwrap().as_ref() {
            let _ = o
                .window
                .set_title(&format!("Cameras - {}", names.join(", ")));
        }
    }

    fn close(&self) {
        if let Some(o) = self.open.lock().unwrap().take() {
            o.stop.store(true, Ordering::Relaxed);
            self.shared.changed.notify_all();
            let _ = o.window.close();
        }
    }
}

impl Drop for CameraGrid {
    fn drop(&mut self) {
        for tile in self.shared.tiles.lock().unwrap().drain(..) {
            tile.feeder.abort();
        }
        self.close();
    }
}

/// Converts each new frame of one camera to the size of its cell.
async fn feed(shared: Arc<Shared>, sid: TrackSid, track: RtcVideoTrack, mirror: bool) {
    // The default queue holds one frame: always convert the newest one.
    let mut stream = NativeVideoStream::new(track);
    while let Some(frame) = stream.next().await {
        let target = {
            let tiles = shared.tiles.lock().unwrap();
            match tiles.iter().find(|t| t.sid == sid) {
                Some(tile) => tile.target,
                None => return,
            }
        };
        let mut i420 = frame.buffer.to_i420();
        let (w, h) = fit((i420.width(), i420.height()), target);
        // Even sizes keep libyuv's 4:2:0 scaling exact.
        let (w, h) = (w & !1, h & !1);
        if w == 0 || h == 0 {
            continue; // window closed or not laid out yet
        }
        let scaled = if (w, h) == (i420.width(), i420.height()) {
            i420
        } else {
            i420.scale(w as i32, h as i32)
        };
        let mut argb = vec![0u32; (w * h) as usize];
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
            bytemuck::cast_slice_mut(&mut argb),
            w * 4,
            w as i32,
            h as i32,
        );
        if mirror {
            for row in argb.chunks_exact_mut(w as usize) {
                row.reverse();
            }
        }
        {
            let mut tiles = shared.tiles.lock().unwrap();
            if let Some(tile) = tiles.iter_mut().find(|t| t.sid == sid) {
                tile.image = Some(Image {
                    argb,
                    width: w,
                    height: h,
                });
            }
        }
        shared.changed.notify_all();
    }
}

fn is_speaking(room: &Room, identity: &str) -> bool {
    let local = room.local_participant();
    if local.identity().as_str() == identity {
        return local.is_speaking();
    }
    room.remote_participants()
        .into_values()
        .find(|p| p.identity().as_str() == identity)
        .is_some_and(|p| p.is_speaking())
}

fn render_loop(window: &Window, shared: &Shared, room: &Room, stop: &AtomicBool) -> Result<()> {
    let context = softbuffer::Context::new(window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut surface =
        softbuffer::Surface::new(&context, window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
    while !stop.load(Ordering::Relaxed) {
        // Fails once the user closed the window.
        let size = window.inner_size()?;
        let (Some(win_w), Some(win_h)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            std::thread::sleep(REPAINT); // minimized
            continue;
        };
        let speaking: Vec<bool> = {
            let tiles = shared.tiles.lock().unwrap();
            tiles
                .iter()
                .map(|t| is_speaking(room, &t.identity))
                .collect()
        };

        surface
            .resize(win_w, win_h)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut buffer = surface.buffer_mut().map_err(|e| anyhow::anyhow!("{e}"))?;
        buffer.fill(0x0011_1214);
        let stride = win_w.get() as usize;
        {
            let mut tiles = shared.tiles.lock().unwrap();
            let rects = cells(tiles.len(), (win_w.get(), win_h.get()));
            for ((tile, &(x, y, w, h)), &speaks) in tiles.iter_mut().zip(&rects).zip(&speaking) {
                tile.target = (w, h);
                fill(&mut buffer, stride, (x, y, w, h), 0);
                if let Some(image) = &tile.image {
                    // Centre the image in its cell (it lags one frame on resize).
                    let iw = image.width.min(w);
                    let ih = image.height.min(h);
                    let (x0, y0) = (x + (w - iw) / 2, y + (h - ih) / 2);
                    for row in 0..ih as usize {
                        let src = &image.argb[row * image.width as usize..][..iw as usize];
                        let start = (y0 as usize + row) * stride + x0 as usize;
                        buffer[start..start + src.len()].copy_from_slice(src);
                    }
                }
                if speaks {
                    frame(&mut buffer, stride, (x, y, w, h), SPEAKING);
                }
            }
        }
        buffer.present().map_err(|e| anyhow::anyhow!("{e}"))?;

        // Wait for a new frame from any camera, or repaint for the speaker frame.
        let tiles = shared.tiles.lock().unwrap();
        drop(shared.changed.wait_timeout(tiles, REPAINT).unwrap());
    }
    Ok(())
}

fn fill(buffer: &mut [u32], stride: usize, (x, y, w, h): (u32, u32, u32, u32), color: u32) {
    for row in y as usize..(y + h) as usize {
        let start = row * stride + x as usize;
        buffer[start..start + w as usize].fill(color);
    }
}

fn frame(buffer: &mut [u32], stride: usize, (x, y, w, h): (u32, u32, u32, u32), color: u32) {
    let b = BORDER.min(w / 2).min(h / 2);
    fill(buffer, stride, (x, y, w, b), color);
    fill(buffer, stride, (x, y + h - b, w, b), color);
    fill(buffer, stride, (x, y, b, h), color);
    fill(buffer, stride, (x + w - b, y, b, h), color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_stays_close_to_square() {
        assert_eq!(grid(0), (1, 1));
        assert_eq!(grid(1), (1, 1));
        assert_eq!(grid(2), (2, 1));
        assert_eq!(grid(3), (2, 2));
        assert_eq!(grid(4), (2, 2));
        assert_eq!(grid(5), (3, 2));
        assert_eq!(grid(10), (4, 3));
    }

    #[test]
    fn cells_fit_into_the_window() {
        let size = (966, 486);
        let rects = cells(3, size);
        assert_eq!(rects.len(), 3);
        // Two columns of (966 - 18) / 2 = 474, two rows of (486 - 18) / 2 = 234.
        assert_eq!(rects[0], (6, 6, 474, 234));
        assert_eq!(rects[1], (486, 6, 474, 234));
        // The single cell of the last row is centred.
        assert_eq!(rects[2], (246, 246, 474, 234));
        for (x, y, w, h) in rects {
            assert!(x + w <= size.0 && y + h <= size.1);
        }
    }
}
