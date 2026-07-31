# OpenAirPlay

A minimal AirPlay 1 (RAOP / AirTunes) audio receiver for Linux, written in
Rust. It aims to accept audio streams from stock Apple clients (iPhone, iPad,
Mac, iTunes) and play them on an ALSA device. mDNS/DNS-SD advertisement is
delegated to the system Avahi daemon.

See [`notes/design.md`](notes/design.md) for the protocol design and
[`notes/milestone-1.md`](notes/milestone-1.md) for the current milestone.

## Status

**Milestone 1 (skeleton) — complete.** RTSP server that logs requests,
answers `OPTIONS` including the `Apple-Challenge` → `Apple-Response` RSA
signature, and advertises `_raop._tcp` via Avahi.

**Milestone 2 (handshake & decryption) — complete.** Handles
ANNOUNCE / SETUP / RECORD: parses the SDP, RSA-OAEP-decrypts the AES session
key, binds the three UDP channels, and receives, AES-CBC-decrypts, and
sanity-checks the incoming audio packets.

**Milestone 3 (sound) — complete.** Decodes the ALAC frames and plays them to
an ALSA device. Use `--alsa-device` to pick the output (default `default`) or
`--no-audio` for decode-only.

**Milestone 4 (robustness) — complete.** A sequence-ordered jitter buffer
reorders UDP packets and requests retransmits for gaps (recovering lost
packets); unrecoverable gaps are concealed with silence to keep timing.
Software volume from `SET_PARAMETER`, FLUSH clears buffered audio, u16
sequence wrap is handled, and only one client may stream at a time (a second
is refused `453`).

**Milestone 5 (timing & sync) — complete.** The NTP timing channel and sync
packets drive a clock model that computes when each frame should reach the
DAC, giving a latency-correct playback start; ALSA queue depth is nudged to
counter clock drift. Verified on real hardware. This completes the roadmap in
[`notes/design.md`](notes/design.md): the receiver does the full RAOP audio
path — discovery, handshake, decryption, ALAC decode, robust buffered
playback, and clock sync.

## Build & test

Building links against ALSA, so the development headers are required
(Debian/Ubuntu: `libasound2-dev`).

```sh
cargo build
cargo test   # no audio hardware needed; tests never open ALSA
```

## Run

```sh
# Advertise as "Living Room" on the default RTSP port 5000.
./target/debug/openairplay --name "Living Room"

# Options
#   --name NAME        friendly name shown on the client (default OpenAirPlay)
#   --port PORT        RTSP TCP port (default 5000)
#   --mac AA:..:FF     MAC used in the service name and challenge signature
#                      (default: auto-detected from /sys/class/net)
#   --alsa-device DEV  ALSA output device (default "default")
#   --no-audio         decode only, don't open an audio device
#   --no-avahi         do not spawn avahi-publish-service

# Verbose logging:
RUST_LOG=debug ./target/debug/openairplay
```

Avahi advertisement requires `avahi-publish-service` (Debian/Ubuntu:
`avahi-utils`) and a running `avahi-daemon`. Without them the receiver still
serves RTSP; it just won't be discoverable, and you can connect by address
for testing.

## Notes

The embedded `src/airport.pem` is the well-known AirPort Express RSA private
key (as shipped by shairport-sync). It is what lets any third-party receiver
answer the `Apple-Challenge`; it is not a secret.
