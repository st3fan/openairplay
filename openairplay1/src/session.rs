//! Per-connection RAOP session state machine: ANNOUNCE → SETUP → RECORD, the
//! bound UDP channels, and the audio-receiver task that decrypts incoming
//! packets.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine;
use log::{debug, info, warn};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::clock::{self, ClockModel};
use crate::crypto;
use crate::dmap;
use crate::events::{Event, EventSender};
use crate::jitter::{Delivery, JitterBuffer};
use crate::player::{Player, PlayerSender};
use crate::rtp::{self, AudioPacket};
use crate::rtsp::{Request, Response};
use crate::sdp::{AlacConfig, Sdp};
use crate::sink::SinkFactory;

/// How often the audio task services retransmit requests and forced skips.
const SERVICE_INTERVAL: Duration = Duration::from_millis(20);
/// Minimum spacing between resend requests for the same sequence number.
const RESEND_BACKOFF: Duration = Duration::from_millis(80);
/// `SET_PARAMETER` content type carrying DMAP track metadata.
const DMAP_CONTENT_TYPE: &str = "application/x-dmap-tagged";

/// A single-streaming-session gate shared across RTSP connections. AirPlay 1
/// senders assume exclusive use of the receiver; a second one that reaches
/// SETUP while another is streaming is refused.
#[derive(Clone, Default)]
pub struct SessionSlot(Arc<AtomicBool>);

impl SessionSlot {
    pub fn new() -> SessionSlot {
        SessionSlot(Arc::new(AtomicBool::new(false)))
    }

    /// Take the slot if free. The returned guard releases it on drop.
    fn try_acquire(&self) -> Option<SlotGuard> {
        self.0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| SlotGuard(self.0.clone()))
    }
}

struct SlotGuard(Arc<AtomicBool>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A decrypted audio packet, surfaced to an optional observer (the
/// integration tests use it to prove the crypto path; production passes no
/// observer).
#[derive(Debug, Clone)]
pub struct DecryptedAudio {
    /// RTP sequence number of the packet.
    pub sequence: u16,
    /// RTP timestamp (derived from the first packet's anchor).
    pub timestamp: u32,
    /// Whether the plaintext passes a basic ALAC stereo sanity check.
    pub looks_like_alac: bool,
    /// The decrypted ALAC frame, exactly as carried in the RTP payload.
    pub frame: Vec<u8>,
}

/// The receiving end of the decrypted-audio observation channel.
pub type AudioObserver = tokio::sync::mpsc::UnboundedSender<DecryptedAudio>;

/// Cryptographic and format parameters captured at ANNOUNCE.
#[derive(Clone)]
struct StreamParams {
    encrypted: bool,
    key: [u8; 16],
    iv: [u8; 16],
    alac: AlacConfig,
}

pub struct Session {
    params: Option<StreamParams>,
    tasks: Vec<JoinHandle<()>>,
    observer: Option<AudioObserver>,
    /// Creates the stream's audio sink at SETUP.
    sink_factory: SinkFactory,
    /// Session milestones for the host.
    events: EventSender,
    /// True between SETUP and TEARDOWN/drop; guards the one-time
    /// [`Event::SessionEnded`].
    streaming: bool,
    /// Metadata/artwork that arrived while no session was active (senders
    /// may push them earlier in the handshake, before SETUP). The latest of
    /// each is latched here and delivered right after `SessionStarted`, so
    /// the host only ever sees them inside a session.
    pending_metadata: Option<Event>,
    pending_artwork: Option<Event>,
    player: Option<Player>,
    /// Signals the audio task to flush its jitter buffer to a given sequence
    /// (or re-anchor when `None`).
    flush_tx: Option<tokio::sync::mpsc::UnboundedSender<Option<u16>>>,
    /// The IP the client connected from, for addressing resend requests.
    peer_ip: IpAddr,
    /// Shared single-session gate and this session's guard once acquired.
    slot: SessionSlot,
    slot_guard: Option<SlotGuard>,
    /// Local UDP ports handed to the client in the SETUP response.
    local_audio_port: u16,
    local_control_port: u16,
    local_timing_port: u16,
}

impl Session {
    pub fn new(
        observer: Option<AudioObserver>,
        sink_factory: SinkFactory,
        events: EventSender,
        peer_ip: IpAddr,
        slot: SessionSlot,
    ) -> Self {
        Session {
            observer,
            sink_factory,
            events,
            streaming: false,
            pending_metadata: None,
            pending_artwork: None,
            player: None,
            flush_tx: None,
            peer_ip,
            slot,
            slot_guard: None,
            params: None,
            tasks: Vec::new(),
            local_audio_port: 0,
            local_control_port: 0,
            local_timing_port: 0,
        }
    }

    /// Handle ANNOUNCE: parse SDP, decrypt the session key, store format and
    /// crypto parameters. Returns the RTSP response (200, or 456 on invalid
    /// crypto material).
    pub fn handle_announce(&mut self, request: &Request) -> Response {
        let body = String::from_utf8_lossy(&request.body);
        let sdp = Sdp::parse(&body);

        let Some(alac) = sdp.fmtp.as_deref().and_then(AlacConfig::parse) else {
            warn!("ANNOUNCE without a usable a=fmtp ALAC line");
            return Response::new(400, "Bad Request");
        };

        let params = match (sdp.rsaaeskey.as_deref(), sdp.aesiv.as_deref()) {
            (None, None) => {
                info!("ANNOUNCE: unencrypted stream, {} Hz", alac.sample_rate);
                StreamParams {
                    encrypted: false,
                    key: [0; 16],
                    iv: [0; 16],
                    alac,
                }
            }
            (Some(rsaaeskey), Some(aesiv)) => match decrypt_stream_key(rsaaeskey, aesiv) {
                Ok((key, iv)) => {
                    info!("ANNOUNCE: encrypted stream, {} Hz", alac.sample_rate);
                    StreamParams {
                        encrypted: true,
                        key,
                        iv,
                        alac,
                    }
                }
                Err(e) => {
                    warn!("ANNOUNCE crypto material rejected: {e}");
                    return Response::new(456, "Header Field Not Valid for Resource");
                }
            },
            _ => {
                warn!("ANNOUNCE has exactly one of rsaaeskey/aesiv");
                return Response::new(456, "Header Field Not Valid for Resource");
            }
        };

        self.params = Some(params);
        Response::ok()
    }

    /// Handle SETUP: bind the three UDP sockets, spawn the receiver tasks,
    /// and report our ports back in the Transport header.
    pub async fn handle_setup(&mut self, request: &Request, local_ip: IpAddr) -> Response {
        let Some(params) = self.params.clone() else {
            warn!("SETUP before ANNOUNCE");
            return Response::new(455, "Method Not Valid in This State");
        };
        // One streaming session at a time; refuse a second client.
        if self.slot_guard.is_none() {
            match self.slot.try_acquire() {
                Some(guard) => self.slot_guard = Some(guard),
                None => {
                    warn!("SETUP refused: another session is already streaming");
                    return Response::new(453, "Not Enough Bandwidth");
                }
            }
        }
        let Some(transport) = request.headers.get("Transport") else {
            return Response::new(400, "Bad Request");
        };
        let (Some(control_port), Some(timing_port)) = (
            transport_param(transport, "control_port"),
            transport_param(transport, "timing_port"),
        ) else {
            warn!("SETUP Transport missing control_port/timing_port: {transport:?}");
            return Response::new(400, "Bad Request");
        };

        // Bind on the interface the RTSP connection arrived on so the client
        // can reach us; fall back to all-interfaces if that address is odd.
        let bind_ip = if local_ip.is_unspecified() {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            local_ip
        };
        let (audio, control, timing) = match bind_three(bind_ip).await {
            Ok(sockets) => sockets,
            Err(e) => {
                warn!("SETUP could not bind UDP sockets: {e}");
                return Response::new(500, "Internal Server Error");
            }
        };
        self.local_audio_port = audio.local_addr().map(|a| a.port()).unwrap_or(0);
        self.local_control_port = control.local_addr().map(|a| a.port()).unwrap_or(0);
        self.local_timing_port = timing.local_addr().map(|a| a.port()).unwrap_or(0);

        info!(
            "SETUP: client control={control_port} timing={timing_port}; \
             ours audio={} control={} timing={}",
            self.local_audio_port, self.local_control_port, self.local_timing_port
        );

        // Shared clock model, updated by the timing exchange (offset) and the
        // sync packets (anchor), read by the player for latency-correct start.
        let clock = Arc::new(Mutex::new(ClockModel::new(params.alac.sample_rate)));

        // The host's sink for this stream, created with the negotiated format;
        // the player thread decodes and feeds it.
        self.send_event(Event::SessionStarted {
            rate: params.alac.sample_rate,
            channels: params.alac.channels,
            peer: self.peer_ip,
        });
        self.streaming = true;
        // Replay metadata/artwork that arrived before the session started,
        // so they land inside it.
        let latched = [self.pending_metadata.take(), self.pending_artwork.take()];
        for event in latched.into_iter().flatten() {
            self.send_event(event);
        }
        let sink = (self.sink_factory)(params.alac.sample_rate, params.alac.channels);
        let player = Player::spawn(&params.alac, sink, clock.clone());
        let player_sender = player.sender();
        self.player = Some(player);

        // Share the control socket: the audio task sends resend requests on it
        // while the control task reads sync packets.
        let control = Arc::new(control);
        let timing = Arc::new(timing);
        let client_control = SocketAddr::new(self.peer_ip, control_port);
        let client_timing = SocketAddr::new(self.peer_ip, timing_port);
        let (flush_tx, flush_rx) = tokio::sync::mpsc::unbounded_channel();
        self.flush_tx = Some(flush_tx);
        // Retransmitted audio arrives on the control channel; the control task
        // forwards it to the audio task, which owns the key and the buffer.
        let (resend_tx, resend_rx) = tokio::sync::mpsc::unbounded_channel();
        self.tasks.push(tokio::spawn(audio_receiver(
            audio,
            params,
            self.observer.clone(),
            Some(player_sender),
            control.clone(),
            client_control,
            AudioInbox {
                flush: flush_rx,
                resends: resend_rx,
            },
        )));
        self.tasks.push(tokio::spawn(control_receiver(
            control,
            clock.clone(),
            resend_tx,
        )));
        self.tasks
            .push(tokio::spawn(timing_task(timing, client_timing, clock)));

        let transport = format!(
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={};server_port={}",
            self.local_control_port, self.local_timing_port, self.local_audio_port
        );
        Response::ok()
            .header("Transport", transport)
            .header("Session", "1")
    }

    /// Handle RECORD: streaming starts. Report the minimum latency.
    pub fn handle_record(&mut self, request: &Request) -> Response {
        if let Some(info) = request.headers.get("RTP-Info") {
            let seq = transport_param(info, "seq");
            let rtptime = info
                .split(';')
                .find_map(|kv| kv.trim().strip_prefix("rtptime="))
                .and_then(|v| v.parse::<u32>().ok());
            info!("RECORD: initial seq={seq:?} rtptime={rtptime:?}");
        }
        Response::ok().header("Audio-Latency", "11025")
    }

    /// Handle FLUSH/TEARDOWN/SET_PARAMETER/GET_PARAMETER. TEARDOWN stops the
    /// UDP tasks. Returns None for methods this session doesn't own.
    pub fn handle_other(&mut self, request: &Request) -> Option<Response> {
        match request.method.as_str() {
            "TEARDOWN" => {
                self.stop_tasks();
                self.player = None;
                self.flush_tx = None;
                self.slot_guard = None; // release the streaming slot immediately
                self.end_session();
                info!("TEARDOWN: session closed");
                Some(Response::ok())
            }
            "FLUSH" => {
                // Clear buffered audio up to the RTP-Info seq (if any) so a
                // seek/pause doesn't play stale audio.
                let flush_to = request
                    .headers
                    .get("RTP-Info")
                    .and_then(|info| transport_param(info, "seq"));
                if let Some(tx) = &self.flush_tx {
                    let _ = tx.send(flush_to);
                }
                self.send_event(Event::Flushed);
                Some(Response::ok())
            }
            "SET_PARAMETER" => {
                self.set_parameter(request.headers.get("Content-Type"), &request.body);
                Some(Response::ok())
            }
            "GET_PARAMETER" => Some(Response::ok()),
            _ => None,
        }
    }

    /// Apply a `SET_PARAMETER` body, dispatched on its `Content-Type`:
    /// DMAP track metadata, cover art, or (the default) `text/parameters`
    /// lines — currently the volume.
    fn set_parameter(&mut self, content_type: Option<&str>, body: &[u8]) {
        // Strip any parameters ("; charset=...") from the media type.
        let media_type = content_type.map(|ct| ct.split(';').next().unwrap_or(ct).trim());
        match media_type {
            Some(ct) if ct.eq_ignore_ascii_case(DMAP_CONTENT_TYPE) => self.set_metadata(body),
            Some(ct)
                if ct
                    .get(..6)
                    .is_some_and(|p| p.eq_ignore_ascii_case("image/")) =>
            {
                self.set_artwork(ct, body)
            }
            _ => self.set_text_parameters(body),
        }
    }

    /// The `text/parameters` flavor: the volume line and the playback
    /// position. The library does not apply gain; the host owns that path.
    fn set_text_parameters(&mut self, body: &[u8]) {
        let text = String::from_utf8_lossy(body);
        for line in text.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("volume:") {
                if let Ok(db) = v.trim().parse::<f32>() {
                    debug!("SET_PARAMETER volume {db} dB");
                    self.send_event(Event::Volume { db });
                }
            } else if let Some(v) = line.strip_prefix("progress:") {
                self.set_progress(v.trim());
            }
        }
    }

    /// `progress: <start>/<current>/<end>` — three RTP timestamps. Converted
    /// to durations with the stream's sample rate so no wire concept reaches
    /// the host; reported only while a session is running, since a position
    /// without a stream means nothing.
    fn set_progress(&mut self, value: &str) {
        let Some(params) = &self.params else { return };
        if !self.streaming {
            return;
        }
        let rate = params.alac.sample_rate;
        let mut parts = value.split('/').map(|p| p.trim().parse::<u32>());
        let (Some(Ok(start)), Some(Ok(current)), Some(Ok(end)), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            debug!("SET_PARAMETER progress: unparseable value {value:?}");
            return;
        };
        // RTP timestamps wrap and a seek can put `current` before `start`;
        // saturating subtraction keeps both readings sane rather than
        // reporting a position of ~27 hours.
        let elapsed = frames_to_duration(current.saturating_sub(start), rate);
        let duration = frames_to_duration(end.saturating_sub(start), rate);
        debug!(
            "SET_PARAMETER progress {:.1}s / {:.1}s",
            elapsed.as_secs_f32(),
            duration.as_secs_f32()
        );
        self.send_event(Event::Progress { elapsed, duration });
    }

    /// DMAP track metadata. Metadata is decoration: an unparseable payload
    /// is dropped with a debug log, never an error to the sender.
    fn set_metadata(&mut self, body: &[u8]) {
        let Some(meta) = dmap::parse(body) else {
            debug!(
                "SET_PARAMETER metadata: unrecognized DMAP payload ({} bytes)",
                body.len()
            );
            return;
        };
        debug!(
            "SET_PARAMETER metadata: title={:?} artist={:?} album={:?}",
            meta.title, meta.artist, meta.album
        );
        self.send_session_event(Event::Metadata {
            title: meta.title,
            artist: meta.artist,
            album: meta.album,
        });
    }

    /// Cover art, forwarded as-is (`image/none`/empty means cleared).
    fn set_artwork(&mut self, content_type: &str, body: &[u8]) {
        debug!(
            "SET_PARAMETER artwork: {content_type}, {} bytes",
            body.len()
        );
        self.send_session_event(Event::Artwork {
            content_type: content_type.to_string(),
            data: body.to_vec(),
        });
    }

    /// Deliver an event the host expects only inside a session; before
    /// `SessionStarted` it is latched (latest wins) and replayed once the
    /// session starts.
    fn send_session_event(&mut self, event: Event) {
        if self.streaming {
            self.send_event(event);
        } else if matches!(event, Event::Artwork { .. }) {
            self.pending_artwork = Some(event);
        } else {
            self.pending_metadata = Some(event);
        }
    }

    fn send_event(&self, event: Event) {
        let _ = self.events.send(event);
    }

    /// Report [`Event::SessionEnded`] once per started session.
    fn end_session(&mut self) {
        if self.streaming {
            self.streaming = false;
            self.send_event(Event::SessionEnded);
        }
    }

    fn stop_tasks(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop_tasks();
        self.end_session();
    }
}

/// Convert a frame count at `rate` Hz to a wall-clock duration.
fn frames_to_duration(frames: u32, rate: u32) -> Duration {
    if rate == 0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64(frames as f64 / rate as f64)
}

/// Decrypt the AES key (RSA-OAEP) and decode the IV (base64, 16 bytes).
fn decrypt_stream_key(
    rsaaeskey_b64: &str,
    aesiv_b64: &str,
) -> Result<([u8; 16], [u8; 16]), String> {
    let iv_bytes = sdp_base64(aesiv_b64).map_err(|_| "aesiv is not valid base64".to_string())?;
    let iv: [u8; 16] = iv_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("aesiv is {} bytes, wanted 16", iv_bytes.len()))?;

    let key_ct =
        sdp_base64(rsaaeskey_b64).map_err(|_| "rsaaeskey is not valid base64".to_string())?;
    let key = crypto::decrypt_aes_key(&key_ct).map_err(|e| e.to_string())?;
    Ok((key, iv))
}

/// Decode a base64 value from an ANNOUNCE SDP field. Apple sends `aesiv` and
/// `rsaaeskey` on the standard alphabet but *without* `=` padding; tolerate
/// its presence or absence so both stock senders and padded encoders work.
fn sdp_base64(value: &str) -> Result<Vec<u8>, base64::DecodeError> {
    STANDARD_NO_PAD.decode(value.trim().trim_end_matches('='))
}

fn transport_param(header: &str, name: &str) -> Option<u16> {
    header
        .split(';')
        .find_map(|kv| kv.trim().strip_prefix(&format!("{name}=")))
        .and_then(|v| v.trim().parse().ok())
}

async fn bind_three(ip: IpAddr) -> io::Result<(UdpSocket, UdpSocket, UdpSocket)> {
    let bind = |ip| async move { UdpSocket::bind(SocketAddr::new(ip, 0)).await };
    Ok((bind(ip).await?, bind(ip).await?, bind(ip).await?))
}

/// Rate-limits resend requests so each missing sequence is asked for at most
/// once per [`RESEND_BACKOFF`].
#[derive(Default)]
struct ResendTracker {
    last: HashMap<u16, Instant>,
}

impl ResendTracker {
    /// Return the subset of `missing` due for a (re)request now, recording the
    /// request time. Entries no longer missing are pruned.
    fn due(&mut self, missing: &[u16], now: Instant) -> Vec<u16> {
        let live: std::collections::HashSet<u16> = missing.iter().copied().collect();
        self.last.retain(|seq, _| live.contains(seq));
        let mut due = Vec::new();
        for &seq in missing {
            let ready = self
                .last
                .get(&seq)
                .is_none_or(|&t| now.duration_since(t) >= RESEND_BACKOFF);
            if ready {
                self.last.insert(seq, now);
                due.push(seq);
            }
        }
        due
    }

    fn clear(&mut self) {
        self.last.clear();
    }
}

async fn audio_receiver(
    socket: UdpSocket,
    params: StreamParams,
    observer: Option<AudioObserver>,
    player: Option<PlayerSender>,
    control: Arc<UdpSocket>,
    client_control: SocketAddr,
    mut inbox: AudioInbox,
) {
    // Real RAOP uses 352-frame packets (~1.4 KiB), but the announced frame
    // length can be larger; 16 KiB holds a 4096-frame 16-bit stereo packet
    // and then some. Too-small a buffer silently truncates the datagram and
    // the payload fails to decode.
    let mut buf = vec![0u8; 16 * 1024];
    let frames_per_packet = params.alac.frames_per_packet;
    let mut jitter = JitterBuffer::default();
    let mut resend = ResendTracker::default();
    // (seq, timestamp) of the first packet, to derive delivered timestamps.
    let mut anchor: Option<(u16, u32)> = None;
    let mut received: u64 = 0;
    let mut service = tokio::time::interval(SERVICE_INTERVAL);

    loop {
        tokio::select! {
            result = socket.recv(&mut buf) => {
                let n = match result {
                    Ok(n) => n,
                    Err(e) => {
                        warn!("audio socket error: {e}");
                        return;
                    }
                };
                let Some(packet) = AudioPacket::parse(&buf[..n]) else {
                    debug!("audio: ignored {n}-byte non-audio datagram");
                    continue;
                };
                received += 1;
                anchor.get_or_insert((packet.sequence, packet.timestamp));

                let frame = if params.encrypted {
                    rtp::decrypt_audio(packet.payload, &params.key, &params.iv)
                } else {
                    packet.payload.to_vec()
                };
                if received <= 3 || received.is_multiple_of(250) {
                    info!(
                        "audio: {received} pkts, seq={} ts={} {} bytes",
                        packet.sequence, packet.timestamp, frame.len()
                    );
                }
                jitter.insert(packet.sequence, frame);
                drain_deliveries(&mut jitter, &player, &observer, anchor, frames_per_packet);
            }
            datagram = inbox.resends.recv() => {
                // A packet we asked for, arriving on the control channel.
                let Some(datagram) = datagram else { return }; // session dropped
                let Some(packet) = AudioPacket::parse(&datagram) else {
                    debug!("audio: unparseable {}-byte resend reply", datagram.len());
                    continue;
                };
                let frame = if params.encrypted {
                    rtp::decrypt_audio(packet.payload, &params.key, &params.iv)
                } else {
                    packet.payload.to_vec()
                };
                debug!(
                    "audio: resend reply seq={} ts={} {} bytes",
                    packet.sequence, packet.timestamp, frame.len()
                );
                jitter.insert(packet.sequence, frame);
                drain_deliveries(&mut jitter, &player, &observer, anchor, frames_per_packet);
            }
            _ = service.tick() => {
                drain_deliveries(&mut jitter, &player, &observer, anchor, frames_per_packet);
                request_resends(&jitter, &mut resend, &control, client_control).await;
            }
            flush = inbox.flush.recv() => {
                let Some(flush_to) = flush else { return }; // session dropped
                debug!("audio: FLUSH to {flush_to:?}");
                jitter.reset(flush_to);
                resend.clear();
                anchor = None;
                if let Some(player) = &player {
                    player.flush();
                }
            }
        }
    }
}

/// Release ready packets from the jitter buffer to the player (and test
/// observer), concealing losses with silence.
fn drain_deliveries(
    jitter: &mut JitterBuffer,
    player: &Option<PlayerSender>,
    observer: &Option<AudioObserver>,
    anchor: Option<(u16, u32)>,
    frames_per_packet: u32,
) {
    for delivery in jitter.pop_ready() {
        match delivery {
            Delivery::Packet { seq, frame } => {
                let ts = derive_ts(seq, anchor, frames_per_packet);
                let looks_like_alac = rtp::looks_like_alac_stereo(&frame);
                match (player, observer) {
                    (Some(player), Some(observer)) => {
                        player.frame(ts, frame.clone());
                        let _ = observer.send(decrypted(seq, ts, frame, looks_like_alac));
                    }
                    (Some(player), None) => player.frame(ts, frame),
                    (None, Some(observer)) => {
                        let _ = observer.send(decrypted(seq, ts, frame, looks_like_alac));
                    }
                    (None, None) => {}
                }
            }
            Delivery::Lost { seq } => {
                debug!("audio: concealing lost packet seq={seq}");
                if let Some(player) = player {
                    player.silence(derive_ts(seq, anchor, frames_per_packet));
                }
            }
        }
    }
}

/// The RTP timestamp for a delivered sequence number: RAOP timestamps advance
/// by `frames_per_packet` per sequence number from the first packet's anchor.
fn derive_ts(seq: u16, anchor: Option<(u16, u32)>, frames_per_packet: u32) -> u32 {
    anchor.map_or(0, |(a_seq, a_ts)| {
        let ahead = crate::jitter::seq_diff(seq, a_seq).max(0) as u32;
        a_ts.wrapping_add(ahead.wrapping_mul(frames_per_packet))
    })
}

fn decrypted(seq: u16, timestamp: u32, frame: Vec<u8>, looks_like_alac: bool) -> DecryptedAudio {
    DecryptedAudio {
        sequence: seq,
        timestamp,
        looks_like_alac,
        frame,
    }
}

/// Ask the client to resend any missing packets, respecting per-seq backoff.
async fn request_resends(
    jitter: &JitterBuffer,
    resend: &mut ResendTracker,
    control: &UdpSocket,
    client_control: SocketAddr,
) {
    let missing = jitter.missing();
    if missing.is_empty() {
        return;
    }
    for seq in resend.due(&missing, Instant::now()) {
        let req = rtp::resend_request(seq, 1);
        if let Err(e) = control.send_to(&req, client_control).await {
            debug!("audio: resend request for seq={seq} failed: {e}");
        }
    }
}

/// What the RTSP path and the control task send to the audio task: FLUSH
/// boundaries (a sequence to flush to, or `None` to re-anchor) and the
/// retransmitted audio packets that arrive on the control channel.
struct AudioInbox {
    flush: tokio::sync::mpsc::UnboundedReceiver<Option<u16>>,
    resends: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
}

/// Read the control channel: parse `0xd4` sync packets and update the clock
/// model's anchor (the frame at the DAC at a given client-clock instant), and
/// hand retransmitted audio packets to the audio task.
///
/// A sender answers our resend requests **here**, on the control channel — not
/// on the audio channel — so a reply that stops at this task is a gap the
/// jitter buffer can never fill.
async fn control_receiver(
    socket: Arc<UdpSocket>,
    clock: Arc<Mutex<ClockModel>>,
    resends: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
) {
    let mut buf = [0u8; 2048];
    while let Ok(n) = socket.recv(&mut buf).await {
        if let Some(sync) = rtp::parse_sync(&buf[..n]) {
            clock
                .lock()
                .unwrap()
                .set_anchor(sync.remote_time_ns, sync.rtp_at_dac);
        } else if let Some(kind) = rtp::classify_control(&buf[..n]) {
            if kind == rtp::ControlKind::RetransmitResponse {
                // The audio task owns the session key and the jitter buffer.
                if resends.send(buf[..n].to_vec()).is_err() {
                    return; // audio task gone: the session is over
                }
            } else {
                debug!("control: {kind:?} ({n} bytes)");
            }
        }
    }
}

/// The NTP timing exchange: periodically send a `0xd2` request to the client's
/// timing port and fold each `0xd3` reply into the clock model's offset. An
/// initial fast burst converges quickly, then it settles to ~every 3 s.
async fn timing_task(
    socket: Arc<UdpSocket>,
    client_timing: SocketAddr,
    clock: Arc<Mutex<ClockModel>>,
) {
    let mut buf = [0u8; 2048];
    let mut request_count: u64 = 0;

    loop {
        // Send a timing request and record when it left (t1).
        let departure_ns = clock::now_ns();
        if let Err(e) = socket.send_to(&rtp::timing_request(), client_timing).await {
            debug!("timing: request send failed: {e}");
        }
        request_count += 1;
        let interval = if request_count <= 3 {
            Duration::from_millis(300)
        } else {
            Duration::from_secs(3)
        };

        // Collect replies until it's time to send the next request.
        let deadline = tokio::time::sleep(interval);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                result = socket.recv(&mut buf) => {
                    let Ok(n) = result else { return };
                    let arrival_ns = clock::now_ns();
                    if let Some(reply) = rtp::parse_timing_reply(&buf[..n]) {
                        clock.lock().unwrap().add_timing(
                            departure_ns,
                            reply.receive_ns,
                            reply.transmit_ns,
                            arrival_ns,
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use std::io::Cursor;
    use std::net::Ipv4Addr;
    use tokio::io::BufReader;

    struct DiscardSink;

    impl crate::sink::AudioSink for DiscardSink {
        fn write(&mut self, _pcm: &[i16]) {}
        fn flush(&mut self) {}
    }

    fn session() -> (Session, tokio::sync::mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let factory: SinkFactory = Arc::new(|_rate, _channels| Box::new(DiscardSink));
        let session = Session::new(
            None,
            factory,
            tx,
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            SessionSlot::new(),
        );
        (session, rx)
    }

    async fn request(raw: &str) -> Request {
        let mut reader = BufReader::new(Cursor::new(raw.as_bytes().to_vec()));
        crate::rtsp::read_request(&mut reader)
            .await
            .unwrap()
            .unwrap()
    }

    const SDP_BODY: &str = "v=0\r\n\
        o=iTunes 3413821438 0 IN IP4 192.168.1.2\r\n\
        s=iTunes\r\n\
        c=IN IP4 192.168.1.10\r\n\
        t=0 0\r\n\
        m=audio 0 RTP/AVP 96\r\n\
        a=rtpmap:96 AppleLossless\r\n\
        a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n";

    #[tokio::test]
    async fn session_lifecycle_emits_the_events() {
        let (mut session, mut events) = session();

        let announce = request(&format!(
            "ANNOUNCE rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 2\r\nContent-Length: {}\r\n\r\n{SDP_BODY}",
            SDP_BODY.len()
        ))
        .await;
        assert_eq!(session.handle_announce(&announce).status(), 200);
        assert!(events.try_recv().is_err(), "no event before SETUP");

        let setup = request(
            "SETUP rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 3\r\n\
             Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\
             \r\n",
        )
        .await;
        let response = session
            .handle_setup(&setup, IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            events.try_recv(),
            Ok(Event::SessionStarted {
                rate: 44100,
                channels: 2,
                peer: IpAddr::V4(Ipv4Addr::LOCALHOST),
            })
        );

        let set_parameter = request(
            "SET_PARAMETER rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 4\r\nContent-Type: text/parameters\r\nContent-Length: 18\r\n\
             \r\nvolume: -20.000000",
        )
        .await;
        assert_eq!(session.handle_other(&set_parameter).unwrap().status(), 200);
        assert_eq!(events.try_recv(), Ok(Event::Volume { db: -20.0 }));

        let flush = request("FLUSH rtsp://192.168.1.10/1 RTSP/1.0\r\nCSeq: 5\r\n\r\n").await;
        assert_eq!(session.handle_other(&flush).unwrap().status(), 200);
        assert_eq!(events.try_recv(), Ok(Event::Flushed));

        let teardown = request("TEARDOWN rtsp://192.168.1.10/1 RTSP/1.0\r\nCSeq: 6\r\n\r\n").await;
        assert_eq!(session.handle_other(&teardown).unwrap().status(), 200);
        assert_eq!(events.try_recv(), Ok(Event::SessionEnded));

        // The end is reported exactly once: drop after TEARDOWN adds nothing.
        drop(session);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn dropping_a_streaming_session_ends_it() {
        let (mut session, mut events) = session();
        let announce = request(&format!(
            "ANNOUNCE rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 2\r\nContent-Length: {}\r\n\r\n{SDP_BODY}",
            SDP_BODY.len()
        ))
        .await;
        session.handle_announce(&announce);
        let setup = request(
            "SETUP rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 3\r\n\
             Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\
             \r\n",
        )
        .await;
        session
            .handle_setup(&setup, IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await;
        assert_eq!(
            events.try_recv(),
            Ok(Event::SessionStarted {
                rate: 44100,
                channels: 2,
                peer: IpAddr::V4(Ipv4Addr::LOCALHOST),
            })
        );

        // The client disconnected without TEARDOWN.
        drop(session);
        assert_eq!(events.try_recv(), Ok(Event::SessionEnded));
    }

    /// A one-track DMAP payload: `mlit` wrapping title/artist/album.
    fn dmap_track(title: &str) -> Vec<u8> {
        fn entry(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut e = tag.to_vec();
            e.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            e.extend_from_slice(payload);
            e
        }
        let inner = [
            entry(b"minm", title.as_bytes()),
            entry(b"asar", b"The Artist"),
            entry(b"asal", b"The Album"),
        ]
        .concat();
        entry(b"mlit", &inner)
    }

    /// Drive ANNOUNCE → SETUP so the session is streaming, and consume the
    /// resulting `SessionStarted`.
    async fn start_session(
        session: &mut Session,
        events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    ) {
        let announce = request(&format!(
            "ANNOUNCE rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 2\r\nContent-Length: {}\r\n\r\n{SDP_BODY}",
            SDP_BODY.len()
        ))
        .await;
        session.handle_announce(&announce);
        let setup = request(
            "SETUP rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 3\r\n\
             Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\
             \r\n",
        )
        .await;
        session
            .handle_setup(&setup, IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await;
        assert_eq!(
            events.try_recv(),
            Ok(Event::SessionStarted {
                rate: 44100,
                channels: 2,
                peer: IpAddr::V4(Ipv4Addr::LOCALHOST),
            })
        );
    }

    #[tokio::test]
    async fn set_parameter_dispatches_on_content_type() {
        let (mut session, mut events) = session();
        start_session(&mut session, &mut events).await;

        session.set_parameter(Some(DMAP_CONTENT_TYPE), &dmap_track("Song"));
        assert_eq!(
            events.try_recv(),
            Ok(Event::Metadata {
                title: Some("Song".into()),
                artist: Some("The Artist".into()),
                album: Some("The Album".into()),
            })
        );

        // Artwork is forwarded byte-for-byte, content type included.
        session.set_parameter(Some("image/png"), b"\x89PNG");
        assert_eq!(
            events.try_recv(),
            Ok(Event::Artwork {
                content_type: "image/png".into(),
                data: b"\x89PNG".to_vec(),
            })
        );

        // image/none with an empty body is the sender clearing the art.
        session.set_parameter(Some("image/none"), b"");
        assert_eq!(
            events.try_recv(),
            Ok(Event::Artwork {
                content_type: "image/none".into(),
                data: Vec::new(),
            })
        );

        // The volume path is unchanged, with or without a charset parameter.
        session.set_parameter(Some("text/parameters; charset=utf-8"), b"volume: -12.5\r\n");
        assert_eq!(events.try_recv(), Ok(Event::Volume { db: -12.5 }));
        session.set_parameter(None, b"volume: -6.0\r\n");
        assert_eq!(events.try_recv(), Ok(Event::Volume { db: -6.0 }));

        // Anything else is acknowledged but produces no event.
        session.set_parameter(Some("application/octet-stream"), b"\x00\x01");
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn progress_is_reported_as_durations() {
        let (mut session, mut events) = session();
        start_session(&mut session, &mut events).await;

        // start/current/end as RTP timestamps at 44100 Hz: 10 s in, 180 s
        // long. Senders send this line on its own or next to the volume.
        session.set_parameter(
            Some("text/parameters"),
            b"progress: 1000/442000/7939000\r\n",
        );
        let Ok(Event::Progress { elapsed, duration }) = events.try_recv() else {
            panic!("expected a Progress event");
        };
        assert!(
            (elapsed.as_secs_f64() - 10.0).abs() < 0.01,
            "elapsed was {elapsed:?}"
        );
        assert!(
            (duration.as_secs_f64() - 180.0).abs() < 0.01,
            "duration was {duration:?}"
        );

        // A seek backwards past the anchor, and a stream with no known end,
        // must not produce a ~27-hour reading from a wrapped subtraction.
        session.set_parameter(Some("text/parameters"), b"progress: 5000/1000/5000");
        assert_eq!(
            events.try_recv(),
            Ok(Event::Progress {
                elapsed: Duration::ZERO,
                duration: Duration::ZERO,
            })
        );

        // Malformed values are ignored, and the volume line in the same body
        // still works.
        for bad in [
            "progress: 1/2\r\n",
            "progress: 1/2/3/4\r\n",
            "progress: a/b/c\r\n",
            "progress:\r\n",
        ] {
            session.set_parameter(Some("text/parameters"), bad.as_bytes());
        }
        assert!(events.try_recv().is_err(), "no event from bad progress");

        session.set_parameter(
            Some("text/parameters"),
            b"volume: -8.0\r\nprogress: 1000/45100/7939000\r\n",
        );
        assert_eq!(events.try_recv(), Ok(Event::Volume { db: -8.0 }));
        assert!(matches!(events.try_recv(), Ok(Event::Progress { .. })));
    }

    #[tokio::test]
    async fn progress_outside_a_session_is_dropped() {
        // A position without a stream means nothing, and before ANNOUNCE
        // there is no sample rate to convert it with.
        let (mut session, mut events) = session();
        session.set_parameter(Some("text/parameters"), b"progress: 1000/442000/7939000");
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn session_started_carries_the_sender_address() {
        let (mut session, mut events) = session();
        start_session(&mut session, &mut events).await; // asserts peer
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn metadata_before_setup_is_latched_until_session_start() {
        // Senders may push metadata during the handshake, but the host's
        // contract is that it only arrives inside a session — so the latest
        // of each is held and replayed after SessionStarted.
        let (mut session, mut events) = session();
        session.set_parameter(Some(DMAP_CONTENT_TYPE), &dmap_track("First"));
        session.set_parameter(Some(DMAP_CONTENT_TYPE), &dmap_track("Second"));
        session.set_parameter(Some("image/jpeg"), b"JPEG");
        assert!(events.try_recv().is_err(), "nothing before SessionStarted");

        start_session(&mut session, &mut events).await;

        // Latest metadata wins, and each is replayed exactly once.
        assert!(matches!(
            events.try_recv(),
            Ok(Event::Metadata { title: Some(t), .. }) if t == "Second"
        ));
        assert!(matches!(events.try_recv(), Ok(Event::Artwork { .. })));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn malformed_metadata_is_ignored_and_never_ends_the_session() {
        let (mut session, mut events) = session();
        start_session(&mut session, &mut events).await;

        session.set_parameter(Some(DMAP_CONTENT_TYPE), b"");
        session.set_parameter(Some(DMAP_CONTENT_TYPE), b"garbage, not dmap");
        // An mlit whose declared length runs off the end of the payload.
        session.set_parameter(Some(DMAP_CONTENT_TYPE), b"mlit\x00\x00\xff\xff");
        assert!(events.try_recv().is_err(), "no event from bad payloads");

        // The session is still live and still handling parameters.
        session.set_parameter(Some("text/parameters"), b"volume: -6.0\r\n");
        assert_eq!(events.try_recv(), Ok(Event::Volume { db: -6.0 }));
    }

    #[test]
    fn extracts_transport_ports() {
        let hdr = "RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002";
        assert_eq!(transport_param(hdr, "control_port"), Some(6001));
        assert_eq!(transport_param(hdr, "timing_port"), Some(6002));
        assert_eq!(transport_param(hdr, "server_port"), None);
    }

    #[test]
    fn stream_key_requires_16_byte_iv() {
        // Valid base64 but only 8 bytes.
        let err = decrypt_stream_key("AAECAwQF", &STANDARD.encode([0u8; 8])).unwrap_err();
        assert!(err.contains("aesiv is 8 bytes"), "got: {err}");
    }

    #[test]
    fn sdp_base64_tolerates_missing_padding() {
        // Apple sends aesiv/rsaaeskey without '=' padding; both forms must
        // decode to the same 16 bytes. (This is the bug a real Mac hit.)
        let iv: [u8; 16] = std::array::from_fn(|i| i as u8);
        let padded = STANDARD.encode(iv); // "...=="
        let unpadded = padded.trim_end_matches('=');
        assert_eq!(unpadded.len(), 22, "16 bytes unpadded is 22 base64 chars");
        assert_eq!(sdp_base64(&padded).unwrap(), iv);
        assert_eq!(sdp_base64(unpadded).unwrap(), iv);
    }
}
