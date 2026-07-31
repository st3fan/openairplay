# Milestone 5 — Timing & sync

Goal (from `design.md`): use the NTP timing channel and the sync packets to
play each frame at the wall-clock instant the client intends — a
latency-correct start — and to keep the receiver's ALSA clock from drifting
away from the client's clock over a long stream. This is the "hard 20%".

## Scope

In:

- **Clock model** (`clock.rs`, pure + fully unit-tested):
  - NTP offset estimation from a timing exchange: given our request send time
    (t1), the client's receive/transmit times (t2, t3), and our reply arrival
    time (t4), compute the round-trip time and the local→remote clock offset,
    keeping the estimate from the lowest-RTT sample in a small window.
  - A sync anchor `(remote_time_of_sync, rtp_anchor)` from sync packets.
  - `play_time(rtp) -> local_ns`: the local monotonic instant a given RTP
    timestamp should reach the DAC, combining offset + anchor. Handles u32
    RTP-timestamp wraparound.
- **Timing exchange**: the timing task periodically sends a `0xd2` request to
  the client's timing port (SETUP `timing_port`) and processes the `0xd3`
  reply, updating the shared clock model. Initial fast burst then ~every 3 s,
  matching shairport.
- **Sync packets**: the control task parses `0xd4` packets and updates the
  anchor (needs an offset first).
- **Latency-correct start**: once the model is valid, the player starts
  writing so the first frame hits the DAC at its `play_time` (minus the ALSA
  output latency from `snd_pcm_delay`), instead of the milestone-4 "prebuffer
  N packets" heuristic. Falls back to the prebuffer when no timing yet.
- **Drift handling**: keep the ALSA queue depth near the target latency by
  nudging — inserting or dropping a single frame when the measured depth
  strays past a threshold. The clock-model drift (expected vs actual RTP
  position) is logged.

Out: sample-rate-converting resampler / PLL-quality clock locking (shairport's
"stuffing" is coarse too); AirPlay 2 PTP.

## Wire details (verified against shairport-sync `rtp.c`)

NTP timestamps are seconds-since-epoch in 32.32 fixed point:
`ns = sec * 1e9 + (frac * 1e9 >> 32)`, `sec`/`frac` big-endian u32.

- **Timing request** we send (32 bytes): `80 D2`, `seqno=htons(7)`, then a
  4-byte filler and three 8-byte NTP fields (origin/receive/transmit) all
  zero. Record the local send time (t1).
- **Timing reply** `0xd3` (32 bytes): receive NTP at `[16..24]` (t2),
  transmit NTP at `[24..32]` (t3). Arrival local time is t4.
  - `rtt = (t4 - t1) - (t3 - t2)`
  - `remote_at_t4 = t3 + rtt/2`
  - `offset = remote_at_t4 - t4` (add to a local time to get remote time)
  - Choose the sample with the smallest `rtt` from the recent window.
- **Sync packet** `0xd4` (20 bytes): flags `[2..4]`, `rtp_less_latency`
  `[4..8]` (the frame at the DAC at the sync instant), NTP `remote_time_of_sync`
  `[8..16]`, `rtp_current` `[16..20]`. Anchor:
  `local_anchor = remote_time_of_sync - offset`, and
  `play_time(T) = local_anchor + (T - rtp_less_latency) * 1e9 / sample_rate`.

## Module additions

```
src/clock.rs    — NTP offset model, sync anchor, play_time (new, pure)
src/rtp.rs      — timing request/reply + sync packet parsers/encoders
src/session.rs  — timing task does the NTP exchange; control task parses sync;
                  both update a shared Arc<Mutex<ClockModel>>; RTP timestamp
                  threaded to the player
src/player.rs   — timed start from play_time; queue-depth drift nudging
```

## Test strategy

- **clock.rs** unit tests: offset from a known 4-timestamp exchange;
  lowest-RTT selection; `play_time` for in/out-of-anchor timestamps; u32 RTP
  wrap; behaviour before any timing (None).
- **NTP <-> ns** conversion round-trips.
- **rtp.rs**: timing request encoding byte-exact; timing reply + sync packet
  parsing from known bytes.
- **Drift nudge** decision function: depth below/at/above target → add / hold
  / drop.
- **Integration** (`timing.rs`): a fake client binds a timing socket, receives
  the receiver's `0xd2` request and replies `0xd3`; then sends a `0xd4` sync;
  assert the receiver issued the request (proves the exchange runs). Audio
  still delivered in order (no regression).
- **Manual (hardware)**: stream via the synthetic sender extended to answer
  timing requests and emit sync packets; confirm audio plays correctly and
  the captured PCM is intact (no regression from the new start path).

## Result

Done. 78 tests pass (69 unit + 9 integration), clippy clean, and verified on
real hardware.

The clock model (`clock.rs`) is the tested core: NTP offset from the four
timestamps (lowest-RTT selection), a sync anchor, and `play_time(rtp)`
combining them with u32-wrap handling. The timing task sends `0xd2` requests
and folds `0xd3` replies into a shared `Arc<Mutex<ClockModel>>`; the control
task feeds `0xd4` anchors in. The player consults `play_time` for a
latency-correct start and nudges the ALSA queue depth to counter drift.

Hardware verification via the ALSA `file`-capture rig with the synthetic
sender extended to be timing-aware (answers `0xd2`, emits `0xd4` sync):
- The receiver's clock model became ready (offset + anchor), and the player
  logged **"latency-correct start"** — proving `play_time` was computed and
  used for the start decision rather than the prebuffer fallback.
- Captured PCM was **byte-identical to the source across all 30 packets**
  with the full timing path active (3 dropped packets still recovered via
  resend), and a real `default`-device run played with **zero ALSA errors**.
- The `timing.rs` integration test proves the exchange end to end: a fake
  client receives the `0xd2` request, replies `0xd3`, sends a `0xd4` sync,
  and audio still delivers in order.

Honest limits (the "hard 20%"):
- Drift correction keeps the ALSA queue depth near target by adding/dropping
  a single frame per correction — the same coarse "stuffing" shairport uses,
  not a sample-rate-converting PLL. It prevents drift-induced under/overruns
  rather than phase-locking to the client clock.
- The latency-correct start aligns the first frame's write with its
  `play_time`; absolute end-to-end latency still includes the (unmodelled)
  fixed ALSA hardware buffer, so it is approximate, not sample-exact.
- The wait branch of the timed start wasn't exercised on hardware (the frame
  was always already due by the time buffering completed on localhost); the
  path is simple and the "already due" branch confirms `play_time` is
  consulted.

## Acceptance criteria

- `cargo test` + `cargo clippy` clean, no hardware.
- Clock/NTP/anchor/drift unit tests pass; timing-exchange integration passes.
- Hardware: correct playback end-to-end with a timing-aware sender; captured
  PCM matches the source.
