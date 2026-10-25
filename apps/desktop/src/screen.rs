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
use serde::Serialize;
use tinlink_hwenc::HardwareEncoder;
use tokio::sync::oneshot;

use crate::viewer::fit;

/// Size, frame rate and bitrate cap of one way of encoding.
#[derive(Debug, Clone, Copy)]
struct Profile {
    /// Larger screens are scaled down to fit, keeping the aspect ratio.
    max_size: (u32, u32),
    fps: u32,
    max_bitrate: u32,
}

const HARDWARE: Profile = Profile {
    max_size: (2560, 1440),
    fps: 60,
    max_bitrate: 12_000_000,
};

const SOFTWARE: Profile = Profile {
    max_size: (1920, 1080),
    fps: 30,
    max_bitrate: 6_000_000,
};

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
    pub width: u32,
    pub height: u32,
    pub fps: u32,
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
}

/// Sent by the capture thread once the first frame decided size and encoder.
type Ready = Result<(NativeVideoSource, ShareInfo)>;

impl ScreenShare {
    pub async fn start(room: &Room, source_id: Option<u64>) -> Result<Self> {
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
                    capture_thread(source_id, stop, ready_tx)
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

        let profile = if info.encoder.is_some() {
            HARDWARE
        } else {
            SOFTWARE
        };
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
                        max_bitrate: u64::from(profile.max_bitrate),
                        max_framerate: f64::from(profile.fps),
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
            info,
        };
        if let Err(e) = published {
            share.stop(room).await;
            return Err(e).context("cannot publish screen share");
        }
        Ok(share)
    }

    pub fn info(&self) -> &ShareInfo {
        &self.info
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

/// Owns the capturer (it must stay on one thread) and paces capture at the
/// frame rate the chosen encoder profile allows.
fn capture_thread(source_id: Option<u64>, stop: Arc<AtomicBool>, ready_tx: oneshot::Sender<Ready>) {
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

    // Until the first frame decided the encoder, capture at the higher rate.
    let fps = Arc::new(AtomicU32::new(HARDWARE.fps));
    let mut sink = FrameSink {
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
    },
    Software {
        source: NativeVideoSource,
        size: (u32, u32),
    },
}

struct FrameSink {
    output: Option<Output>,
    ready_tx: Option<oneshot::Sender<Ready>>,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    fps: Arc<AtomicU32>,
}

/// Fits the frame into `max` and rounds down to even dimensions (needed by
/// the 4:2:0 formats).
fn output_size((width, height): (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let (w, h) = if width > max.0 || height > max.1 {
        fit((width, height), max)
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
            let output = self.choose_output(input);
            self.output = Some(output);
        }
        match self.output.as_ref() {
            Some(Output::Hardware {
                encoder,
                source,
                size,
            }) => {
                if source.take_keyframe_request() {
                    encoder.request_keyframe();
                }
                if let Some(rate) = source.take_rate_control_request() {
                    let bps = rate.target_bitrate_bps.min(u64::from(HARDWARE.max_bitrate));
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
    fn choose_output(&mut self, input: (u32, u32)) -> Output {
        let size = output_size(input, HARDWARE.max_size);
        let resolution = VideoResolution {
            width: size.0,
            height: size.1,
        };
        let source = NativeVideoSource::new_encoded(resolution.clone());
        let sink = source.clone();
        let config = tinlink_hwenc::Config {
            width: size.0,
            height: size.1,
            fps: HARDWARE.fps,
            bitrate_bps: HARDWARE.max_bitrate,
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
                self.fps.store(HARDWARE.fps, Ordering::Relaxed);
                let info = ShareInfo {
                    encoder: Some(encoder.name().to_owned()),
                    width: size.0,
                    height: size.1,
                    fps: HARDWARE.fps,
                };
                (
                    Output::Hardware {
                        encoder,
                        source,
                        size,
                    },
                    info,
                )
            }
            Err(e) => {
                tracing::info!("no hardware encoding ({e}), using software H.264");
                let size = output_size(input, SOFTWARE.max_size);
                let source = NativeVideoSource::new(
                    VideoResolution {
                        width: size.0,
                        height: size.1,
                    },
                    true,
                );
                self.fps.store(SOFTWARE.fps, Ordering::Relaxed);
                let info = ShareInfo {
                    encoder: None,
                    width: size.0,
                    height: size.1,
                    fps: SOFTWARE.fps,
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
    use super::output_size;

    #[test]
    fn output_size_fits_and_is_even() {
        assert_eq!(output_size((3840, 2160), (2560, 1440)), (2560, 1440));
        assert_eq!(output_size((1920, 1080), (2560, 1440)), (1920, 1080));
        assert_eq!(output_size((1365, 767), (2560, 1440)), (1364, 766));
        // Ultrawide: limited by width.
        assert_eq!(output_size((5120, 1440), (2560, 1440)), (2560, 720));
    }
}
