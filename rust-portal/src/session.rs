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
use tracing::{debug, info, warn};

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
}

impl Shared {
    pub fn new(initial_bps: u32) -> Arc<Self> {
        Arc::new(Self {
            image: Mutex::new(None),
            flags: Mutex::new(Flags::default()),
            wake: Condvar::new(),
            target_bps: AtomicU32::new(initial_bps),
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

/// Sends a JSON message to the browser on the data channel.
fn send_dc(rtc: &mut Rtc, chan: Option<ChannelId>, value: &serde_json::Value) {
    if let Some(mut ch) = chan.and_then(|id| rtc.channel(id)) {
        if let Err(e) = ch.write(false, value.to_string().as_bytes()) {
            debug!("data channel write failed: {e}");
        }
    }
}

fn send_event(rtc: &mut Rtc, chan: Option<ChannelId>, event: RdpEvent) {
    use base64::Engine as _;
    match event {
        RdpEvent::Cursor(p) => {
            // Browsers ignore CSS cursors larger than 128 px.
            if p.width == 0 || p.height == 0 || p.width > 128 || p.height > 128 {
                return send_dc(rtc, chan, &json!({ "t": "cursor", "kind": "default" }));
            }
            let rgba = base64::engine::general_purpose::STANDARD.encode(&p.bitmap_data);
            send_dc(
                rtc,
                chan,
                &json!({ "t": "cursor", "w": p.width, "h": p.height, "x": p.hotspot_x, "y": p.hotspot_y, "rgba": rgba }),
            );
        }
        RdpEvent::CursorDefault => send_dc(rtc, chan, &json!({ "t": "cursor", "kind": "default" })),
        RdpEvent::CursorHidden => send_dc(rtc, chan, &json!({ "t": "cursor", "kind": "none" })),
        RdpEvent::Clipboard(text) => {
            let chars: Vec<char> = text.chars().collect();
            let parts: Vec<String> = chars.chunks(CLIP_CHUNK).map(|c| c.iter().collect()).collect();
            if parts.is_empty() {
                return send_dc(rtc, chan, &json!({ "t": "clip", "d": "", "more": false }));
            }
            let last = parts.len() - 1;
            for (i, part) in parts.into_iter().enumerate() {
                send_dc(rtc, chan, &json!({ "t": "clip", "d": part, "more": i < last }));
            }
        }
        RdpEvent::Size { width, height } => send_dc(rtc, chan, &json!({ "t": "size", "w": width, "h": height })),
        RdpEvent::ResizeUnsupported => send_dc(rtc, chan, &json!({ "t": "size_unsupported" })),
    }
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
    let reason = match run_inner(&mut ws, conn, &config, &stop).await {
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
struct Retryable(&'static str);

impl std::fmt::Display for Retryable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
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
const MAX_SIZE: (u16, u16) = (2560, 1600);

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
    let mut chan: Option<ChannelId> = None;
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
                        chan = Some(id);
                    }
                    Event::KeyframeRequest(_) => shared.request_keyframe(),
                    Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                        let bps = u32::try_from(estimate.as_u64()).unwrap_or(u32::MAX);
                        shared.target_bps.store(bps.min(config.max_bitrate), Ordering::Relaxed);
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
            break Err(Retryable("media connection closed").into());
        }
        if ice_lost.is_some_and(|t| t.elapsed() > ICE_GRACE) {
            break Err(Retryable("media connection lost").into());
        }
        if !connected && started.elapsed() > Duration::from_secs(20) {
            break Err(anyhow::anyhow!(
                "media connection failed: UDP {}-{} may be blocked",
                config.udp_ports.0,
                config.udp_ports.1
            ));
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
                if let (true, Some((mid, params))) = (connected, &video) {
                    if let Some(writer) = rtc.writer(*mid) {
                        let ts = MediaTime::from_90khz(started.elapsed().as_micros() as u64 * 9 / 100);
                        if let Err(e) = writer.write(params.pt(), Instant::now(), ts, frame.data) {
                            break Err(e.into());
                        }
                    }
                }
                let _ = frame.keyframe;
                continue;
            }
            Some(event) = events_rx.recv() => {
                send_event(&mut rtc, chan, event);
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

struct StopOnDrop(Arc<Shared>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}
