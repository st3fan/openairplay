# Milestone 1 — Skeleton

Goal (from `design.md`): an RTSP server that logs every request, answers
OPTIONS with a correct `Apple-Response`, and advertises itself via Avahi so
an iPhone/Mac *lists* the receiver and *connects* to it. No audio yet.

## Scope

In:

- Cargo binary crate `openairplay` (tokio-based, structured as lib + bin so
  integration tests can drive the server in-process).
- RTSP/1.0 request parsing (request line, headers, `Content-Length` body)
  and response writing, Apple dialect (`CSeq` echo, `Server:` header,
  `Audio-Jack-Status`).
- `OPTIONS` handled fully, including the `Apple-Challenge` →
  `Apple-Response` RSA signature (the challenge may arrive on any request,
  so it is handled generically in the dispatcher).
- All other methods: logged (method, URI, headers, printable bodies) and
  answered `501 Not Implemented` — they get real handlers in milestone 2.
- Avahi advertisement by spawning `avahi-publish-service` with the
  shairport-compatible `_raop._tcp` TXT records; graceful warning if the
  binary is missing (as on this dev box).
- MAC address discovery from `/sys/class/net` (needed twice: the mDNS
  service name `<MAC>@<Name>` and the Apple-Response buffer must use the
  same MAC).
- CLI flags: `--name`, `--port`, `--mac`, `--no-avahi`.

Out: ANNOUNCE/SETUP/RECORD semantics, UDP channels, decryption, ALAC, ALSA,
password auth, D-Bus Avahi registration (subprocess is fine for now).

## Apple-Response algorithm (verified against shairport-sync `rtsp.c`)

1. Base64-decode `Apple-Challenge` (clients may omit `=` padding); reject
   if longer than 16 bytes.
2. Build buffer: challenge bytes ‖ local socket IP (4 bytes IPv4, 16 bytes
   IPv6; a v4-mapped IPv6 address contributes its 4 IPv4 bytes) ‖ 6-byte
   MAC. Zero-pad to at least 32 bytes.
3. Sign with the well-known AirPort Express RSA private key (from
   shairport-sync `common.c`), raw PKCS#1 v1.5 padding, no digest
   (`Pkcs1v15Sign::new_unprefixed()` in the `rsa` crate — equivalent to
   OpenSSL `EVP_PKEY_sign` with `RSA_PKCS1_PADDING` and no md).
4. Base64-encode without `=` padding → `Apple-Response` header.

## TXT records (copied from shairport-sync classic mode)

```
txtvers=1 ch=2 cn=0,1 ek=1 et=0,1 sv=false da=true sr=44100 ss=16
pw=false vn=65537 tp=UDP md=0,1,2 am=OpenAirPlay vs=105.1 sf=0x4
```

## Module layout

```
src/main.rs    — CLI args, logging init, MAC discovery, avahi spawn, serve
src/lib.rs     — module declarations, Config
src/crypto.rs  — embedded RSA key, apple_response()
src/rtsp.rs    — Request/Response types, async parse/write
src/server.rs  — accept loop, per-connection dispatch, logging
src/avahi.rs   — avahi-publish-service child process management
src/mac.rs     — MAC discovery from /sys/class/net
```

## Acceptance criteria

- `cargo build` and `cargo test` pass; `cargo clippy` clean.
- Unit tests: RTSP parser (headers, body, bad input), Apple-Response
  (signature round-trip verified with the public key, no-padding base64,
  v4-mapped address handling, oversized challenge rejected).
- Integration test: server on an ephemeral port answers OPTIONS with 200,
  echoed `CSeq`, `Public:` method list, and a verifiable `Apple-Response`;
  unknown method gets 501.
- Manual check documented: run the binary, `nc`-style OPTIONS exchange.
  (Real-iPhone discovery requires an Avahi-equipped host; this box has no
  avahi, so that check is deferred to hardware testing.)

## Result

Done. 17 tests pass (14 unit + 3 integration), clippy clean. Manual smoke
test with `nc`: OPTIONS → `200 OK` with echoed CSeq, full `Public:` list, and
a `Apple-Response` that verifies against the AirPort public key; a follow-up
ANNOUNCE on the same server → `501 Not Implemented` without dropping the
connection. IPv4 clients arrive as v4-mapped IPv6 on the dual-stack socket
and the challenge code uses their 4 IPv4 bytes, matching what the client
signs.

Implementation note: the RustCrypto `pkcs1` PEM reader rejects shairport's
76-column line wrapping, so `src/airport.pem` is the same key re-emitted by
`openssl rsa -traditional` at the standard 64-column width (modulus
unchanged — verified identical).

Not verified on this box (deferred to hardware with Avahi + an Apple client):
actual mDNS discovery and connection from an iPhone/Mac.
