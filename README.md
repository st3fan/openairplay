# OpenAirPlay

A minimal **AirPlay 1 (RAOP / AirTunes) audio receiver** for Linux, written in
Rust. It lets stock Apple clients — iPhone, iPad, Mac, iTunes — discover it and
stream audio to it, which it plays on an ALSA device. Point Control Center (or
the AirPlay menu) at it and press play.

It implements the full RAOP audio path: mDNS discovery, the RTSP handshake with
the `Apple-Challenge` response, AES/RSA decryption, ALAC decode, a jitter buffer
with packet retransmission, software volume, and NTP clock synchronisation for a
latency-correct start.

## Features

- **Discoverable** as an AirPlay speaker — advertises `_raop._tcp` by talking to
  the system `avahi-daemon` directly over D-Bus (no `avahi-utils` needed).
- **Authenticated handshake** — answers the `Apple-Challenge` with the
  well-known AirPort Express key, so real Apple senders accept it.
- **Encrypted or unencrypted** ALAC streams (RSA-OAEP session key, AES-128-CBC
  audio), decoded to 44.1 kHz / 16-bit / stereo PCM.
- **Robust playback** — a sequence-ordered jitter buffer reorders UDP packets,
  requests retransmits for gaps, and conceals unrecoverable losses with silence.
- **Clock sync** — uses the NTP timing channel and sync packets to start
  playback at the right moment and to counter drift between the sender's clock
  and the sound card.
- **Software volume** from `SET_PARAMETER`, and single-client exclusion (a
  second sender is refused `453`).

Out of scope: AirPlay 2 (HomeKit pairing, PTP multi-room), video / screen
mirroring, and password protection.

## Requirements

- Linux with **ALSA** and its development headers (Debian/Ubuntu:
  `libasound2-dev`) — needed to build.
- A working audio output device (or run with `--no-audio` to decode only).
- A running **`avahi-daemon`** with access to the system D-Bus — needed only for
  discovery. Without it the receiver still serves RTSP; you can connect by
  address for testing.
- A recent stable **Rust** toolchain.

## Build

```sh
cargo build --release        # binary at target/release/openairplay
```

## Run

The simplest case — advertise under a friendly name and play to the default
ALSA device:

```sh
./target/release/openairplay --name "Living Room"
```

You should see it announce itself:

```
INFO openairplay: starting receiver "Living Room" (mac AABBCCDDEEFF, rtsp port 5000, audio default)
INFO openairplay::avahi: advertising "AABBCCDDEEFF@Living Room" on _raop._tcp port 5000 via avahi 0.8
```

Now open Control Center on an iPhone/iPad/Mac (or the speaker menu in iTunes),
pick **Living Room**, and start playing — audio comes out of your ALSA device.
Press `Ctrl-C` to stop; the receiver withdraws its advertisement on exit.

### More examples

```sh
# Play to a specific ALSA device (see `aplay -L` for names)
./target/release/openairplay --name "Kitchen" --alsa-device plughw:0,0

# Decode only, don't open any audio device (useful on a headless box)
./target/release/openairplay --no-audio

# Run without advertising (connect by IP address for testing)
./target/release/openairplay --no-avahi

# Verbose logging (RTSP requests, per-packet and player/timing detail)
RUST_LOG=debug ./target/release/openairplay
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

Logging is controlled by `RUST_LOG` (`error`/`warn`/`info`/`debug`); it defaults
to `info`.

## How it works

Each RTSP connection drives a session state machine
(`ANNOUNCE → SETUP → RECORD`), which binds the three UDP channels (audio,
control, timing) and spawns the tasks that decrypt, reorder, decode, and play
the stream. The source is organised as a library plus a thin binary:

| Module | Responsibility |
|--------|----------------|
| `crypto` | AirPort RSA key, `Apple-Response`, AES session-key decrypt |
| `rtsp` / `sdp` | RTSP message parsing, SDP / `fmtp` parsing |
| `session` | Per-connection state machine, UDP tasks, retransmit + timing exchange |
| `rtp` | UDP packet parsing, AES-CBC audio decrypt, resend/timing packets |
| `jitter` | Sequence-ordered jitter buffer with loss reporting |
| `clock` | NTP offset model, sync anchor, `play_time` |
| `decode` | ALAC decoding (via the `alac` crate) |
| `player` | ALSA output thread: prebuffer, timed start, volume, drift |
| `avahi` | `_raop._tcp` registration over the Avahi D-Bus API |

See [`notes/design.md`](notes/design.md) for the protocol design, and
`notes/milestone-*.md` for how each part was built and verified.

## Testing

```sh
cargo test        # no audio hardware or Avahi daemon needed
```

The suite is hardware-independent: it never opens ALSA and stubs the network,
covering the crypto, RTSP/SDP parsing, jitter buffer, clock/NTP math, volume,
and the packet formats, plus end-to-end integration tests that drive the RTSP
handshake and audio path over real sockets. Audio output, discovery, and clock
sync were additionally verified manually on real hardware with a synthetic
sender.

## Notes and limitations

- The embedded `src/airport.pem` is the well-known AirPort Express RSA private
  key (as shipped by shairport-sync). It is what lets any third-party receiver
  answer the `Apple-Challenge`; it is not a secret.
- Drift correction is coarse frame-level "stuffing" (as shairport's is), not a
  sample-rate-converting resampler — it prevents drift-induced under/overruns
  rather than phase-locking to the sender's clock.
- Only one sender can stream at a time.
