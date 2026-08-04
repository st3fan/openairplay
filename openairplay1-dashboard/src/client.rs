//! The WebSocket client: keeps a connection to a receiver's dashboard
//! endpoint, and reports what happens on it to the display.
//!
//! The receiver may be down when the dashboard starts, may restart under it,
//! or may live on a machine that reboots — so this reconnects for as long as
//! the dashboard runs, and the display shows the connection state rather than
//! a stale screen or an exit.

use std::time::Duration;

use futures_util::StreamExt;
use log::{debug, warn};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use openairplay1_dashboard_protocol::Message;

/// First reconnect delay, doubling up to [`MAX_BACKOFF`] and reset by a
/// successful connection.
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// What the client tells the display.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// The socket is up; a snapshot follows immediately.
    Connected,
    /// A message from the receiver. Boxed: a snapshot carries the artwork,
    /// which dwarfs every other variant.
    Message(Box<Message>),
    /// The socket went away; the client is retrying.
    Disconnected,
}

/// Connect to `url` and forward everything to `updates`, reconnecting for as
/// long as the display is listening. Returns when the display is gone.
pub async fn run(url: String, updates: UnboundedSender<Update>) {
    let mut backoff = FIRST_BACKOFF;
    loop {
        match session(&url, &updates).await {
            // A connection that worked earns a fresh backoff.
            Ok(true) => backoff = FIRST_BACKOFF,
            Ok(false) => return, // the display hung up
            Err(e) => debug!("dashboard connection to {url} failed: {e}"),
        }
        if updates.send(Update::Disconnected).is_err() {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// One connection attempt. `Ok(true)` means it connected and later dropped;
/// `Ok(false)` means the display is gone and we should stop.
async fn session(
    url: &str,
    updates: &UnboundedSender<Update>,
) -> Result<bool, tokio_tungstenite::tungstenite::Error> {
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await?;
    debug!("connected to {url}");
    if updates.send(Update::Connected).is_err() {
        return Ok(false);
    }

    while let Some(frame) = socket.next().await {
        let text = match frame? {
            WsMessage::Text(text) => text,
            // Pings are answered by the library; nothing else is expected.
            WsMessage::Close(_) => break,
            _ => continue,
        };
        match serde_json::from_str::<Message>(&text) {
            Ok(message) => {
                if updates.send(Update::Message(Box::new(message))).is_err() {
                    return Ok(false);
                }
            }
            // A message type this build doesn't know: skip it rather than
            // dropping the connection, so an older dashboard keeps working
            // against a newer receiver.
            Err(e) => warn!("ignoring unrecognized dashboard message: {e}"),
        }
    }
    Ok(true)
}
