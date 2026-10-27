//! Publishes the webcam: `tinlink-webcam` captures NV12, which is converted
//! to I420 and encoded by WebRTC (VP8 with simulcast, so small grid tiles
//! get a small layer).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use livekit::options::{TrackPublishOptions, VideoCodec, VideoEncoding};
use livekit::prelude::*;
use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::prelude::{
    I420Buffer, RtcVideoSource, VideoFrame, VideoResolution, VideoRotation,
};
use livekit::webrtc::video_source::native::NativeVideoSource;
use serde::{Deserialize, Serialize};
use tinlink_webcam::{Camera, Frame, Request};
use tokio::sync::oneshot;

/// A camera for the device selection.
#[derive(Debug, Clone, Serialize)]
pub struct CameraSource {
    pub id: String,
    pub name: String,
}

pub fn list_sources() -> Vec<CameraSource> {
    match tinlink_webcam::list() {
        Ok(cameras) => cameras
            .into_iter()
            .map(|c| CameraSource {
                id: c.id,
                name: c.name,
            })
            .collect(),
        Err(e) => {
            tracing::debug!("cannot list cameras: {e}");
            Vec::new()
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraSettings {
    /// 720, 540 or 360.
    pub height: u32,
}

/// Bitrate of the top simulcast layer for a camera of `height` at 30 fps.
fn bitrate(height: u32) -> u64 {
    match height {
        h if h >= 720 => 1_700_000,
        h if h >= 540 => 1_000_000,
        _ => 500_000,
    }
}

pub struct CameraShare {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    track: LocalVideoTrack,
}

type Ready = Result<(NativeVideoSource, String)>;

impl CameraShare {
    pub async fn start(
        room: &Room,
        device: Option<String>,
        settings: CameraSettings,
    ) -> Result<Self> {
        let height = settings.height.clamp(360, 720);
        let request = Request {
            width: height * 16 / 9,
            height,
            fps: 30,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = oneshot::channel();
        let thread = {
            let stop = stop.clone();
            let rt = tokio::runtime::Handle::current();
            std::thread::Builder::new()
                .name("camera-capture".into())
                .spawn(move || {
                    // The video source spawns a keepalive task on creation.
                    let _rt = rt.enter();
                    capture_thread(device, request, stop, ready_tx)
                })?
        };
        // Opening a camera can take a few seconds (USB, privacy prompt).
        let (source, name) = match tokio::time::timeout(Duration::from_secs(10), ready_rx).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => return Err(anyhow!("camera stopped unexpectedly")),
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                return Err(anyhow!("the camera delivered no picture within 10 s"));
            }
        };
        tracing::info!(camera = %name, height, "publishing camera");

        let track = LocalVideoTrack::create_video_track("camera", RtcVideoSource::Native(source));
        let published = room
            .local_participant()
            .publish_track(
                LocalTrack::Video(track.clone()),
                TrackPublishOptions {
                    source: TrackSource::Camera,
                    video_codec: VideoCodec::VP8,
                    video_encoding: Some(VideoEncoding {
                        max_bitrate: bitrate(height),
                        max_framerate: 30.0,
                    }),
                    simulcast: true,
                    ..Default::default()
                },
            )
            .await;
        let mut share = Self {
            stop,
            thread: Some(thread),
            track,
        };
        if let Err(e) = published {
            share.shutdown(room).await;
            return Err(e).context("cannot publish the camera");
        }
        Ok(share)
    }

    pub fn track(&self) -> &LocalVideoTrack {
        &self.track
    }

    pub async fn stop(mut self, room: &Room) {
        self.shutdown(room).await;
    }

    async fn shutdown(&mut self, room: &Room) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
        if let Err(e) = room
            .local_participant()
            .unpublish_track(&self.track.sid())
            .await
        {
            tracing::debug!("unpublish camera: {e}");
        }
    }
}

/// Owns the camera (it must stay on one thread) and forwards every frame.
fn capture_thread(
    device: Option<String>,
    request: Request,
    stop: Arc<AtomicBool>,
    ready_tx: oneshot::Sender<Ready>,
) {
    let mut camera = match Camera::open(device.as_deref(), request) {
        Ok(camera) => camera,
        Err(e) => {
            let _ = ready_tx.send(Err(anyhow!("cannot open the camera: {e}")));
            return;
        }
    };
    let mut ready_tx = Some(ready_tx);
    let mut source: Option<NativeVideoSource> = None;
    let epoch = std::time::Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let frame = match camera.read() {
            Ok(Some(frame)) => frame,
            Ok(None) => continue,
            Err(e) => {
                tracing::error!("camera failed: {e}");
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(anyhow!("camera failed: {e}")));
                }
                break;
            }
        };
        let source = source.get_or_insert_with(|| {
            let source = NativeVideoSource::new(
                VideoResolution {
                    width: frame.width,
                    height: frame.height,
                },
                false,
            );
            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(Ok((source.clone(), camera.name().to_owned())));
            }
            source
        });
        source.capture_frame(&VideoFrame {
            rotation: VideoRotation::VideoRotation0,
            timestamp_us: epoch.elapsed().as_micros() as i64,
            frame_metadata: None,
            buffer: to_i420(&frame),
        });
    }
    tracing::info!("camera stopped");
}

fn to_i420(frame: &Frame) -> I420Buffer {
    // Even sizes for 4:2:0; WebRTC keeps a reference, so one buffer per frame.
    let (width, height) = (frame.width & !1, frame.height & !1);
    let mut buffer = I420Buffer::new(width, height);
    let (stride_y, stride_u, stride_v) = buffer.strides();
    let (y, u, v) = buffer.data_mut();
    yuv_helper::nv12_to_i420(
        frame.y(),
        frame.stride,
        frame.uv(),
        frame.stride,
        y,
        stride_y,
        u,
        stride_u,
        v,
        stride_v,
        width as i32,
        height as i32,
    );
    buffer
}

#[cfg(test)]
mod tests {
    use super::bitrate;

    #[test]
    fn bitrate_follows_quality() {
        assert_eq!(bitrate(720), 1_700_000);
        assert_eq!(bitrate(540), 1_000_000);
        assert_eq!(bitrate(360), 500_000);
    }
}
