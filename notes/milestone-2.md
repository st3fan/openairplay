# Milestone 2 — Handshake & decryption

Goal (from `design.md`): handle ANNOUNCE / SETUP / RECORD, bind the three UDP
channels, receive the encrypted audio packets, decrypt them, and confirm the
plaintext is plausible ALAC. Still no ALAC decode and no ALSA output — that's
milestone 3. This proves the full crypto/transport path end to end.

## Scope

In:

- **Session state machine** (`session.rs`): per-connection state advancing
  ANNOUNCE → SETUP → RECORD, holding the AES key/IV, the ALAC `fmtp` config,
  and the bound UDP ports. TEARDOWN tears it down.
- **SDP parsing** (`sdp.rs`): pull `a=rtpmap`, `a=fmtp`, `a=rsaaeskey`,
  `a=aesiv`, `a=min-latency` out of the ANNOUNCE body.
- **RSA-OAEP key decrypt** (`crypto.rs`): decrypt `rsaaeskey` with the
  AirPort private key, OAEP/SHA-1 padding (matches shairport's
  `RSA_MODE_KEY`). Add `decrypt_aes_key()`.
- **ANNOUNCE handler**: parse SDP, decrypt the session key, store IV + fmtp.
  Reject with 456 if exactly one of aesiv/rsaaeskey is present, or if key/IV
  aren't 16 bytes. Unencrypted (both absent) is allowed.
- **SETUP handler**: parse `Transport:` for `control_port`/`timing_port`,
  bind three UDP sockets on ephemeral ports, reply with
  `Transport: RTP/AVP/UDP;unicast;mode=record;control_port=…;timing_port=…;
  server_port=…` and `Session: 1`.
- **RECORD handler**: reply 200 with `Audio-Latency: 11025`; parse the
  `RTP-Info` seq/rtptime for logging.
- **FLUSH / TEARDOWN / SET_PARAMETER / GET_PARAMETER**: minimal 200 replies
  (SET/GET_PARAMETER accept-and-log volume so senders don't error; real
  handling later).
- **UDP audio receiver** (`rtp.rs`): task per session reading the audio
  socket, parse the RTP-ish header, AES-CBC-decrypt the payload, sanity-check
  the plaintext, and log a periodic summary (packet count, seq range, decrypt
  OK). This is the milestone's observable proof.

Out: jitter buffer, retransmits, ALAC decode, ALSA, timing/sync, volume
application, metadata, password. Control/timing sockets are bound and drained
(so the client is happy) but their packets are only logged.

## Wire details (verified against shairport-sync)

### ANNOUNCE / SDP

- `a=fmtp:` → 12 ints `96 352 0 16 40 10 14 2 255 0 0 44100` =
  payload-type, frames/packet, …, sample-size(16), …, channels(2), …,
  sample-rate(44100). Stored for the milestone-3 decoder.
- `a=aesiv:` → base64, must decode to 16 bytes → CBC IV.
- `a=rsaaeskey:` → base64 → RSA-OAEP(SHA-1) decrypt with AirPort key → must
  be 16 bytes → AES-128 session key.
- Both aesiv+rsaaeskey absent ⇒ unencrypted stream. Exactly one present ⇒
  456.

### SETUP

Client `Transport: …;control_port=6001;timing_port=6002;…`. We bind audio,
control, timing UDP sockets (ephemeral) and answer with our three ports plus
`Session: 1`.

### Audio packet (payload type 0x60, or 0x56 resend)

12-byte RTP header. `packet[1] & ~0x80` = type. For 0x56 skip 4 extra bytes.
`seq = u16(be, off 2)`, `timestamp = u32(be, off 4)`, payload starts at
off 12. Decrypt: `aeslen = plen & ~0xf`; AES-128-CBC decrypt those bytes with
IV **reset to aesiv each packet**; append the trailing `plen - aeslen`
plaintext bytes unchanged.

## ALAC plaintext sanity check

RAOP ALAC frames are raw (no sync word). We can't fully validate without
decoding, but a cheap check catches a wrong key/IV: for 44.1/16/2 the frame
opens with a channel-pair element, so the top 3 bits of byte 0 are `001`
(element ID 1). We log the leading-bits distribution over the first N packets;
if key/IV were wrong the plaintext would look random and this check would
rarely hold. (Definitive confirmation is milestone 3, when decoded audio
either sounds right or doesn't.)

## Module additions

```
src/sdp.rs      — SDP attribute extraction (new)
src/session.rs  — per-connection session state (new)
src/rtp.rs      — UDP packet parse + AES-CBC audio decrypt (new)
src/crypto.rs   — + decrypt_aes_key() (RSA-OAEP)
src/server.rs   — dispatch ANNOUNCE/SETUP/RECORD/… into the session
Cargo.toml      — + aes, cbc
```

## Acceptance criteria

- `cargo test` + `cargo clippy` clean.
- Unit tests: SDP parser; RSA-OAEP key decrypt round-trip (encrypt with the
  public key → decrypt → original 16 bytes); AES-CBC audio decrypt with a
  known key/IV/ciphertext incl. the non-block-multiple tail; RTP audio header
  parse incl. the 0x56 resend offset; SETUP Transport parse.
- Integration test: drive ANNOUNCE (real SDP with an RSA-encrypted key) →
  SETUP → RECORD over TCP; assert 200s and that the SETUP response advertises
  three ports; then send a hand-built encrypted audio packet to the returned
  server_port and assert the receiver decrypts it to the known plaintext
  (expose the decrypt result via a test hook / channel).
- Manual: run against a real sender (owntone / an iPhone) if hardware is
  available and confirm packets arrive and decrypt; otherwise the synthetic
  integration test stands in (no Apple device on this dev box).

## Result

Done. 37 tests pass (31 unit + 3 milestone-1 integration + 3 milestone-2
integration), clippy clean. The `handshake.rs` integration test drives the
real path over TCP+UDP: it RSA-OAEP-wraps an AES key with the AirPort public
key exactly as a sender does, runs ANNOUNCE → SETUP → RECORD, then sends an
AES-CBC-encrypted audio packet (with a non-block-multiple cleartext tail) to
the negotiated `server_port` and asserts the receiver recovers the exact
plaintext, sequence, and timestamp. Negative cases covered: SETUP before
ANNOUNCE → 455, ANNOUNCE with only one of aesiv/rsaaeskey → 456.

Design choices worth noting:
- `serve_with_observer` exposes decrypted packets through an mpsc channel so
  tests can assert on the crypto result; production `serve` passes `None` and
  the branch compiles out to a no-op send.
- The audio/control/timing UDP tasks are `JoinHandle`s owned by the `Session`
  and aborted on TEARDOWN or when the `Session` drops (connection close), so
  a client reconnecting doesn't leak sockets or tasks.
- `looks_like_alac_stereo` is a cheap wrong-key tripwire (checks the ALAC
  channel-pair element tag), not full validation — that arrives with the
  real decoder in milestone 3.

Not verified on this box (deferred to hardware): decryption of packets from
an actual iPhone/Mac/owntone sender. The synthetic sender in the integration
test mirrors the exact wire format shairport-sync accepts.
