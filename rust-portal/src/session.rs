//! One viewer session: WebSocket signaling, the WebRTC peer (str0m, sans-IO,
//! one UDP socket per session), the RDP task and the encoder thread.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use axum::extract::ws::{Message, WebSocket};
use ironrdp_input::{MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp_session::image::DecodedImage;
use serde::Deserialize;
use serde_json::json;
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::channel::ChannelId;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{Instrument as _, debug, info, warn};

use crate::Config;
use crate::db::Connection;
use crate::rdp::{InputMsg, MAX_CLIPBOARD, RdpEvent, RdpOut};

#[derive(Default, Clone, Copy)]
pub struct Flags {
    pub dirty: bool,
    pub keyframe: bool,
    pub stop: bool,
}

/// State shared between the RDP task, the encoder thread and the WebRTC loop.
pub struct Shared {
    image: Mutex<Option<DecodedImage>>,
    flags: Mutex<Flags>,
    wake: Condvar,
    /// Latest send-side bandwidth estimate, bits per second.
    pub target_bps: AtomicU32,
    /// Bitrate the encoder currently aims for (for the stats log).
    pub tier_bps: AtomicU32,
}

impl Shared {
    pub fn new(initial_bps: u32) -> Arc<Self> {
        Arc::new(Self {
            image: Mutex::new(None),
            flags: Mutex::new(Flags::default()),
            wake: Condvar::new(),
            target_bps: AtomicU32::new(initial_bps),
            tier_bps: AtomicU32::new(0),
        })
    }

    pub fn set_image(&self, image: DecodedImage) {
        *self.image.lock().unwrap() = Some(image);
        self.update(|f| f.keyframe = true);
    }

    /// Runs `f` on the framebuffer. Panics if RDP is not connected yet.
    pub fn with_image<T>(&self, f: impl FnOnce(&mut DecodedImage) -> T) -> T {
        f(self.image.lock().unwrap().as_mut().expect("framebuffer exists once RDP is connected"))
    }

    pub fn with_image_opt<T>(&self, f: impl FnOnce(&DecodedImage) -> T) -> Option<T> {
        self.image.lock().unwrap().as_ref().map(f)
    }

    fn update(&self, f: impl FnOnce(&mut Flags)) {
        f(&mut self.flags.lock().unwrap());
        self.wake.notify_all();
    }

    pub fn mark_dirty(&self) {
        self.update(|f| f.dirty = true);
    }

    pub fn request_keyframe(&self) {
        self.update(|f| f.keyframe = true);
    }

    pub fn stop(&self) {
        self.update(|f| f.stop = true);
    }

    /// Waits up to `timeout` for a flag, then takes (clears) dirty/keyframe.
    pub fn wait(&self, timeout: Duration) -> Flags {
        let guard = self.flags.lock().unwrap();
        let (mut guard, _) = self
            .wake
            .wait_timeout_while(guard, timeout, |f| !f.dirty && !f.keyframe && !f.stop)
            .unwrap();
        let out = *guard;
        guard.dirty = false;
        guard.keyframe = false;
        out
    }

    pub fn take_flags(&self) -> Flags {
        let mut guard = self.flags.lock().unwrap();
        let out = *guard;
        guard.dirty = false;
        guard.keyframe = false;
        out
    }
}

#[derive(Deserialize)]
struct Start {
    offer: String,
    width: u16,
    height: u16,
}

/// Input messages on the "input" data channel.
#[derive(Deserialize)]
#[serde(tag = "t")]
enum ClientInput {
    /// Pointer moved, remote pixel coordinates.
    #[serde(rename = "m")]
    Move { x: u16, y: u16 },
    /// Mouse button (web numbering) down/up.
    #[serde(rename = "b")]
    Button { b: u8, d: bool, x: u16, y: u16 },
    /// Wheel, in RDP units (120 per notch); `h` for horizontal.
    #[serde(rename = "w")]
    Wheel { v: i16, #[serde(default)] h: bool },
    /// Key by scancode (0xE0xx for extended keys).
    #[serde(rename = "k")]
    Key { c: u16, d: bool },
    /// Release everything (window lost focus).
    #[serde(rename = "r")]
    Release,
    /// Part of the browser's clipboard text; `more` until the last part.
    #[serde(rename = "c")]
    Clip { d: String, #[serde(default)] more: bool },
    /// New desktop size wanted (window resized, full screen, or picked).
    #[serde(rename = "size")]
    Size { w: u16, h: u16 },
}

fn to_input(msg: ClientInput) -> InputMsg {
    match msg {
        ClientInput::Move { x, y } => InputMsg::Ops(vec![Operation::MouseMove(MousePosition { x, y })]),
        ClientInput::Button { b, d, x, y } => {
            let mut ops = vec![Operation::MouseMove(MousePosition { x, y })];
            if let Some(button) = MouseButton::from_web_button(b) {
                ops.push(if d {
                    Operation::MouseButtonPressed(button)
                } else {
                    Operation::MouseButtonReleased(button)
                });
            }
            InputMsg::Ops(ops)
        }
        ClientInput::Wheel { v, h } => InputMsg::Ops(vec![Operation::WheelRotations(WheelRotations {
            is_vertical: !h,
            rotation_units: v,
        })]),
        ClientInput::Key { c, d } => {
            let code = Scancode::from_u16(c);
            InputMsg::Ops(vec![if d {
                Operation::KeyPressed(code)
            } else {
                Operation::KeyReleased(code)
            }])
        }
        ClientInput::Release => InputMsg::ReleaseAll,
        ClientInput::Size { w, h } => InputMsg::Resize {
            width: even_clamp(w, MIN_SIZE.0, MAX_SIZE.0),
            height: even_clamp(h, MIN_SIZE.1, MAX_SIZE.1),
        },
        ClientInput::Clip { .. } => unreachable!("clipboard parts are joined by the caller"),
    }
}

/// Characters of clipboard text per data channel message (stays far below
/// the SCTP message size limit, even for 4-byte UTF-8).
const CLIP_CHUNK: usize = 16 * 1024;

/// A message larger than str0m's whole SCTP send buffer (128 KB) can never be
/// sent; such messages are dropped (none of ours come close).
const DC_MAX_MESSAGE: usize = 120 * 1024;

/// What the browser should show as the mouse pointer.
enum CursorState {
    Shape(Arc<ironrdp_graphics::pointer::DecodedPointer>),
    Default,
    Hidden,
}

/// Messages for the browser on the "input" data channel.
///
/// str0m refuses a write that does not fit its SCTP send buffer, which only
/// frees up as the browser acknowledges data. At logon Windows sends a burst
/// of pointer shapes, so writing directly lost some, and the pointer could
/// stay hidden. Messages now wait here and go out in order as room frees up
/// (and wait for the channel to open). Only the newest pointer matters, so it
/// has its own slot that a newer one replaces. Each pointer image is sent once;
/// later uses refer to it by id.
#[derive(Default)]
struct DcOut {
    chan: Option<ChannelId>,
    cursor: Option<CursorState>,
    queue: std::collections::VecDeque<Vec<u8>>,
    /// Pointer images the browser has (ids it has been sent in full).
    sent_cursors: std::collections::HashSet<u64>,
}

impl DcOut {
    fn push(&mut self, value: &serde_json::Value) {
        let bytes = value.to_string().into_bytes();
        if bytes.len() > DC_MAX_MESSAGE {
            warn!(len = bytes.len(), "data channel message too large, dropped");
            return;
        }
        self.queue.push_back(bytes);
    }

    /// Writes what fits now; call again later for the rest.
    fn flush(&mut self, rtc: &mut Rtc) {
        let Some(mut ch) = self.chan.and_then(|id| rtc.channel(id)) else {
            return;
        };
        if let Some(cursor) = self.cursor.take() {
            let (msg, full_id) = cursor_message(&cursor, &self.sent_cursors);
            match ch.write(false, msg.as_bytes()) {
                Ok(true) => {
                    if let Some(id) = full_id {
                        if self.sent_cursors.len() >= 512 {
                            self.sent_cursors.clear();
                        }
                        self.sent_cursors.insert(id);
                    }
                }
                Ok(false) => {
                    self.cursor = Some(cursor);
                    return;
                }
                Err(e) => debug!("data channel write failed: {e}"),
            }
        }
        while let Some(front) = self.queue.front() {
            match ch.write(false, front) {
                Ok(true) => {}
                Ok(false) => return,
                Err(e) => debug!("data channel write failed: {e}"),
            }
            self.queue.pop_front();
        }
    }
}

/// The pointer message, and the image id if this one carries the image.
fn cursor_message(cursor: &CursorState, sent: &std::collections::HashSet<u64>) -> (String, Option<u64>) {
    use base64::Engine as _;
    use std::hash::{Hash as _, Hasher as _};
    let p = match cursor {
        CursorState::Default => return (json!({ "t": "cursor", "kind": "default" }).to_string(), None),
        CursorState::Hidden => return (json!({ "t": "cursor", "kind": "none" }).to_string(), None),
        CursorState::Shape(p) => p,
    };
    // Browsers ignore CSS cursors larger than 128 px.
    if p.width == 0 || p.height == 0 || p.width > 128 || p.height > 128 {
        return (json!({ "t": "cursor", "kind": "default" }).to_string(), None);
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (p.width, p.height, p.hotspot_x, p.hotspot_y, &p.bitmap_data).hash(&mut h);
    let id = h.finish();
    let id_text = format!("{id:x}");
    if sent.contains(&id) {
        return (json!({ "t": "cursor", "id": id_text }).to_string(), None);
    }
    let rgba = base64::engine::general_purpose::STANDARD.encode(&p.bitmap_data);
    let msg = json!({
        "t": "cursor", "id": id_text, "w": p.width, "h": p.height,
        "x": p.hotspot_x, "y": p.hotspot_y, "rgba": rgba,
    });
    (msg.to_string(), Some(id))
}

fn send_event(rtc: &mut Rtc, out: &mut DcOut, event: RdpEvent) {
    match event {
        RdpEvent::Cursor(p) => out.cursor = Some(CursorState::Shape(p)),
        RdpEvent::CursorDefault => out.cursor = Some(CursorState::Default),
        RdpEvent::CursorHidden => out.cursor = Some(CursorState::Hidden),
        RdpEvent::Clipboard(text) => {
            let chars: Vec<char> = text.chars().collect();
            let parts: Vec<String> = chars.chunks(CLIP_CHUNK).map(|c| c.iter().collect()).collect();
            if parts.is_empty() {
                out.push(&json!({ "t": "clip", "d": "", "more": false }));
            }
            let last = parts.len().saturating_sub(1);
            for (i, part) in parts.into_iter().enumerate() {
                out.push(&json!({ "t": "clip", "d": part, "more": i < last }));
            }
        }
        RdpEvent::Size { width, height } => out.push(&json!({ "t": "size", "w": width, "h": height })),
        RdpEvent::ResizeUnsupported => out.push(&json!({ "t": "size_unsupported" })),
    }
    out.flush(rtc);
}

async fn send_json(ws: &mut WebSocket, value: serde_json::Value) {
    let _ = ws.send(Message::Text(value.to_string().into())).await;
}

pub async fn run(
    mut ws: WebSocket,
    conn: Connection,
    config: Arc<Config>,
    username: String,
    stop: Arc<tokio::sync::Notify>,
) {
    let id = conn.id;
    info!(user = %username, connection = id, "session start");
    // Every log line of the session (stats, WebRTC, errors) names the user.
    let span = tracing::info_span!("session", user = %username, connection = id);
    let reason = match run_inner(&mut ws, conn, &config, &stop).instrument(span).await {
        Ok(reason) => reason.to_owned(),
        Err(e) => {
            // The browser reconnects by itself after a retryable error.
            let retry = e.is::<Retryable>();
            warn!(user = %username, connection = id, retry, "session error: {e:#}");
            send_json(&mut ws, json!({ "error": format!("{e:#}"), "retry": retry })).await;
            format!("{e:#}")
        }
    };
    info!(user = %username, connection = id, %reason, "session end");
    let _ = ws.send(Message::Close(None)).await;
}

/// A session error the browser should answer by reconnecting (network trouble,
/// not a decision by the user, an administrator or Windows).
#[derive(Debug)]
struct Retryable(String);

impl std::fmt::Display for Retryable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Retryable {}

/// How long the media path may stay down before the session ends. ICE
/// "disconnected" is often brief (Wi-Fi hiccup, a phone pausing a background
/// tab) and recovers on its own.
const ICE_GRACE: Duration = Duration::from_secs(45);

async fn bind_udp(config: &Config) -> anyhow::Result<UdpSocket> {
    let (lo, hi) = config.udp_ports;
    for port in lo..=hi {
        if let Ok(socket) = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], port))).await {
            return Ok(socket);
        }
    }
    bail!("no free UDP port in {lo}-{hi}")
}

/// How often the signaling WebSocket carries a message, so proxies keep it open.
const WS_KEEPALIVE: Duration = Duration::from_secs(30);

/// Desktop size limits, in pixels (width, height).
const MIN_SIZE: (u16, u16) = (640, 480);
const MAX_SIZE: (u16, u16) = (1920, 1080);

fn even_clamp(v: u16, lo: u16, hi: u16) -> u16 {
    v.clamp(lo, hi) & !1
}

async fn run_inner(
    ws: &mut WebSocket,
    conn: Connection,
    config: &Config,
    stop: &tokio::sync::Notify,
) -> anyhow::Result<&'static str> {
    // 1. The browser sends its offer and screen size.
    let start: Start = loop {
        let msg = tokio::time::timeout(Duration::from_secs(15), ws.recv())
            .await
            .context("no offer from the browser")?
            .context("browser closed the connection")??;
        if let Message::Text(text) = msg {
            break serde_json::from_str(&text).context("bad start message")?;
        }
    };
    let width = even_clamp(start.width, MIN_SIZE.0, MAX_SIZE.0);
    let height = even_clamp(start.height, MIN_SIZE.1, MAX_SIZE.1);

    // 2. WebRTC answer. One UDP port per session; the advertised candidate is
    // the public IP (the cloud NAT maps it 1:1 to this host).
    let socket = bind_udp(config).await?;
    let public = SocketAddr::new(config.public_ip, socket.local_addr()?.port());
    let mut rtc = Rtc::builder()
        .clear_codecs()
        .enable_h264(true)
        .enable_opus(true, false)
        .enable_bwe(Some(Bitrate::kbps(800)))
        .set_stats_interval(Some(LINK_STATS_INTERVAL))
        .build(Instant::now());
    rtc.add_local_candidate(Candidate::host(public, "udp")?);
    let offer = SdpOffer::from_sdp_string(&start.offer).context("bad SDP offer")?;
    let answer = rtc.sdp_api().accept_offer(offer).context("could not accept offer")?;
    rtc.bwe()
        .set_bitrate(Bitrate::kbps(150), Bitrate::bps(u64::from(config.max_bitrate)));
    send_json(ws, json!({ "answer": answer.to_sdp_string(), "width": width, "height": height })).await;

    // 3. RDP + encoder.
    let shared = Shared::new(800_000);
    let (input_tx, input_rx) = mpsc::channel::<InputMsg>(256);
    let (frame_tx, mut frame_rx) = mpsc::channel(1);
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    // About 1 s of sound; older packets are dropped if the viewer stalls.
    let (audio_tx, mut audio_rx) = mpsc::channel(50);
    let out = RdpOut { events: events_tx, audio: audio_tx };
    let mut rdp = tokio::spawn(crate::rdp::run(conn, width, height, shared.clone(), input_rx, out));
    let encoder = crate::encode::spawn(shared.clone(), config.max_bitrate, frame_tx);
    let _guard = StopOnDrop(shared.clone());

    let started = Instant::now();
    let mut connected = false;
    let mut video: Option<(Mid, str0m::format::PayloadParams)> = None;
    let mut audio: Option<(Mid, str0m::format::PayloadParams)> = None;
    let mut dc = DcOut::default();
    let mut link = LinkStats::new();
    // RTP time of the next sound packet (48 kHz), and when the last one went out.
    let mut audio_ts: u64 = 0;
    let mut audio_last: Option<Instant> = None;
    let mut clip_in = String::new();
    // Since when the media path is down, if it is.
    let mut ice_lost: Option<Instant> = None;
    let mut keepalive = tokio::time::interval_at(
        tokio::time::Instant::now() + WS_KEEPALIVE,
        WS_KEEPALIVE,
    );
    let mut buf = vec![0u8; 2000];
    let result: anyhow::Result<&'static str> = 'session: loop {
        // Send what waited for room in the SCTP buffer (freed by acks just handled).
        dc.flush(&mut rtc);
        // Drain str0m until it wants to wait.
        let timeout = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Ok(Output::Event(event)) => match event {
                    Event::IceConnectionStateChange(state) => {
                        debug!(?state, "ICE state");
                        if state == IceConnectionState::Disconnected {
                            ice_lost.get_or_insert_with(Instant::now);
                        } else if state.is_connected() {
                            if ice_lost.take().is_some() {
                                info!("media connection recovered");
                                shared.request_keyframe();
                            }
                        }
                    }
                    Event::Connected => {
                        connected = true;
                        info!(ms = started.elapsed().as_millis() as u64, "WebRTC connected");
                        send_json(ws, json!({ "status": "connected" })).await;
                        shared.request_keyframe();
                    }
                    Event::MediaAdded(m) if m.kind == MediaKind::Video => {
                        let params = rtc.writer(m.mid).and_then(|w| {
                            let all: Vec<_> = w.payload_params().cloned().collect();
                            all.iter()
                                .find(|p| p.spec().codec == Codec::H264 && p.spec().format.packetization_mode == Some(1))
                                .or_else(|| all.iter().find(|p| p.spec().codec == Codec::H264))
                                .cloned()
                        });
                        match params {
                            Some(p) => video = Some((m.mid, p)),
                            None => break 'session Err(anyhow::anyhow!("browser does not offer H.264")),
                        }
                    }
                    Event::MediaAdded(m) if m.kind == MediaKind::Audio => {
                        audio = rtc.writer(m.mid).and_then(|w| {
                            w.payload_params().find(|p| p.spec().codec == Codec::Opus).cloned().map(|p| (m.mid, p))
                        });
                        if audio.is_none() {
                            warn!("browser does not offer Opus; no sound");
                        }
                    }
                    Event::ChannelOpen(id, label) => {
                        debug!(%label, "data channel open");
                        dc.chan = Some(id);
                        dc.flush(&mut rtc);
                    }
                    Event::KeyframeRequest(_) => shared.request_keyframe(),
                    Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                        let bps = u32::try_from(estimate.as_u64()).unwrap_or(u32::MAX);
                        shared.target_bps.store(bps.min(config.max_bitrate), Ordering::Relaxed);
                        link.estimate(bps);
                    }
                    Event::MediaEgressStats(m) if video.as_ref().is_some_and(|v| v.0 == m.mid) => {
                        link.media(m.nacks, m.plis + m.firs, m.loss);
                    }
                    Event::PeerStats(p) => {
                        link.log(&p, shared.tier_bps.load(Ordering::Relaxed));
                    }
                    Event::ChannelData(data) => match serde_json::from_slice::<ClientInput>(&data.data) {
                        Ok(ClientInput::Clip { d, more }) => {
                            if clip_in.len() + d.len() <= MAX_CLIPBOARD {
                                clip_in.push_str(&d);
                            }
                            if !more {
                                // Clipboard must not be lost to a full queue: wait for room.
                                let text = std::mem::take(&mut clip_in);
                                let _ = input_tx.send(InputMsg::Clipboard(text)).await;
                            }
                        }
                        Ok(msg) => {
                            let _ = input_tx.try_send(to_input(msg));
                        }
                        Err(e) => debug!("bad input message: {e}"),
                    },
                    _ => {}
                },
                Err(e) => break 'session Err(e.into()),
            }
        };

        if !rtc.is_alive() {
            break Err(Retryable("media connection closed".into()).into());
        }
        if ice_lost.is_some_and(|t| t.elapsed() > ICE_GRACE) {
            break Err(Retryable("media connection lost".into()).into());
        }
        if !connected && started.elapsed() > Duration::from_secs(20) {
            // Retryable: the browser retries only when this was a reconnect
            // (it worked before), so a first-time failure still shows the message.
            break Err(Retryable(format!(
                "media connection failed: UDP {}-{} may be blocked",
                config.udp_ports.0, config.udp_ports.1
            ))
            .into());
        }

        let sleep_until = tokio::time::Instant::from_std(timeout.min(Instant::now() + Duration::from_secs(1)));
        let input = tokio::select! {
            r = socket.recv_from(&mut buf) => {
                let (n, source) = r?;
                match buf[..n].try_into() {
                    Ok(contents) => Input::Receive(
                        Instant::now(),
                        Receive { proto: Protocol::Udp, source, destination: public, contents },
                    ),
                    Err(_) => continue,
                }
            }
            _ = tokio::time::sleep_until(sleep_until) => Input::Timeout(Instant::now()),
            frame = frame_rx.recv() => {
                let Some(frame) = frame else { break Ok("encoder stopped") };
                link.frame(frame.data.len(), frame.keyframe, frame.encode_ms);
                if let (true, Some((mid, params))) = (connected, &video) {
                    if let Some(writer) = rtc.writer(*mid) {
                        let ts = MediaTime::from_90khz(started.elapsed().as_micros() as u64 * 9 / 100);
                        if let Err(e) = writer.write(params.pt(), Instant::now(), ts, frame.data) {
                            break Err(e.into());
                        }
                    }
                }
                continue;
            }
            Some(event) = events_rx.recv() => {
                send_event(&mut rtc, &mut dc, event);
                continue;
            }
            Some(packet) = audio_rx.recv() => {
                if let (true, Some((mid, params))) = (connected, &audio) {
                    let now = Instant::now();
                    // After a pause (Windows sends nothing during silence), restart
                    // from the wall clock so the browser sees the gap in time.
                    if audio_last.is_none_or(|t| now.duration_since(t) > Duration::from_millis(200)) {
                        audio_ts = audio_ts.max(started.elapsed().as_micros() as u64 * 48 / 1000);
                    }
                    audio_last = Some(now);
                    if let Some(writer) = rtc.writer(*mid) {
                        let ts = MediaTime::new(audio_ts, Frequency::FORTY_EIGHT_KHZ);
                        if let Err(e) = writer.write(params.pt(), now, ts, packet.data) {
                            break Err(e.into());
                        }
                    }
                    audio_ts += 960;
                }
                continue;
            }
            _ = stop.notified() => break Err(anyhow::anyhow!("An administrator ended this session.")),
            _ = keepalive.tick() => {
                // Proxies (Cloudflare: 100 s) drop idle WebSockets, which would end the session.
                send_json(ws, json!({ "t": "ping" })).await;
                continue;
            }
            msg = ws.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break Ok("viewer closed the connection"),
                _ => continue,
            },
            r = &mut rdp => {
                break match r {
                    Ok(Ok(())) => Err(anyhow::anyhow!("the remote computer ended the session")),
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(e.into()),
                };
            }
        };
        if let Err(e) = rtc.handle_input(input) {
            break Err(e.into());
        }
    };

    // Let RDP log off cleanly (closing the input channel triggers it).
    drop(input_tx);
    shared.stop();
    if !rdp.is_finished() {
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut rdp).await;
        rdp.abort();
    }
    let _ = tokio::task::spawn_blocking(move || encoder.join()).await;
    result
}

/// How often each session logs its link quality.
const LINK_STATS_INTERVAL: Duration = Duration::from_secs(30);

/// Per-session numbers for the periodic `link` log line: what the network
/// estimate did, what the encoder sent, and what the browser reported.
struct LinkStats {
    est_min: u32,
    est_max: u32,
    frames: u32,
    bytes: u64,
    keyframes: u32,
    max_frame: usize,
    max_encode_ms: u32,
    /// Cumulative counters from the browser's RTCP, and their last logged values.
    nacks: u64,
    plis: u64,
    logged_nacks: u64,
    logged_plis: u64,
    /// Fraction of video packets the browser reports lost.
    loss: Option<f32>,
    since: Instant,
}

impl LinkStats {
    fn new() -> Self {
        Self {
            est_min: u32::MAX,
            est_max: 0,
            frames: 0,
            bytes: 0,
            keyframes: 0,
            max_frame: 0,
            max_encode_ms: 0,
            nacks: 0,
            plis: 0,
            logged_nacks: 0,
            logged_plis: 0,
            loss: None,
            since: Instant::now(),
        }
    }

    fn estimate(&mut self, bps: u32) {
        self.est_min = self.est_min.min(bps);
        self.est_max = self.est_max.max(bps);
    }

    fn frame(&mut self, len: usize, keyframe: bool, encode_ms: u32) {
        self.max_encode_ms = self.max_encode_ms.max(encode_ms);
        self.frames += 1;
        self.bytes += len as u64;
        self.keyframes += u32::from(keyframe);
        self.max_frame = self.max_frame.max(len);
    }

    fn media(&mut self, nacks: u64, plis: u64, loss: Option<f32>) {
        self.nacks = nacks;
        self.plis = plis;
        if loss.is_some() {
            self.loss = loss;
        }
    }

    fn log(&mut self, peer: &str0m::stats::PeerStats, tier: u32) {
        let secs = self.since.elapsed().as_secs_f64().max(1.0);
        let kbps = |b: u32| if b == u32::MAX { 0 } else { b / 1000 };
        info!(
            rtt_ms = peer.rtt.map_or(-1, |d| d.as_millis() as i64),
            loss_pct = format_args!("{:.1}", self.loss.or(peer.egress_loss_fraction).unwrap_or(0.0) * 100.0),
            est_kbps = format_args!("{}-{}", kbps(self.est_min), kbps(self.est_max)),
            enc_kbps = tier / 1000,
            sent_kbps = (self.bytes as f64 * 8.0 / secs / 1000.0) as u64,
            fps = format_args!("{:.1}", f64::from(self.frames) / secs),
            keyframes = self.keyframes,
            max_frame_kb = self.max_frame / 1024,
            max_encode_ms = self.max_encode_ms,
            nacks = self.nacks - self.logged_nacks.min(self.nacks),
            plis = self.plis - self.logged_plis.min(self.plis),
            "link"
        );
        let (nacks, plis) = (self.nacks, self.plis);
        *self = Self::new();
        self.logged_nacks = nacks;
        self.logged_plis = plis;
        self.nacks = nacks;
        self.plis = plis;
    }
}

struct StopOnDrop(Arc<Shared>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}

#[cfg(test)]
mod dc_tests {
    use super::*;
    use ironrdp_graphics::pointer::DecodedPointer;

    #[test]
    fn cursor_image_sent_once() {
        let p = Arc::new(DecodedPointer {
            width: 2,
            height: 2,
            hotspot_x: 1,
            hotspot_y: 0,
            bitmap_data: vec![255; 16],
        });
        let mut sent = std::collections::HashSet::new();
        let (first, id) = cursor_message(&CursorState::Shape(p.clone()), &sent);
        assert!(first.contains("rgba"));
        sent.insert(id.expect("first use carries the image"));
        let (again, id) = cursor_message(&CursorState::Shape(p), &sent);
        assert!(!again.contains("rgba") && again.contains("\"id\"") && id.is_none());
    }
}
