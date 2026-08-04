//! End-to-end milestone 2 test: drive ANNOUNCE → SETUP → RECORD over TCP with
//! a real RSA-encrypted session key, then send an encrypted audio packet to
//! the negotiated server_port and confirm the receiver decrypts it back to
//! the known plaintext.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aes::cipher::{BlockEncryptMut, KeyIvInit};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::{Oaep, RsaPrivateKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

use openairplay1::server::{self, Context};
use openairplay1::AudioSink;
use openairplay1::DecryptedAudio;
use openairplay1::{crypto, Config};

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

/// Tests never touch audio hardware: the sink discards everything.
struct Discard;

impl AudioSink for Discard {
    fn write(&mut self, _pcm: &[i16]) {}
    fn flush(&mut self) {}
}

const KEY: [u8; 16] = [
    0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18, 0x29, 0x3a, 0x4b, 0x5c, 0x6d, 0x7e, 0x8f, 0x90,
];
const IV: [u8; 16] = [
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00,
];

async fn start() -> (
    SocketAddr,
    tokio::sync::mpsc::UnboundedReceiver<DecryptedAudio>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        name: "Test".to_string(),
        port: addr.port(),
        mac: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
    };
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (events, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let context = Arc::new(Context {
        config,
        sink_factory: Arc::new(|_rate, _channels| Box::new(Discard)),
        events,
    });
    tokio::spawn(server::serve_with_observer(listener, context, Some(tx)));
    (addr, rx)
}

/// Send one RTSP request and return (status_line, headers).
async fn rtsp(stream: &mut TcpStream, request: &str) -> (String, Vec<(String, String)>) {
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        assert_eq!(stream.read(&mut byte).await.unwrap(), 1, "eof from server");
        raw.push(byte[0]);
    }
    let text = String::from_utf8(raw).unwrap();
    let mut lines = text.split("\r\n");
    let status = lines.next().unwrap().to_string();
    let headers = lines
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    (status, headers)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn transport_port(headers: &[(String, String)], name: &str) -> u16 {
    let transport = header(headers, "Transport").expect("Transport header");
    transport
        .split(';')
        .find_map(|kv| kv.trim().strip_prefix(&format!("{name}=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no {name} in {transport:?}"))
}

/// Encrypt like a real sender: RSA-OAEP-wrap the AES key with the AirPort
/// public key for the SDP.
fn wrap_key() -> String {
    let public = RsaPrivateKey::from_pkcs1_pem(crypto::AIRPORT_PRIVATE_KEY_PEM)
        .unwrap()
        .to_public_key();
    let mut rng = rand::thread_rng();
    let ct = public
        .encrypt(&mut rng, Oaep::new::<sha1::Sha1>(), &KEY)
        .unwrap();
    // Stock Apple senders omit the '=' padding — encode the same way so the
    // test exercises the real wire format.
    STANDARD.encode(ct).trim_end_matches('=').to_string()
}

fn encrypt_audio(plaintext: &[u8]) -> Vec<u8> {
    let block_len = plaintext.len() & !0xf;
    let mut out = plaintext.to_vec();
    let mut cipher = Aes128CbcEnc::new(&KEY.into(), &IV.into());
    for block in out[..block_len].chunks_exact_mut(16) {
        let ga = aes::cipher::generic_array::GenericArray::from_mut_slice(block);
        cipher.encrypt_block_mut(ga);
    }
    out
}

#[tokio::test]
async fn full_handshake_and_audio_decrypt() {
    let (addr, mut audio_rx) = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    // ANNOUNCE with a real RSA-encrypted key and a 16-byte IV.
    let sdp = format!(
        "v=0\r\no=iTunes 1 0 IN IP4 127.0.0.1\r\ns=iTunes\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\n\
         a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n\
         a=rsaaeskey:{}\r\na=aesiv:{}\r\n",
        wrap_key(),
        STANDARD.encode(IV).trim_end_matches('=') // unpadded, like a real sender
    );
    let announce = format!(
        "ANNOUNCE rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 1\r\n\
         Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    let (status, _) = rtsp(&mut stream, &announce).await;
    assert_eq!(status, "RTSP/1.0 200 OK", "ANNOUNCE should be accepted");

    // SETUP: advertise fake client ports; capture our server_port.
    let setup = "SETUP rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 2\r\n\
                 Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\r\n";
    let (status, headers) = rtsp(&mut stream, setup).await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    assert_eq!(header(&headers, "Session"), Some("1"));
    let server_port = transport_port(&headers, "server_port");
    assert!(transport_port(&headers, "control_port") > 0);
    assert!(transport_port(&headers, "timing_port") > 0);

    // RECORD.
    let record = "RECORD rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 3\r\n\
                  RTP-Info: seq=100;rtptime=200\r\n\r\n";
    let (status, headers) = rtsp(&mut stream, record).await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    assert_eq!(header(&headers, "Audio-Latency"), Some("11025"));

    // Send an encrypted audio packet to server_port. Plaintext opens with a
    // stereo channel-pair element tag (top 3 bits 001) so the sanity check
    // fires, plus a non-block-multiple tail to exercise the cleartext copy.
    let mut plaintext = vec![0x20u8];
    plaintext.extend((0..36).map(|i| i as u8));
    let ciphertext = encrypt_audio(&plaintext);
    let mut packet = vec![0x80, 0x60, 0x00, 0x64]; // seq 100
    packet.extend_from_slice(&300u32.to_be_bytes()); // timestamp
    packet.extend_from_slice(&[0, 0, 0, 1]); // SSRC
    packet.extend_from_slice(&ciphertext);

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(&packet, SocketAddr::from(([127, 0, 0, 1], server_port)))
        .await
        .unwrap();

    let decrypted = tokio::time::timeout(Duration::from_secs(2), audio_rx.recv())
        .await
        .expect("audio packet should be received")
        .expect("observer channel open");
    assert_eq!(decrypted.sequence, 100);
    assert_eq!(decrypted.timestamp, 300);
    assert!(decrypted.looks_like_alac, "sanity check should pass");
    assert_eq!(decrypted.frame, plaintext, "decrypt must recover plaintext");
}

#[tokio::test]
async fn setup_before_announce_is_rejected() {
    let (addr, _rx) = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let setup = "SETUP rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 1\r\n\
                 Transport: RTP/AVP/UDP;control_port=6001;timing_port=6002\r\n\r\n";
    let (status, _) = rtsp(&mut stream, setup).await;
    assert_eq!(status, "RTSP/1.0 455 Method Not Valid in This State");
}

#[tokio::test]
async fn announce_with_only_iv_is_rejected() {
    let (addr, _rx) = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let sdp = format!(
        "v=0\r\na=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\na=aesiv:{}\r\n",
        STANDARD.encode(IV)
    );
    let announce = format!(
        "ANNOUNCE rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 1\r\n\
         Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    let (status, _) = rtsp(&mut stream, &announce).await;
    assert_eq!(status, "RTSP/1.0 456 Header Field Not Valid for Resource");
}
