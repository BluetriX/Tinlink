//! Native screen capture and publishing.
//!
//! libwebrtc's desktop capturer uses Windows Graphics Capture (falling back
//! to DXGI desktop duplication). Frames arrive as BGRA, are converted to
//! I420 with libyuv and handed to WebRTC, which encodes them with the
//! hardware H.264 encoder when one is available (NVENC, AMF, Quick Sync).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use livekit::options::{TrackPublishOptions, VideoCodec, VideoEncoding};
use livekit::prelude::*;
use livekit::webrtc::desktop_capturer::{
    CaptureError, DesktopCaptureSourceType, DesktopCapturer, DesktopCapturerOptions, DesktopFrame,
};
use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::prelude::{
    I420Buffer, RtcVideoSource, VideoFrame, VideoResolution, VideoRotation,
};
use livekit::webrtc::rtp_parameters::DegradationPreference;
use livekit::webrtc::video_source::native::NativeVideoSource;
use serde::Serialize;
use tokio::sync::oneshot;

const FPS: u32 = 30;
const MAX_BITRATE: u64 = 8_000_000;

#[derive(Serialize)]
pub struct ScreenSource {
    pub id: u64,
    pub title: String,
}

fn capturer() -> Option<DesktopCapturer> {
    let mut options = DesktopCapturerOptions::new(DesktopCaptureSourceType::Screen);
    options.set_include_cursor(true);
    DesktopCapturer::new(options)
}

/// Lists the monitors that can be shared.
pub fn list_sources() -> Vec<ScreenSource> {
    let Some(capturer) = capturer() else {
        return Vec::new();
    };
    capturer
        .get_source_list()
        .into_iter()
        .enumerate()
        .map(|(i, s)| ScreenSource {
            id: s.id(),
            title: if s.title().is_empty() {
                format!("Screen {}", i + 1)
            } else {
                s.title()
            },
        })
        .collect()
}

pub struct ScreenShare {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    track: LocalVideoTrack,
}

impl ScreenShare {
    pub async fn start(room: &Room, source_id: Option<u64>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = oneshot::channel();
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("screen-capture".into())
                .spawn(move || capture_thread(source_id, stop, ready_tx))?
        };
        let source = match tokio::time::timeout(Duration::from_secs(5), ready_rx).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => return Err(anyhow!("screen capture stopped unexpectedly")),
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                return Err(anyhow!("screen capture produced no frame within 5 s"));
            }
        };

        let track = LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(source));
        let published = room
            .local_participant()
            .publish_track(
                LocalTrack::Video(track.clone()),
                TrackPublishOptions {
                    source: TrackSource::Screenshare,
                    video_codec: VideoCodec::H264,
                    video_encoding: Some(VideoEncoding {
                        max_bitrate: MAX_BITRATE,
                        max_framerate: FPS as f64,
                    }),
                    // One full-quality layer; simulcast comes with more viewers.
                    simulcast: false,
                    // Text must stay sharp: drop frames before resolution.
                    degradation_preference: Some(DegradationPreference::MaintainResolution),
                    ..Default::default()
                },
            )
            .await;
        let share = Self {
            stop,
            thread: Some(thread),
            track,
        };
        if let Err(e) = published {
            share.stop(room).await;
            return Err(e).context("cannot publish screen share");
        }
        Ok(share)
    }

    pub async fn stop(mut self, room: &Room) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
        let local = room.local_participant();
        if let Err(e) = local.unpublish_track(&self.track.sid()).await {
            tracing::debug!("unpublish screen share: {e}");
        }
    }
}

/// Owns the capturer (it must stay on one thread) and paces capture at [`FPS`].
fn capture_thread(
    source_id: Option<u64>,
    stop: Arc<AtomicBool>,
    ready_tx: oneshot::Sender<Result<NativeVideoSource>>,
) {
    let Some(mut capturer) = capturer() else {
        let _ = ready_tx.send(Err(anyhow!("screen capture is not available")));
        return;
    };
    let sources = capturer.get_source_list();
    let source = match source_id {
        Some(id) => sources.into_iter().find(|s| s.id() == id),
        None => sources.into_iter().next(),
    };
    let Some(source) = source else {
        let _ = ready_tx.send(Err(anyhow!("screen not found")));
        return;
    };
    tracing::info!(%source, "starting screen capture");

    let mut sink = FrameSink {
        source: None,
        ready_tx: Some(ready_tx),
        epoch: Instant::now(),
        stop: stop.clone(),
    };
    capturer.start_capture(Some(source), move |result| sink.on_frame(result));

    let interval = Duration::from_secs(1) / FPS;
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        // The callback runs synchronously inside capture_frame().
        let started = Instant::now();
        capturer.capture_frame();
        let took = started.elapsed();
        if took > interval {
            tracing::debug!(?took, "screen capture slower than frame interval");
        }
        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    tracing::info!("screen capture stopped");
}

struct FrameSink {
    /// Created from the first frame, which tells us the resolution.
    source: Option<NativeVideoSource>,
    ready_tx: Option<oneshot::Sender<Result<NativeVideoSource>>>,
    epoch: Instant,
    stop: Arc<AtomicBool>,
}

impl FrameSink {
    fn on_frame(&mut self, result: Result<DesktopFrame, CaptureError>) {
        let frame = match result {
            Ok(frame) => frame,
            // No new frame (nothing changed on screen) or a transient error.
            Err(CaptureError::Temporary) => return,
            Err(CaptureError::Permanent) => {
                tracing::error!("screen capture failed permanently");
                if let Some(tx) = self.ready_tx.take() {
                    let _ = tx.send(Err(anyhow!("screen capture failed")));
                }
                self.stop.store(true, Ordering::Relaxed);
                return;
            }
        };
        let (width, height) = (frame.width(), frame.height());
        if width <= 0 || height <= 0 {
            return;
        }
        let source = self.source.get_or_insert_with(|| {
            let source = NativeVideoSource::new(
                VideoResolution {
                    width: width as u32,
                    height: height as u32,
                },
                true,
            );
            if let Some(tx) = self.ready_tx.take() {
                let _ = tx.send(Ok(source.clone()));
            }
            source
        });

        // WebRTC keeps a reference to the buffer, so each frame needs its own.
        let mut buffer = I420Buffer::new(width as u32, height as u32);
        let (stride_y, stride_u, stride_v) = buffer.strides();
        let (y, u, v) = buffer.data_mut();
        // libyuv "ARGB" is BGRA in memory, which is what the capturer delivers.
        yuv_helper::argb_to_i420(
            frame.data(),
            frame.stride(),
            y,
            stride_y,
            u,
            stride_u,
            v,
            stride_v,
            width,
            height,
        );
        let video_frame = VideoFrame {
            rotation: VideoRotation::VideoRotation0,
            timestamp_us: self.epoch.elapsed().as_micros() as i64,
            frame_metadata: None,
            buffer,
        };
        source.capture_frame(&video_frame);
    }
}
