//! Session events delivered to the host.
//!
//! Sent over an unbounded `tokio::sync::mpsc` channel so the host consumes
//! them at its own pace; a dropped receiver is tolerated (events are then
//! discarded). Wire concepts (RTSP, SDP, RTP, sequence numbers) never appear
//! here.

/// What the host needs to know about the streaming session. Transport
/// handling (FLUSH) is already done inside the library — that variant is
/// informational.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// `SETUP` completed; a sink is about to be created and used.
    SessionStarted {
        /// Sample rate in Hz (44100 for every stock sender).
        rate: u32,
        /// Channel count (2 for every stock sender).
        channels: u8,
    },
    /// `SET_PARAMETER volume`, in AirPlay dB (0 = full, −144 = mute). The
    /// library does not apply gain — the host maps this onto its own volume
    /// model.
    Volume {
        /// AirPlay volume in dB: 0 = full scale, −30 ≈ minimum, −144 = mute.
        db: f32,
    },
    /// `FLUSH` (seek/stop from the sender). The library already reset its
    /// jitter buffer/prebuffer and called [`crate::sink::AudioSink::flush`].
    Flushed,
    /// `TEARDOWN`, or the control connection closed.
    SessionEnded,
}

/// The sending half handed to the library; the host keeps the receiver.
pub type EventSender = tokio::sync::mpsc::UnboundedSender<Event>;
