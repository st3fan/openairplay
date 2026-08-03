# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

An AirPlay 1 (RAOP / AirTunes) audio **receiver** (Rust) — the AirPlay 1 counterpart of
[openairplay2](https://github.com/st3fan/openairplay2). Stock Apple senders (iPhone, iPad, Mac,
iTunes) discover it, handshake with it, and stream ALAC to it over RTP/UDP; audio comes out of an
ALSA device with seek/volume and a latency-correct start.

Currently a single crate: `lib.rs` re-exports everything `pub`, and `main.rs` is a thin CLI over
it. [plans/20260803-01-embeddable-library.md](plans/20260803-01-embeddable-library.md) is the
active plan to split it into an embeddable library (`openairplay1`) plus a standalone Linux/ALSA
binary (`openairplay1-receiver`), mirroring the sister project's refactor — update this file as
that lands.

Deliberate scope: **one sender → one stream → one output** (a second sender is refused `453`).
Also out of scope: `PAUSE` (answered `501` today), metadata/DACP, password protection, AirPlay 2
(HomeKit pairing, PTP multi-room), video/screen mirroring.

## Build, test, run

The target platform is Linux: ALSA (`libasound2-dev`) to build, a running `avahi-daemon` for
discovery, and `/sys/class/net` for MAC discovery. Tests need none of that hardware.

```bash
cargo build --release && cargo test && cargo clippy --all-targets && cargo fmt --check
```

```bash
cargo test --test handshake            # one integration test file
cargo test drift_action                # one unit test by name
```

```bash
RUST_LOG=debug ./target/release/openairplay --name "Living Room" --alsa-device default
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

Request flow: [rtsp.rs](src/rtsp.rs) parses → [server.rs](src/server.rs) `dispatch` answers
`OPTIONS` and any `Apple-Challenge`, delegating the audio methods to
[session.rs](src/session.rs). The audio path is
[session.rs](src/session.rs) `audio_receiver` (decrypt → jitter buffer → retransmit requests) →
[player.rs](src/player.rs) (dedicated thread: ALAC decode, prebuffer, latency-correct start off
the clock model, ALSA `writei`, frame-stuffing drift correction).

Key modules: [crypto.rs](src/crypto.rs) (AirPort RSA key, `Apple-Response`, session-key
decrypt), [sdp.rs](src/sdp.rs) (SDP/`fmtp` → `AlacConfig`), [rtp.rs](src/rtp.rs) (packet
parsing, AES-CBC decrypt, resend/timing wire formats), [jitter.rs](src/jitter.rs)
(sequence-ordered buffer with loss reporting), [clock.rs](src/clock.rs) (NTP offset model, sync
anchor, `play_time`), [decode.rs](src/decode.rs) (ALAC via the `alac` crate),
[avahi.rs](src/avahi.rs) (`_raop._tcp` registration over the Avahi D-Bus API).

## Invariants worth knowing before editing

- **The embedded `src/airport.pem` is the well-known AirPort Express RSA private key** (taken
  verbatim from shairport-sync's `common.c`). It answers the `Apple-Challenge` and decrypts the
  AES session key; it is not a secret and not original to this project. Read
  [notes/licensing.md](notes/licensing.md) before touching anything near it — its legal status is
  Apple's copyright/DMCA, not this repo's MIT license.
- **Apple senders omit base64 `=` padding** in `Apple-Challenge`, `aesiv`, and `rsaaeskey`; all
  decode paths must tolerate padded and unpadded forms (a real Mac hit this).
- **Every response carries `Server: AirTunes/105.1`, `Audio-Jack-Status`, the echoed `CSeq`, and
  an `Apple-Response` for any `Apple-Challenge`** — clients drop the connection if the challenge
  answer is missing or wrong.
- **The MAC in the service name must be the MAC used in the `Apple-Challenge` signature** —
  clients verify against it. Hence one shared `Config`.
- **Timing is NTP-based, not backpressure**: the first chunk is held until its frame's
  `play_time` off the clock model (falling back to the prebuffer if there is no sync yet), and
  drift is corrected afterwards by nudging the ALSA queue depth ±1 frame. Don't "simplify" this
  to play-on-arrival.
- **One streaming session at a time** — the `SessionSlot` gate is acquired at SETUP and released
  at TEARDOWN; a second sender gets `453`.
- **`GET_PARAMETER` must get a 200** (even with an empty body) — some senders abort otherwise.

## Tests

Unit tests live inline (`#[cfg(test)] mod tests`) next to the code; integration tests in
`tests/` run the real RTSP server over real sockets and drive it with a synthetic sender —
[handshake.rs](tests/handshake.rs) completes ANNOUNCE → SETUP → RECORD with a real RSA-wrapped
session key and an encrypted audio packet, [robustness.rs](tests/robustness.rs) covers
jitter-buffer reordering/retransmit and the single-session gate, [timing.rs](tests/timing.rs)
the NTP exchange and sync packets. The golden ALAC fixture (`tests/data/golden_*`) is captured
from a real sender and also used by the decoder unit test. The suite never opens ALSA and needs
no Avahi daemon.

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
