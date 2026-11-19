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
/// At most this much sent video may still be draining (at the estimate)
/// when the next frame goes out. Bounds the queue when large frames come
/// faster than MAX_FRAME_BUDGET alone would allow for.
const MAX_BACKLOG: Duration = Duration::from_secs(1);

/// Estimates below this count as this, so pacing waits stay finite.
const MIN_ESTIMATE: u32 = 20_000;
/// While a frame waits for the backlog to drain, how often the wait is
/// worked out again from the latest estimate.
const PACE_RECHECK: Duration = Duration::from_millis(50);

const TIERS: [u32; 9] = [
    150_000, 300_000, 500_000, 800_000, 1_200_000, 2_000_000, 3_000_000, 4_500_000, 6_000_000,
];

/// A lower estimate must last this long before the encoder drops a tier.
const TIER_DOWN_HOLD: Duration = Duration::from_secs(1);
/// A higher estimate must last this long before the encoder climbs.
const TIER_UP_HOLD: Duration = Duration::from_secs(3);
/// Climbing back to a tier left less than TIER_RETURN_WINDOW ago waits only
/// TIER_RETURN_HOLD. A burst (a keyframe, a full-screen change) makes str0m
/// cut its estimate to 0.85 x the acknowledged bitrate, which on a quiet
/// desktop is about the 150 kbps padding floor, and its probes find the real
/// figure again within seconds. The slow climb then kept the picture at the
/// lowest tier, blurred, for seconds after every such dip.
const TIER_RETURN_WINDOW: Duration = Duration::from_secs(20);
const TIER_RETURN_HOLD: Duration = Duration::from_millis(500);

/// Picks the encoder's bitrate tier from the estimate: drops after a short
/// hold, climbs after a longer one, and goes back quickly to a tier it has
/// just left.
struct TierControl {
    tier: u32,
    /// Direction (true = down), the tier to move to and since when. Down:
    /// the highest tier wanted during the hold; up: the lowest. So a
    /// wobbling estimate still moves the tier, by the smallest step that
    /// held all along.
    pending: Option<(bool, u32, Instant)>,
    /// The highest tier recent drops left, and when the last drop was.
    dropped_from: Option<(u32, Instant)>,
}

impl TierControl {
    fn new(tier: u32) -> Self {
        Self { tier, pending: None, dropped_from: None }
    }

    /// Feeds the tier the estimate allows now; returns the new tier when it changes.
    fn update(&mut self, wanted: u32, now: Instant) -> Option<u32> {
        if wanted == self.tier {
            self.pending = None;
            return None;
        }
        let down = wanted < self.tier;
        let (to, since) = match self.pending {
            Some((d, t, since)) if d == down => (if down { t.max(wanted) } else { t.min(wanted) }, since),
            _ => (wanted, now),
        };
        self.pending = Some((down, to, since));
        let recent = self.dropped_from.filter(|(_, at)| now.duration_since(*at) < TIER_RETURN_WINDOW);
        let hold = if down {
            TIER_DOWN_HOLD
        } else if recent.is_some_and(|(from, _)| to <= from) {
            TIER_RETURN_HOLD
        } else {
            TIER_UP_HOLD
        };
        if now.duration_since(since) < hold {
            return None;
        }
        if down {
            let from = recent.map_or(self.tier, |(from, _)| from.max(self.tier));
            self.dropped_from = Some((from, now));
        } else if recent.is_some_and(|(from, _)| to >= from) {
            self.dropped_from = None;
        }
        self.tier = to;
        self.pending = None;
        Some(to)
    }
}

/// Video sent but not yet drained at the estimate, in bits. It mirrors
/// str0m's pacer, which sends at the current estimate (x 1.1). Kept in bits
/// rather than as a finishing time, so a recovered estimate shortens the wait
/// at once: a keyframe sent during a brief dip to 100 kbps had held back the
/// next frame for seconds after the estimate was back at megabits.
struct Backlog {
    bits: f64,
    at: Instant,
}

impl Backlog {
    fn new(now: Instant) -> Self {
        Self { bits: 0.0, at: now }
    }

    fn drain(&mut self, bps: u32, now: Instant) {
        let secs = now.saturating_duration_since(self.at).as_secs_f64();
        self.bits = (self.bits - secs * f64::from(bps)).max(0.0);
        self.at = self.at.max(now);
    }

    fn add(&mut self, bits: u64) {
        self.bits += bits as f64;
    }

    /// How long everything queued takes to go out at `bps`.
    fn time(&self, bps: u32) -> Duration {
        Duration::from_secs_f64(self.bits / f64::from(bps.max(1)))
    }

    /// How long until at most `max` of sending remains queued at `bps`.
    fn wait(&self, bps: u32, max: Duration) -> Duration {
        self.time(bps).saturating_sub(max)
    }
}

/// The current bandwidth estimate as the encoder uses it for pacing.
fn estimate(shared: &Shared) -> u32 {
    shared.target_bps.load(Ordering::Relaxed).max(MIN_ESTIMATE)
}

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
/// openh264 refuses a target above the maximum (or a maximum below the
/// target). The maximum is first raised to `ceiling`, so the change works
/// whatever the encoder holds now, then lowered to the new target. Ordering
/// the two by the old tier failed whenever the encoder's own values were not
/// that tier (`SetOption(5, 2000000) failed: 1`), and the fallback then cost
/// a keyframe.
fn set_bitrate(encoder: &mut Encoder, to: u32, ceiling: u32) -> anyhow::Result<()> {
    let mut set = |option, layer, bitrate: u32| -> anyhow::Result<()> {
        let mut info = SBitrateInfo { iLayer: layer, iBitrate: i32::try_from(bitrate).unwrap_or(i32::MAX) };
        // SAFETY: both options take a pointer to SBitrateInfo, valid for the call.
        let rc = unsafe { encoder.raw_api().set_option(option, (&raw mut info).cast::<c_void>()) };
        anyhow::ensure!(rc == 0, "openh264 SetOption({option}, {bitrate}) failed: {rc}");
        Ok(())
    };
    set(ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0, ceiling.max(to))?;
    set(ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL, to)?;
    set(ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0, to)
}

fn make_encoder(bitrate: u32) -> anyhow::Result<Encoder> {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(bitrate))
        .max_frame_rate(FrameRate::from_hz(30.0))
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        // No skipping inside openh264: its skip buffer drains per encode call
        // (bitrate / 30 fps each), not per second, and we call it only when
        // the screen changes or once a second when idle. After one large
        // frame on a slow link it skipped every frame for 10 s or more, and
        // the picture came back only once typing made enough calls. The
        // pacing in `run` keeps the bitrate instead, by the clock.
        .skip_frames(false)
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
    let mut tiers = TierControl::new(tier_for(estimate(shared), max_bitrate));
    let mut encoder = make_encoder(tiers.tier)?;
    shared.tier_bps.store(tiers.tier, Ordering::Relaxed);
    // Size of the frames the encoder was last given.
    let mut size: Option<(u16, u16)> = None;
    let mut last_frame = Instant::now() - Duration::from_secs(1);
    let mut budget_until = Instant::now();
    let mut backlog = Backlog::new(Instant::now());
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

        // Pace: at most 30 fps, and no faster than the estimate drains what
        // was sent. The estimate can change while waiting, so look again.
        loop {
            let now = Instant::now();
            let bps = estimate(shared);
            backlog.drain(bps, now);
            let earliest = (last_frame + MIN_FRAME_INTERVAL)
                .max(budget_until)
                .max(now + backlog.wait(bps, MAX_BACKLOG));
            if now >= earliest {
                break;
            }
            std::thread::sleep((earliest - now).min(PACE_RECHECK));
            let more = shared.take_flags();
            flags.dirty |= more.dirty;
            flags.keyframe |= more.keyframe;
            if more.stop {
                return Ok(());
            }
        }

        // Follow the bandwidth estimate (see TierControl).
        let target = estimate(shared);
        let from = tiers.tier;
        if let Some(tier) = tiers.update(tier_for(target, max_bitrate), Instant::now()) {
            debug!(from, to = tier, estimate = target, "encoder bitrate change");
            shared.tier_bps.store(tier, Ordering::Relaxed);
            if let Err(e) = set_bitrate(&mut encoder, tier, max_bitrate) {
                // Fall back to a new encoder (costs a keyframe).
                warn!("{e:#}");
                encoder = make_encoder(tier)?;
            }
        }

        let Some((yuv, frame_size)) = shared.with_image_opt(|image| {
            let yuv = YUVBuffer::from_rgba8_source(RgbaSliceU8::new(
                image.data(),
                (usize::from(image.width()), usize::from(image.height())),
            ));
            (yuv, (image.width(), image.height()))
        }) else {
            continue;
        };
        // On a new size the openh264 crate re-initializes the encoder with the
        // bitrate it was created with, not the current tier, so the encoder
        // ran at the wrong bitrate after every resize and the next change
        // failed. A new encoder at the current tier instead (the new size
        // starts with a keyframe either way).
        if size.is_some_and(|s| s != frame_size) {
            encoder = make_encoder(tiers.tier)?;
        }
        size = Some(frame_size);

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

        // Timed by the real estimate, also below the lowest tier: rounded up
        // to 150 kbps, a link estimated at 50 kbps was sent three times what
        // it carried (RTT grew to 35 s), and the keyframe quiet time ended
        // long before the keyframe could arrive, so each keyframe request
        // was answered with another keyframe.
        let bps = estimate(shared);
        let bits = u64::try_from(data.len()).unwrap_or(u64::MAX).saturating_mul(8);
        let transfer = Duration::from_micros(bits.saturating_mul(1_000_000) / u64::from(bps));
        budget_until = last_frame + transfer.min(MAX_FRAME_BUDGET);
        backlog.drain(bps, last_frame);
        backlog.add(bits);
        if keyframe {
            // It arrives after everything still queued ahead of it.
            shared.keyframe_sent(backlog.time(bps));
        }

        stats.1 += 1;
        stats.2 += data.len();
        if stats.0.elapsed() >= Duration::from_secs(10) {
            debug!(
                fps = stats.1 / 10,
                kbps = stats.2 * 8 / 10 / 1000,
                tier = tiers.tier,
                estimate = bps,
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
    fn no_frames_skipped_after_large_frame() {
        // A busy 1366x768 screen at the lowest tier: the first frame is far
        // larger than a second's worth of bits.
        let mut encoder = make_encoder(TIERS[0]).unwrap();
        let (w, h) = (1366, 768);
        let mut seed = 1u32;
        let mut rgba: Vec<u8> = (0..w * h * 4)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (seed >> 16) as u8
            })
            .collect();
        let first = encoder.encode(&YUVBuffer::from_rgba8_source(RgbaSliceU8::new(&rgba, (w, h)))).unwrap().to_vec();
        assert!(first.len() * 8 > TIERS[0] as usize, "first frame only {} bytes", first.len());
        // Then typing: one small change per call, as few calls as on a quiet
        // screen. With openh264's own skipping, these came out empty.
        for i in 0..20u8 {
            for y in 300..316 {
                for x in 400..410 {
                    rgba[(y * w + x) * 4] = i.wrapping_mul(40);
                }
            }
            let out = encoder.encode(&YUVBuffer::from_rgba8_source(RgbaSliceU8::new(&rgba, (w, h)))).unwrap().to_vec();
            assert!(!out.is_empty(), "frame {i} skipped");
        }
    }

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
        for to in [6_000_000, 150_000, 2_000_000, 300_000] {
            set_bitrate(&mut encoder, to, 6_000_000).unwrap();
            let kind = encode(&mut encoder, (to / 1000) as u8);
            assert!(!matches!(kind, FrameType::IDR | FrameType::I), "keyframe after change to {to}");
        }
    }

    #[test]
    fn bitrate_change_after_resize() {
        // The crate re-initializes at its creation bitrate (800 kbps) on a new
        // size, while the session believes 3 Mbps: both directions must work.
        let mut encoder = make_encoder(800_000).unwrap();
        let frame = |w: usize, h: usize| YUVBuffer::from_rgba8_source(RgbaSliceU8::new(&vec![90u8; w * h * 4], (w, h)));
        encoder.encode(&frame(640, 480)).unwrap();
        set_bitrate(&mut encoder, 3_000_000, 6_000_000).unwrap();
        encoder.encode(&frame(800, 600)).unwrap();
        set_bitrate(&mut encoder, 2_000_000, 6_000_000).unwrap();
        encoder.encode(&frame(640, 480)).unwrap();
        set_bitrate(&mut encoder, 300_000, 6_000_000).unwrap();
        set_bitrate(&mut encoder, 150_000, 6_000_000).unwrap();
    }

    #[test]
    fn tier_returns_quickly_after_brief_dip() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut c = TierControl::new(3_000_000);
        // A dip shorter than the down hold changes nothing.
        assert_eq!(c.update(150_000, at(0)), None);
        assert_eq!(c.update(150_000, at(900)), None);
        assert_eq!(c.update(3_000_000, at(950)), None);
        // A longer one drops, by the smallest step that held all along.
        assert_eq!(c.update(150_000, at(1_000)), None);
        assert_eq!(c.update(500_000, at(1_500)), None);
        assert_eq!(c.update(150_000, at(2_000)), Some(500_000));
        // Back up: to the tier just left after the short hold, not 3 s.
        assert_eq!(c.update(3_000_000, at(2_100)), None);
        assert_eq!(c.update(3_000_000, at(2_600)), Some(3_000_000));
        // Above that tier the normal hold applies.
        assert_eq!(c.update(6_000_000, at(3_000)), None);
        assert_eq!(c.update(6_000_000, at(5_000)), None);
        assert_eq!(c.update(6_000_000, at(6_000)), Some(6_000_000));
        // Long after a drop, climbing back is slow again.
        let mut c = TierControl::new(3_000_000);
        c.update(150_000, at(0));
        assert_eq!(c.update(150_000, at(1_000)), Some(150_000));
        assert_eq!(c.update(3_000_000, at(30_000)), None);
        assert_eq!(c.update(3_000_000, at(30_500)), None);
        assert_eq!(c.update(3_000_000, at(33_000)), Some(3_000_000));
    }

    #[test]
    fn backlog_follows_the_current_estimate() {
        let t0 = Instant::now();
        let mut b = Backlog::new(t0);
        // A 42 KB keyframe during a dip to 100 kbps: about 3.4 s queued.
        b.add(42 * 1024 * 8);
        assert!(b.wait(100_000, MAX_BACKLOG) > Duration::from_secs(2));
        // 200 ms later the estimate is back at 5 Mbps: no wait left.
        b.drain(100_000, t0 + Duration::from_millis(200));
        assert_eq!(b.wait(5_000_000, MAX_BACKLOG), Duration::ZERO);
        b.drain(5_000_000, t0 + Duration::from_millis(300));
        assert_eq!(b.time(5_000_000), Duration::ZERO);
    }
}


