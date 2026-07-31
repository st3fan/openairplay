# Milestone 4 — Robustness

Goal (from `design.md`): make playback robust on a real network — a
sequence-ordered jitter buffer with retransmit requests, software volume,
u16 sequence-wrap handling, FLUSH that clears buffered audio, and sane
handling of a second client. Clock/timing sync stays in milestone 5; this
milestone still paces on arrival, it just reorders, fills gaps, and recovers
lost packets first.

## Scope

In:

- **Jitter buffer** (`jitter.rs`): a fixed-capacity, sequence-indexed ring
  that accepts packets in any order and releases them **in order** to the
  player. Handles u16 wraparound via signed-16-bit sequence diffs. When it
  has to wait on a missing packet but newer packets keep arriving, it
  requests a resend; if the gap can't be filled before the buffer runs too
  far ahead (or a deadline passes), it skips the hole, emitting a
  *lost-packet* marker so the player can insert silence and keep A/V timing.
- **Retransmit requests**: on a detected gap, send the RAOP resend request
  (8 bytes: `80 55|80`, our seq=1, first-missing-seq, count) from our
  control socket to the client's control port (client IP + the SETUP
  `control_port`). Per-sequence backoff so we don't flood. Resend responses
  arrive as `0x56` packets on the audio socket (already parsed since
  milestone 2) and are fed back into the buffer.
- **Lost-packet concealment**: on a skipped hole the player writes one
  packet of silence (`frames_per_packet * channels` zeroed samples) so the
  stream stays time-aligned instead of jumping.
- **Volume** (`SET_PARAMETER volume: <dB>`): AirPlay sends a dB attenuation
  in [-30, 0], with -144 = mute. Convert to a linear gain and apply it to
  the PCM in the player before the ALSA write. Sent to the player over the
  command channel; applied per sample.
- **FLUSH**: clear the jitter buffer (drop buffered audio up to the
  `RTP-Info` seq) and re-arm the player prebuffer, so a seek/pause doesn't
  play stale audio.
- **Second-client handling**: only one streaming session at a time. An RAII
  guard acquired at SETUP; a second client that reaches SETUP while one is
  active is refused `453 Not Enough Bandwidth`. Released on TEARDOWN or when
  the connection drops.

Out: NTP timing channel, clock-drift correction, latency-accurate start
(milestone 5); metadata/artwork; password auth.

## Wire details (verified against shairport-sync `rtp.c`)

- **Resend request** → client control port, 8 bytes:
  `[0]=0x80`, `[1]=0x55|0x80=0xD5`, `[2..4]=htons(1)` (our seq, always 1),
  `[4..6]=htons(first_missing_seq)`, `[6..8]=htons(count)`.
  Destination = RTSP peer IP + `control_port` from the SETUP Transport.
- **Resend response** = a normal audio packet behind payload type `0x56`
  with a 4-byte prefix; `AudioPacket::parse` already strips it and yields the
  embedded seq/timestamp/payload. Most senders send these to the audio
  (server) port, which is where we already listen.
- Sequence numbers are u16 and wrap; ordering uses
  `(a as i16).wrapping_sub(b as i16)` semantics via `seq_diff`.

## Jitter buffer behaviour

- `next_seq` = the next sequence to deliver; set from the first packet
  received after RECORD/FLUSH.
- `insert(seq, frame)`: ignore packets older than `next_seq` (duplicates /
  too-late); otherwise store in `slots[seq % CAP]`.
- `pop_ready()`: yield `Delivery::Packet` for each consecutive present slot
  from `next_seq`, advancing it; stop at the first gap.
- Forced skip: if the highest received seq runs `CAP` (or a configured
  `max_lead`) ahead of `next_seq`, or the gap has been outstanding past a
  deadline, deliver `Delivery::Lost` for `next_seq` and advance — bounding
  latency and memory.
- `missing()`: the seqs in `(next_seq ..= highest)` whose slots are empty,
  for the retransmit requester (which rate-limits per seq).

## Module additions

```
src/jitter.rs   — sequence-ordered jitter buffer + missing-seq reporting (new)
src/session.rs  — drive the buffer in the audio task, send resend requests,
                  store client control addr + volume, single-session guard
src/player.rs   — Command::Volume + Command::Silence; apply gain; write silence
src/server.rs   — shared single-session guard across connections
```

## Test strategy (no hardware needed)

- **Jitter buffer** unit tests: in-order passthrough; reordered delivery;
  duplicate/late drop; forced skip emits Lost and advances; u16 wrap across
  0xFFFF→0x0000; `missing()` reports the right gaps.
- **seq_diff** wrap arithmetic.
- **Volume**: dB→gain mapping (0 dB → 1.0, -144 → 0.0) and sample scaling
  (i16 with rounding/clamping).
- **Resend request encoding**: byte-exact against the shairport layout.
- **Integration** (extend `handshake.rs`): send audio packets out of order
  and with a hole to the server; assert the observer sees them delivered
  in order and that a resend request datagram arrives on the client control
  port for the missing seq. Second-SETUP-gets-453 over two TCP connections.

## Acceptance criteria

- `cargo test` + `cargo clippy` clean, no hardware.
- Jitter/volume/seq_diff/resend unit tests pass.
- Integration: reorder + hole delivered in order with a resend observed;
  second client refused 453.
- Manual (hardware): stream with the synthetic sender injecting reordering
  and drops; audio stays correct; captured PCM aligns.

## Result

Done. 64 tests pass (55 unit + 9 integration), clippy clean, and the new
paths were verified on real hardware via the ALSA `file`-capture rig:

- **Retransmit recovery**: the synthetic sender streamed 20 packets while
  deliberately dropping seqs 3, 7, and 11 on first transmission and honoring
  resend requests on its control socket. The receiver requested all three,
  the sender resent them, and the captured PCM was **byte-for-byte complete
  and in order with zero silence gaps** — the jitter buffer held the stream
  for the missing packets and slotted the resends into place.
- **Software volume**: `SET_PARAMETER volume: -6.0206` (≈0.5 gain) produced a
  captured peak amplitude of exactly 2048, half the golden peak of 4095.

Unit/integration coverage for the rest: reordering + resend request with
distinct payloads (`robustness.rs`), FLUSH discards buffered audio and
re-anchors, second client refused `453`, forced-skip → `Lost` → silence
concealment, u16 sequence wrap, dB→gain and sample scaling, and byte-exact
resend-request encoding.

Design notes:
- The jitter buffer is a pure, fully-unit-tested `CAPACITY`-slot ring; all
  the async concerns (retransmit backoff, flush signalling, socket sharing)
  live in the audio task around it.
- The control socket is shared (`Arc<UdpSocket>`): the audio task sends
  resend requests on it while the control task reads sync packets.
- Delivered-packet timestamps are derived from the first packet's
  (seq, ts) anchor (RAOP advances ts by `frames_per_packet` per seq) —
  groundwork the milestone-5 clock sync will build on.
- Single-session exclusion is an RAII `SlotGuard`: acquired at SETUP,
  released on TEARDOWN or whenever the connection's `Session` drops.

Not hardware-verified (covered by tests instead): lost-packet silence
concealment only triggers when a gap can't be filled before the buffer runs
`max_lead` ahead, which a short synthetic stream never reaches; the
`forced_skip` unit test exercises it directly.
