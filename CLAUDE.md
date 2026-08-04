# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

An AirPlay 1 (RAOP / AirTunes) audio **receiver** (Rust) — the AirPlay 1 counterpart of
[openairplay2](https://github.com/st3fan/openairplay2). Stock Apple senders (iPhone, iPad, Mac,
iTunes) discover it, handshake with it, and stream ALAC to it over RTP/UDP; audio comes out of an
ALSA device with seek/volume and a latency-correct start.

A cargo workspace with two members:

- **`openairplay1/`** — the embeddable library: network → PCM. Owns discovery advertisement, the
  RTSP handshake (`Apple-Challenge`), the three UDP channels, decrypt, the jitter buffer with
  retransmits, the NTP clock model, ALAC decode, and the prebuffer/latency-correct start. No ALSA
  dependency; builds and tests on macOS as well as Linux. Public surface: `Receiver` + builder,
  `AudioSink`, `Event`, `Config`, `txt_records` — everything else is private or `#[doc(hidden)]`
  (test-sender pieces: `server`, `crypto`, `clock`, `DecryptedAudio`/`AudioObserver`).
- **`openairplay1-receiver/`** — the standalone Linux-only binary: CLI + `AlsaSink` (ALSA output,
  frame-stuffing drift correction, dB→linear gain). It consumes only the library's public API
  (it is embedder #1).

Deliberate scope: **one sender → one stream → one output** (a second sender is refused `453`).
Also out of scope: `PAUSE` (answered `501`), metadata/DACP, password protection, AirPlay 2
(HomeKit pairing, PTP multi-room), video/screen mirroring.

## Build, test, run

The receiver's target platform is Linux: ALSA (`libasound2-dev`), a running `avahi-daemon` for
discovery, and `/sys/class/net` for MAC discovery. The **library** must additionally keep
building and passing its tests on macOS (`cargo test -p openairplay1`) — that portability is a
deliverable, not an accident. Tests need none of that hardware.

```bash
cargo build --release && cargo test && cargo clippy --all-targets && cargo fmt --check
```

```bash
cargo test -p openairplay1             # library only (the macOS-portable subset)
cargo test drift_action                # one unit test by name (receiver crate)
cargo test --test handshake            # one integration test file
```

```bash
RUST_LOG=debug ./target/release/openairplay1-receiver --name "Living Room" --alsa-device default
```

`RUST_LOG=debug` logs every RTSP request head and body — the way to see what a real sender
actually sends. `--no-audio` decodes without opening ALSA; `--no-avahi` skips advertising. Full
option list is in the [README](README.md).

## Architecture

One RTSP control connection (TCP) drives a per-connection session state machine,
`ANNOUNCE → SETUP → RECORD`, plus three UDP channels bound at SETUP:

- **audio** — RTP packets, payload AES-128-CBC encrypted with the session key (RSA-OAEP-wrapped
  in the SDP's `rsaaeskey`), each decrypting to one ALAC packet of 352 frames.
- **control** — `0xd4` sync packets (the frame at the DAC at a client-clock instant) and resend
  replies; we send resend requests for jitter-buffer gaps here.
- **timing** — NTP-style exchange: we send `0xd2` requests, fold `0xd3` replies into the clock
  model's offset.

Request flow: [rtsp.rs](openairplay1/src/rtsp.rs) parses →
[server.rs](openairplay1/src/server.rs) `dispatch` answers `OPTIONS` and any `Apple-Challenge`,
delegating the audio methods to [session.rs](openairplay1/src/session.rs). The audio path is
[session.rs](openairplay1/src/session.rs) `audio_receiver` (decrypt → jitter buffer → retransmit
requests) → [player.rs](openairplay1/src/player.rs) (dedicated thread: ALAC decode, prebuffer,
latency-correct start off the clock model, feeding the host's `AudioSink`).

**The sink seam.** The library ends at PCM: at SETUP it calls the host's sink factory
`(rate, channels) → Box<dyn AudioSink>` and thereafter delivers only audio that should actually
play — the prebuffer and the clock-driven start gate live in the library (they are protocol
semantics: the start instant comes from the NTP clock model), while the device, its pacing
(blocking `writei`), drift correction (which reads the ALSA queue depth), and gain live in the
sink. [openairplay1-receiver/src/player.rs](openairplay1-receiver/src/player.rs) is the binary's
sink: `AlsaSink` (open, blocking `writei`, `drop`+`prepare` reset, drift stuffing) plus
`SharedGain`/`volume_to_gain`. Session milestones (`SessionStarted`, `Volume` in dB, `Flushed`,
`SessionEnded`) reach the host over an unbounded event channel; the library does **not** apply
volume — the host does ([events.rs](openairplay1/src/events.rs)).

The embedding facade is [receiver.rs](openairplay1/src/receiver.rs): `Receiver::builder()`
(name/port/mac/advertise) → `build()` → `run(sink_factory, events)` on the caller's runtime.
`advertise(false)` + `Receiver::txt_records()` supports hosts that own their mDNS.

Key modules: [crypto.rs](openairplay1/src/crypto.rs) (AirPort RSA key, `Apple-Response`,
session-key decrypt), [sdp.rs](openairplay1/src/sdp.rs) (SDP/`fmtp` → `AlacConfig`),
[rtp.rs](openairplay1/src/rtp.rs) (packet parsing, AES-CBC decrypt, resend/timing wire formats),
[jitter.rs](openairplay1/src/jitter.rs) (sequence-ordered buffer with loss reporting),
[clock.rs](openairplay1/src/clock.rs) (NTP offset model, sync anchor, `play_time`),
[decode.rs](openairplay1/src/decode.rs) (ALAC via the `alac` crate),
[dmap.rs](openairplay1/src/dmap.rs) (DMAP walker for the track metadata a
sender pushes with `SET_PARAMETER`; the same walker openairplay2 uses),
[avahi.rs](openairplay1/src/avahi.rs) (`_raop._tcp` registration over the Avahi D-Bus API).

## Invariants worth knowing before editing

- **The library must stay free of audio-output dependencies** (no `alsa` anywhere in
  `cargo tree -p openairplay1`) and free of RAOP wire types in its documented public API.
- **The embedded `openairplay1/src/airport.pem` is the well-known AirPort Express RSA private
  key** (taken verbatim from shairport-sync's `common.c`). It answers the `Apple-Challenge` and
  decrypts the AES session key; it is not a secret and not original to this project. Read
  [notes/licensing.md](notes/licensing.md) before touching anything near it — its legal status
  is Apple's copyright/DMCA, not this repo's MIT license.
- **Apple senders omit base64 `=` padding** in `Apple-Challenge`, `aesiv`, and `rsaaeskey`; all
  decode paths must tolerate padded and unpadded forms (a real Mac hit this).
- **Every response carries `Server: AirTunes/105.1`, `Audio-Jack-Status`, the echoed `CSeq`, and
  an `Apple-Response` for any `Apple-Challenge`** — clients drop the connection if the challenge
  answer is missing or wrong.
- **The MAC in the service name must be the MAC used in the `Apple-Challenge` signature** —
  clients verify against it. Hence one shared `Config`.
- **Timing is NTP-based, not backpressure**: the first chunk is held until its frame's
  `play_time` off the clock model (falling back to the prebuffer if there is no sync yet), and
  drift is corrected afterwards in the sink by nudging the ALSA queue depth ±1 frame. Don't
  "simplify" this to play-on-arrival, and don't move the start gate host-side — the clock model
  is not knowable to a host.
- **One streaming session at a time** — the `SessionSlot` gate is acquired at SETUP and released
  at TEARDOWN; a second sender gets `453`.
- **`GET_PARAMETER` must get a 200** (even with an empty body) — some senders abort otherwise.
- **`SET_PARAMETER` is dispatched on `Content-Type`**: `text/parameters` (volume and
  `progress:`, the latter converted to `Duration`s with the stream's sample rate so RTP
  timestamps stay out of the public API),
  `application/x-dmap-tagged` (track metadata), `image/*` (cover art). Metadata is decoration —
  an unparseable payload is logged and dropped, never an error or a teardown. `Metadata` and
  `Artwork` reach the host only between `SessionStarted` and `SessionEnded`; anything that
  arrives earlier is latched and replayed right after `SessionStarted`. Senders only send any
  of it because the `md=0,1,2` TXT record advertises it — an empty capture starts there.

## Runbooks

Operational procedures live in `runbooks/`. When asked to do a **release**, follow
[runbooks/releasing.md](runbooks/releasing.md) — tag-driven crates.io publishing via the Release
workflow, with the failure procedure and the autopilot arrangement. CI
([.github/workflows/ci.yml](.github/workflows/ci.yml)) runs the workspace on Linux and the
library on macOS for every PR — the macOS portability deliverable is enforced there.

## Tests

Unit tests live inline (`#[cfg(test)] mod tests`) next to the code; integration tests in
`openairplay1/tests/` run the real RTSP server over real sockets and drive it with a synthetic
sender — [handshake.rs](openairplay1/tests/handshake.rs) completes ANNOUNCE → SETUP → RECORD
with a real RSA-wrapped session key and an encrypted audio packet,
[robustness.rs](openairplay1/tests/robustness.rs) covers jitter-buffer
reordering/retransmit and the single-session gate, [timing.rs](openairplay1/tests/timing.rs)
the NTP exchange and sync packets. They reach the server through `#[doc(hidden)]` modules
(`server`, `crypto`, `clock`) on ephemeral ports, which is what those exist for. The golden
ALAC fixture (`openairplay1/tests/data/golden_*`) is captured from a real sender and drives both
the decoder test and the playback-thread tests against a recording fake `AudioSink`
([player.rs](openairplay1/src/player.rs)). The suite never opens ALSA and needs no Avahi daemon.

Anything touching the wire protocol or timing behavior is also expected to be verified against a
real iPhone/Mac; that hardware check is part of each milestone's acceptance criteria.

## Working conventions

New features and changes start with a **plan** in `plans/YYYYMMDD-NN-slug.md` (`NN` is a
per-day sequence number). A plan holds the high-level implementation details for one change:
background, scope with explicit out-of-scope, module layout, test strategy, acceptance
criteria — the shape the `notes/milestone-*.md` files already use.

**A plan and its implementation live together in one stack** (managed with the `gh-stack`
extension). The plan document is the bottom PR of a fresh stack; the implementation follows in
one or more **phases**, each phase one PR stacked on top, one branch per phase, each based on
the one below it:

- Open the stack with the plan PR alone, and **wait for Stefan to approve the plan** (review
  feedback on the open PR — not a merge) before stacking implementation PRs onto it.
- The plan PR **stays open for the whole task** — that is the point of stacking it: if the work
  reveals mid-way that the plan needs adjusting, or decisions worth recording, commit them to
  the plan document on its still-open branch (then `gh stack rebase --upstack`), so the plan
  that eventually merges matches what was actually built.
- At the end Stefan reviews and merges the whole stack himself.

**All changes land through pull requests. Never commit directly to `main`** — always branch first.

**Never assume the status of a pull request.** Whether a PR is open, merged, closed, approved,
or green in CI is only knowable by asking: run `gh pr view <n>` / `gh pr status` /
`gh pr checks <n>` before acting on that status or reporting it.

The completed receiver was built as milestones 1–5, recorded under `notes/`:
[notes/design.md](notes/design.md) is the protocol design, `notes/milestone-*.md` the
per-milestone plans and verification records, [notes/licensing.md](notes/licensing.md)
provenance and attribution. Keep the README current as behavior changes.
