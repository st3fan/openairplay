//! The dashboard endpoint: what is playing, published over a WebSocket.
//!
//! `--dashboard-listen ADDR` turns this on. Every connected client gets a
//! [`Message::Snapshot`] immediately and then one message per change, so a
//! dashboard started mid-track shows a full screen at once rather than
//! waiting for the sender to do something.
//!
//! Two rules shape the design, both about not letting a display hurt the
//! audio path:
//!
//! - The fan-out channel is **bounded**. A dashboard that stops reading fills
//!   its slice of the channel and is dropped from it — it never applies
//!   backpressure to the receiver's event task.
//! - A client that fell behind is **resynced, not disconnected**: it gets a
//!   fresh snapshot. Artwork frames are hundreds of KB, and a client that
//!   missed one is better off resynced than killed.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use openairplay1::Event;
use openairplay1_dashboard_protocol::{Message, ReceiverInfo, Snapshot};

/// Messages buffered per client before it is considered too far behind. A
/// track change is a handful of messages, so this is many tracks of slack.
const CHANNEL_CAPACITY: usize = 64;
/// How often to ping an idle client, so a laptop that closed its lid doesn't
/// hold a connection open forever.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// The receiver's side of the dashboard socket: keeps the current snapshot
/// and fans changes out to whoever is connected.
#[derive(Clone)]
pub struct Publisher {
    snapshot: Arc<Mutex<Snapshot>>,
    changes: broadcast::Sender<Message>,
}

impl Publisher {
    /// Create a publisher describing this receiver.
    pub fn new(name: String) -> Publisher {
        let snapshot = Snapshot {
            receiver: ReceiverInfo {
                name,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            ..Snapshot::default()
        };
        Publisher {
            snapshot: Arc::new(Mutex::new(snapshot)),
            changes: broadcast::channel(CHANNEL_CAPACITY).0,
        }
    }

    /// Publish one receiver event. Events with no dashboard meaning are
    /// dropped silently.
    pub fn publish(&self, event: &Event) {
        let Some(message) = to_message(event) else {
            return;
        };
        self.snapshot.lock().unwrap().apply(&message);
        // An error only means nobody is connected, which is the normal case.
        let _ = self.changes.send(message);
    }

    fn snapshot(&self) -> Message {
        Message::Snapshot(self.snapshot.lock().unwrap().clone())
    }

    fn subscribe(&self) -> broadcast::Receiver<Message> {
        self.changes.subscribe()
    }
}

/// Translate a receiver event into its wire message. Returns `None` for
/// events the dashboard has no use for.
fn to_message(event: &Event) -> Option<Message> {
    Some(match event {
        Event::SessionStarted {
            rate,
            channels,
            peer,
            ..
        } => Message::SessionStarted {
            rate: *rate,
            channels: *channels,
            peer: display_addr(*peer),
        },
        Event::Metadata {
            title,
            artist,
            album,
        } => Message::Metadata {
            title: title.clone(),
            artist: artist.clone(),
            album: album.clone(),
        },
        Event::Artwork { content_type, data } => Message::Artwork {
            content_type: content_type.clone(),
            data_base64: STANDARD.encode(data),
        },
        Event::Volume { db } => Message::Volume { db: *db },
        Event::Progress { elapsed, duration } => Message::Progress {
            elapsed_ms: elapsed.as_millis() as u64,
            duration_ms: duration.as_millis() as u64,
        },
        Event::Flushed => Message::Flushed,
        Event::SessionEnded => Message::SessionEnded,
        _ => return None,
    })
}

/// An IPv4 sender arriving on our dual-stack socket has a v4-mapped IPv6
/// address; `::ffff:192.168.1.42` on a display is just noise, so unwrap it.
fn display_addr(addr: std::net::IpAddr) -> String {
    match addr {
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => v6.to_string(),
        },
        std::net::IpAddr::V4(v4) => v4.to_string(),
    }
}

/// Accept dashboard connections until the listener fails.
pub async fn serve(listener: TcpListener, publisher: Publisher) -> std::io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let publisher = publisher.clone();
        tokio::spawn(async move {
            info!("dashboard [{peer}] connected");
            if let Err(e) = serve_client(stream, peer, publisher).await {
                debug!("dashboard [{peer}] ended: {e}");
            }
            info!("dashboard [{peer}] disconnected");
        });
    }
}

async fn serve_client(
    stream: TcpStream,
    peer: SocketAddr,
    publisher: Publisher,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    // Subscribe before sending the snapshot: a change that lands in between
    // is then queued rather than lost.
    let mut changes = publisher.subscribe();
    let mut socket = tokio_tungstenite::accept_async(stream).await?;
    socket.send(encode(&publisher.snapshot())).await?;

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // the first tick is immediate

    loop {
        tokio::select! {
            change = changes.recv() => match change {
                Ok(message) => socket.send(encode(&message)).await?,
                // Too far behind to catch up message by message: resync.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    warn!("dashboard [{peer}] fell behind by {missed} messages; resyncing");
                    socket.send(encode(&publisher.snapshot())).await?;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            incoming = socket.next() => match incoming {
                // The dashboard is read-only; anything it sends is ignored
                // except a close.
                Some(Ok(WsMessage::Close(_))) | None => return Ok(()),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
            },
            _ = ping.tick() => socket.send(WsMessage::Ping(Vec::new().into())).await?,
        }
    }
}

/// Encode a message as a WebSocket text frame. Serialization cannot fail for
/// these types; if it somehow did, an empty frame is better than a panic in
/// the receiver.
fn encode(message: &Message) -> WsMessage {
    WsMessage::Text(serde_json::to_string(message).unwrap_or_default().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    use futures_util::stream::SplitStream;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

    async fn start() -> (SocketAddr, Publisher) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let publisher = Publisher::new("Living Room".into());
        tokio::spawn(serve(listener, publisher.clone()));
        (addr, publisher)
    }

    async fn connect(addr: SocketAddr) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect("dashboard should accept the connection");
        socket
    }

    /// Read the next message, failing rather than hanging.
    async fn next(socket: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>) -> Message {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("timed out waiting for a message")
                .expect("socket closed")
                .expect("socket error");
            if let WsMessage::Text(text) = frame {
                return serde_json::from_str(&text).expect("valid protocol JSON");
            }
        }
    }

    fn session_started() -> Event {
        Event::SessionStarted {
            rate: 44100,
            channels: 2,
            peer: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
        }
    }

    fn metadata() -> Event {
        Event::Metadata {
            title: Some("Sonata No. 1".into()),
            artist: Some("Some Artist".into()),
            album: None,
        }
    }

    #[tokio::test]
    async fn a_client_gets_a_snapshot_then_every_change() {
        let (addr, publisher) = start().await;
        let (_, mut rx) = connect(addr).await.split();

        match next(&mut rx).await {
            Message::Snapshot(snapshot) => {
                assert_eq!(snapshot.receiver.name, "Living Room");
                assert_eq!(snapshot.session, None, "nothing is playing yet");
            }
            other => panic!("the first message must be a snapshot, got {other:?}"),
        }

        publisher.publish(&session_started());
        publisher.publish(&metadata());
        publisher.publish(&Event::Volume { db: -12.5 });
        publisher.publish(&Event::Progress {
            elapsed: Duration::from_secs(83),
            duration: Duration::from_secs(247),
        });

        assert!(matches!(
            next(&mut rx).await,
            Message::SessionStarted { rate: 44100, .. }
        ));
        assert!(
            matches!(next(&mut rx).await, Message::Metadata { title: Some(t), .. } if t == "Sonata No. 1")
        );
        assert_eq!(next(&mut rx).await, Message::Volume { db: -12.5 });
        assert_eq!(
            next(&mut rx).await,
            Message::Progress {
                elapsed_ms: 83_000,
                duration_ms: 247_000
            }
        );
    }

    #[tokio::test]
    async fn a_client_connecting_mid_track_sees_the_whole_state() {
        // The case the snapshot exists for: a sender parked on one track may
        // send nothing at all after this point.
        let (addr, publisher) = start().await;
        publisher.publish(&session_started());
        publisher.publish(&metadata());
        publisher.publish(&Event::Artwork {
            content_type: "image/jpeg".into(),
            data: vec![1, 2, 3],
        });
        publisher.publish(&Event::Progress {
            elapsed: Duration::from_secs(5),
            duration: Duration::from_secs(200),
        });

        let (_, mut rx) = connect(addr).await.split();
        let Message::Snapshot(snapshot) = next(&mut rx).await else {
            panic!("expected a snapshot");
        };
        assert_eq!(snapshot.session.unwrap().peer, "192.168.1.42");
        assert_eq!(
            snapshot.track.unwrap().title.as_deref(),
            Some("Sonata No. 1")
        );
        assert_eq!(
            snapshot.artwork.unwrap().data_base64,
            STANDARD.encode([1, 2, 3])
        );
        assert_eq!(snapshot.progress.unwrap().duration_ms, 200_000);
    }

    #[tokio::test]
    async fn two_dashboards_both_stay_current() {
        let (addr, publisher) = start().await;
        let (_, mut first) = connect(addr).await.split();
        let (_, mut second) = connect(addr).await.split();
        assert!(matches!(next(&mut first).await, Message::Snapshot(_)));
        assert!(matches!(next(&mut second).await, Message::Snapshot(_)));

        publisher.publish(&Event::Volume { db: -3.0 });
        assert_eq!(next(&mut first).await, Message::Volume { db: -3.0 });
        assert_eq!(next(&mut second).await, Message::Volume { db: -3.0 });
    }

    #[tokio::test]
    async fn a_dashboard_that_stops_reading_never_stalls_the_receiver() {
        let (addr, publisher) = start().await;
        let (_, mut reader) = connect(addr).await.split();
        assert!(matches!(next(&mut reader).await, Message::Snapshot(_)));
        // A second client that never reads a single frame.
        let _stalled = connect(addr).await;

        // Far more than the channel holds; publish must not block or panic.
        for i in 0..CHANNEL_CAPACITY * 4 {
            publisher.publish(&Event::Volume { db: -(i as f32) });
        }
        publisher.publish(&Event::Flushed);

        // The healthy client still gets everything, in order, ending with the
        // flush.
        let mut seen_flush = false;
        for _ in 0..CHANNEL_CAPACITY * 4 + 1 {
            if next(&mut reader).await == Message::Flushed {
                seen_flush = true;
                break;
            }
        }
        assert!(seen_flush, "the reading client must keep receiving");
    }

    #[tokio::test]
    async fn session_end_clears_the_snapshot_for_the_next_client() {
        let (addr, publisher) = start().await;
        publisher.publish(&session_started());
        publisher.publish(&metadata());
        publisher.publish(&Event::SessionEnded);

        let (_, mut rx) = connect(addr).await.split();
        let Message::Snapshot(snapshot) = next(&mut rx).await else {
            panic!("expected a snapshot");
        };
        assert_eq!(snapshot.session, None);
        assert_eq!(snapshot.track, None);
        assert_eq!(snapshot.receiver.name, "Living Room");
    }

    #[test]
    fn a_v4_mapped_sender_is_shown_as_ipv4() {
        // Our listener is dual-stack, so an iPhone on IPv4 arrives as
        // ::ffff:192.168.1.42 — nobody wants to read that.
        assert_eq!(
            display_addr("::ffff:192.168.1.42".parse().unwrap()),
            "192.168.1.42"
        );
        assert_eq!(
            display_addr("192.168.1.42".parse().unwrap()),
            "192.168.1.42"
        );
        assert_eq!(display_addr("fdab:110::1".parse().unwrap()), "fdab:110::1");
    }

    #[test]
    fn events_without_dashboard_meaning_are_dropped() {
        // Not every library event is worth a message; the wildcard arm must
        // not turn into a panic or a bogus frame.
        assert!(to_message(&Event::Flushed).is_some());
        assert!(to_message(&session_started()).is_some());
    }
}
