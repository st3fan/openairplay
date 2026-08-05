//! Milestone 5 integration test: the receiver runs the NTP timing exchange —
//! it sends a `0xd2` request to the client's timing port and folds the `0xd3`
//! reply into its clock model — and consumes sync packets, all without
//! disturbing in-order audio delivery.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

use openairplay1::clock::ns_to_ntp;
use openairplay1::server::{self, Context};
use openairplay1::AudioSink;
use openairplay1::Config;
use openairplay1::DecryptedAudio;

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
        pincode: None,
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

fn transport_port(headers: &[(String, String)], name: &str) -> u16 {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Transport"))
        .map(|(_, v)| v.as_str())
        .unwrap()
        .split(';')
        .find_map(|kv| kv.trim().strip_prefix(&format!("{name}=")))
        .and_then(|v| v.parse().ok())
        .unwrap()
}

#[tokio::test]
async fn receiver_runs_the_timing_exchange_and_takes_sync() {
    let (addr, mut audio_rx) = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    // Unencrypted ANNOUNCE so audio payloads are the raw bytes we send.
    let sdp = "v=0\r\no=s 1 0 IN IP4 127.0.0.1\r\ns=s\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
               m=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\n\
               a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n";
    let req = format!(
        "ANNOUNCE rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 1\r\n\
         Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    let (status, _) = rtsp(&mut stream, &req).await;
    assert_eq!(status, "RTSP/1.0 200 OK");

    // Bind timing and control sockets and advertise their ports in SETUP.
    let timing = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let control = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let timing_port = timing.local_addr().unwrap().port();
    let control_port = control.local_addr().unwrap().port();
    let setup = format!(
        "SETUP rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 2\r\n\
         Transport: RTP/AVP/UDP;unicast;mode=record;control_port={control_port};timing_port={timing_port}\r\n\r\n"
    );
    let (status, headers) = rtsp(&mut stream, &setup).await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    let server_port = transport_port(&headers, "server_port");

    rtsp(
        &mut stream,
        "RECORD rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 3\r\nRTP-Info: seq=100;rtptime=0\r\n\r\n",
    )
    .await;

    // The receiver should send us a 0xd2 timing request; reply with 0xd3.
    let mut buf = [0u8; 64];
    let (n, from) = tokio::time::timeout(Duration::from_secs(2), timing.recv_from(&mut buf))
        .await
        .expect("timing request should arrive")
        .unwrap();
    assert_eq!(n, 32);
    assert_eq!(buf[0], 0x80);
    assert_eq!(buf[1], 0xD2, "must be a timing request");

    // Reply: client receive at 5.0 s, transmit at 5.001 s (its clock).
    let mut reply = [0u8; 32];
    reply[0] = 0x80;
    reply[1] = 0xD3;
    let (rs, rf) = ns_to_ntp(5_000_000_000);
    let (ts, tf) = ns_to_ntp(5_001_000_000);
    reply[16..20].copy_from_slice(&rs.to_be_bytes());
    reply[20..24].copy_from_slice(&rf.to_be_bytes());
    reply[24..28].copy_from_slice(&ts.to_be_bytes());
    reply[28..32].copy_from_slice(&tf.to_be_bytes());
    timing.send_to(&reply, from).await.unwrap();

    // Send a sync packet on the control channel: at client time 5.0 s, RTP 100
    // is at the DAC.
    let mut sync = [0u8; 20];
    sync[0] = 0x80;
    sync[1] = 0xD4;
    sync[4..8].copy_from_slice(&100u32.to_be_bytes()); // rtp at DAC
    let (ss, sf) = ns_to_ntp(5_000_000_000);
    sync[8..12].copy_from_slice(&ss.to_be_bytes());
    sync[12..16].copy_from_slice(&sf.to_be_bytes());
    sync[16..20].copy_from_slice(&11125u32.to_be_bytes()); // rtp current
    control.send_to(&sync, from).await.unwrap();

    // Audio still flows in order regardless of the timing machinery.
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dst = SocketAddr::from(([127, 0, 0, 1], server_port));
    let mut pkt = vec![0x80, 0x60, 0x00, 100]; // seq 100
    pkt.extend_from_slice(&0u32.to_be_bytes());
    pkt.extend_from_slice(&[0, 0, 0, 1]);
    pkt.extend_from_slice(&[0x20, 0x11, 0x22]);
    client.send_to(&pkt, dst).await.unwrap();

    let delivered = tokio::time::timeout(Duration::from_secs(2), audio_rx.recv())
        .await
        .expect("audio delivered")
        .unwrap();
    assert_eq!(delivered.sequence, 100);
    assert_eq!(delivered.frame, [0x20, 0x11, 0x22]);
}
