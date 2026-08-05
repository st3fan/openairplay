//! End-to-end metadata/artwork test: drive ANNOUNCE → SETUP over a real TCP
//! connection, then send the `SET_PARAMETER` requests a sender pushes for
//! now-playing info, and confirm they arrive on the host's event channel with
//! the right content. This is what proves the `Content-Type` plumbing —
//! header to event — rather than just the parsing.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use openairplay1::server::{self, Context};
use openairplay1::{AudioSink, Config, Event};

/// Tests never touch audio hardware: the sink discards everything.
struct Discard;

impl AudioSink for Discard {
    fn write(&mut self, _pcm: &[i16]) {}
    fn flush(&mut self) {}
}

const SDP_BODY: &str = "v=0\r\n\
    o=iTunes 3413821438 0 IN IP4 127.0.0.1\r\n\
    s=iTunes\r\n\
    c=IN IP4 127.0.0.1\r\n\
    t=0 0\r\n\
    m=audio 0 RTP/AVP 96\r\n\
    a=rtpmap:96 AppleLossless\r\n\
    a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n";

async fn start() -> (SocketAddr, tokio::sync::mpsc::UnboundedReceiver<Event>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        name: "Test".to_string(),
        port: addr.port(),
        mac: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
        password: None,
    };
    let (events, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let context = Arc::new(Context {
        config,
        sink_factory: Arc::new(|_rate, _channels| Box::new(Discard)),
        events,
    });
    tokio::spawn(server::serve(listener, context));
    (addr, event_rx)
}

/// Send one RTSP request (head plus optional body) and return its status line.
async fn rtsp(stream: &mut TcpStream, head: &str, body: &[u8]) -> String {
    let mut raw = head.as_bytes().to_vec();
    raw.extend_from_slice(body);
    stream.write_all(&raw).await.unwrap();

    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    while !response.ends_with(b"\r\n\r\n") {
        assert_eq!(stream.read(&mut byte).await.unwrap(), 1, "eof from server");
        response.push(byte[0]);
    }
    String::from_utf8(response)
        .unwrap()
        .split("\r\n")
        .next()
        .unwrap()
        .to_string()
}

/// A `SET_PARAMETER` carrying `body` with the given content type.
async fn set_parameter(
    stream: &mut TcpStream,
    cseq: u32,
    content_type: &str,
    body: &[u8],
) -> String {
    let head = format!(
        "SET_PARAMETER rtsp://127.0.0.1/1 RTSP/1.0\r\n\
         CSeq: {cseq}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    rtsp(stream, &head, body).await
}

fn dmap_entry(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut entry = tag.to_vec();
    entry.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    entry.extend_from_slice(payload);
    entry
}

/// The shape a sender pushes on a track change: `mlit` around the fields,
/// with tags we don't care about interleaved.
fn dmap_track() -> Vec<u8> {
    let inner = [
        dmap_entry(b"mikd", &[2]),
        dmap_entry(b"minm", "Sonata No. 1".as_bytes()),
        dmap_entry(b"asar", "Some Artist".as_bytes()),
        dmap_entry(b"astm", &180_000u32.to_be_bytes()),
        dmap_entry(b"asal", "Some Album".as_bytes()),
    ]
    .concat();
    dmap_entry(b"mlit", &inner)
}

/// Wait for the next event, failing rather than hanging if none arrives.
async fn next_event(events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("event channel closed")
}

/// Take a connection through ANNOUNCE → SETUP so it is streaming.
async fn start_session(
    stream: &mut TcpStream,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
) {
    let announce = format!(
        "ANNOUNCE rtsp://127.0.0.1/1 RTSP/1.0\r\n\
         CSeq: 1\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
        SDP_BODY.len()
    );
    assert_eq!(
        rtsp(stream, &announce, SDP_BODY.as_bytes()).await,
        "RTSP/1.0 200 OK"
    );
    let setup = "SETUP rtsp://127.0.0.1/1 RTSP/1.0\r\n\
         CSeq: 2\r\n\
         Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\r\n";
    assert_eq!(rtsp(stream, setup, b"").await, "RTSP/1.0 200 OK");
    assert!(matches!(
        next_event(events).await,
        Event::SessionStarted {
            rate: 44100,
            channels: 2,
            ..
        }
    ));
}

#[tokio::test]
async fn metadata_and_artwork_reach_the_host() {
    let (addr, mut events) = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    start_session(&mut stream, &mut events).await;

    let status = set_parameter(&mut stream, 3, "application/x-dmap-tagged", &dmap_track()).await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    assert_eq!(
        next_event(&mut events).await,
        Event::Metadata {
            title: Some("Sonata No. 1".into()),
            artist: Some("Some Artist".into()),
            album: Some("Some Album".into()),
        }
    );

    // Artwork the size real senders send (~180 KB in one capture) — and well
    // over the RTSP body limit this library used to have.
    let jpeg: Vec<u8> = std::iter::once(0xff)
        .chain(std::iter::once(0xd8))
        .chain((0..2 * 1024 * 1024).map(|i| (i % 251) as u8))
        .collect();
    let status = set_parameter(&mut stream, 4, "image/jpeg", &jpeg).await;
    assert_eq!(status, "RTSP/1.0 200 OK", "a 2 MB body must not be refused");
    match next_event(&mut events).await {
        Event::Artwork { content_type, data } => {
            assert_eq!(content_type, "image/jpeg");
            assert_eq!(data, jpeg, "artwork is forwarded byte-for-byte");
        }
        other => panic!("expected Artwork, got {other:?}"),
    }

    // Mid-track clear: image/none with an empty body.
    let status = set_parameter(&mut stream, 5, "image/none", b"").await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    assert_eq!(
        next_event(&mut events).await,
        Event::Artwork {
            content_type: "image/none".into(),
            data: Vec::new(),
        }
    );

    // The volume path still works on the same connection, and a payload we
    // don't recognize is acknowledged without an event.
    let status = set_parameter(&mut stream, 6, "text/parameters", b"volume: -20.000000").await;
    assert_eq!(status, "RTSP/1.0 200 OK");
    assert_eq!(next_event(&mut events).await, Event::Volume { db: -20.0 });

    let status = set_parameter(&mut stream, 7, "application/octet-stream", b"\x00\x01").await;
    assert_eq!(status, "RTSP/1.0 200 OK");

    let teardown = "TEARDOWN rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 8\r\n\r\n";
    assert_eq!(rtsp(&mut stream, teardown, b"").await, "RTSP/1.0 200 OK");
    assert_eq!(next_event(&mut events).await, Event::SessionEnded);
}

#[tokio::test]
async fn metadata_sent_before_setup_arrives_inside_the_session() {
    // Whether a sender pushes metadata before or after SETUP is up to the
    // sender; the host's contract is that it lands inside the session either
    // way.
    let (addr, mut events) = start().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    let status = set_parameter(&mut stream, 0, "application/x-dmap-tagged", &dmap_track()).await;
    assert_eq!(status, "RTSP/1.0 200 OK");

    start_session(&mut stream, &mut events).await; // asserts SessionStarted comes first

    assert_eq!(
        next_event(&mut events).await,
        Event::Metadata {
            title: Some("Sonata No. 1".into()),
            artist: Some("Some Artist".into()),
            album: Some("Some Album".into()),
        }
    );
}
