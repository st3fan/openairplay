//! Pincode protection (classic AirPlay 1 password, RFC 2617 Digest auth):
//! drive the real RTSP server over TCP and verify the challenge / authorize
//! / refuse cycle, mirroring how shairport-sync's `rtsp_classic_airplay_auth`
//! behaves.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use openairplay1::server::{self, Context};
use openairplay1::AudioSink;
use openairplay1::{crypto, Config};

struct Discard;

impl AudioSink for Discard {
    fn write(&mut self, _pcm: &[i16]) {}
    fn flush(&mut self) {}
}

async fn start(pincode: Option<&str>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        name: "Test".to_string(),
        port: addr.port(),
        mac: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
        pincode: pincode.map(str::to_string),
    };
    let (events, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let context = Arc::new(Context {
        config,
        sink_factory: Arc::new(|_rate, _channels| Box::new(Discard)),
        events,
    });
    tokio::spawn(server::serve(listener, context));
    addr
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

/// Pull the `nonce="…"` out of a `WWW-Authenticate: Digest …` value.
fn nonce_from_auth(auth: &str) -> String {
    let needle = "nonce=\"";
    let start = auth
        .find(needle)
        .unwrap_or_else(|| panic!("no nonce in {auth:?}"))
        + needle.len();
    let end = auth[start..]
        .find('"')
        .unwrap_or_else(|| panic!("unterminated nonce in {auth:?}"))
        + start;
    auth[start..end].to_string()
}

/// Build an `Authorization: Digest` header the way a Digest-speaking client
/// would, using the same RFC 2617 formula the receiver validates.
fn digest_auth(nonce: &str, username: &str, password: &str, method: &str, uri: &str) -> String {
    let response = crypto::digest_response(username, "raop", password, method, uri, nonce);
    format!(
        "Digest realm=\"raop\", username=\"{username}\", nonce=\"{nonce}\", uri=\"{uri}\", \
         response=\"{response}\""
    )
}

/// A minimal unencrypted ANNOUNCE body (no rsaaeskey/aesiv).
const SDP: &str = "v=0\r\n\
    o=iTunes 3413821438 0 IN IP4 192.168.1.2\r\n\
    s=iTunes\r\n\
    c=IN IP4 192.168.1.10\r\n\
    t=0 0\r\n\
    m=audio 0 RTP/AVP 96\r\n\
    a=rtpmap:96 AppleLossless\r\n\
    a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n";

#[tokio::test]
async fn challenges_until_authorized_then_streams() {
    let addr = start(Some("1234")).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    // First contact, no credentials -> 401 with a Digest challenge.
    let options = "OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n";
    let (status, headers) = rtsp(&mut stream, options).await;
    assert_eq!(status, "RTSP/1.0 401 Unauthorized");
    let auth = header(&headers, "WWW-Authenticate").expect("challenge");
    assert!(auth.starts_with("Digest realm=\"raop\""), "got {auth:?}");
    let nonce = nonce_from_auth(auth);

    // Retry with the correct digest -> authorized.
    let auth_hdr = digest_auth(&nonce, "player", "1234", "OPTIONS", "*");
    let options_auth =
        format!("OPTIONS * RTSP/1.0\r\nCSeq: 2\r\nAuthorization: {auth_hdr}\r\n\r\n");
    let (status, _) = rtsp(&mut stream, &options_auth).await;
    assert_eq!(status, "RTSP/1.0 200 OK", "authorized OPTIONS");

    // The rest of the handshake now proceeds unchallenged.
    let announce = format!(
        "ANNOUNCE rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 3\r\n\
         Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{SDP}",
        SDP.len()
    );
    let (status, _) = rtsp(&mut stream, &announce).await;
    assert_eq!(status, "RTSP/1.0 200 OK");

    let setup = "SETUP rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 4\r\n\
                 Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\r\n";
    let (status, _) = rtsp(&mut stream, setup).await;
    assert_eq!(status, "RTSP/1.0 200 OK", "streaming SETUP accepted");
}

#[tokio::test]
async fn wrong_pincode_is_refused_and_stays_locked() {
    let addr = start(Some("1234")).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    let options = "OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n";
    let (status, headers) = rtsp(&mut stream, options).await;
    assert_eq!(status, "RTSP/1.0 401 Unauthorized");
    let nonce = nonce_from_auth(header(&headers, "WWW-Authenticate").unwrap());

    // A wrong pincode computes a different response and is refused.
    let bad = digest_auth(&nonce, "player", "nope", "OPTIONS", "*");
    let req = format!("OPTIONS * RTSP/1.0\r\nCSeq: 2\r\nAuthorization: {bad}\r\n\r\n");
    let (status, _) = rtsp(&mut stream, &req).await;
    assert_eq!(status, "RTSP/1.0 401 Unauthorized", "wrong pincode refused");

    // Still not authorized: a later method is challenged too.
    let announce = format!(
        "ANNOUNCE rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 3\r\n\
         Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{SDP}",
        SDP.len()
    );
    let (status, _) = rtsp(&mut stream, &announce).await;
    assert_eq!(status, "RTSP/1.0 401 Unauthorized");
}

#[tokio::test]
async fn malformed_authorization_header_is_refused() {
    let addr = start(Some("1234")).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    let options = "OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n";
    let (status, _) = rtsp(&mut stream, options).await;
    assert_eq!(status, "RTSP/1.0 401 Unauthorized");

    // Not a Digest auth at all.
    let basic = "OPTIONS * RTSP/1.0\r\nCSeq: 2\r\nAuthorization: Basic dXNlcjpwYXNz\r\n\r\n";
    let (status, _) = rtsp(&mut stream, basic).await;
    assert_eq!(status, "RTSP/1.0 401 Unauthorized");

    // Digest auth missing one of realm/username/response/uri.
    let partial =
        "OPTIONS * RTSP/1.0\r\nCSeq: 3\r\nAuthorization: Digest realm=\"raop\", username=\"x\"\r\n\r\n";
    let (status, _) = rtsp(&mut stream, partial).await;
    assert_eq!(status, "RTSP/1.0 401 Unauthorized");
}

#[tokio::test]
async fn no_pincode_means_no_challenge() {
    let addr = start(None).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    let options = "OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n";
    let (status, headers) = rtsp(&mut stream, options).await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    assert!(
        header(&headers, "WWW-Authenticate").is_none(),
        "no pincode, no challenge"
    );
}
