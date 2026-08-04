//! The wire format between an `openairplay1-receiver` and a dashboard.
//!
//! The receiver publishes what it knows about the current stream as JSON text
//! frames on a WebSocket; `openairplay1-dashboard` (or any other client — a
//! browser, say) subscribes and renders it. This crate is only the types, so
//! both ends agree on one definition.
//!
//! Two shapes travel over the socket, both variants of [`Message`]:
//!
//! - a [`Snapshot`], sent once immediately on connect, carrying everything
//!   the receiver currently knows. A dashboard that starts mid-track would
//!   otherwise sit blank until the next change, which for a sender parked on
//!   one track may be never.
//! - one message per change afterwards, mirroring the receiver's own events.
//!
//! Conventions: durations are integer milliseconds (JSON has no duration),
//! the sender's address is a string (it may be IPv4 or IPv6), and cover art
//! is base64 so a browser can use it as a `data:` URL without a second
//! request. Empty artwork data means the sender cleared the art.
//!
//! ```
//! use openairplay1_dashboard_protocol::Message;
//!
//! let json = r#"{"type":"volume","db":-12.5}"#;
//! assert!(matches!(
//!     serde_json::from_str::<Message>(json).unwrap(),
//!     Message::Volume { db } if db == -12.5
//! ));
//! ```

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

/// One message on the dashboard socket.
///
/// `#[non_exhaustive]`: clients must tolerate message types they don't know
/// (`serde` will fail to deserialize them, which a client should treat as
/// "ignore and carry on", not as a fatal error).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// Everything the receiver knows right now. Sent on connect, and again to
    /// resync a client that fell behind.
    Snapshot(Snapshot),
    /// A sender started streaming.
    SessionStarted {
        /// Sample rate in Hz.
        rate: u32,
        /// Channel count.
        channels: u8,
        /// The address the sender connected from.
        peer: String,
    },
    /// Track metadata: a complete statement about the current track, not a
    /// delta — absent fields are `null` and replace whatever was there.
    Metadata {
        /// Track title.
        title: Option<String>,
        /// Track artist.
        artist: Option<String>,
        /// Track album.
        album: Option<String>,
    },
    /// Cover art, base64-encoded. Empty `data_base64` means it was cleared.
    Artwork {
        /// The image media type as the sender labelled it.
        content_type: String,
        /// Base64 of the image bytes; empty when cleared.
        data_base64: String,
    },
    /// Volume in AirPlay dB (0 = full scale, −144 = mute).
    Volume {
        /// AirPlay volume in dB.
        db: f32,
    },
    /// Playback position. Senders report this every few seconds rather than
    /// continuously, so a running clock extrapolates between messages.
    Progress {
        /// How far into the track playback is.
        elapsed_ms: u64,
        /// The track's length; zero when the sender reports no known end.
        duration_ms: u64,
    },
    /// The sender sought or stopped; the position is stale until the next
    /// [`Message::Progress`].
    Flushed,
    /// The streaming session ended.
    SessionEnded,
}

/// Everything the receiver currently knows, as one message.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The receiver publishing this.
    pub receiver: ReceiverInfo,
    /// The stream in progress, if any.
    pub session: Option<SessionInfo>,
    /// The current track, if the sender sent metadata.
    pub track: Option<Track>,
    /// Volume in AirPlay dB, if the sender set one.
    pub volume_db: Option<f32>,
    /// Playback position as last reported.
    pub progress: Option<Progress>,
    /// The current cover art, if any.
    pub artwork: Option<Artwork>,
}

/// Which receiver a dashboard is looking at.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReceiverInfo {
    /// The name the receiver advertises over mDNS.
    pub name: String,
    /// The receiver's crate version, so a dashboard can spot a mismatch.
    pub version: String,
}

/// The stream's format and where it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Sample rate in Hz.
    pub rate: u32,
    /// Channel count.
    pub channels: u8,
    /// The address the sender connected from.
    pub peer: String,
}

/// Track metadata.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Track title.
    pub title: Option<String>,
    /// Track artist.
    pub artist: Option<String>,
    /// Track album.
    pub album: Option<String>,
}

/// Playback position within the current track.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// How far into the track playback is.
    pub elapsed_ms: u64,
    /// The track's length; zero when there is no known end.
    pub duration_ms: u64,
}

/// Cover art as sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artwork {
    /// The image media type as the sender labelled it.
    pub content_type: String,
    /// Base64 of the image bytes.
    pub data_base64: String,
}

impl Snapshot {
    /// Fold a change message into this snapshot, so the receiver can hand the
    /// current state to a client that connects later. A [`Message::Snapshot`]
    /// replaces the whole thing.
    pub fn apply(&mut self, message: &Message) {
        match message {
            Message::Snapshot(snapshot) => *self = snapshot.clone(),
            Message::SessionStarted {
                rate,
                channels,
                peer,
            } => {
                self.session = Some(SessionInfo {
                    rate: *rate,
                    channels: *channels,
                    peer: peer.clone(),
                });
            }
            Message::Metadata {
                title,
                artist,
                album,
            } => {
                self.track = Some(Track {
                    title: title.clone(),
                    artist: artist.clone(),
                    album: album.clone(),
                });
            }
            Message::Artwork {
                content_type,
                data_base64,
            } => {
                self.artwork = (!data_base64.is_empty()).then(|| Artwork {
                    content_type: content_type.clone(),
                    data_base64: data_base64.clone(),
                });
            }
            Message::Volume { db } => self.volume_db = Some(*db),
            Message::Progress {
                elapsed_ms,
                duration_ms,
            } => {
                self.progress = Some(Progress {
                    elapsed_ms: *elapsed_ms,
                    duration_ms: *duration_ms,
                });
            }
            // A seek makes the position stale until the sender reports again.
            Message::Flushed => self.progress = None,
            // Everything but the receiver's own identity goes away.
            Message::SessionEnded => {
                let receiver = std::mem::take(&mut self.receiver);
                *self = Snapshot {
                    receiver,
                    ..Snapshot::default()
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(message: &Message) -> Message {
        let json = serde_json::to_string(message).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn every_message_survives_a_roundtrip() {
        for message in [
            Message::SessionStarted {
                rate: 44100,
                channels: 2,
                peer: "192.168.1.42".into(),
            },
            Message::Metadata {
                title: Some("Sonata No. 1".into()),
                artist: None,
                album: Some("Some Album".into()),
            },
            Message::Artwork {
                content_type: "image/jpeg".into(),
                data_base64: "AAEC".into(),
            },
            Message::Volume { db: -12.5 },
            Message::Progress {
                elapsed_ms: 83_000,
                duration_ms: 247_000,
            },
            Message::Flushed,
            Message::SessionEnded,
        ] {
            assert_eq!(roundtrip(&message), message);
        }
    }

    /// The JSON is the published contract; changing it silently would break
    /// every client. These are the bytes.
    #[test]
    fn the_wire_format_is_what_the_plan_documents() {
        let cases = [
            (
                Message::SessionStarted {
                    rate: 44100,
                    channels: 2,
                    peer: "192.168.1.42".into(),
                },
                r#"{"type":"session_started","rate":44100,"channels":2,"peer":"192.168.1.42"}"#,
            ),
            (
                Message::Metadata {
                    title: Some("Song".into()),
                    artist: None,
                    album: None,
                },
                r#"{"type":"metadata","title":"Song","artist":null,"album":null}"#,
            ),
            (
                Message::Artwork {
                    content_type: "image/jpeg".into(),
                    data_base64: "AAEC".into(),
                },
                r#"{"type":"artwork","content_type":"image/jpeg","data_base64":"AAEC"}"#,
            ),
            (
                Message::Volume { db: -12.5 },
                r#"{"type":"volume","db":-12.5}"#,
            ),
            (
                Message::Progress {
                    elapsed_ms: 83_000,
                    duration_ms: 247_000,
                },
                r#"{"type":"progress","elapsed_ms":83000,"duration_ms":247000}"#,
            ),
            (Message::Flushed, r#"{"type":"flushed"}"#),
            (Message::SessionEnded, r#"{"type":"session_ended"}"#),
        ];
        for (message, expected) in cases {
            assert_eq!(serde_json::to_string(&message).unwrap(), expected);
        }
    }

    #[test]
    fn a_snapshot_carries_its_fields_inline_with_the_tag() {
        let snapshot = Snapshot {
            receiver: ReceiverInfo {
                name: "Living Room".into(),
                version: "0.2.0".into(),
            },
            ..Snapshot::default()
        };
        let json = serde_json::to_string(&Message::Snapshot(snapshot.clone())).unwrap();
        assert_eq!(
            json,
            r#"{"type":"snapshot","receiver":{"name":"Living Room","version":"0.2.0"},"session":null,"track":null,"volume_db":null,"progress":null,"artwork":null}"#
        );
        assert_eq!(
            serde_json::from_str::<Message>(&json).unwrap(),
            Message::Snapshot(snapshot)
        );
    }

    #[test]
    fn applying_messages_builds_the_snapshot_a_late_client_gets() {
        let mut snapshot = Snapshot {
            receiver: ReceiverInfo {
                name: "Living Room".into(),
                version: "0.2.0".into(),
            },
            ..Snapshot::default()
        };
        snapshot.apply(&Message::SessionStarted {
            rate: 44100,
            channels: 2,
            peer: "192.168.1.42".into(),
        });
        snapshot.apply(&Message::Metadata {
            title: Some("Song".into()),
            artist: Some("Artist".into()),
            album: None,
        });
        snapshot.apply(&Message::Volume { db: -6.0 });
        snapshot.apply(&Message::Progress {
            elapsed_ms: 1_000,
            duration_ms: 2_000,
        });
        snapshot.apply(&Message::Artwork {
            content_type: "image/jpeg".into(),
            data_base64: "AAEC".into(),
        });

        assert_eq!(snapshot.session.as_ref().unwrap().rate, 44100);
        assert_eq!(
            snapshot.track.as_ref().unwrap().title.as_deref(),
            Some("Song")
        );
        assert_eq!(snapshot.track.as_ref().unwrap().album, None);
        assert_eq!(snapshot.volume_db, Some(-6.0));
        assert_eq!(snapshot.progress.unwrap().elapsed_ms, 1_000);
        assert!(snapshot.artwork.is_some());
    }

    #[test]
    fn metadata_replaces_wholesale_and_empty_artwork_clears() {
        let mut snapshot = Snapshot::default();
        snapshot.apply(&Message::Metadata {
            title: Some("First".into()),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
        });
        snapshot.apply(&Message::Metadata {
            title: Some("Second".into()),
            artist: None,
            album: None,
        });
        let track = snapshot.track.as_ref().unwrap();
        assert_eq!(track.title.as_deref(), Some("Second"));
        assert_eq!(track.artist, None, "a new statement replaces, not merges");

        snapshot.apply(&Message::Artwork {
            content_type: "image/jpeg".into(),
            data_base64: "AAEC".into(),
        });
        snapshot.apply(&Message::Artwork {
            content_type: "image/none".into(),
            data_base64: String::new(),
        });
        assert!(snapshot.artwork.is_none(), "empty data clears the art");
    }

    #[test]
    fn a_flush_stales_the_position_and_session_end_clears_the_rest() {
        let mut snapshot = Snapshot {
            receiver: ReceiverInfo {
                name: "Living Room".into(),
                version: "0.2.0".into(),
            },
            ..Snapshot::default()
        };
        snapshot.apply(&Message::Progress {
            elapsed_ms: 1,
            duration_ms: 2,
        });
        snapshot.apply(&Message::Flushed);
        assert_eq!(snapshot.progress, None);

        snapshot.apply(&Message::SessionStarted {
            rate: 44100,
            channels: 2,
            peer: "::1".into(),
        });
        snapshot.apply(&Message::SessionEnded);
        assert_eq!(snapshot.session, None);
        assert_eq!(
            snapshot.receiver.name, "Living Room",
            "the receiver's identity outlives the session"
        );
    }
}
