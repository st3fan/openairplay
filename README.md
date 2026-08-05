# OpenAirPlay

[![crates.io](https://img.shields.io/crates/v/openairplay1.svg)](https://crates.io/crates/openairplay1)
[![docs.rs](https://img.shields.io/docsrs/openairplay1)](https://docs.rs/openairplay1)
[![CI](https://github.com/st3fan/openairplay1/actions/workflows/ci.yml/badge.svg)](https://github.com/st3fan/openairplay1/actions/workflows/ci.yml)

A minimal **AirPlay 1 (RAOP / AirTunes) audio receiver**, written in Rust.
Stock Apple clients — iPhone, iPad, Mac, iTunes — discover it and stream
audio to it. This repo produces two artifacts:

- **`openairplay1`** — an embeddable receiver **library**: network in,
  decoded PCM + session events out. No ALSA; builds and tests on Linux and
  macOS. See [Embedding](#embedding).
- **`openairplay1-receiver`** — the standalone **Linux/ALSA binary** built
  on it: point Control Center (or the AirPlay menu) at it and press play.
- **`openairplay1-dashboard`** — a **full-screen now-playing display** that
  connects to a receiver over a WebSocket and draws the current track and its
  cover art in your terminal. See [Now-playing display](#now-playing-display).

It implements the full RAOP audio path: mDNS discovery, the RTSP handshake
with the `Apple-Challenge` response, AES/RSA decryption, ALAC decode, a
jitter buffer with packet retransmission, software volume, and NTP clock
synchronisation for a latency-correct start.

## Features

- **Discoverable** as an AirPlay speaker — advertises `_raop._tcp` by talking
  to the system `avahi-daemon` directly over D-Bus (no `avahi-utils` needed).
- **Authenticated handshake** — answers the `Apple-Challenge` with the
  well-known AirPort Express key, so real Apple senders accept it.
- **Encrypted or unencrypted** ALAC streams (RSA-OAEP session key,
  AES-128-CBC audio), decoded to 44.1 kHz / 16-bit / stereo PCM.
- **Robust playback** — a sequence-ordered jitter buffer reorders UDP
  packets, requests retransmits for gaps, and conceals unrecoverable losses
  with silence.
- **Clock sync** — uses the NTP timing channel and sync packets to start
  playback at the right moment and to counter drift between the sender's
  clock and the sound card.
- **Software volume** from `SET_PARAMETER`, and single-client exclusion (a
  second sender is refused `453`).
- **Optional pincode protection** — `--pincode CODE` requires a sender to
  authenticate before it can stream (`pw=1` advertised); without it the
  receiver is open (`pw=false`). The pincode never appears in the
  advertisement, logs, or any response.

Out of scope: AirPlay 2 (HomeKit pairing, PTP multi-room) and video / screen
mirroring.

## Requirements

The **receiver binary** targets Linux:

- Linux with **ALSA** and its development headers (Debian/Ubuntu:
  `libasound2-dev`) — needed to build.
- A working audio output device (or run with `--no-audio` to decode only).
- A running **`avahi-daemon`** with access to the system D-Bus — needed only
  for discovery. Without it the receiver still serves RTSP; you can connect
  by address for testing.

The **library** has no audio-output dependency and also builds and tests on
macOS (`cargo test -p openairplay1`). Both need a recent stable **Rust**
toolchain.

## Build & run

```sh
cargo build --release        # binary at target/release/openairplay1-receiver
```

Or install the released binary straight from crates.io:

```sh
cargo install openairplay1-receiver
```

The simplest case — advertise under a friendly name and play to the default
ALSA device:

```sh
./target/release/openairplay1-receiver --name "Living Room"
```

You should see it announce itself:

```
INFO openairplay1_receiver: starting AirPlay 1 receiver "Living Room" (mac AABBCCDDEEFF, rtsp port 5000)
INFO openairplay1::avahi: advertising "AABBCCDDEEFF@Living Room" on _raop._tcp port 5000 via avahi 0.8
```

Now open Control Center on an iPhone/iPad/Mac (or the speaker menu in
iTunes), pick **Living Room**, and start playing — audio comes out of your
ALSA device. Press `Ctrl-C` to stop; the receiver withdraws its
advertisement on exit.

### More examples

```sh
# Play to a specific ALSA device (see `aplay -L` for names)
./target/release/openairplay1-receiver --name "Kitchen" --alsa-device plughw:0,0

# Decode only, don't open any audio device (useful on a headless box)
./target/release/openairplay1-receiver --no-audio

# Run without advertising (connect by IP address for testing)
./target/release/openairplay1-receiver --no-avahi

# Verbose logging (RTSP requests, per-packet and player/timing detail)
RUST_LOG=debug ./target/release/openairplay1-receiver
```

### Options

| Flag | Default | Meaning |
|------|---------|---------|
| `--name NAME` | `OpenAirPlay` | Friendly name shown to the client |
| `--port PORT` | `5000` | RTSP TCP port |
| `--mac AA:BB:CC:DD:EE:FF` | auto-detected from `/sys/class/net` | MAC used in the service name and the `Apple-Challenge` signature |
| `--alsa-device DEV` | `default` | ALSA output device |
| `--no-audio` | — | Decode only; don't open an audio device |
| `--no-avahi` | — | Don't advertise the service |
| `--log-file PATH` | — | Write the log to a file instead of stderr |
| `--dashboard-listen ADDR` | — | Serve the now-playing WebSocket on `ADDR` (e.g. `127.0.0.1:7392`) |

Logging is controlled by `RUST_LOG` (`error`/`warn`/`info`/`debug`); it
defaults to `info`, which is startup and problems only — everything that
happens because music is playing (RTSP requests, packet counters, track
changes) is at `debug`, so a long-running receiver stays quiet.

## Now-playing display

`openairplay1-dashboard` is a separate program: start it, stop it, restart it,
or run it on another machine, without touching the receiver. It shows the
current track centered on screen with its cover art, a progress clock, the
sender's address, the stream format and the volume.

```sh
# receiver, publishing what it plays
./target/release/openairplay1-receiver --name "Living Room" \
    --dashboard-listen 127.0.0.1:7392

# display, in another terminal (or on another machine)
./target/release/openairplay1-dashboard --connect ws://127.0.0.1:7392
```

```
                     Sonata No. 1
                      Some Artist
                       Some Album

                ━━━━━━━──────────────
                    1:23 / 4:07

   Living Room · 192.168.1.42 · 44100 Hz 2ch · -12.5 dB
```

Cover art is drawn as a real image on terminals that support it — the Kitty
graphics protocol (**Ghostty**, Kitty, WezTerm) or iTerm2 inline images
(iTerm2, WezTerm, Konsole). The terminal is detected by asking it (and by
`TERM`/`TERM_PROGRAM` if it doesn't answer); anywhere else the display is
text-only. Press `q` (or `Ctrl-C`) to quit.

| Flag | Default | Meaning |
|------|---------|---------|
| `--connect URL` | `ws://127.0.0.1:7392` | Receiver's dashboard endpoint |
| `--images MODE` | `auto` | Terminal graphics: `auto`, `kitty`, `iterm2`, `none` |
| `--log-file PATH` | — | Write the log to a file (the display owns the screen, so logs are otherwise dropped) |

The dashboard reconnects on its own: it can be started before the receiver,
and it survives the receiver restarting under it, showing the connection state
in place of a stale screen.

### The endpoint

`--dashboard-listen` publishes what is playing as JSON text frames on a
WebSocket: a `snapshot` message on connect with everything the receiver
currently knows, then one message per change (`session_started`, `metadata`,
`artwork` as base64, `volume`, `progress`, `flushed`, `session_ended`). The
message types live in
[`openairplay1-dashboard-protocol`](openairplay1-dashboard-protocol/src/lib.rs),
so anything that speaks WebSocket — a browser, say — can consume it.

It is off unless the flag is given, and worth keeping on loopback (or behind a
reverse proxy): the stream carries now-playing metadata and cover art, and
there is no authentication.

## Embedding

The library's public API is small: build a `Receiver`, hand it a sink
factory and an event channel, run it on your tokio runtime.

```toml
[dependencies]
openairplay1 = "0.3"
```

```rust,no_run
use openairplay1::{AudioSink, Event, Receiver};

struct MySink; // your PCM → speaker path

impl AudioSink for MySink {
    fn write(&mut self, pcm: &[i16]) { /* blocking write paces playback */ }
    fn flush(&mut self) { /* seek: drop your device state */ }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let receiver = Receiver::builder().name("Office").build()?;
    let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let Event::Volume { db } = event { /* your gain path */ }
        }
    });
    receiver.run(|_rate, _channels| Box::new(MySink), events).await
}
```

The library keeps the session semantics (RTSP handshake, decrypt, jitter
buffer and retransmits, the NTP clock model, ALAC decode, the prebuffer and
latency-correct start, FLUSH handling); the host sees only PCM and events —
`SessionStarted` (rate, channels, and the sender's address), `Volume` (in
AirPlay dB), `Metadata` (track title, artist, album), `Artwork` (cover art
bytes as sent), `Progress` (position and length as `Duration`s), `Flushed`,
`SessionEnded`. `Metadata`, `Artwork` and `Progress` arrive only between
`SessionStarted` and `SessionEnded`; each `Metadata` is a complete statement
about the current track, not a delta, and empty `Artwork` data means the
sender cleared it. `Progress` follows the audio being played rather than wall
time — display it as-is, and note that a paused sender simply stops producing
it. A
host that owns its mDNS registration builds with `.advertise(false)` and
publishes `receiver.txt_records()` itself under
`receiver.config().service_name()`.

## How it works

Each RTSP connection drives a session state machine
(`ANNOUNCE → SETUP → RECORD`), which binds the three UDP channels (audio,
control, timing) and spawns the tasks that decrypt, reorder, decode, and
play the stream. The workspace has two crates:

**`openairplay1/`** — the library (network → PCM):

| Module | Responsibility |
|--------|----------------|
| `receiver` | The public facade: `Receiver::builder()` → `run(sink_factory, events)` |
| `crypto` | AirPort RSA key, `Apple-Response`, AES session-key decrypt |
| `rtsp` / `sdp` | RTSP message parsing, SDP / `fmtp` parsing |
| `session` | Per-connection state machine, UDP tasks, retransmit + timing exchange |
| `rtp` | UDP packet parsing, AES-CBC audio decrypt, resend/timing packets |
| `jitter` | Sequence-ordered jitter buffer with loss reporting |
| `clock` | NTP offset model, sync anchor, `play_time` |
| `decode` | ALAC decoding (via the `alac` crate) |
| `dmap` | DMAP/DAAP walker for the sender's track metadata |
| `player` | Playback thread: prebuffer, timed start, feeding the host's sink |
| `sink` / `events` | The host boundary: `AudioSink` trait, session `Event`s |
| `avahi` | `_raop._tcp` registration over the Avahi D-Bus API |

**`openairplay1-receiver/`** — the binary (PCM → speaker): the CLI plus the
ALSA sink (blocking `writei`, frame-stuffing drift correction, dB→linear
gain) and the dashboard WebSocket; it consumes only the library's public API.

**`openairplay1-dashboard/`** — the display: a WebSocket client with
reconnect, the ratatui screen, and the Kitty/iTerm2 artwork encoders. It knows
nothing about AirPlay — only the protocol crate.

**`openairplay1-dashboard-protocol/`** — the message types the receiver and
the dashboard share (serde only, no I/O).

See [`notes/design.md`](notes/design.md) for the protocol design, and
`notes/milestone-*.md` for how each part was built and verified.

## Testing

```sh
cargo test        # no audio hardware or Avahi daemon needed
```

The suite is hardware-independent: it never opens ALSA and stubs the
network, covering the crypto, RTSP/SDP parsing, jitter buffer, clock/NTP
math, volume, and the packet formats, the playback thread against a fake
sink, plus end-to-end integration tests that drive the RTSP handshake and
audio path over real sockets. Audio output, discovery, and clock sync were
additionally verified manually on real hardware with a synthetic sender.

## Notes and limitations

- The embedded `openairplay1/src/airport.pem` is the well-known AirPort
  Express RSA private key (as shipped by shairport-sync). It is what lets
  any third-party receiver answer the `Apple-Challenge`; it is not a
  secret.
- Drift correction is coarse frame-level "stuffing" (as shairport's is),
  not a sample-rate-converting resampler — it prevents drift-induced
  under/overruns rather than phase-locking to the sender's clock.
- Only one sender can stream at a time.
