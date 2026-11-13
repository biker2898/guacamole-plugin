//! RDP client side: connects to a Windows host with IronRDP, keeps the decoded
//! framebuffer in [`Shared`], and turns browser input into RDP input events.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use ironrdp_connector::connection_activation::ConnectionActivationState;
use ironrdp_connector::sspi::generator::NetworkRequest;
use ironrdp_cliprdr::CliprdrClient;
use ironrdp_cliprdr::pdu::{ClipboardFormat, ClipboardFormatId, FormatDataResponse};
use ironrdp_connector::{ClientConnector, ConnectionOutcome, ConnectorResult, Credentials, DesktopSize, ServerName};
use ironrdp_core::WriteBuf;
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_graphics::pointer::DecodedPointer;
use ironrdp_input::{Database, Operation};
use ironrdp_pdu::gcc::{ConnectionType, KeyboardType};
use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp_pdu::rdp::capability_sets::{MajorPlatformType, RailSupportLevel};
use ironrdp_pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp_rdpdr::{NoopRdpdrBackend, Rdpdr};
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_dvc::DrdynvcClient;
use ironrdp_rdpsnd::client::{Rdpsnd, RdpsndDvcState};
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::{FramedWrite as _, NetworkClient, TokioFramed};
use sqlx::SqlitePool;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::channels::{AudioHandler, AudioPacket, ClipBackend, ClipEvent};
use crate::db::Connection;
use crate::session::Shared;

/// Input from the browser, already translated to RDP terms.
pub enum InputMsg {
    Ops(Vec<Operation>),
    ReleaseAll,
    /// The browser's clipboard text, to offer to Windows.
    Clipboard(String),
    /// The viewer wants a new desktop size (already clamped and even).
    Resize { width: u16, height: u16 },
}

/// What the RDP side tells the viewer, besides video and sound.
pub enum RdpEvent {
    Cursor(Arc<DecodedPointer>),
    CursorDefault,
    CursorHidden,
    /// Text Windows copied.
    Clipboard(String),
    /// The desktop changed size (after a resize request).
    Size { width: u16, height: u16 },
    /// Windows cannot resize this session live; the viewer reconnects instead.
    ResizeUnsupported,
}

/// Where the RDP task sends things for the viewer.
pub struct RdpOut {
    pub events: mpsc::UnboundedSender<RdpEvent>,
    pub audio: mpsc::Sender<AudioPacket>,
}

/// Largest clipboard text passed either way, in bytes of UTF-8.
pub const MAX_CLIPBOARD: usize = 1 << 20;

/// CredSSP only needs a network client for Kerberos; we use NTLM.
struct NoNetwork;

impl NetworkClient for NoNetwork {
    async fn send(&mut self, _request: &NetworkRequest) -> ConnectorResult<Vec<u8>> {
        Err(ironrdp_connector::general_err!("Kerberos is not supported"))
    }
}

fn build_config(conn: &Connection, width: u16, height: u16) -> ironrdp_connector::Config {
    ironrdp_connector::Config {
        desktop_size: DesktopSize { width, height },
        monitor_layout: None,
        desktop_scale_factor: 0,
        desktop_physical_size: None,
        desktop_orientation: None,
        device_scale_factor: None,
        monitor_extended_layout: None,
        relative_mouse_input: false,
        enable_tls: true,
        enable_credssp: true,
        enable_standard_rdp_security: false,
        credentials: Credentials::UsernamePassword {
            username: conn.username.clone(),
            password: conn.password.clone(),
        },
        domain: conn.domain.clone(),
        client_build: 0,
        client_name: "rust-portal".to_owned(),
        keyboard_type: KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0,
        connection_type: ConnectionType::Lan,
        ime_file_name: String::new(),
        bitmap: None,
        dig_product_id: String::new(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: RailSupportLevel::empty(),
        platform: MajorPlatformType::UNIX,
        hardware_id: None,
        request_data: None,
        autologon: false,
        enable_audio_playback: true,
        enable_audio_capture: false,
        performance_flags: PerformanceFlags::DISABLE_WALLPAPER | PerformanceFlags::ENABLE_FONT_SMOOTHING,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        // The VM link is a LAN: decompression would only cost CPU here.
        compression_type: None,
        // The browser draws the cursor itself: it moves with no network delay,
        // and Windows leaves it out of the video.
        enable_server_pointer: true,
        pointer_software_rendering: false,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: false,
    }
}

type Framed = TokioFramed<ironrdp_tls::TlsStream<TcpStream>>;

async fn connect(
    conn: &Connection,
    width: u16,
    height: u16,
    audio: [AudioHandler; 2],
    clip: ClipBackend,
) -> anyhow::Result<(ironrdp_connector::ConnectionResult, Framed, ironrdp_connector::Config)> {
    let config = build_config(conn, width, height);
    let port = u16::try_from(conn.port).unwrap_or(3389);
    let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((conn.hostname.as_str(), port)))
        .await
        .context("timed out connecting to the computer")?
        .context("could not reach the computer")?;
    tcp.set_nodelay(true)?;
    let client_addr = tcp.local_addr()?;

    let mut framed = TokioFramed::new(tcp);
    // Sound: Windows 8 and later use the AUDIO_PLAYBACK_DVC dynamic channel
    // and fall back to the static RDPSND one; both are offered. Windows only
    // sends sound to clients that also have the device redirection channel
    // (as FreeRDP does), so RDPDR is announced, with no devices in it.
    let [audio_static, audio_dvc] = audio;
    // Windows opens the lossy channel too and drops sound if it is refused;
    // both share one negotiation state.
    let audio_dvc = RdpsndDvcState::new(Box::new(audio_dvc));
    let mut connector = ClientConnector::new(config.clone(), client_addr)
        .with_static_channel(Rdpdr::new(Box::new(NoopRdpdrBackend), "rust-portal".to_owned()))
        .with_static_channel(Rdpsnd::new(Box::new(audio_static)))
        .with_static_channel(
            DrdynvcClient::new()
                .with_dynamic_channel(audio_dvc.channel())
                .with_dynamic_channel(audio_dvc.lossy_channel())
                // Live resolution changes (MS-RDPEDISP, Windows 8.1 / 2012 R2 and later).
                .with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new()))),
        )
        .with_static_channel(CliprdrClient::new(Box::new(clip)));
    let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
        .await
        .context("RDP negotiation")?;

    let initial = framed.into_inner_no_leftover();
    let (tls, cert) = ironrdp_tls::upgrade(initial, &conn.hostname).await.context("TLS upgrade")?;
    let server_public_key = ironrdp_tls::extract_tls_server_public_key(&cert)
        .context("server certificate has no public key")?
        .to_owned();
    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);

    let mut framed = TokioFramed::new(tls);
    let outcome = ironrdp_tokio::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut NoNetwork,
        ServerName::new(conn.hostname.clone()),
        server_public_key,
        None,
    )
    .await
    .context("RDP login")?;

    match outcome {
        ConnectionOutcome::Connected(result) => Ok((result, framed, config)),
        ConnectionOutcome::Redirected(_) => bail!("server redirection is not supported"),
    }
}

/// Runs one RDP session until the server ends it or `input_rx` closes.
pub async fn run(
    conn: Connection,
    width: u16,
    height: u16,
    shared: Arc<Shared>,
    mut input_rx: mpsc::Receiver<InputMsg>,
    out: RdpOut,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let clip_events = Arc::new(Mutex::new(Vec::new()));
    let audio = [AudioHandler::new(out.audio.clone())?, AudioHandler::new(out.audio.clone())?];
    let (result, mut framed, _config) =
        connect(&conn, width, height, audio, ClipBackend::new(clip_events.clone())).await?;
    info!(
        connection = conn.id,
        width = result.desktop_size.width,
        height = result.desktop_size.height,
        ms = started.elapsed().as_millis() as u64,
        "RDP connected"
    );

    shared.set_image(DecodedImage::new(
        PixelFormat::RgbA32,
        result.desktop_size.width,
        result.desktop_size.height,
    ));

    let io_channel_id = result.io_channel_id;
    let user_channel_id = result.user_channel_id;
    let activation_factory = result.activation_factory;
    let mut stage = ActiveStageBuilder {
        static_channels: result.static_channels,
        user_channel_id: result.user_channel_id,
        io_channel_id: result.io_channel_id,
        message_channel_id: result.message_channel_id,
        share_id: result.share_id,
        compression_type: result.compression_type,
        enable_server_pointer: result.enable_server_pointer,
        pointer_software_rendering: result.pointer_software_rendering,
    }
    .build();
    let mut input = Database::new();
    let mut clip = Clipboard::default();
    let mut clip_timer = tokio::time::interval(Duration::from_secs(5));
    let mut resize = Resize { wanted: None, connected_at: Instant::now() };

    loop {
        let outputs = tokio::select! {
            r = framed.read_pdu() => {
                let (action, payload) = r.context("connection to the computer lost")?;
                let received_at = framed.last_read_at();
                shared.with_image(|image| stage.process(image, action, &payload, received_at))?
            }
            msg = input_rx.recv() => {
                let events = match msg {
                    Some(InputMsg::Ops(ops)) => apply_input(&mut input, ops),
                    Some(InputMsg::ReleaseAll) => input.release_all().into_vec(),
                    Some(InputMsg::Clipboard(text)) => {
                        clip.local = Some(text);
                        if let Some(c) = stage.get_svc_processor_mut::<CliprdrClient>().filter(|c| c.is_ready()) {
                            let msgs = c.initiate_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)])?;
                            let bytes = stage.process_svc_processor_messages(msgs)?;
                            framed.write_all(&bytes).await?;
                        }
                        continue;
                    }
                    Some(InputMsg::Resize { width, height }) => {
                        resize.wanted = Some((width, height));
                        resize.try_send(&mut stage, &mut framed, &out, conn.id).await?;
                        continue;
                    }
                    None => {
                        // The viewer left: log off cleanly so Windows keeps the session.
                        let frames = stage.graceful_shutdown()?;
                        for f in frames {
                            if let ActiveStageOutput::ResponseFrame(bytes) = f {
                                framed.write_all(&bytes).await?;
                            }
                        }
                        // Give the server a moment to answer, then stop.
                        let _ = tokio::time::timeout(Duration::from_secs(2), framed.read_pdu()).await;
                        return Ok(());
                    }
                };
                shared.with_image(|image| stage.process_fastpath_input(image, &events))?
            }
            _ = clip_timer.tick() => {
                resize.try_send(&mut stage, &mut framed, &out, conn.id).await?;
                if let Some(c) = stage.get_svc_processor_mut::<CliprdrClient>() {
                    let msgs = c.drive_timeouts()?;
                    let bytes = stage.process_svc_processor_messages(msgs)?;
                    if !bytes.is_empty() {
                        framed.write_all(&bytes).await?;
                    }
                }
                continue;
            }
        };

        let mut changed = false;
        for output in outputs {
            match output {
                ActiveStageOutput::ResponseFrame(bytes) => framed.write_all(&bytes).await?,
                ActiveStageOutput::GraphicsUpdate(_) => changed = true,
                ActiveStageOutput::Terminate(reason) => {
                    info!(connection = conn.id, ?reason, "RDP session ended by server");
                    return Ok(());
                }
                ActiveStageOutput::DeactivateAll => {
                    let (width, height) =
                        reactivate(&mut framed, &mut stage, &activation_factory, io_channel_id, user_channel_id, &shared)
                            .await?;
                    let _ = out.events.send(RdpEvent::Size { width, height });
                    changed = true;
                }
                ActiveStageOutput::PointerBitmap(p) => {
                    let _ = out.events.send(RdpEvent::Cursor(p));
                }
                ActiveStageOutput::PointerDefault => {
                    let _ = out.events.send(RdpEvent::CursorDefault);
                }
                ActiveStageOutput::PointerHidden => {
                    let _ = out.events.send(RdpEvent::CursorHidden);
                }
                ActiveStageOutput::PointerPosition { .. } => {}
                other => debug!(?other, "unhandled RDP output"),
            }
        }
        if changed {
            shared.mark_dirty();
        }

        let events = std::mem::take(&mut *clip_events.lock().unwrap());
        for event in events {
            if let Some(bytes) = clip.handle(&mut stage, event, &out)? {
                framed.write_all(&bytes).await?;
            }
        }
        resize.try_send(&mut stage, &mut framed, &out, conn.id).await?;
    }
}

/// How long after logon Windows may take to open the display control channel.
const DISPLAY_CONTROL_WAIT: Duration = Duration::from_secs(15);

/// A desktop size the viewer asked for, sent once Windows can take it.
struct Resize {
    wanted: Option<(u16, u16)>,
    connected_at: Instant,
}

impl Resize {
    async fn try_send(&mut self, stage: &mut ActiveStage, framed: &mut Framed, out: &RdpOut, id: i64) -> anyhow::Result<()> {
        let Some((width, height)) = self.wanted else {
            return Ok(());
        };
        match stage.encode_resize(u32::from(width), u32::from(height), None, None) {
            Some(Ok(bytes)) => {
                self.wanted = None;
                info!(connection = id, width, height, "resize requested");
                framed.write_all(&bytes).await?;
            }
            Some(Err(e)) => {
                self.wanted = None;
                warn!("resize failed: {e}");
            }
            // Channel not open (yet): wait a while, then let the viewer reconnect instead.
            None if self.connected_at.elapsed() > DISPLAY_CONTROL_WAIT => {
                self.wanted = None;
                info!(connection = id, "live resize not available; viewer reconnects");
                let _ = out.events.send(RdpEvent::ResizeUnsupported);
            }
            None => {}
        }
        Ok(())
    }
}

/// Turns browser input into RDP events. A key pressed again while held (the
/// browser's auto-repeat) goes out as a plain press, as a real keyboard's
/// repeat does; `Database::apply` would send release + press, which Windows
/// counts as separate taps (five Shift taps turn Sticky Keys on).
fn apply_input(input: &mut Database, ops: Vec<Operation>) -> Vec<FastPathInputEvent> {
    let mut events = Vec::new();
    for op in ops {
        match op {
            Operation::KeyPressed(scancode) if input.is_key_pressed(scancode) => {
                let (extended, code) = scancode.as_u8();
                let flags = if extended { KeyboardFlags::EXTENDED } else { KeyboardFlags::empty() };
                events.push(FastPathInputEvent::KeyboardEvent(flags, code));
            }
            op => events.extend(input.apply([op])),
        }
    }
    events
}

/// Clipboard state of one session (text only).
#[derive(Default)]
struct Clipboard {
    /// The browser's latest clipboard text.
    local: Option<String>,
}

impl Clipboard {
    /// Acts on one channel event; returns bytes to send to the server, if any.
    fn handle(&mut self, stage: &mut ActiveStage, event: ClipEvent, out: &RdpOut) -> anyhow::Result<Option<Vec<u8>>> {
        let Some(c) = stage.get_svc_processor_mut::<CliprdrClient>() else {
            return Ok(None);
        };
        let msgs = match event {
            ClipEvent::RequestFormatList => {
                let formats: &[ClipboardFormat] = if self.local.is_some() {
                    &[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]
                } else {
                    &[]
                };
                c.initiate_copy(formats)?
            }
            ClipEvent::RemoteCopy { has_text: true } => c.initiate_paste(ClipboardFormatId::CF_UNICODETEXT)?,
            ClipEvent::RemoteCopy { has_text: false } => return Ok(None),
            ClipEvent::DataRequest(format) => {
                let response = match (&self.local, format) {
                    (Some(text), ClipboardFormatId::CF_UNICODETEXT) => FormatDataResponse::new_unicode_string(text),
                    _ => FormatDataResponse::new_error(),
                };
                c.submit_format_data(response)?
            }
            ClipEvent::DataResponse(Some(mut text)) => {
                if text.len() > MAX_CLIPBOARD {
                    let mut end = MAX_CLIPBOARD;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                }
                let _ = out.events.send(RdpEvent::Clipboard(text));
                return Ok(None);
            }
            ClipEvent::DataResponse(None) => return Ok(None),
        };
        Ok(Some(stage.process_svc_processor_messages(msgs)?))
    }
}

/// Deactivation-Reactivation Sequence: the server re-negotiates capabilities,
/// for example after a resolution change.
async fn reactivate(
    framed: &mut Framed,
    stage: &mut ActiveStage,
    factory: &ironrdp_connector::connection_activation::ConnectionActivationFactory,
    io_channel_id: u16,
    user_channel_id: u16,
    shared: &Shared,
) -> anyhow::Result<(u16, u16)> {
    let mut sequence = factory.create();
    let mut buf = WriteBuf::new();
    loop {
        ironrdp_tokio::single_sequence_step(framed, &mut sequence, &mut buf)
            .await
            .context("RDP reactivation")?;
        if let ConnectionActivationState::Finalized {
            desktop_size,
            share_id,
            enable_server_pointer,
            pointer_software_rendering,
            static_channel_chunk_size,
            ..
        } = sequence.connection_activation_state()
        {
            if !stage.reactivate(
                io_channel_id,
                user_channel_id,
                share_id,
                enable_server_pointer,
                pointer_software_rendering,
                static_channel_chunk_size,
            ) {
                bail!("server sent an invalid virtual channel chunk size");
            }
            info!(width = desktop_size.width, height = desktop_size.height, "RDP reactivated");
            shared.set_image(DecodedImage::new(PixelFormat::RgbA32, desktop_size.width, desktop_size.height));
            return Ok((desktop_size.width, desktop_size.height));
        }
    }
}

/// `rust-portal probe <id>`: connects, wiggles the mouse, encodes for a few
/// seconds and prints what the pipeline did. Checks RDP + H.264 without a browser.
pub async fn probe(pool: &SqlitePool, id: i64) -> anyhow::Result<()> {
    let conn = crate::db::connection_by_id(pool, id).await?.context("no such connection")?;
    let shared = Shared::new(1_500_000);
    let (input_tx, input_rx) = mpsc::channel(64);
    let (frame_tx, mut frame_rx) = mpsc::channel(1);
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (audio_tx, mut audio_rx) = mpsc::channel(256);
    let out = RdpOut { events: events_tx, audio: audio_tx };
    let rdp = tokio::spawn(run(conn, 1366, 768, shared.clone(), input_rx, out));
    let encoder = crate::encode::spawn(shared.clone(), 6_000_000, frame_tx);

    let begin = Instant::now();
    let deadline = begin + Duration::from_secs(12);
    let mut resize_sent = false;
    let mut resized = None;
    let (mut frames, mut bytes, mut keyframes) = (0u32, 0usize, 0u32);
    let (mut cursors, mut audio_packets, mut audio_bytes) = (0u32, 0u32, 0usize);
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut x = 100u16;
    while Instant::now() < deadline {
        tokio::select! {
            f = frame_rx.recv() => match f {
                Some(f) => { frames += 1; bytes += f.data.len(); keyframes += u32::from(f.keyframe); }
                None => break,
            },
            Some(e) = events_rx.recv() => {
                match e {
                    RdpEvent::Cursor(p) => {
                        cursors += 1;
                        debug!(w = p.width, h = p.height, "cursor");
                    }
                    RdpEvent::Size { width, height } => resized = Some(format!("{width}x{height}")),
                    RdpEvent::ResizeUnsupported => resized = Some("unsupported".to_owned()),
                    _ => {}
                }
            }
            Some(p) = audio_rx.recv() => { audio_packets += 1; audio_bytes += p.data.len(); }
            _ = tick.tick() => {
                x = if x > 1200 { 100 } else { x + 20 };
                let _ = input_tx.send(InputMsg::Ops(vec![Operation::MouseMove(ironrdp_input::MousePosition { x, y: 400 })])).await;
                // Halfway through, check live resizing.
                if !resize_sent && begin.elapsed() > Duration::from_secs(5) {
                    resize_sent = true;
                    let _ = input_tx.send(InputMsg::Resize { width: 1280, height: 720 }).await;
                }
            }
        }
        if rdp.is_finished() {
            break;
        }
    }
    drop(input_tx);
    shared.stop();
    match tokio::time::timeout(Duration::from_secs(5), rdp).await {
        Ok(Ok(Err(e))) => warn!("RDP error: {e:#}"),
        Ok(Err(e)) => warn!("RDP task failed: {e}"),
        _ => {}
    }
    let _ = encoder.join();
    println!(
        "frames={frames} keyframes={keyframes} bytes={bytes} avg_kbps={} cursors={cursors} audio_packets={audio_packets} audio_bytes={audio_bytes} resized={}",
        bytes * 8 / 12 / 1000,
        resized.as_deref().unwrap_or("no answer")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_input::Scancode;

    #[test]
    fn held_key_repeats_without_release() {
        let mut input = Database::new();
        let shift = Scancode::from_u16(0x2a);
        let press = |input: &mut Database| apply_input(input, vec![Operation::KeyPressed(shift)]);
        let first = press(&mut input);
        let repeat = press(&mut input);
        let release = apply_input(&mut input, vec![Operation::KeyReleased(shift)]);
        let is_release = |e: &FastPathInputEvent| {
            matches!(e, FastPathInputEvent::KeyboardEvent(f, 0x2a) if f.contains(KeyboardFlags::RELEASE))
        };
        assert_eq!((first.len(), repeat.len(), release.len()), (1, 1, 1));
        assert!(!is_release(&first[0]) && !is_release(&repeat[0]) && is_release(&release[0]));
    }
}
