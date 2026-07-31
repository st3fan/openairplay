# OpenAirPlay — AirPlay 1 Receiver in Rust

## Goal

A minimal AirPlay 1 (AirTunes / RAOP) *audio receiver* in Rust. It accepts
streams from stock clients (iPhone, iPad, Mac, iTunes) and plays them on a
Linux ALSA device. Non-goals for now: video/photos/screen mirroring, AirPlay 2
(HomeKit pairing, PTP multi-room), DACP remote control, password protection.

mDNS/DNS-SD is **not** implemented in-process — we register the service with
the system Avahi daemon.

## Protocol overview

AirPlay 1 audio is RAOP (Remote Audio Output Protocol): an RTSP-like control
connection over TCP plus three UDP channels. Historical summary: the protocol
was reverse-engineered from the AirPort Express; Jon Lech Johansen extracted
the RSA *public* key from iTunes (enabling third-party senders), and James
Laird extracted the RSA *private* key from an AirPort Express (enabling
third-party receivers — his `shairport` is the ancestor of `shairport-sync`).
That well-known private key is what lets us act as a receiver: we use it to

1. answer the client's `Apple-Challenge` (proves we are a "real" AirPort), and
2. decrypt the AES session key the client sends in the SDP.

The audio itself is ALAC (Apple Lossless), 44.1 kHz / 16-bit / stereo,
352 samples per frame, carried in RTP packets whose payload is AES-128-CBC
encrypted with the session key.

### Session flow

```
Client                              Receiver (us)
  |--- mDNS browse _raop._tcp --------->|   (Avahi advertises us)
  |--- TCP connect (RTSP port) -------->|
  |--- OPTIONS (Apple-Challenge) ------>|  respond + Apple-Response (RSA sign)
  |--- ANNOUNCE (SDP: codec, keys) ---->|  decrypt AES key with RSA priv key
  |--- SETUP (Transport: ports) ------->|  open 3 UDP sockets, reply with ports
  |--- RECORD (RTP-Info: seq, rtptime)->|  reply with Audio-Latency
  |=== UDP audio / sync / timing ======>|  decrypt, decode, buffer, play
  |--- SET_PARAMETER (volume/metadata)->|
  |--- FLUSH (pause/seek) ------------->|  drop buffered audio
  |--- TEARDOWN ----------------------->|  end session
```

## RTSP control channel

RTSP/1.0-style requests over TCP (one client session at a time; a second
client is either rejected `453 Not Enough Bandwidth` or preempts — pick one,
shairport-sync makes it configurable). Standard port: pick something like
5000–5010 and advertise it via Avahi. Methods we must handle:

- **OPTIONS** — reply with the supported method list. Any request may carry
  an `Apple-Challenge: <base64>` header (16 random bytes). Response must
  include `Apple-Response`: concatenate challenge bytes + our IPv4 address
  (the one the RTSP connection arrived on) + our MAC address (6 bytes, the
  one used in the mDNS name), zero-pad to 32 bytes, RSA-sign with PKCS#1
  v1.5 padding using the AirPort private key, base64-encode (unpadded).
- **ANNOUNCE** — body is SDP (`application/sdp`). Relevant attributes:
  - `a=rtpmap:96 AppleLossless`
  - `a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100` — the ALAC config
    (frame length 352, sample size 16, 2 channels, 44100 Hz, plus ALAC
    tuning params). These twelve values map directly onto the ALAC "magic
    cookie" needed to initialize the decoder.
  - `a=rsaaeskey:<base64>` — 128-bit AES key, RSA-OAEP encrypted with the
    AirPort public key. Decrypt with the private key.
  - `a=aesiv:<base64>` — 16-byte AES-CBC IV.
  - If `rsaaeskey`/`aesiv` are absent the stream is unencrypted (we
    advertise `et=0,1` so both are legal).
- **SETUP** — `Transport:` header carries the client's `control_port` and
  `timing_port`. We bind three UDP sockets and reply with
  `Transport: ...;server_port=X;control_port=Y;timing_port=Z` and a
  `Session:` header (any value, e.g. `1`).
- **RECORD** — streaming starts. `RTP-Info: seq=<n>;rtptime=<t>` gives the
  first sequence number/timestamp. Reply with `Audio-Latency: <samples>`
  (e.g. 11025 minimum; ~2 s = 88200 is a comfortable default).
- **SET_PARAMETER** — `text/parameters` body `volume: <float>` (dB
  attenuation, −30..0 typical, −144 = mute); also DAAP metadata
  (`application/x-dmap-tagged`), JPEG artwork, and `progress:` — all
  optional, safe to accept and ignore initially.
- **FLUSH** — pause/seek: discard the jitter buffer up to the seq/rtptime in
  `RTP-Info`, keep the session.
- **TEARDOWN** — close UDP sockets, drop the session.
- **GET_PARAMETER** — often just a keep-alive (`volume` query); reply 200.

Every response echoes `CSeq` and should include `Server: AirTunes/105.1` (or
similar) and `Audio-Jack-Status: connected; type=analog`.

## UDP channels

All three sockets are ours (bound in SETUP); the client's control/timing
ports come from the SETUP Transport header. Packets are RTP-ish, big-endian.

### Audio data (payload type 0x60)

Standard 12-byte RTP header: `[0]=0x80`, `[1]=0x60` (marker bit set on the
first packet after RECORD/FLUSH → `0xe0`), `[2..4]` u16 seq, `[4..8]` u32 RTP
timestamp (in samples), `[8..12]` SSRC. Payload: the encrypted ALAC frame.

Decryption: AES-128-CBC with the session key, IV **reset to `aesiv` for every
packet**; only the whole 16-byte blocks are encrypted — the trailing
`len % 16` bytes are plaintext and appended as-is.

### Control channel

- **Sync packets** (client → us, 1/s): `[1]=0xd4` (0x54 | marker; first one
  after RECORD has the extension bit: `[0]=0x90`). 20 bytes: RTP timestamp
  that should be *playing now* (already minus latency), 8-byte NTP wall
  clock time of "now" on the client's clock, and the RTP timestamp of the
  next outgoing sample. This is the anchor that maps RTP time → client NTP
  time → (via the timing channel) our local clock.
- **Retransmit request** (us → client control port): `[0]=0x80, [1]=0xd5`,
  u16 request seq, then u16 first-missed-seq, u16 count.
- **Retransmit response** (client → us): `[1]=0xd6` (0x56 | marker), 4-byte
  header, then the complete original audio packet (RTP header + encrypted
  payload).

### Timing channel (NTP-style, us → client every ~3 s)

Request `[1]=0xd2` (0x52), response `[1]=0xd3` (0x53); 32 bytes: 8-byte
header + three 8-byte NTP timestamps (origin = our send time echoed back,
receive = client receive time, transmit = client send time). Classic NTP
offset/delay math gives us the client-clock ↔ local-clock mapping used with
sync packets to schedule playback. NTP timestamps are seconds since 1900 in
32.32 fixed point.

## Receiver pipeline

```
UDP audio ──> decrypt (AES-CBC) ──> jitter buffer (keyed by seq, u16 wrap)
                                        │        ▲
              retransmit requests <─────┘        │ FLUSH clears
                                                 ▼
                     ALAC decode ──> f32/i16 PCM ──> volume ──> ALSA
```

- **Jitter buffer**: ring buffer indexed by sequence number sized for the
  latency (~2 s ≈ 250 frames of 352 samples). On gap detection, send a
  retransmit request; on timeout, insert silence (or interpolate later).
  Handle u16 sequence wraparound with signed 16-bit diff.
- **Clock/sync**: start playback when the buffer reaches the target latency;
  use sync packets + timing offset to decide *when* the anchor RTP timestamp
  should hit the DAC. Long-term drift between the client clock and the ALSA
  clock is corrected by occasionally inserting/dropping a frame
  (shairport-sync calls this "stuffing") — v1 can simply ignore drift and
  resync on underrun/overrun.
- **Volume**: apply in software (dB → linear scale on samples) rather than
  touching the ALSA mixer; −144 dB means mute.
- **ALSA**: `hw`/`default` device, S16_LE, 2ch, 44100 Hz, buffer ~0.5 s.
  Use `snd_pcm_delay` to know actual output latency when scheduling start.

## Avahi registration

Register `_raop._tcp` with service name `<MAC>@<FriendlyName>` (MAC as 12 hex
digits, e.g. `AABBCCDDEEFF@Living Room`). Two options:

1. Shell out to `avahi-publish-service` (dead simple, fine for v1).
2. Talk to the Avahi daemon over D-Bus with the `zbus` crate (proper
   lifecycle, no subprocess).

TXT records (shairport-compatible):

```
txtvers=1 ch=2 cn=0,1 et=0,1 sv=false da=true sr=44100 ss=16
pw=false vn=3 tp=UDP md=0,1,2 am=OpenAirPlay vs=105.1 sf=0x4
```

(`cn`=codecs PCM+ALAC, `et`=encryption none+RSA, `tp`=transport UDP,
`pw`=no password, `sr`/`ss`=44100/16.)

## Crate choices

| Concern | Crate |
|---|---|
| Async runtime / sockets | `tokio` (RTSP TCP + 3 UDP sockets fit its model well; plain threads also viable) |
| RSA (challenge sign + key decrypt) | `rsa` (supports PKCS#1 v1.5 sign and OAEP decrypt) |
| AES-128-CBC | `aes` + `cbc` (RustCrypto) |
| ALAC decode | `alac` crate (Rust port of Apple's decoder) — verify it accepts the fmtp-derived magic cookie; fallback: small hand port, ALAC decode is ~600 lines |
| ALSA | `alsa` (alsa-rs, safe libasound wrapper) |
| Base64 / hex | `base64`, `hex` |
| D-Bus (option 2) | `zbus` |
| RTSP/SDP parsing | hand-rolled — the subset is tiny and Apple's dialect is quirky; existing RTSP crates don't buy much |

The AirPort Express RSA private key is public knowledge (shipped in
shairport/shairport-sync sources) and gets embedded as a PEM constant.

## Proposed module layout

```
src/
  main.rs        — config, Avahi registration, accept loop
  rtsp.rs        — RTSP request parsing / response writing, method dispatch
  sdp.rs         — minimal SDP attribute parser
  crypto.rs      — AirPort RSA key, Apple-Response, AES session decrypt
  session.rs     — per-connection state machine (ANNOUNCE→SETUP→RECORD…)
  rtp.rs         — UDP packet parsing (audio/sync/retransmit/timing)
  buffer.rs      — seq-indexed jitter buffer, retransmit logic
  timing.rs      — NTP timestamp math, clock offset estimation
  player.rs      — ALAC decode → volume → ALSA output thread
```

## Milestones

1. **Skeleton**: RTSP server that logs requests, answers OPTIONS with a
   correct Apple-Response. Avahi advertisement. An iPhone should *list and
   connect to* the receiver.
2. **Handshake**: ANNOUNCE/SETUP/RECORD handled, UDP sockets bound, encrypted
   packets arriving and decrypting (verify ALAC magic bytes in plaintext).
3. **Sound**: ALAC decode + naive playback (start when buffer half full, no
   sync). Music comes out of the speakers.
4. **Robustness**: jitter buffer with retransmits, FLUSH/TEARDOWN, volume,
   sequence wrap, second-client handling.
5. **Sync**: timing channel, latency-correct start, drift handling.
6. **Nice-to-have**: metadata/artwork logging, password (Digest auth),
   config file, multiple ALSA device selection.

## Risks / open questions

- **Modern clients**: current iOS/macOS still speak AirPlay 1 to receivers
  that only advertise `_raop._tcp` (shairport-sync in AirPlay 1 mode works
  today), but behavior differs subtly between sender versions — test with
  both an iPhone and a Mac early (milestone 1/2).
- **ALAC crate compatibility** with the raw fmtp cookie needs a spike.
- **Unencrypted mode**: some senders skip encryption if `et=0` is offered —
  handy for debugging with e.g. `raop_play`/owntone as test senders.
- **Clock drift** is the hard 20%; deferring it (milestone 5) keeps early
  milestones honest.

## Sources

- [Unofficial AirPlay Protocol Specification (nto)](https://nto.github.io/AirPlay.html) — best single overview of RAOP: RTSP flow, SDP, packet formats, timing.
- [Unofficial AirPlay Specification (openairplay)](https://openairplay.github.io/airplay-spec/audio/rtsp_requests/announce.html) — per-request detail: [ANNOUNCE](https://openairplay.github.io/airplay-spec/audio/rtsp_requests/announce.html), [SETUP](https://openairplay.github.io/airplay-spec/audio/rtsp_requests/setup.html), [RECORD](https://openairplay.github.io/airplay-spec/audio/rtsp_requests/record.html), [OPTIONS](https://openairplay.github.io/airplay-spec/audio/rtsp_requests/options.html).
- [shairport-sync](https://github.com/mikebrady/shairport-sync) — the reference C implementation (AirPlay 1 receiver, ALSA backend, contains the RSA private key and the "stuffing" drift approach).
- [shairplay (juhovh)](https://github.com/juhovh/shairplay) — smaller, older C receiver; good for cross-checking packet handling.
- [RAOP technical note (xmms2 wiki)](https://github.com/xmms2/wiki/wiki/Technical-note-that-describes-the-Remote-Audio-Access-Protocol-(RAOP)-used-in-AirTunes) and [Airtunes2 spec](https://git.zx2c4.com/Airtunes2/about/) — early reverse-engineering notes incl. timing.
- [Remote Audio Output Protocol (Wikipedia)](https://en.wikipedia.org/wiki/Remote_Audio_Output_Protocol) — history of the key extractions.
- Sender-side references for packet formats: [owntone raop.c](https://github.com/owntone/owntone-server/blob/master/src/outputs/raop.c), [philippe44/RAOP-Player](https://github.com/philippe44/RAOP-Player/blob/master/src/raop_client.c), [rust-raop-player](https://github.com/LinusU/rust-raop-player) (Rust sender, useful crate precedents).
- Crates: [`alsa`](https://crates.io/crates/alsa), [`rsa`](https://crates.io/crates/rsa), [`aes`](https://crates.io/crates/aes), [`cbc`](https://crates.io/crates/cbc), ALAC via the Rust port of Apple's decoder.
