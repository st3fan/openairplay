//! End-to-end tests: real TCP connection against the in-process RTSP server.

use std::net::SocketAddr;
use std::sync::Arc;

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::{Pkcs1v15Sign, RsaPrivateKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use openairplay1::server::{self, Context};
use openairplay1::AudioSink;
use openairplay1::{crypto, Config};

const MAC: [u8; 6] = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];

/// Tests never touch audio hardware: the sink discards everything.
struct Discard;

impl AudioSink for Discard {
    fn write(&mut self, _pcm: &[i16]) {}
    fn flush(&mut self) {}
}

async fn start_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        name: "Test".to_string(),
        port: addr.port(),
        mac: MAC,
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

async fn roundtrip(stream: &mut TcpStream, request: &str) -> (String, Vec<(String, String)>) {
    stream.write_all(request.as_bytes()).await.unwrap();
    // Responses in milestone 1 have no body, so the double CRLF ends them.
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        assert!(
            stream.read(&mut byte).await.unwrap() == 1,
            "eof from server"
        );
        raw.push(byte[0]);
    }
    let text = String::from_utf8(raw).unwrap();
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap().to_string();
    let headers = lines
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    (status_line, headers)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn options_answers_challenge() {
    let addr = start_server().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    let challenge = [42u8; 16];
    let request = format!(
        "OPTIONS * RTSP/1.0\r\nCSeq: 3\r\nApple-Challenge: {}\r\n\r\n",
        STANDARD_NO_PAD.encode(challenge)
    );
    let (status, headers) = roundtrip(&mut stream, &request).await;

    assert_eq!(status, "RTSP/1.0 200 OK");
    assert_eq!(header(&headers, "CSeq"), Some("3"));
    assert_eq!(header(&headers, "Server"), Some(server::SERVER_ID));
    let public = header(&headers, "Public").expect("Public header");
    for method in ["ANNOUNCE", "SETUP", "RECORD", "TEARDOWN", "OPTIONS"] {
        assert!(public.contains(method), "Public is missing {method}");
    }

    // Verify the signature the way a client would: over
    // challenge ‖ server IP (127.0.0.1) ‖ server MAC, zero-padded to 32.
    let response_b64 = header(&headers, "Apple-Response").expect("Apple-Response header");
    let signature = STANDARD_NO_PAD.decode(response_b64).unwrap();
    let mut message = Vec::new();
    message.extend_from_slice(&challenge);
    message.extend_from_slice(&[127, 0, 0, 1]);
    message.extend_from_slice(&MAC);
    message.resize(32, 0);
    RsaPrivateKey::from_pkcs1_pem(crypto::AIRPORT_PRIVATE_KEY_PEM)
        .unwrap()
        .to_public_key()
        .verify(Pkcs1v15Sign::new_unprefixed(), &message, &signature)
        .expect("Apple-Response must be a valid signature");
}

#[tokio::test]
async fn challenge_with_padding_is_accepted() {
    let addr = start_server().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nApple-Challenge: {}\r\n\r\n",
        STANDARD.encode([1u8; 16]) // '='-padded variant
    );
    let (status, headers) = roundtrip(&mut stream, &request).await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    assert!(header(&headers, "Apple-Response").is_some());
}

#[tokio::test]
async fn multiple_requests_per_connection_and_501_for_unimplemented() {
    let addr = start_server().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    let (status, headers) = roundtrip(&mut stream, "OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n").await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    assert_eq!(header(&headers, "CSeq"), Some("1"));

    // PAUSE is advertised but not implemented; it must still 501 cleanly.
    let pause = "PAUSE rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 2\r\n\r\n";
    let (status, headers) = roundtrip(&mut stream, pause).await;
    assert_eq!(status, "RTSP/1.0 501 Not Implemented");
    assert_eq!(header(&headers, "CSeq"), Some("2"));

    // The connection must survive the 501.
    let (status, _) = roundtrip(&mut stream, "OPTIONS * RTSP/1.0\r\nCSeq: 3\r\n\r\n").await;
    assert_eq!(status, "RTSP/1.0 200 OK");
}
