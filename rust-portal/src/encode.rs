//! H.264 encoder thread. Encodes the RDP framebuffer only when it changed,
//! and never faster than the network estimate allows, so frames do not pile
//! up in queues: under heavy animation the frame rate drops instead of the
//! latency growing.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::ffi::c_void;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, QpRange, RateControlMode,
    UsageType,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer};
use tokio::sync::mpsc;
use openh264_sys2::{ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, SBitrateInfo, SPATIAL_LAYER_0, SPATIAL_LAYER_ALL};
use tracing::{debug, warn};

use crate::session::Shared;

pub struct Frame {
    /// Annex-B H.264 access unit.
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// How long encoding took (for the stats log).
    pub encode_ms: u32,
}

const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// After the screen stops changing, a few extra frames let the encoder sharpen
/// text that was blurred during motion.
const SETTLE_FRAMES: u32 = 4;
const SETTLE_INTERVAL: Duration = Duration::from_millis(150);
/// On a still screen, a frame at least this often. Chrome asks for a
/// keyframe when RTP keeps arriving (bandwidth probes) but no frame has come
/// for 3 s; a fully static desktop then got a 50 KB keyframe every 3 s.
/// An unchanged frame encodes to a small P-frame instead.
const IDLE_HEARTBEAT: Duration = Duration::from_secs(1);
/// Longest wait after a large frame before the next one. Pacing by the
/// estimate alone stalled the picture for seconds when a full-screen change
/// (Excel scrolling) met a low estimate; str0m's pacer smooths the rest.
const MAX_FRAME_BUDGET: Duration = Duration::from_millis(250);

const TIERS: [u32; 9] = [
    150_000, 300_000, 500_000, 800_000, 1_200_000, 2_000_000, 3_000_000, 4_500_000, 6_000_000,
];

fn tier_for(estimate: u32, max: u32) -> u32 {
    // Leave headroom for retransmissions and RTCP.
    let usable = estimate.saturating_mul(4) / 5;
    TIERS
        .iter()
        .copied()
        .filter(|t| *t <= max)
        .rev()
        .find(|t| *t <= usable)
        .unwrap_or(TIERS[0])
}

/// Changes the target bitrate of a running encoder. Re-creating the encoder
/// instead (as before) started every change with a full keyframe, and on a
/// fluctuating estimate those big frames came several times a second.
///
/// The openh264 crate sets the layer's maximum equal to the target, and
/// openh264 refuses a target above the maximum, so the maximum moves first
/// when going up and last when going down.
fn set_bitrate(encoder: &mut Encoder, from: u32, to: u32) -> anyhow::Result<()> {
    let mut set = |option, layer, bitrate: u32| -> anyhow::Result<()> {
        let mut info = SBitrateInfo { iLayer: layer, iBitrate: i32::try_from(bitrate).unwrap_or(i32::MAX) };
        // SAFETY: both options take a pointer to SBitrateInfo, valid for the call.
        let rc = unsafe { encoder.raw_api().set_option(option, (&raw mut info).cast::<c_void>()) };
        anyhow::ensure!(rc == 0, "openh264 SetOption({option}, {bitrate}) failed: {rc}");
        Ok(())
    };
    if to > from {
        set(ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0, to)?;
        set(ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL, to)
    } else {
        set(ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL, to)?;
        set(ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0, to)
    }
}

fn make_encoder(bitrate: u32) -> anyhow::Result<Encoder> {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(bitrate))
        .max_frame_rate(FrameRate::from_hz(30.0))
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .skip_frames(true)
        .complexity(Complexity::Low)
        .qp(QpRange::new(12, 42))
        .intra_frame_period(IntraFramePeriod::from_num_frames(0))
        .num_threads(1);
    Ok(Encoder::with_api_config(OpenH264API::from_source(), config)?)
}

pub fn spawn(shared: Arc<Shared>, max_bitrate: u32, tx: mpsc::Sender<Frame>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("h264".into())
        .spawn(move || {
            if let Err(e) = run(&shared, max_bitrate, &tx) {
                warn!("encoder stopped: {e:#}");
            }
        })
        .expect("spawn encoder thread")
}

fn run(shared: &Shared, max_bitrate: u32, tx: &mpsc::Sender<Frame>) -> anyhow::Result<()> {
    let mut tier = tier_for(shared.target_bps.load(Ordering::Relaxed), max_bitrate);
    let mut encoder = make_encoder(tier)?;
    shared.tier_bps.store(tier, Ordering::Relaxed);
    let mut pending_tier: Option<(u32, Instant)> = None;
    let mut last_frame = Instant::now() - Duration::from_secs(1);
    let mut budget_until = Instant::now();
    let mut settle = 0u32;
    // When a frame last actually went out (rate control may skip some).
    let mut last_sent = Instant::now();
    let mut stats = (Instant::now(), 0u32, 0usize);

    loop {
        let wait = if settle > 0 { SETTLE_INTERVAL } else { Duration::from_secs(1) };
        let mut flags = shared.wait(wait);
        if flags.stop {
            return Ok(());
        }
        let heartbeat = last_sent.elapsed() >= IDLE_HEARTBEAT;
        if !flags.dirty && !flags.keyframe && settle == 0 && !heartbeat {
            continue;
        }

        // Pace: at most 30 fps, and no faster than the bitrate estimate drains.
        let earliest = (last_frame + MIN_FRAME_INTERVAL).max(budget_until);
        let now = Instant::now();
        if now < earliest {
            std::thread::sleep(earliest - now);
            let more = shared.take_flags();
            flags.dirty |= more.dirty;
            flags.keyframe |= more.keyframe;
            if more.stop {
                return Ok(());
            }
        }

        // Follow the bandwidth estimate: drop quickly, climb slowly.
        let target = shared.target_bps.load(Ordering::Relaxed).max(TIERS[0]);
        let wanted = tier_for(target, max_bitrate);
        if wanted != tier {
            let hold = if wanted < tier { Duration::from_millis(500) } else { Duration::from_secs(3) };
            match pending_tier {
                Some((t, since)) if t == wanted && since.elapsed() >= hold => {
                    debug!(from = tier, to = wanted, estimate = target, "encoder bitrate change");
                    let from = tier;
                    tier = wanted;
                    shared.tier_bps.store(tier, Ordering::Relaxed);
                    if let Err(e) = set_bitrate(&mut encoder, from, tier) {
                        // Fall back to a new encoder (costs a keyframe).
                        warn!("{e:#}");
                        encoder = make_encoder(tier)?;
                    }
                    pending_tier = None;
                }
                Some((t, _)) if t == wanted => {}
                _ => pending_tier = Some((wanted, Instant::now())),
            }
        } else {
            pending_tier = None;
        }

        let Some(yuv) = shared.with_image_opt(|image| {
            YUVBuffer::from_rgba8_source(RgbaSliceU8::new(
                image.data(),
                (usize::from(image.width()), usize::from(image.height())),
            ))
        }) else {
            continue;
        };

        if flags.keyframe {
            encoder.force_intra_frame();
        }
        let started = Instant::now();
        let bitstream = encoder.encode(&yuv)?;
        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let data = bitstream.to_vec();
        let encode_ms = started.elapsed().as_millis();
        last_frame = Instant::now();

        if flags.dirty {
            settle = SETTLE_FRAMES;
        } else {
            settle = settle.saturating_sub(1);
        }
        if data.is_empty() {
            // Skipped by rate control.
            continue;
        }
        last_sent = last_frame;

        let bits = u64::try_from(data.len()).unwrap_or(u64::MAX).saturating_mul(8);
        budget_until = last_frame
            + Duration::from_micros(bits.saturating_mul(1_000_000) / u64::from(target)).min(MAX_FRAME_BUDGET);

        stats.1 += 1;
        stats.2 += data.len();
        if stats.0.elapsed() >= Duration::from_secs(10) {
            debug!(
                fps = stats.1 / 10,
                kbps = stats.2 * 8 / 10 / 1000,
                tier,
                target,
                encode_ms,
                "encoder stats"
            );
            stats = (Instant::now(), 0, 0);
        }

        let encode_ms = u32::try_from(encode_ms).unwrap_or(u32::MAX);
        if tx.blocking_send(Frame { data, keyframe, encode_ms }).is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_changes_without_keyframe() {
        let mut encoder = make_encoder(300_000).unwrap();
        let mut rgba = vec![0u8; 640 * 480 * 4];
        rgba.iter_mut().enumerate().for_each(|(i, p)| *p = (i % 251) as u8);
        // Each frame changes one small block, as typing or a blinking caret would.
        let mut encode = |encoder: &mut Encoder, shade: u8| {
            for y in 100..140 {
                for x in 100..180 {
                    rgba[(y * 640 + x) * 4] = shade;
                }
            }
            let yuv = YUVBuffer::from_rgba8_source(RgbaSliceU8::new(&rgba, (640, 480)));
            encoder.encode(&yuv).unwrap().frame_type()
        };
        assert!(matches!(encode(&mut encoder, 0), FrameType::IDR | FrameType::I));
        assert!(!matches!(encode(&mut encoder, 1), FrameType::IDR | FrameType::I), "keyframe with no change");
        let mut from = 300_000;
        for to in [6_000_000, 150_000, 2_000_000, 300_000] {
            set_bitrate(&mut encoder, from, to).unwrap();
            from = to;
            let kind = encode(&mut encoder, (to / 1000) as u8);
            assert!(!matches!(kind, FrameType::IDR | FrameType::I), "keyframe after change to {to}");
        }
    }
}


