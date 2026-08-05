//! An embeddable AirPlay 1 (RAOP / AirTunes) audio **receiver**: stock
//! Apple senders (iPhone, iPad, Mac, iTunes) discover it, handshake with
//! it, and stream ALAC to it; the host application gets decoded PCM and
//! session events. The library owns network → PCM (discovery advertisement,
//! the RTSP handshake with `Apple-Challenge`, the three RTP channels,
//! decrypt, the jitter buffer with retransmits, the NTP clock model, ALAC
//! decode, and the prebuffer/latency-correct start); the host owns PCM →
//! speaker via an [`AudioSink`].
//!
//! Deliberate scope: one sender → one stream → one output. No AirPlay wire
//! concepts (RTSP, SDP, RTP, sequence numbers) appear in this API.
//!
//! ```no_run
//! use openairplay1::{AudioSink, Event, Receiver};
//!
//! struct MySink;
//!
//! impl AudioSink for MySink {
//!     fn write(&mut self, pcm: &[i16]) { /* blocking write paces playback */ }
//!     fn flush(&mut self) { /* seek: drop device state */ }
//! }
//!
//! #[tokio::main]
//! async fn main() -> std::io::Result<()> {
//!     let receiver = Receiver::builder().name("Office").build()?;
//!     let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
//!     tokio::spawn(async move {
//!         while let Some(event) = rx.recv().await {
//!             if let Event::Volume { db } = event { /* your gain path */ }
//!         }
//!     });
//!     receiver.run(|_rate, _channels| Box::new(MySink), events).await
//! }
//! ```

#![warn(missing_docs)]

mod avahi;
mod decode;
mod dmap;
mod events;
mod jitter;
mod mac;
mod player;
mod receiver;
mod rtp;
mod rtsp;
mod sdp;
mod session;
mod sink;

// Sender-facing pieces the integration tests (and a future test-sender)
// drive the real server with. Public so the tests can reach them, but not
// part of the documented embedding API.
#[doc(hidden)]
pub mod clock;
#[doc(hidden)]
pub mod crypto;
#[doc(hidden)]
pub mod server;
#[doc(hidden)]
pub use session::{AudioObserver, DecryptedAudio};

pub use avahi::txt_records;
pub use events::{Event, EventSender};
pub use receiver::{Receiver, ReceiverBuilder};
pub use sink::{AudioSink, SinkFactory};

/// Receiver-wide configuration, resolved by [`ReceiverBuilder::build`].
/// Shared by the RTSP server and the mDNS advertisement: the MAC address
/// must be the same in both places — clients verify the `Apple-Response`
/// signature against the MAC embedded in the service name.
#[derive(Debug, Clone)]
pub struct Config {
    /// The receiver name senders see in the AirPlay picker.
    pub name: String,
    /// TCP port of the RTSP control server.
    pub port: u16,
    /// The MAC address used in the service name and the `Apple-Challenge`
    /// signature.
    pub mac: [u8; 6],
    /// `Some` → the receiver requires a password to stream (advertised as
    /// `pw=true` in `_raop._tcp`); `None` (the default) → open (`pw=false`).
    /// The password itself never appears in the advertisement or any log.
    pub password: Option<String>,
}

impl Config {
    /// MAC as 12 uppercase hex digits, e.g. `AABBCCDDEEFF`, as used in the
    /// `_raop._tcp` service name.
    pub fn mac_hex(&self) -> String {
        self.mac.iter().map(|b| format!("{b:02X}")).collect()
    }

    /// The mDNS service name: `<MAC>@<FriendlyName>`.
    pub fn service_name(&self) -> String {
        format!("{}@{}", self.mac_hex(), self.name)
    }
}
