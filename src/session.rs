//! Per-connection RAOP session state machine: ANNOUNCE → SETUP → RECORD, the
//! bound UDP channels, and the audio-receiver task that decrypts incoming
//! packets.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use log::{debug, info, warn};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::clock::{self, ClockModel};
use crate::crypto;
use crate::jitter::{Delivery, JitterBuffer};
use crate::player::{Player, PlayerSender};
use crate::rtp::{self, AudioPacket};
use crate::rtsp::{Request, Response};
use crate::sdp::{AlacConfig, Sdp};

/// How often the audio task services retransmit requests and forced skips.
const SERVICE_INTERVAL: Duration = Duration::from_millis(20);
/// Minimum spacing between resend requests for the same sequence number.
const RESEND_BACKOFF: Duration = Duration::from_millis(80);

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

/// A decrypted audio packet, surfaced to an optional observer. Milestone 2
/// uses this to prove the crypto path in tests and logs; milestone 3 will
/// feed it to the ALAC decoder.
#[derive(Debug, Clone)]
pub struct DecryptedAudio {
    pub sequence: u16,
    pub timestamp: u32,
    pub looks_like_alac: bool,
    pub frame: Vec<u8>,
}

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
    /// ALSA device to play to, or `None` for decode-only (`--no-audio`).
    audio_device: Option<String>,
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
        audio_device: Option<String>,
        peer_ip: IpAddr,
        slot: SessionSlot,
    ) -> Self {
        Session {
            observer,
            audio_device,
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

        // Spawn the ALSA player (decode-only if no device / open fails) and
        // hand the receiver a sender so decrypted frames reach playback.
        let player = Player::spawn(&params.alac, self.audio_device.clone(), clock.clone());
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
        self.tasks.push(tokio::spawn(audio_receiver(
            audio,
            params,
            self.observer.clone(),
            Some(player_sender),
            control.clone(),
            client_control,
            flush_rx,
        )));
        self.tasks
            .push(tokio::spawn(control_receiver(control, clock.clone())));
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
                Some(Response::ok())
            }
            "SET_PARAMETER" => {
                let body = String::from_utf8_lossy(&request.body);
                for line in body.lines() {
                    if let Some(v) = line.trim().strip_prefix("volume:") {
                        if let Ok(db) = v.trim().parse::<f32>() {
                            debug!("SET_PARAMETER volume {db} dB");
                            if let Some(player) = &self.player {
                                player.set_volume_db(db);
                            }
                        }
                    }
                }
                Some(Response::ok())
            }
            "GET_PARAMETER" => Some(Response::ok()),
            _ => None,
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
    }
}

/// Decrypt the AES key (RSA-OAEP) and decode the IV (base64, 16 bytes).
fn decrypt_stream_key(
    rsaaeskey_b64: &str,
    aesiv_b64: &str,
) -> Result<([u8; 16], [u8; 16]), String> {
    let iv_bytes = STANDARD
        .decode(aesiv_b64)
        .map_err(|_| "aesiv is not valid base64".to_string())?;
    let iv: [u8; 16] = iv_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("aesiv is {} bytes, wanted 16", iv_bytes.len()))?;

    let key_ct = STANDARD
        .decode(rsaaeskey_b64)
        .map_err(|_| "rsaaeskey is not valid base64".to_string())?;
    let key = crypto::decrypt_aes_key(&key_ct).map_err(|e| e.to_string())?;
    Ok((key, iv))
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
    mut flush_rx: tokio::sync::mpsc::UnboundedReceiver<Option<u16>>,
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
            _ = service.tick() => {
                drain_deliveries(&mut jitter, &player, &observer, anchor, frames_per_packet);
                request_resends(&jitter, &mut resend, &control, client_control).await;
            }
            flush = flush_rx.recv() => {
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

/// Read the control channel: parse `0xd4` sync packets and update the clock
/// model's anchor (the frame at the DAC at a given client-clock instant).
async fn control_receiver(socket: Arc<UdpSocket>, clock: Arc<Mutex<ClockModel>>) {
    let mut buf = [0u8; 2048];
    while let Ok(n) = socket.recv(&mut buf).await {
        if let Some(sync) = rtp::parse_sync(&buf[..n]) {
            clock
                .lock()
                .unwrap()
                .set_anchor(sync.remote_time_ns, sync.rtp_at_dac);
        } else if let Some(kind) = rtp::classify_control(&buf[..n]) {
            debug!("control: {kind:?} ({n} bytes)");
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
}
