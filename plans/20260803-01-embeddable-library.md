# Plan: turn openairplay1 into an embeddable library

- **Date:** 2026-08-03
- **Status:** proposed
- **Scope:** this repository only. The eventual embedding into
  [st3fan/radio](https://github.com/st3fan/radio)'s `radiod` motivates the
  design but is explicitly not part of this plan; that integration happens
  later, in the radio repository. This plan mirrors
  `plans/20260801-01-embeddable-library.md` in the sister project
  [openairplay2](https://github.com/st3fan/openairplay2) (implemented there
  in PRs #14–#17), adapted to the AirPlay 1 architecture.

## Background

openairplay1 is a working AirPlay 1 (RAOP / AirTunes) receiver, currently
shaped as one crate where the library target exists mainly so the integration
tests can reach the modules — `lib.rs` re-exports everything `pub`, and
`main.rs` is a thin CLI over it. There is no designed embedding API: the
protocol code, the ALSA playback path, and the CLI are all one unit, and the
crate only builds on Linux because the `alsa` dependency is unconditional.

We want to embed the AirPlay 1 receiver into another daemon (radiod), which
has its own audio output path, its own volume model, and its own idea of what
to do when a session starts. That requires a real library boundary.

At the same time, the standalone receiver stays a first-class artifact of
this repo: `openairplay1-receiver`, a Linux-only, ALSA-only binary that is a
useful AirPlay 1 receiver on its own. It is deliberately *not* generic over
audio backends — the library deals in PCM, the receiver deals in ALSA.

## Goals

- A library crate whose public API is a designed embedding surface:
  configuration in, decoded PCM + session events out. No RAOP wire concepts
  (RTSP, SDP, RTP, sequence numbers) leak to the host.
- The library builds and its tests pass on macOS and Linux (no `alsa`
  dependency in the library).
- A separate `openairplay1-receiver` binary crate in the same workspace:
  Linux-only, ALSA-only, functionally identical to today's binary. This is a
  main artifact, not an example.
- Behavior validated against real Apple senders is preserved at every phase:
  streaming, stop/start, seek (FLUSH), volume, the latency-correct start,
  and drift handling.

## Non-goals

- No radiod integration work (later, in the radio repo).
- No new protocol features: `PAUSE` stays unimplemented (answered `501`
  today), no metadata/DACP, no password protection, no AirPlay 2 — unchanged
  scope.
- No generic audio backend abstraction in the receiver binary; it is ALSA
  only.
- No crates.io publication (git dependency is sufficient for now; publishing
  can be its own step later).

## Design

### Where the seam is

The library owns **network to PCM**: discovery advertisement (optional), the
RTSP handshake (`OPTIONS`, `Apple-Challenge`/`Apple-Response`),
ANNOUNCE/SETUP/RECORD, the three UDP channels (audio, control, timing), AES
decrypt, the jitter buffer with retransmits, the NTP clock model, ALAC
decode, the prebuffer with the latency-correct start, FLUSH semantics, and
the single-session gate. The host owns **PCM to speaker**: output device,
pacing against the hardware, drift correction, and gain.

Concretely, today's `player.rs` splits in two:

- The **ALAC decode + prebuffer + latency-correct start** stay in the
  library, on the dedicated playback thread. This is a deliberate divergence
  from openairplay2, where the prebuffer cushion could move host-side
  because timing there is pure backpressure: here the start instant comes
  from the NTP clock model (`play_time` of the first frame), which is
  protocol semantics no host can know. The prebuffer release and the start
  wait are one mechanism and stay together.
- The **ALSA output, gain application, and drift correction** move to the
  host side, behind a sink trait. Drift correction reads the device queue
  depth (`snd_pcm_delay`) — pacing against the hardware is the host's job.
  The receiver binary's sink is today's `AlsaOutput` (open, blocking
  `writei`, `drop`+`prepare` reset) plus the frame-stuffing drift logic and
  `apply_gain`.

### The sink trait

```rust
/// Called from a dedicated library-managed thread; `write` may block —
/// blocking is the pacing mechanism.
pub trait AudioSink: Send + 'static {
    fn write(&mut self, pcm: &[i16]);
    /// Seek/stop: immediately drop anything the sink has queued or buffered.
    fn flush(&mut self);
}
```

The library calls `write` only with audio that should actually play: PCM is
held in the prebuffer until release and the start instant, and a FLUSH
resets the prebuffer/start gate and calls `AudioSink::flush` so the sink
drops its own device state (for ALSA: `snd_pcm_drop` + `prepare`).

The library does **not** apply gain. Volume arrives at the host as an event
(dB, as sent by the sender: 0 = full, −144 = mute), and the host applies it
in its own gain path. The receiver binary keeps the current dB→linear
mapping and `apply_gain` behavior; an embedding host (radiod) maps it onto
its own volume model.

### Events

```rust
pub enum Event {
    /// SETUP completed; a sink is about to be used.
    SessionStarted { rate: u32, channels: u8 },
    /// SET_PARAMETER volume, in AirPlay dB (0 = full, −144 = mute).
    Volume { db: f32 },
    /// FLUSH (seek/stop from the sender). Informational: the library already
    /// reset its jitter buffer/prebuffer and called `AudioSink::flush`.
    Flushed,
    /// TEARDOWN or connection closed.
    SessionEnded,
}
```

Delivered over a channel (`tokio::sync::mpsc`, unbounded) so the host
consumes them at its own pace. There is no `Paused` variant: RAOP `PAUSE` is
answered `501` today and implementing it is a new protocol feature — out of
scope here, candidate for a follow-up plan. The enum is `#[non_exhaustive]`
from day one.

### Facade

```rust
let receiver = Receiver::builder()
    .name("Living Room")
    .port(5000)             // defaults: "OpenAirPlay", 5000, discovered MAC
    .advertise(true)        // false: host owns mDNS; txt_records() exposed
    .build()?;

receiver.run(sink_factory, event_tx).await?;   // runs on the caller's runtime
```

- `sink_factory: impl Fn(u32, u8) -> Box<dyn AudioSink>` — invoked at SETUP
  with the rate/channels negotiated in the SDP, once per stream.
- **No identity**: AirPlay 1 uses the embedded AirPort Express RSA key
  (`airport.pem`) shared by every third-party receiver, so — unlike
  openairplay2 — the builder has no required input. Name, port, and MAC all
  have defaults (MAC discovered from `/sys/class/net` with a
  locally-administered fallback).
- The library never creates a tokio runtime; it runs inside the caller's.
  The receiver binary keeps `#[tokio::main]`.
- Advertisement is optional: an embedding host may own all of its Avahi
  registration. `txt_records()` stays public so a self-advertising host gets
  the `_raop._tcp` records right. The `avahi` module (zbus) stays in the
  library — zbus compiles on macOS, only `alsa` does not.

### Crate layout

Cargo workspace, two members:

```
Cargo.toml                    # workspace
openairplay1/                 # the library (no alsa dependency)
  src/{lib,receiver,server,session,sink,events,player,crypto,rtsp,sdp,
       rtp,jitter,clock,decode,avahi,mac}.rs + airport.pem
  tests/                      # integration tests (synthetic sender), data/
openairplay1-receiver/        # the binary (Linux-only: alsa)
  src/{main,player}.rs        # CLI, AlsaSink (ALSA + drift + gain), NullSink,
                              # SharedGain
```

- The library crate is renamed `openairplay` → `openairplay1` (all moves are
  `git mv`, so history follows); the binary is `openairplay1-receiver`.
- Library `tokio` features trimmed to what it needs (`rt`, `net`, `io-util`,
  `sync`, `time`); `rt-multi-thread` and `macros` become dev-dependencies
  for tests, `signal` moves to the receiver. `alsa` and `env_logger` are
  receiver-only.
- Protocol modules become private except the designed surface (`Receiver`,
  `ReceiverBuilder`, `AudioSink`, `SinkFactory`, `Event`, `EventSender`,
  `Config`, `txt_records`). The integration tests need sender-facing pieces
  (`server::serve`/`serve_with_observer`, `DecryptedAudio`/`AudioObserver`,
  `crypto`, `clock::ns_to_ntp`); these stay `pub` but `#[doc(hidden)]` —
  usable by tests and by a future test-sender, absent from the documented
  API.
- The CLI flags, defaults (name `OpenAirPlay`, port 5000, ALSA device
  `default`) are unchanged in the receiver.

### What deliberately stays inside the library

Session lifecycle (ANNOUNCE → SETUP → RECORD), the Apple-Challenge
signature, the three UDP channels, packet decrypt, the jitter buffer with
retransmit requests and loss concealment, the NTP timing exchange and clock
model, ALAC decode, the prebuffer/latency-correct start, FLUSH handling
(including the `RTP-Info` seq boundary), the SET_PARAMETER/GET_PARAMETER
bookkeeping, and the single-session gate. Hosts see PCM and `Event`s,
nothing else.

## Phases

Stacked PRs via `gh-stack`, per CLAUDE.md: this plan is the bottom PR of one
stack, the phases follow stacked on top. Each phase leaves the tree green
(`cargo test`, `clippy`, `fmt`) and the receiver verifiable against a real
sender.

### Phase 1 — cut the seam in place

Inside the existing single crate: introduce `AudioSink` and `Event`; split
`player.rs` into the library-side decode/prebuffer/start-gate half and an
`AlsaSink` (ALSA output + drift + gain) used by `main.rs`, plus a `NullSink`
for `--no-audio`; thread volume through as an event consumed by the binary
instead of a lib-applied gain; session emits the events. No workspace change
yet. Behavior identical — verified against a real sender (stream, stop/start,
seek/FLUSH, volume, latency feel).

### Phase 2 — workspace split

Create the workspace; rename the crate to `openairplay1`; move `main.rs` +
the ALSA sink into `openairplay1-receiver`; drop `alsa` from the library;
trim tokio features. CI-able check: the library builds and tests pass on
macOS *and* Linux; the receiver builds on Linux and behaves identically on
hardware.

### Phase 3 — the public facade

Add `Receiver`/builder/`run()`; make the binary consume only the public API
(it is embedder #1); tighten visibility (private modules + `#[doc(hidden)]`
test surface as above); make `advertise(false)` + `txt_records()` work for
host-owned mDNS; write rustdoc for the public surface (with a
compile-checked embedding example) and update README + CLAUDE.md for the
workspace layout. Final hardware validation.

## Test strategy

- Existing unit tests move with their code: prebuffer/decode tests stay in
  the library (including the golden ALAC fixture test); the volume/gain and
  drift tests move to the receiver crate.
- The integration tests (`tests/handshake.rs`, `tests/robustness.rs`,
  `tests/rtsp_server.rs`, `tests/timing.rs`) keep running against the
  library — after phase 3, through the `#[doc(hidden)]` test surface
  (`serve`/`serve_with_observer` on ephemeral ports), since the facade binds
  its own listener.
- New unit tests: the library-side playback thread delivers to a recording
  fake `AudioSink` fed with the golden ALAC fixture (decoded PCM arrives in
  order, FLUSH resets the prebuffer and calls `AudioSink::flush`, lost
  packets are concealed with silence), and the session emits the expected
  events (`SessionStarted` on SETUP, `Volume` on SET_PARAMETER,
  `SessionEnded` once on TEARDOWN/drop).
- macOS: `cargo test -p openairplay1` must pass (this is new — the crate
  currently cannot build there). Needs a run on a real Mac; not possible in
  the development environment.
- Hardware (per phase): a real iPhone/Mac streams, stop/start, seek (FLUSH),
  volume slider live, no audible regression in the latency-correct start.

## Acceptance criteria

- `cargo build --release && cargo test && cargo clippy --all-targets &&
  cargo fmt --check` green on the workspace (Linux) and for the library
  alone (macOS, pending a Mac run).
- `openairplay1-receiver` is drop-in equivalent to today's binary: same
  flags, same defaults, validated on hardware.
- The library's documented public API contains no ALSA and no RAOP wire
  types; a host can embed it with: builder → `run(sink_factory, events)`.
- README documents the two artifacts; CLAUDE.md reflects the workspace.

## Open questions (resolved during planning)

- **Drift correction placement** — host-side: it reads the ALSA queue depth
  (`delay()`), which is pacing-against-hardware. Its target depth (200 ms) is
  derived from the sink factory's `(rate, channels)`.
- **Prebuffer placement** — library-side, unlike openairplay2: it is tied to
  the clock-driven start instant (see "Where the seam is").
- **No identity** — the builder has no required input, unlike openairplay2's
  (AirPlay 1's AirPort key is shared, not per-receiver).
- **`PAUSE`** — stays `501` (no new protocol features); a pause/hold
  mechanism like openairplay2's can be its own plan later.
- **`Receiver::run` shutdown** — no shutdown token; cancellation semantics
  are documented on the method (dropping the future stops accepting and
  withdraws the advertisement), as in openairplay2. A token can be added
  later without breaking the builder.
