//! Native screen capture and publishing.
//!
//! libwebrtc's desktop capturer uses Windows Graphics Capture (falling back
//! to DXGI desktop duplication) and delivers BGRA frames.
//!
//! The LiveKit SDK has no hardware encoder on Windows (its NVENC support is
//! Linux only). When the GPU offers a Media Foundation H.264 encoder (NVENC,
//! AMF, Quick Sync), frames are converted to NV12, encoded by
//! `tinlink-hwenc` and handed to the SDK pre-encoded. Otherwise they are
//! converted to I420 and WebRTC encodes them in software (OpenH264) with a
//! smaller size and frame rate.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use livekit::options::{TrackPublishOptions, VideoCodec, VideoEncoderBackend, VideoEncoding};
use livekit::prelude::*;
use livekit::webrtc::desktop_capturer::{
    CaptureError, DesktopCaptureSourceType, DesktopCapturer, DesktopCapturerOptions, DesktopFrame,
};
use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::prelude::{
    I420Buffer, RtcVideoSource, VideoFrame, VideoResolution, VideoRotation,
};
use livekit::webrtc::rtp_parameters::DegradationPreference;
use livekit::webrtc::video_frame::{EncodedFrameType, EncodedVideoCodec, EncodedVideoFrame};
use livekit::webrtc::video_source::native::NativeVideoSource;
use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tinlink_hwenc::HardwareEncoder;
use tokio::sync::oneshot;

use crate::border::ShareBorder;
use crate::viewer::fit;

/// What the user picked for a share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareSettings {
    /// Scale down to at most this height (keeping the aspect ratio); `None`
    /// keeps the native resolution. Never scales up.
    pub max_height: Option<u32>,
    pub fps: u32,
}

/// Software H.264 (OpenH264) cannot keep up beyond this.
const SOFTWARE_MAX_HEIGHT: u32 = 1080;
const SOFTWARE_MAX_FPS: u32 = 30;
/// Bits per pixel and frame; screen content compresses well.
const BITS_PER_PIXEL: f64 = 0.06;
const MIN_BITRATE: u32 = 1_500_000;
const MAX_BITRATE: u32 = 20_000_000;

/// Size, frame rate and bitrate a share actually runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Plan {
    size: (u32, u32),
    fps: u32,
    bitrate: u32,
}

/// Applies the user's choice and the encoder's limits to the screen size.
fn plan(input: (u32, u32), settings: ShareSettings, hardware: bool) -> Plan {
    let mut max_height = settings.max_height.unwrap_or(u32::MAX);
    let mut fps = settings.fps.clamp(1, 60);
    if !hardware {
        max_height = max_height.min(SOFTWARE_MAX_HEIGHT);
        fps = fps.min(SOFTWARE_MAX_FPS);
    }
    let size = output_size(input, max_height);
    let bits = f64::from(size.0) * f64::from(size.1) * f64::from(fps) * BITS_PER_PIXEL;
    let bitrate = (bits as u32).clamp(MIN_BITRATE, MAX_BITRATE);
    Plan { size, fps, bitrate }
}

#[derive(Serialize)]
pub struct ScreenSource {
    pub id: u64,
    pub title: String,
}

/// What the share runs with, shown in the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareInfo {
    /// Hardware encoder name, `None` for software encoding.
    pub encoder: Option<String>,
    /// Captured area on the desktop (x, y, width, height in physical pixels).
    pub area: (i32, i32, u32, u32),
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
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
    info: ShareInfo,
    border: Option<ShareBorder>,
}

/// Sent by the capture thread once the first frame decided size and encoder.
type Ready = Result<(NativeVideoSource, ShareInfo)>;

impl ScreenShare {
    pub async fn start(
        app: &AppHandle,
        room: &Room,
        source_id: Option<u64>,
        settings: ShareSettings,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = oneshot::channel();
        let thread = {
            let stop = stop.clone();
            let rt = tokio::runtime::Handle::current();
            std::thread::Builder::new()
                .name("screen-capture".into())
                .spawn(move || {
                    // The video source spawns a keepalive task on creation.
                    let _rt = rt.enter();
                    capture_thread(source_id, settings, stop, ready_tx)
                })?
        };
        let (source, info) = match tokio::time::timeout(Duration::from_secs(5), ready_rx).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => return Err(anyhow!("screen capture stopped unexpectedly")),
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                return Err(anyhow!("screen capture produced no frame within 5 s"));
            }
        };
        tracing::info!(?info, "publishing screen share");

        let track = LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(source));
        let published = room
            .local_participant()
            .publish_track(
                LocalTrack::Video(track.clone()),
                TrackPublishOptions {
                    source: TrackSource::Screenshare,
                    video_codec: VideoCodec::H264,
                    video_encoder: if info.encoder.is_some() {
                        VideoEncoderBackend::PreEncoded
                    } else {
                        VideoEncoderBackend::Auto
                    },
                    video_encoding: Some(VideoEncoding {
                        max_bitrate: u64::from(info.bitrate_bps),
                        max_framerate: f64::from(info.fps),
                    }),
                    // One full-quality layer; simulcast comes with more viewers.
                    simulcast: false,
                    // Text must stay sharp: drop frames before resolution.
                    degradation_preference: Some(DegradationPreference::MaintainResolution),
                    ..Default::default()
                },
            )
            .await;
        let mut share = Self {
            stop,
            thread: Some(thread),
            track,
            info,
            border: None,
        };
        if let Err(e) = published {
            share.stop(room).await;
            return Err(e).context("cannot publish screen share");
        }
        match ShareBorder::show(app, share.info.area) {
            Ok(border) => share.border = Some(border),
            Err(e) => tracing::warn!("cannot show share border: {e:#}"),
        }
        Ok(share)
    }

    pub fn info(&self) -> &ShareInfo {
        &self.info
    }

    pub async fn stop(mut self, room: &Room) {
        self.border = None;
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

/// Owns the capturer (it must stay on one thread) and paces capture at the
/// frame rate of the plan.
fn capture_thread(
    source_id: Option<u64>,
    settings: ShareSettings,
    stop: Arc<AtomicBool>,
    ready_tx: oneshot::Sender<Ready>,
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

    // Until the first frame decided the plan, capture at the chosen rate.
    let fps = Arc::new(AtomicU32::new(settings.fps.max(1)));
    let mut sink = FrameSink {
        settings,
        output: None,
        ready_tx: Some(ready_tx),
        epoch: Instant::now(),
        stop: stop.clone(),
        fps: fps.clone(),
    };
    capturer.start_capture(Some(source), move |result| sink.on_frame(result));

    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let interval = Duration::from_secs(1) / fps.load(Ordering::Relaxed).max(1);
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

/// Where converted frames go, decided on the first frame.
enum Output {
    Hardware {
        encoder: HardwareEncoder,
        source: NativeVideoSource,
        size: (u32, u32),
        max_bitrate: u32,
    },
    Software {
        source: NativeVideoSource,
        size: (u32, u32),
    },
}

struct FrameSink {
    settings: ShareSettings,
    output: Option<Output>,
    ready_tx: Option<oneshot::Sender<Ready>>,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    fps: Arc<AtomicU32>,
}

/// Scales the frame down to `max_height` (keeping the aspect ratio, never
/// up) and rounds down to even dimensions (needed by the 4:2:0 formats).
fn output_size((width, height): (u32, u32), max_height: u32) -> (u32, u32) {
    let (w, h) = if height > max_height {
        fit((width, height), (u32::MAX, max_height))
    } else {
        (width, height)
    };
    (w & !1, h & !1)
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
        if width < 2 || height < 2 {
            return;
        }
        let input = (width as u32, height as u32);
        if self.output.is_none() {
            tracing::debug!(
                left = frame.left(),
                top = frame.top(),
                ?input,
                "first captured frame"
            );
            let area = (frame.left(), frame.top(), input.0, input.1);
            let output = self.choose_output(input, area);
            self.output = Some(output);
        }
        match self.output.as_ref() {
            Some(Output::Hardware {
                encoder,
                source,
                size,
                max_bitrate,
            }) => {
                if source.take_keyframe_request() {
                    encoder.request_keyframe();
                }
                if let Some(rate) = source.take_rate_control_request() {
                    let bps = rate.target_bitrate_bps.min(u64::from(*max_bitrate));
                    encoder.set_bitrate(bps as u32);
                }
                let nv12 = to_nv12(&frame, input, *size);
                encoder.push(nv12, self.epoch.elapsed().as_micros() as i64);
            }
            Some(Output::Software { source, size }) => {
                let buffer = to_i420(&frame, input, *size);
                source.capture_frame(&VideoFrame {
                    rotation: VideoRotation::VideoRotation0,
                    timestamp_us: self.epoch.elapsed().as_micros() as i64,
                    frame_metadata: None,
                    buffer,
                });
            }
            None => {}
        }
    }

    /// Starts the hardware encoder if there is one, else falls back to
    /// software encoding, and reports the result to [`ScreenShare::start`].
    fn choose_output(&mut self, input: (u32, u32), area: (i32, i32, u32, u32)) -> Output {
        let hw = plan(input, self.settings, true);
        let size = hw.size;
        let resolution = VideoResolution {
            width: size.0,
            height: size.1,
        };
        let source = NativeVideoSource::new_encoded(resolution.clone());
        let sink = source.clone();
        let config = tinlink_hwenc::Config {
            width: size.0,
            height: size.1,
            fps: hw.fps,
            bitrate_bps: hw.bitrate,
        };
        let started = HardwareEncoder::start(config, move |frame| {
            sink.capture_encoded_frame(&EncodedVideoFrame {
                codec: EncodedVideoCodec::H264,
                payload: &frame.data,
                // 0: stamped by the SDK when the encoded frame arrives.
                timestamp_us: 0,
                frame_type: if frame.keyframe {
                    EncodedFrameType::Key
                } else {
                    EncodedFrameType::Delta
                },
                resolution: resolution.clone(),
                frame_metadata: None,
            });
        });
        let (output, info) = match started {
            Ok(encoder) => {
                self.fps.store(hw.fps, Ordering::Relaxed);
                let info = ShareInfo {
                    encoder: Some(encoder.name().to_owned()),
                    area,
                    width: size.0,
                    height: size.1,
                    fps: hw.fps,
                    bitrate_bps: hw.bitrate,
                };
                (
                    Output::Hardware {
                        encoder,
                        source,
                        size,
                        max_bitrate: hw.bitrate,
                    },
                    info,
                )
            }
            Err(e) => {
                tracing::info!("no hardware encoding ({e}), using software H.264");
                let sw = plan(input, self.settings, false);
                let size = sw.size;
                let source = NativeVideoSource::new(
                    VideoResolution {
                        width: size.0,
                        height: size.1,
                    },
                    true,
                );
                self.fps.store(sw.fps, Ordering::Relaxed);
                let info = ShareInfo {
                    encoder: None,
                    area,
                    width: size.0,
                    height: size.1,
                    fps: sw.fps,
                    bitrate_bps: sw.bitrate,
                };
                (Output::Software { source, size }, info)
            }
        };
        if let Some(tx) = self.ready_tx.take() {
            let source = match &output {
                Output::Hardware { source, .. } | Output::Software { source, .. } => source.clone(),
            };
            let _ = tx.send(Ok((source, info)));
        }
        output
    }
}

/// Converts a BGRA frame to I420 of `size`, scaling if needed.
fn to_i420(frame: &DesktopFrame, (width, height): (u32, u32), size: (u32, u32)) -> I420Buffer {
    // WebRTC keeps a reference to the buffer, so each frame needs its own.
    let mut buffer = I420Buffer::new(width, height);
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
        width as i32,
        height as i32,
    );
    if (width, height) == size {
        buffer
    } else {
        buffer.scale(size.0 as i32, size.1 as i32)
    }
}

/// Converts a BGRA frame to tightly packed NV12 of `size`, scaling if needed.
fn to_nv12(frame: &DesktopFrame, input: (u32, u32), (width, height): (u32, u32)) -> Vec<u8> {
    let luma = (width * height) as usize;
    let mut nv12 = vec![0u8; luma + luma / 2];
    let (y, uv) = nv12.split_at_mut(luma);
    if input == (width, height) {
        yuv_helper::argb_to_nv12(
            frame.data(),
            frame.stride(),
            y,
            width,
            uv,
            width,
            width as i32,
            height as i32,
        );
    } else {
        let i420 = to_i420(frame, input, (width, height));
        let (sy, su, sv) = i420.strides();
        let (src_y, src_u, src_v) = i420.data();
        yuv_helper::i420_to_nv12(
            src_y,
            sy,
            src_u,
            su,
            src_v,
            sv,
            y,
            width,
            uv,
            width,
            width as i32,
            height as i32,
        );
    }
    nv12
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_size_scales_down_by_height_only() {
        assert_eq!(output_size((3840, 2160), 1440), (2560, 1440));
        assert_eq!(
            output_size((1920, 1080), 1440),
            (1920, 1080),
            "never upscale"
        );
        assert_eq!(
            output_size((1365, 767), u32::MAX),
            (1364, 766),
            "even sizes"
        );
        // Ultrawide keeps its shape.
        assert_eq!(output_size((5120, 1440), 1080), (3840, 1080));
    }

    #[test]
    fn plan_applies_choice_and_encoder_limits() {
        let native60 = ShareSettings {
            max_height: None,
            fps: 60,
        };
        let hw = plan((3840, 2160), native60, true);
        assert_eq!((hw.size, hw.fps), ((3840, 2160), 60));
        assert_eq!(hw.bitrate, MAX_BITRATE, "4K60 hits the cap");
        // Software encoding is limited to 1080p30.
        let sw = plan((3840, 2160), native60, false);
        assert_eq!((sw.size, sw.fps), ((1920, 1080), 30));

        let small = ShareSettings {
            max_height: Some(720),
            fps: 15,
        };
        let p720 = plan((2560, 1440), small, true);
        assert_eq!((p720.size, p720.fps), ((1280, 720), 15));
        assert_eq!(p720.bitrate, MIN_BITRATE, "small and slow hits the floor");

        let p1440 = plan(
            (3840, 2160),
            ShareSettings {
                max_height: Some(1440),
                fps: 60,
            },
            true,
        );
        assert_eq!(p1440.bitrate, 13_271_040);
    }
}
