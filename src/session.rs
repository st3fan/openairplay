//! Per-connection RAOP session state machine: ANNOUNCE → SETUP → RECORD, the
//! bound UDP channels, and the audio-receiver task that decrypts incoming
//! packets.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use log::{debug, info, warn};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::crypto;
use crate::player::{Player, PlayerSender};
use crate::rtp::{self, AudioPacket};
use crate::rtsp::{Request, Response};
use crate::sdp::{AlacConfig, Sdp};

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
    /// Local UDP ports handed to the client in the SETUP response.
    local_audio_port: u16,
    local_control_port: u16,
    local_timing_port: u16,
}

impl Session {
    pub fn new(observer: Option<AudioObserver>, audio_device: Option<String>) -> Self {
        Session {
            observer,
            audio_device,
            player: None,
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

        // Spawn the ALSA player (decode-only if no device / open fails) and
        // hand the receiver a sender so decrypted frames reach playback.
        let player = Player::spawn(&params.alac, self.audio_device.clone());
        let player_sender = player.sender();
        self.player = Some(player);

        self.tasks.push(tokio::spawn(audio_receiver(
            audio,
            params,
            self.observer.clone(),
            Some(player_sender),
        )));
        self.tasks.push(tokio::spawn(control_receiver(control)));
        self.tasks.push(tokio::spawn(timing_receiver(timing)));

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
                info!("TEARDOWN: session closed");
                Some(Response::ok())
            }
            "FLUSH" => {
                if let Some(player) = &self.player {
                    player.flush();
                }
                Some(Response::ok())
            }
            "SET_PARAMETER" => {
                let body = String::from_utf8_lossy(&request.body);
                for line in body.lines() {
                    if let Some(v) = line.trim().strip_prefix("volume:") {
                        debug!("SET_PARAMETER volume {}", v.trim());
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

async fn audio_receiver(
    socket: UdpSocket,
    params: StreamParams,
    observer: Option<AudioObserver>,
    player: Option<PlayerSender>,
) {
    // Real RAOP uses 352-frame packets (~1.4 KiB), but the announced frame
    // length can be larger; 16 KiB holds a 4096-frame 16-bit stereo packet
    // and then some. Too-small a buffer silently truncates the datagram and
    // the payload fails to decode.
    let mut buf = vec![0u8; 16 * 1024];
    let mut received: u64 = 0;
    let mut decrypt_ok: u64 = 0;
    loop {
        let n = match socket.recv(&mut buf).await {
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

        let frame = if params.encrypted {
            rtp::decrypt_audio(packet.payload, &params.key, &params.iv)
        } else {
            packet.payload.to_vec()
        };
        let looks_like_alac = rtp::looks_like_alac_stereo(&frame);
        if looks_like_alac {
            decrypt_ok += 1;
        }

        if received <= 3 || received.is_multiple_of(250) {
            info!(
                "audio: {received} pkts, seq={} ts={} {} bytes, alac-ok {decrypt_ok}/{received}",
                packet.sequence,
                packet.timestamp,
                frame.len()
            );
        }

        // Forward to playback and/or the test observer. Only clone when both
        // want the bytes; the production path (player only) moves them.
        match (&player, &observer) {
            (Some(player), Some(observer)) => {
                player.frame(frame.clone());
                let _ = observer.send(DecryptedAudio {
                    sequence: packet.sequence,
                    timestamp: packet.timestamp,
                    looks_like_alac,
                    frame,
                });
            }
            (Some(player), None) => player.frame(frame),
            (None, Some(observer)) => {
                let _ = observer.send(DecryptedAudio {
                    sequence: packet.sequence,
                    timestamp: packet.timestamp,
                    looks_like_alac,
                    frame,
                });
            }
            (None, None) => {}
        }
    }
}

async fn control_receiver(socket: UdpSocket) {
    let mut buf = [0u8; 2048];
    while let Ok(n) = socket.recv(&mut buf).await {
        if let Some(kind) = rtp::classify_control(&buf[..n]) {
            debug!("control: {kind:?} ({n} bytes)");
        }
    }
}

async fn timing_receiver(socket: UdpSocket) {
    let mut buf = [0u8; 2048];
    while let Ok(n) = socket.recv(&mut buf).await {
        debug!("timing: {n} bytes");
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
