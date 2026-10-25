//! Encodes synthetic frames on the real GPU. Skipped (passes) on machines
//! without a hardware H.264 encoder, such as CI runners.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use tinlink_hwenc::{Config, EncodedFrame, Error, HardwareEncoder, nal};

const W: u32 = 1280;
const H: u32 = 720;

/// A moving gradient so every frame differs.
fn nv12_frame(n: u32) -> Vec<u8> {
    let mut frame = vec![128u8; (W * H * 3 / 2) as usize];
    for y in 0..H {
        for x in 0..W {
            frame[(y * W + x) as usize] = ((x + y + n * 8) % 256) as u8;
        }
    }
    frame
}

#[test]
fn encodes_frames_with_low_latency() {
    let (tx, rx) = mpsc::channel::<(EncodedFrame, Instant)>();
    let config = Config {
        width: W,
        height: H,
        fps: 30,
        bitrate_bps: 4_000_000,
    };
    let encoder = match HardwareEncoder::start(config, move |f| {
        let _ = tx.send((f, Instant::now()));
    }) {
        Ok(encoder) => encoder,
        Err(Error::NoEncoder | Error::Unsupported) => {
            eprintln!("no hardware encoder, skipping");
            return;
        }
        Err(e) => panic!("cannot start encoder: {e}"),
    };
    eprintln!("using {}", encoder.name());

    let mut latencies = Vec::new();
    let mut frames = Vec::new();
    for n in 0..60u32 {
        if n == 30 {
            encoder.request_keyframe();
        }
        let frame = nv12_frame(n);
        let pushed_at = Instant::now();
        assert!(
            encoder.push(frame, i64::from(n) * 33_333),
            "frame {n} dropped"
        );
        let (frame, done_at) = rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| panic!("no output for frame {n}"));
        latencies.push(done_at - pushed_at);
        frames.push(frame);
        std::thread::sleep(Duration::from_millis(10));
    }

    let first = &frames[0];
    assert!(first.keyframe, "first frame must be a keyframe");
    assert!(
        nal::contains_nal(&first.data, nal::NAL_SPS),
        "keyframe without SPS"
    );
    assert!(
        nal::contains_nal(&first.data, nal::NAL_PPS),
        "keyframe without PPS"
    );
    assert!(
        frames[30..33].iter().any(|f| f.keyframe),
        "requested keyframe did not arrive"
    );
    assert!(
        frames[1..30].iter().all(|f| !f.keyframe),
        "unexpected keyframes"
    );

    latencies.sort();
    let median = latencies[latencies.len() / 2];
    eprintln!(
        "encode latency median {median:?}, max {:?}",
        latencies.last().unwrap()
    );
    assert!(
        median < Duration::from_millis(15),
        "median encode latency {median:?}"
    );
}

#[test]
fn rejects_odd_sizes() {
    let config = Config {
        width: 1279,
        height: 720,
        fps: 30,
        bitrate_bps: 1_000_000,
    };
    assert!(matches!(
        HardwareEncoder::start(config, |_| {}),
        Err(Error::InvalidSize(1279, 720) | Error::Unsupported)
    ));
}
