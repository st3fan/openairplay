# Milestone 3 — Sound

Goal (from `design.md`): decode the ALAC frames and play them to a Linux
ALSA device. Naive playback — prebuffer a little, then stream in arrival
order; no jitter buffer, retransmits, or clock sync yet (those are
milestones 4–5). The observable outcome: music comes out of the speakers.

## Scope

In:

- **ALAC decode** (`decode.rs`): wrap the `alac` crate. Build a
  `StreamInfo` from the SDP `fmtp` (our `AlacConfig.raw[1..12]`, i.e. the 11
  fields without the payload-type prefix) via
  `StreamInfo::from_sdp_format_parameters`, and decode each decrypted packet
  to interleaved `i16` PCM.
- **ALSA output** (`player.rs`): a dedicated OS thread owning the `alsa` PCM
  handle (blocking writes pace playback in real time) and the ALAC decoder.
  It receives decrypted ALAC frames over a channel, decodes, and writes
  `S16_LE` / 2ch / 44100. Underruns (`EPIPE`) are recovered and playback
  continues.
- **Prebuffer**: hold the first ~`prebuffer` packets (default ~1/2 the
  Audio-Latency, configurable) before the first ALSA write so the device
  buffer has a cushion; then write each packet as it arrives.
- **Wiring**: the session's audio-receiver decrypts as today and also
  forwards the plaintext ALAC frame to the player. The player is created on
  the first RECORD (we know the stream params by then) and stopped on
  TEARDOWN / session drop. FLUSH resets the prebuffer.
- **Config/CLI**: `--alsa-device` (default `default`), `--no-audio` to skip
  opening ALSA (decode-only, useful for headless boxes / debugging).
- Graceful degradation: if ALSA can't be opened, log a warning and drop
  frames rather than killing the session, so the receiver still runs on a
  box with no working audio device (and so tests never require hardware).

Out: sequence-ordered jitter buffer + retransmit requests (milestone 4),
timing-channel clock sync and drift correction (milestone 5), volume
application from SET_PARAMETER, metadata.

## `alac` crate notes (verified against source)

- `StreamInfo::from_sdp_format_parameters(s)` wants the 11 space-separated
  fmtp fields **without** the leading payload type: `frame_length
  compatible_version bit_depth pb mb kb num_channels max_run max_frame_bytes
  avg_bit_rate sample_rate`. Our `AlacConfig.raw[0]` is the payload type
  (96), so we pass `raw[1..12]`.
- `Decoder::new(stream_info)` then
  `decode_packet::<i16>(&frame, &mut out) -> &[i16]`. The `out` buffer must
  be at least `max_samples_per_packet() = frame_length * num_channels`; the
  returned slice is interleaved L/R and its length is
  `frames_decoded * channels` (a partial last packet returns fewer).
- MIT/Apache-2.0 licensed, decode-only (no encoder), pure Rust.

## Test strategy

Automated tests must not require a sound card or ffmpeg, so ALSA is never
opened under `cargo test`.

- **Golden decode vector** (`tests/data/`, generated once with ffmpeg +
  a CAF extractor, committed): `golden_packet.bin` is a raw 3378-byte ALAC
  packet, `golden_pcm_i16le.bin` is the expected 8192 interleaved i16
  samples, `golden_fmtp.txt` is the matching fmtp
  (`4096 0 16 40 10 14 2 0 16388 1411200 44100`). A test builds the decoder
  from the fmtp, decodes the packet, and asserts the PCM matches byte-for-
  byte. (The fixture uses ffmpeg's 4096-frame packets; RAOP uses 352, but
  the decode path is frame-length-agnostic — driven entirely by the fmtp —
  so this exercises the real wiring.)
- **Prebuffer state machine**: unit-tested as a pure struct (no ALSA) — it
  buffers until the threshold then releases, and FLUSH re-arms it.
- **Decoder-from-fmtp**: building `StreamInfo` from our `AlacConfig.raw`
  yields 44100/2 and the right max-samples.

Manual (this box *does* have a working ALSA card, unlike milestones 1–2):
run the receiver with a real or synthetic sender and confirm audible,
correct playback; a `--no-audio` run confirms decode works headless.

## Module additions

```
src/decode.rs   — AlacDecoder wrapper over the `alac` crate (new)
src/player.rs   — ALSA output thread + prebuffer (new)
src/session.rs  — create/stop player on RECORD/TEARDOWN, feed frames
src/main.rs     — --alsa-device / --no-audio, thread into Config
Cargo.toml      — + alac, alsa
```

## Acceptance criteria

- `cargo test` + `cargo clippy` clean, no hardware needed.
- Golden decode test passes (PCM matches).
- Prebuffer unit tests pass.
- Manual: audible correct playback from a sender on real hardware;
  `--no-audio` decode-only run logs decoded frame counts without error.

## Result

Done. 44 tests pass (38 unit + 6 integration), clippy clean, and — unlike
milestones 1–2 — this was **verified on real hardware**, since the dev box
turned out to have a working ALSA card.

End-to-end hardware verification: a synthetic RAOP sender (scratchpad, path-
deps on this crate) does the full ANNOUNCE → SETUP → RECORD handshake and
streams the golden ALAC packet as encrypted RTP in real time. Pointing the
receiver's `--alsa-device` at an ALSA `file` plugin (null slave) captured the
played-out PCM to disk; it is **byte-for-byte identical** to the golden sine
across all 20 packets (peak amplitude 4095, i.e. real signal, not silence).
A second run against the real `default` device decoded and played 15 packets
with no ALSA errors.

Two bugs were found and fixed by that hardware test, which the synthetic
unit/integration tests had missed:

1. **Player shutdown deadlock**: `Player::drop` dropped its sender and set a
   stop flag, but the audio task still held a cloned `PlayerSender`, so the
   channel never closed and the thread's idle `recv()` blocked forever —
   `join()` hung the whole test suite. Fixed by sending an explicit
   `Command::Stop` to wake the recv, with the flag still there to skip a
   queued backlog on teardown.
2. **UDP receive buffer too small**: the audio socket read into a 2048-byte
   buffer, which silently truncated the 3378-byte golden packet so decode
   failed. Real RAOP 352-frame packets fit, but the announced frame length
   can be larger; bumped to 16 KiB.

Design notes:
- The player runs on a dedicated OS thread (blocking ALSA writes pace real
  time) fed over a channel; decoding happens on that thread so the async
  runtime never blocks on `libasound`.
- The golden fixture uses ffmpeg's 4096-frame packets (ffmpeg's ALAC encoder
  doesn't emit RAOP's 352); the decode path is frame-length-agnostic
  (driven entirely by the fmtp) so this still exercises the real wiring.
- Graceful degradation confirmed: a bad/absent device logs and drops audio
  instead of killing the session.
