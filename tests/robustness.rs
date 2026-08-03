//! Milestone 4 integration tests: jitter-buffer reordering with a retransmit
//! request, and refusal of a second streaming client.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

use openairplay::server::{self, Context};
use openairplay::session::DecryptedAudio;
use openairplay::sink::AudioSink;
use openairplay::Config;

/// Tests never touch audio hardware: the sink discards everything.
struct Discard;

impl AudioSink for Discard {
    fn write(&mut self, _pcm: &[i16]) {}
    fn flush(&mut self) {}
}

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

async fn rtsp(stream: &mut TcpStream, req: &str) -> (String, Vec<(String, String)>) {
    stream.write_all(req.as_bytes()).await.unwrap();
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
    header(headers, "Transport")
        .unwrap()
        .split(';')
        .find_map(|kv| kv.trim().strip_prefix(&format!("{name}=")))
        .and_then(|v| v.parse().ok())
        .unwrap()
}

/// Unencrypted ANNOUNCE (no rsaaeskey/aesiv), so audio payloads are the raw
/// bytes we send — easy to assert on.
async fn announce(stream: &mut TcpStream) {
    let sdp = "v=0\r\no=s 1 0 IN IP4 127.0.0.1\r\ns=s\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
               m=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\n\
               a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n";
    let req = format!(
        "ANNOUNCE rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 1\r\n\
         Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    let (status, _) = rtsp(stream, &req).await;
    assert_eq!(status, "RTSP/1.0 200 OK");
}

fn audio_packet(seq: u16, ts: u32, payload: &[u8]) -> Vec<u8> {
    let mut pkt = vec![0x80, 0x60];
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&ts.to_be_bytes());
    pkt.extend_from_slice(&[0, 0, 0, 1]);
    pkt.extend_from_slice(payload);
    pkt
}

#[tokio::test]
async fn reorders_and_requests_resend() {
    let (addr, mut audio_rx) = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    announce(&mut stream).await;

    // Bind a UDP socket to act as the client control port; the receiver sends
    // resend requests here.
    let control = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let control_port = control.local_addr().unwrap().port();

    let setup = format!(
        "SETUP rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 2\r\n\
         Transport: RTP/AVP/UDP;unicast;mode=record;control_port={control_port};timing_port=6002\r\n\r\n"
    );
    let (status, headers) = rtsp(&mut stream, &setup).await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    let server_port = transport_port(&headers, "server_port");

    let (status, _) = rtsp(
        &mut stream,
        "RECORD rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 3\r\nRTP-Info: seq=100;rtptime=0\r\n\r\n",
    )
    .await;
    assert_eq!(status, "RTSP/1.0 200 OK");

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dst = SocketAddr::from(([127, 0, 0, 1], server_port));

    // Send seq 100 (payload A) and seq 102 (payload C); 101 is missing.
    let payload_a = [0x20u8, 0xAA, 0xAA];
    let payload_b = [0x20u8, 0xBB, 0xBB];
    let payload_c = [0x20u8, 0xCC, 0xCC];
    client
        .send_to(&audio_packet(100, 0, &payload_a), dst)
        .await
        .unwrap();
    client
        .send_to(&audio_packet(102, 704, &payload_c), dst)
        .await
        .unwrap();

    // 100 is delivered immediately; 102 is held pending 101.
    let first = tokio::time::timeout(Duration::from_secs(2), audio_rx.recv())
        .await
        .expect("delivery")
        .unwrap();
    assert_eq!(first.sequence, 100);
    assert_eq!(first.frame, payload_a);
    assert!(
        audio_rx.try_recv().is_err(),
        "102 must not be delivered yet"
    );

    // A resend request for the missing seq 101 should arrive on our control
    // socket: 80 D5, our-seq=1, first=101, count=1.
    let mut buf = [0u8; 32];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), control.recv_from(&mut buf))
        .await
        .expect("resend request")
        .unwrap();
    assert_eq!(&buf[..n], &[0x80, 0xD5, 0x00, 0x01, 0x00, 101, 0x00, 0x01]);

    // Now deliver the missing packet; 101 then 102 come out in order.
    client
        .send_to(&audio_packet(101, 352, &payload_b), dst)
        .await
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(2), audio_rx.recv())
        .await
        .expect("delivery")
        .unwrap();
    let third = tokio::time::timeout(Duration::from_secs(2), audio_rx.recv())
        .await
        .expect("delivery")
        .unwrap();
    assert_eq!(
        (second.sequence, second.frame.as_slice()),
        (101, payload_b.as_slice())
    );
    assert_eq!(
        (third.sequence, third.frame.as_slice()),
        (102, payload_c.as_slice())
    );
}

#[tokio::test]
async fn flush_discards_buffered_audio() {
    let (addr, mut audio_rx) = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    announce(&mut stream).await;
    let setup = "SETUP rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 2\r\n\
                 Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\r\n";
    let (_, headers) = rtsp(&mut stream, setup).await;
    let server_port = transport_port(&headers, "server_port");
    rtsp(
        &mut stream,
        "RECORD rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 3\r\nRTP-Info: seq=100;rtptime=0\r\n\r\n",
    )
    .await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dst = SocketAddr::from(([127, 0, 0, 1], server_port));

    // Deliver seq 100, then FLUSH forward to seq 200.
    client
        .send_to(&audio_packet(100, 0, &[0x20, 1]), dst)
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), audio_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.sequence, 100);

    let (status, _) = rtsp(
        &mut stream,
        "FLUSH rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 4\r\nRTP-Info: seq=200;rtptime=0\r\n\r\n",
    )
    .await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    tokio::time::sleep(Duration::from_millis(50)).await; // let the flush land

    // A late pre-flush packet (seq 100) is dropped; seq 200 plays.
    client
        .send_to(&audio_packet(100, 0, &[0x20, 9]), dst)
        .await
        .unwrap();
    client
        .send_to(&audio_packet(200, 0, &[0x20, 2]), dst)
        .await
        .unwrap();
    let next = tokio::time::timeout(Duration::from_secs(2), audio_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        next.sequence, 200,
        "post-flush packet, not the stale seq 100"
    );
    assert_eq!(next.frame, [0x20, 2]);
}

#[tokio::test]
async fn second_client_is_refused() {
    let (addr, _rx) = start().await;

    // Client A completes SETUP and holds the connection (and the slot) open.
    let mut a = TcpStream::connect(addr).await.unwrap();
    announce(&mut a).await;
    let setup = "SETUP rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 2\r\n\
                 Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\r\n";
    let (status, _) = rtsp(&mut a, setup).await;
    assert_eq!(status, "RTSP/1.0 200 OK");

    // Client B is refused at SETUP while A is active.
    let mut b = TcpStream::connect(addr).await.unwrap();
    announce(&mut b).await;
    let (status, _) = rtsp(&mut b, setup).await;
    assert_eq!(status, "RTSP/1.0 453 Not Enough Bandwidth");

    // After A disconnects (drops the slot), a new client can SETUP.
    drop(a);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut c = TcpStream::connect(addr).await.unwrap();
    announce(&mut c).await;
    let (status, _) = rtsp(&mut c, setup).await;
    assert_eq!(status, "RTSP/1.0 200 OK");
}
