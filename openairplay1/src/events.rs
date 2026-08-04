//! Session events delivered to the host.
//!
//! Sent over an unbounded `tokio::sync::mpsc` channel so the host consumes
//! them at its own pace; a dropped receiver is tolerated (events are then
//! discarded). Wire concepts (RTSP, SDP, RTP, sequence numbers) never appear
//! here — playback position, for instance, arrives on the wire as RTP
//! timestamps and is reported here as [`Duration`].

use std::net::IpAddr;
use std::time::Duration;

/// What the host needs to know about the streaming session. Transport
/// handling (FLUSH) is already done inside the library — that variant is
/// informational.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// `SETUP` completed; a sink is about to be created and used.
    ///
    /// Deliberately *not* `#[non_exhaustive]`: hosts build this variant in
    /// their own tests (the receiver's TUI tests do), and a non-exhaustive
    /// variant cannot be constructed outside this crate. Matching with `..`
    /// is still the friendlier habit.
    SessionStarted {
        /// Sample rate in Hz (44100 for every stock sender).
        rate: u32,
        /// Channel count (2 for every stock sender).
        channels: u8,
        /// The address the sender connected from.
        peer: IpAddr,
    },
    /// `SET_PARAMETER volume`, in AirPlay dB (0 = full, −144 = mute). The
    /// library does not apply gain — the host maps this onto its own volume
    /// model.
    Volume {
        /// AirPlay volume in dB: 0 = full scale, −30 ≈ minimum, −144 = mute.
        db: f32,
    },
    /// `SET_PARAMETER` track metadata (DMAP). A complete statement about
    /// the current track, not a delta: fields the sender's payload did not
    /// carry are `None` and replace the previous value. Delivered only
    /// between [`Event::SessionStarted`] and [`Event::SessionEnded`].
    Metadata {
        /// Track title (DMAP `minm`).
        title: Option<String>,
        /// Track artist (DAAP `asar`).
        artist: Option<String>,
        /// Track album (DAAP `asal`).
        album: Option<String>,
    },
    /// `SET_PARAMETER` cover art, exactly as sent (typically `image/jpeg`
    /// or `image/png`, tens to hundreds of KB). Empty `data` — the
    /// `image/none` content type — means the sender cleared the artwork.
    /// Delivered only between [`Event::SessionStarted`] and
    /// [`Event::SessionEnded`].
    Artwork {
        /// The image media type as sent, e.g. `image/jpeg` or `image/png`
        /// (`image/none` accompanies a clear).
        content_type: String,
        /// The image bytes, exactly as sent; empty means cleared.
        data: Vec<u8>,
    },
    /// Where playback is in the current track, reported about once a second
    /// as the audio plays.
    ///
    /// This follows the audio, not the clock on the wall: the sender only
    /// says where a track starts and ends (one capture showed a 251-second
    /// track play to its end without a single further report), so the
    /// position comes from the RTP timestamp of the audio actually being
    /// played. A host can therefore display it as-is and should **not**
    /// extrapolate between events — when a sender pauses, the reports simply
    /// stop and the last one remains true. Delivered only between
    /// [`Event::SessionStarted`] and [`Event::SessionEnded`].
    Progress {
        /// How far into the track playback is.
        elapsed: Duration,
        /// The track's total length. Zero when the sender reports a stream
        /// with no known end (live radio, for instance).
        duration: Duration,
    },
    /// `FLUSH` (seek/stop from the sender). The library already reset its
    /// jitter buffer/prebuffer and called [`crate::sink::AudioSink::flush`].
    Flushed,
    /// `TEARDOWN`, or the control connection closed.
    SessionEnded,
}

/// The sending half handed to the library; the host keeps the receiver.
pub type EventSender = tokio::sync::mpsc::UnboundedSender<Event>;
