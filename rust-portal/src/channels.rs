//! RDP virtual channels: sound (RDPSND -> Opus) and clipboard (CLIPRDR, text only).
//!
//! IronRDP calls the handlers below synchronously from `ActiveStage::process`,
//! inside the RDP task. Sound is encoded right there (Opus on 20 ms of audio
//! takes microseconds) and handed to the WebRTC loop. Clipboard callbacks only
//! queue events; the RDP loop acts on them after each PDU, because answering
//! needs `&mut ActiveStage`, which is busy while the callback runs.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_cliprdr::backend::CliprdrBackend;
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse,
    FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::impl_as_any;
use ironrdp_rdpsnd::client::RdpsndClientHandler;
use ironrdp_rdpsnd::pdu::{AudioFormat, PitchPdu, VolumePdu, WaveFormat};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Opus works at 48 kHz; 20 ms frames.
pub const OPUS_RATE: u32 = 48_000;
const FRAME_SAMPLES: usize = 960;
const OPUS_BITRATE: i32 = 96_000;

/// One encoded Opus packet (20 ms, stereo).
pub struct AudioPacket {
    pub data: Vec<u8>,
}

// ---------- sound ----------

/// Receives PCM from Windows, converts it to 48 kHz stereo and encodes Opus.
pub struct AudioHandler {
    formats: Vec<AudioFormat>,
    encoder: opus::Encoder,
    /// Interleaved stereo samples at 48 kHz waiting for a full 20 ms frame.
    pending: Vec<i16>,
    resampler: Resampler,
    tx: mpsc::Sender<AudioPacket>,
    dropped: u64,
}

impl core::fmt::Debug for AudioHandler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AudioHandler").field("formats", &self.formats).finish_non_exhaustive()
    }
}

impl AudioHandler {
    pub fn new(tx: mpsc::Sender<AudioPacket>) -> anyhow::Result<Self> {
        let mut encoder = opus::Encoder::new(OPUS_RATE, opus::Channels::Stereo, opus::Application::Audio)?;
        encoder.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE))?;
        Ok(Self {
            // What we can take; the server's own entries are chosen in `choose_formats`.
            formats: [48_000, 44_100, 22_050]
                .into_iter()
                .map(|rate| pcm_format(2, rate))
                .collect(),
            encoder,
            pending: Vec::with_capacity(FRAME_SAMPLES * 4),
            resampler: Resampler::default(),
            tx,
            dropped: 0,
        })
    }

    fn encode_pending(&mut self) {
        let mut out = [0u8; 4000];
        let frame = FRAME_SAMPLES * 2;
        let mut start = 0;
        while self.pending.len() - start >= frame {
            match self.encoder.encode(&self.pending[start..start + frame], &mut out) {
                Ok(n) => {
                    if self.tx.try_send(AudioPacket { data: out[..n].to_vec() }).is_err() {
                        self.dropped += 1;
                        if self.dropped.is_power_of_two() {
                            debug!(dropped = self.dropped, "audio packets dropped (viewer not keeping up)");
                        }
                    }
                }
                Err(e) => warn!("opus encode failed: {e}"),
            }
            start += frame;
        }
        self.pending.drain(..start);
    }
}

fn pcm_format(channels: u16, rate: u32) -> AudioFormat {
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: channels,
        n_samples_per_sec: rate,
        n_avg_bytes_per_sec: rate * u32::from(channels) * 2,
        n_block_align: channels * 2,
        bits_per_sample: 16,
        data: None,
    }
}

impl RdpsndClientHandler for AudioHandler {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    /// Offers exactly one format, the best 16-bit PCM the server lists, copied
    /// from the server's own entry so negotiation cannot mismatch.
    fn choose_formats(&self, server_formats: &[AudioFormat]) -> Vec<AudioFormat> {
        let score = |f: &AudioFormat| {
            let rate = match f.n_samples_per_sec {
                48_000 => 4,
                44_100 => 3,
                r if r >= 22_050 => 2,
                _ => 1,
            };
            rate * 2 + u32::from(f.n_channels == 2)
        };
        let best = server_formats
            .iter()
            .filter(|f| f.format == WaveFormat::PCM && f.bits_per_sample == 16 && (1..=2).contains(&f.n_channels))
            .max_by_key(|f| score(f))
            .cloned();
        match best {
            Some(f) => {
                info!(rate = f.n_samples_per_sec, channels = f.n_channels, "sound format chosen");
                vec![f]
            }
            None => {
                warn!(?server_formats, "server offers no 16-bit PCM sound format");
                Vec::new()
            }
        }
    }

    fn wave(&mut self, format: &AudioFormat, _ts: u32, data: Cow<'_, [u8]>) {
        if format.format != WaveFormat::PCM || format.bits_per_sample != 16 {
            return;
        }
        let channels = usize::from(format.n_channels.max(1));
        let frames = data.chunks_exact(2 * channels).map(|frame| {
            let left = i16::from_le_bytes([frame[0], frame[1]]);
            let right = if channels > 1 { i16::from_le_bytes([frame[2], frame[3]]) } else { left };
            (left, right)
        });
        self.resampler.run(format.n_samples_per_sec, frames, &mut self.pending);
        self.encode_pending();
    }

    fn playback_latency(&self) -> Duration {
        // Rough browser-side buffering; Windows uses it for A/V sync only.
        Duration::from_millis(60)
    }

    fn set_volume(&mut self, _volume: VolumePdu) {}

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {
        self.pending.clear();
        self.resampler = Resampler::default();
    }
}

/// Linear-interpolation resampler to 48 kHz. Plenty for desktop sound, and it
/// keeps state across wave blocks so block edges do not click.
#[derive(Default)]
struct Resampler {
    rate: u32,
    /// Position of the next output sample, in input frames, where index 0 is
    /// `prev` (the last frame of the previous block) and index i+1 is input[i].
    pos: f64,
    prev: (i16, i16),
}

impl Resampler {
    fn run(&mut self, rate: u32, frames: impl Iterator<Item = (i16, i16)>, out: &mut Vec<i16>) {
        if rate == 0 {
            return;
        }
        if rate != self.rate {
            *self = Self { rate, pos: 1.0, prev: (0, 0) };
        }
        if rate == OPUS_RATE {
            for (l, r) in frames {
                out.extend_from_slice(&[l, r]);
            }
            return;
        }
        let input: Vec<(i16, i16)> = frames.collect();
        if input.is_empty() {
            return;
        }
        let step = f64::from(rate) / f64::from(OPUS_RATE);
        let n = input.len() as f64;
        let lerp = |a: i16, b: i16, t: f64| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as i16;
        while self.pos < n {
            let k = self.pos.floor() as usize;
            let t = self.pos - k as f64;
            let a = if k == 0 { self.prev } else { input[k - 1] };
            let b = input[k];
            out.push(lerp(a.0, b.0, t));
            out.push(lerp(a.1, b.1, t));
            self.pos += step;
        }
        self.pos -= n;
        self.prev = *input.last().expect("not empty");
    }
}

// ---------- clipboard ----------

/// What the clipboard channel asked for while IronRDP was processing a PDU.
#[derive(Debug)]
pub enum ClipEvent {
    /// Channel is up: advertise what we have (required, even if nothing).
    RequestFormatList,
    /// Windows copied something; true if it includes text.
    RemoteCopy { has_text: bool },
    /// Windows wants our clipboard in this format (the user pasted).
    DataRequest(ClipboardFormatId),
    /// Text we asked for, or None if Windows could not provide it.
    DataResponse(Option<String>),
}

#[derive(Debug)]
pub struct ClipBackend {
    events: Arc<Mutex<Vec<ClipEvent>>>,
}

impl ClipBackend {
    pub fn new(events: Arc<Mutex<Vec<ClipEvent>>>) -> Self {
        Self { events }
    }

    fn push(&self, event: ClipEvent) {
        self.events.lock().unwrap().push(event);
    }
}

impl_as_any!(ClipBackend);

impl CliprdrBackend for ClipBackend {
    fn temporary_directory(&self) -> &str {
        "C:\\Windows\\Temp"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {}

    fn on_request_format_list(&mut self) {
        self.push(ClipEvent::RequestFormatList);
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        let has_text = available_formats.iter().any(|f| f.id == ClipboardFormatId::CF_UNICODETEXT);
        self.push(ClipEvent::RemoteCopy { has_text });
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        self.push(ClipEvent::DataRequest(request.format));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let text = if response.is_error() {
            None
        } else {
            response.to_unicode_string().ok()
        };
        self.push(ClipEvent::DataResponse(text));
    }

    // Text only: we never advertise files, so these are not expected.
    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_keeps_rate() {
        let mut r = Resampler::default();
        let mut out = Vec::new();
        // 1 s at 44.1 kHz in 10 blocks -> about 1 s at 48 kHz.
        for _ in 0..10 {
            r.run(44_100, std::iter::repeat_n((1000, -1000), 4410), &mut out);
        }
        let frames = out.len() / 2;
        assert!((47_990..=48_010).contains(&frames), "{frames}");
        assert!(out[2..].chunks(2).all(|f| f == [1000, -1000]));
    }
}
