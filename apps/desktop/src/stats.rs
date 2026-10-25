//! Condenses WebRTC statistics into the few numbers that matter for latency.
//!
//! All round trip times are measured between this client and the SFU, so the
//! network part of mouth-to-ear latency between two clients is roughly
//! `(rtt_a + rtt_b) / 2`, plus the receiver's jitter buffer delay.

use livekit::webrtc::stats::RtcStats;
use serde::Serialize;

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkStats {
    /// Round trip time to the SFU in milliseconds.
    pub rtt_ms: Option<f64>,
    /// Interarrival jitter in milliseconds.
    pub jitter_ms: Option<f64>,
    /// Packet loss in percent since the track started.
    pub loss_pct: Option<f64>,
    /// Average delay added by the receive jitter buffer in milliseconds,
    /// over the last stats interval (see [`LinkStats::since`]).
    pub jitter_buffer_ms: Option<f64>,
    /// Cumulative jitter buffer delay (s) and emitted sample count.
    #[serde(skip)]
    jitter_buffer_total: Option<(f64, u64)>,
    /// Received video: frames per second, resolution and average decode time.
    pub video_fps: Option<f64>,
    pub video_size: Option<(u32, u32)>,
    pub decode_ms: Option<f64>,
}

/// Summarises the stats of a remote (received) track.
pub fn inbound(stats: &[RtcStats]) -> LinkStats {
    let mut out = LinkStats {
        rtt_ms: transport_rtt(stats),
        ..Default::default()
    };
    for s in stats {
        if let RtcStats::InboundRtp(rtp) = s {
            let rx = &rtp.received;
            out.jitter_ms = Some(rx.jitter * 1000.0);
            out.loss_pct = loss_pct(rx.packets_lost, rx.packets_received);
            let inb = &rtp.inbound;
            out.jitter_buffer_total =
                Some((inb.jitter_buffer_delay, inb.jitter_buffer_emitted_count));
            out.jitter_buffer_ms = jitter_buffer_ms(None, out.jitter_buffer_total);
            if rtp.stream.kind == "video" {
                out.video_fps = Some(inb.frames_per_second);
                out.video_size = Some((inb.frame_width, inb.frame_height));
                if inb.frames_decoded > 0 {
                    out.decode_ms =
                        Some(inb.total_decode_time / inb.frames_decoded as f64 * 1000.0);
                }
            }
        }
    }
    out
}

impl LinkStats {
    /// Turns cumulative averages into averages over the interval since `prev`.
    pub fn since(mut self, prev: Option<&LinkStats>) -> Self {
        if let Some(prev) = prev.and_then(|p| p.jitter_buffer_total) {
            self.jitter_buffer_ms = jitter_buffer_ms(Some(prev), self.jitter_buffer_total);
        }
        self
    }

    /// Combines audio stats (jitter, loss) with the video numbers of `video`.
    pub fn with_video(self, video: &LinkStats) -> Self {
        Self {
            video_fps: video.video_fps,
            video_size: video.video_size,
            decode_ms: video.decode_ms,
            ..self
        }
    }
}

/// Average jitter buffer delay in ms between two cumulative samples; `None`
/// when no audio was played out in between (e.g. the sender is muted).
fn jitter_buffer_ms(prev: Option<(f64, u64)>, cur: Option<(f64, u64)>) -> Option<f64> {
    let (delay, emitted) = cur?;
    let (prev_delay, prev_emitted) = prev.unwrap_or((0.0, 0));
    let n = emitted.checked_sub(prev_emitted).filter(|n| *n > 0)?;
    Some((delay - prev_delay) / n as f64 * 1000.0)
}

/// Summarises the stats of a local (published) track, as reported back by the SFU.
pub fn outbound(stats: &[RtcStats]) -> LinkStats {
    let mut out = LinkStats {
        rtt_ms: transport_rtt(stats),
        ..Default::default()
    };
    for s in stats {
        if let RtcStats::RemoteInboundRtp(r) = s {
            if r.remote_inbound.round_trip_time_measurements > 0 {
                out.rtt_ms = Some(r.remote_inbound.round_trip_time * 1000.0);
            }
            out.jitter_ms = Some(r.received.jitter * 1000.0);
            out.loss_pct = Some(r.remote_inbound.fraction_lost * 100.0);
        }
    }
    out
}

/// RTT of the selected ICE candidate pair (STUN consent checks), available
/// even while no media flows.
fn transport_rtt(stats: &[RtcStats]) -> Option<f64> {
    stats.iter().find_map(|s| match s {
        RtcStats::CandidatePair(p) if p.candidate_pair.nominated => {
            Some(p.candidate_pair.current_round_trip_time * 1000.0)
        }
        _ => None,
    })
}

fn loss_pct(lost: i64, received: u64) -> Option<f64> {
    let lost = lost.max(0) as f64;
    let total = lost + received as f64;
    (total > 0.0).then(|| lost / total * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_percentage() {
        assert_eq!(loss_pct(0, 0), None);
        assert_eq!(loss_pct(1, 99), Some(1.0));
        // Duplicates can make the lost counter negative.
        assert_eq!(loss_pct(-3, 100), Some(0.0));
    }

    #[test]
    fn jitter_buffer_interval_average() {
        assert_eq!(jitter_buffer_ms(None, None), None);
        assert_eq!(jitter_buffer_ms(None, Some((2.0, 100))), Some(20.0));
        // 1 s of extra delay over 50 samples since the previous sample.
        assert_eq!(
            jitter_buffer_ms(Some((2.0, 100)), Some((3.0, 150))),
            Some(20.0)
        );
        // Nothing played out in between.
        assert_eq!(jitter_buffer_ms(Some((2.0, 100)), Some((2.0, 100))), None);
    }
}
