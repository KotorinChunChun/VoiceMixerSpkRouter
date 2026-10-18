//! Device-independent Edge PCM client helpers used by protocol tests.

use crate::edge::{control::EdgeControlMessage, receiver::EdgeHub};
use std::collections::VecDeque;

pub const MAX_EDGE_FRAMES: usize = 4096;

#[derive(Debug, Clone, Copy)]
pub struct EdgeSimulationConfig {
    pub duration_secs: u32,
    pub jitter_ms: u32,
    pub drift_ppm: i32,
    pub reconnect_at_secs: Option<u32>,
}

impl Default for EdgeSimulationConfig {
    fn default() -> Self {
        Self {
            duration_secs: 60,
            jitter_ms: 20,
            drift_ppm: 100,
            reconnect_at_secs: Some(30),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EdgeSimulationReport {
    pub blocks: usize,
    pub underruns: usize,
    pub reconnect_warmup_underruns: usize,
    pub reconnects: usize,
    pub max_fill_ratio: f32,
}

/// Runs a deterministic, device-independent PCM simulation through EdgeHub.
/// The scheduler varies packet size and arrival gaps instead of sleeping, so
/// CI can cover jitter, drift and reconnect behavior quickly.
pub fn simulate_stream(config: EdgeSimulationConfig) -> crate::error::Result<EdgeSimulationReport> {
    let hub = EdgeHub::default();
    let key = "simulated-edge-tab";
    let stream = hub.register(key, 48_000, 2);
    let total_blocks = config.duration_secs as usize * 100;
    let mut report = EdgeSimulationReport::default();
    let mut reconnecting = false;
    let mut delayed_packets: VecDeque<(usize, usize)> = VecDeque::new();
    let mut drift_accumulator = 0i64;
    let drift_per_block = i64::from(config.drift_ppm) * 480;

    // Prime the queue beyond the start threshold before consuming blocks.
    for _ in 0..110 {
        let bytes = pcm_bytes(480);
        hub.push_pcm_f32le(key, &bytes)?;
    }
    for block in 0..total_blocks {
        if config.reconnect_at_secs == Some((block / 100) as u32) && block % 100 == 0 {
            hub.remove(key);
            let _ = hub.register(key, 48_000, 2);
            report.reconnects += 1;
            reconnecting = true;
            delayed_packets.clear();
        }

        // A bounded deterministic delay models jitter without dropping PCM.
        // Delayed packets are delivered on a later tick; reconnect drops any
        // old lease's pending packets.
        let jitter_gap = config.jitter_ms > 0 && (block % 37 == 0 || block % 53 == 0);
        drift_accumulator += drift_per_block;
        let drift_frames = (drift_accumulator / 1_000_000) as i32;
        drift_accumulator %= 1_000_000;
        let frames = (480 + drift_frames).clamp(1, MAX_EDGE_FRAMES as i32) as usize;
        delayed_packets.push_back((block + usize::from(jitter_gap) * 2, frames));
        let mut ready = Vec::new();
        delayed_packets.retain(|(due, packet_frames)| {
            if *due <= block {
                ready.push(*packet_frames);
                false
            } else {
                true
            }
        });
        for packet_frames in ready {
            hub.push_pcm_f32le(key, &pcm_bytes(packet_frames))?;
        }
        let mut output = vec![0.0f32; 480 * 2];
        let read = stream.jitter.read_into(&mut output);
        if read < output.len() && block > 100 {
            if reconnecting && !stream.jitter.is_started() {
                report.reconnect_warmup_underruns += 1;
            } else {
                report.underruns += 1;
            }
        }
        reconnecting &= !stream.jitter.is_started();
        report.max_fill_ratio = report.max_fill_ratio.max(stream.jitter.fill_ratio());
        report.blocks += 1;
    }
    Ok(report)
}

fn pcm_bytes(frames: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(frames * 2 * 4);
    for _ in 0..frames * 2 {
        bytes.extend_from_slice(&0.0f32.to_le_bytes());
    }
    bytes
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeFrameError {
    InvalidByteLength,
    InvalidChannels,
    TooManyFrames,
}

pub fn parse_pcm_frame(
    bytes: &[u8],
    channels: u16,
) -> std::result::Result<Vec<f32>, EdgeFrameError> {
    if !(1..=2).contains(&channels) {
        return Err(EdgeFrameError::InvalidChannels);
    }
    if bytes.len() % 4 != 0 || bytes.len() / 4 % channels as usize != 0 {
        return Err(EdgeFrameError::InvalidByteLength);
    }
    if bytes.len() / 4 / channels as usize > MAX_EDGE_FRAMES {
        return Err(EdgeFrameError::TooManyFrames);
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

pub fn build_hello(
    stream_key: impl Into<String>,
    token: impl Into<String>,
    tab_id: u64,
    title: impl Into<String>,
    url: impl Into<String>,
    sample_rate: u32,
    channels: u16,
) -> EdgeControlMessage {
    EdgeControlMessage::hello_v2(stream_key, token, tab_id, title, url, sample_rate, channels)
}

#[cfg(test)]
mod tests {
    use super::{
        build_hello, parse_pcm_frame, simulate_stream, EdgeFrameError, EdgeSimulationConfig,
        MAX_EDGE_FRAMES,
    };
    use crate::edge::receiver::{EdgeHub, EdgeStreamState};
    use std::time::Duration;

    #[test]
    fn t08_u_01_simulator_builds_authenticated_hello() {
        let hello = build_hello(
            "tab",
            "secret",
            99,
            "Title",
            "https://example.test",
            44_100,
            2,
        );
        let value = serde_json::to_value(hello).unwrap();
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["tab_id"], 99);
        assert_eq!(value["sample_rate"], 44_100);
    }

    #[test]
    fn t08_u_03_pcm_parser_rejects_malformed_and_oversized_frames() {
        assert_eq!(
            parse_pcm_frame(&[0, 0, 0], 2),
            Err(EdgeFrameError::InvalidByteLength)
        );
        assert_eq!(
            parse_pcm_frame(&[0; 8], 3),
            Err(EdgeFrameError::InvalidChannels)
        );
        let oversized = vec![0u8; (MAX_EDGE_FRAMES + 1) * 2 * 4];
        assert_eq!(
            parse_pcm_frame(&oversized, 2),
            Err(EdgeFrameError::TooManyFrames)
        );
    }

    #[test]
    fn t08_i_02_i_03_i_05_simulator_covers_jitter_drift_and_reconnect() {
        let report = simulate_stream(EdgeSimulationConfig {
            duration_secs: 2,
            jitter_ms: 20,
            drift_ppm: 100,
            reconnect_at_secs: Some(1),
        })
        .unwrap();
        assert_eq!(report.blocks, 200);
        assert_eq!(report.reconnects, 1);
        assert_eq!(
            report.underruns, 0,
            "no loss is allowed after the reconnect fill threshold"
        );
        assert!(
            report.reconnect_warmup_underruns > 0,
            "a reset must wait for new PCM before resuming"
        );
        assert!(report.max_fill_ratio > 0.0);
    }

    #[test]
    fn t08_i_04_heartbeat_timeout_and_recovery_are_deterministic() {
        let hub = EdgeHub::default();
        let stream = hub.register("heartbeat-tab", 48_000, 2);
        assert_eq!(stream.state(Duration::ZERO), EdgeStreamState::Timeout);
        stream.heartbeat();
        assert_eq!(
            stream.state(Duration::from_secs(1)),
            EdgeStreamState::Running
        );
    }
}
