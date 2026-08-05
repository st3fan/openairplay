# OpenAirPlay — AirPlay 1 password protocol notes

These notes record what this project learned about **classic AirPlay 1
(RAOP / AirTunes) password protection** while building the optional
`--password` feature (see `plans/20260804-01-pincode.md`). They are
empirical and reference-verified, not from Apple documentation: the wire
behavior was captured from a real sender, and the mechanism was cross-checked
against [shairport-sync](https://github.com/mikebrady/shairport-sync) (the
canonical third-party receiver), which this notes file cites at
`rtsp.c:rtsp_classic_airplay_auth`.

> **Naming.** What iOS shows the user as a prompt is a *password*. This
> project's flag and API use `--password`/`Config.password` to match.
> Apple's AirPlay 2 equivalent (on-screen pairing) is a separate, *different*
> mechanism and out of scope here.

## The `pw` TXT record

The `_raop._tcp` advertisement carries a `pw` key that tells Apple senders
whether the receiver requires a password:

```
no password   -> pw=false
password set  -> pw=true
```

Two things matter here:

- **The value is a boolean** — `pw=true` / `pw=false`, not `pw=1` / `pw=0`
  (our first cut advertised `pw=1`, which is wrong; shairport-sync emits
  `pw=true`).
- **The password itself is never in the advertisement.** `pw` only flips the
  protection bit; the secret is set out-of-band on the receiver.

Just advertising `pw=true` is **not** enough to gate anything. It is the
*sender-facing* signal; the enforcement happens over RTSP (below). A receiver
that advertises `pw=true` but never challenges behaves exactly like an open
one — this is the trap that made our first probe look like "iOS ignores the
password." It wasn't ignoring it; nothing had challenged it.

## The authentication mechanism: RFC 2617 Digest over RTSP

Enforcement is standard HTTP **Digest** authentication (RFC 2617) carried in
the existing RTSP request/response exchange, exactly as shairport-sync
implements it.

### Flow

```
Client                              Receiver (password set)
  |--- any RTSP request ---------------->|
  |        (no Authorization header)     |  generate per-connection nonce
  |<--------- 401 Unauthorized ----------|
  |        WWW-Authenticate: Digest realm="raop", nonce="…"
  |--- same request -------------------->|
  |        Authorization: Digest realm="raop", username="…",   \
  |                      response="…", uri="…", nonce="…"        |
  |                                     |  compute expected response,
  |                                     |  constant-time compare
  |<--------- 200 (or real status) ------|
  |  ... authorized for the rest of this connection ...
```

- The challenge is per-**connection**: the receiver picks a fresh nonce
  (8 random bytes, base64), keeps it for the session, and issues the same
  value on repeat challenges until the connection authenticates.
- Once a connection answers a challenge correctly it is marked authorized and
  every later request on it passes without re-challenging (no per-request
  re-auth, even across ANNOUNCE → SETUP → RECORD).
- The `Apple-Challenge` / `Apple-Response` device handshake is **orthogonal**
  and still happens on every response, 401s included. `Apple-Challenge`
  proves the *receiver* is a genuine AirPort; the Digest exchange proves the
  *sender* knows the password. Both are answered; neither replaces the other.
- Without a password configured, every connection is authorized immediately
  and the wire is byte-for-byte identical to an unprotected receiver.

### The digest computation

The value sent in `Authorization: Digest` `response="…"` and matched by the
receiver (32-char lowercase hex) is nested MD5:

```text
HA1      = MD5(username ":" realm ":" password)
HA2      = MD5(method ":" uri)
response = MD5(hex(HA1) ":" nonce ":" hex(HA2))
```

with `realm = "raop"`, `method` = the RTSP method (e.g. `OPTIONS`), `uri` =
the RTSP request URI, and `nonce` = the receiver's per-connection nonce.

## Reference implementation (shairport-sync)

The behavior above mirrors shairport-sync's `rtsp_classic_airplay_auth`
(`rtsp.c`), which:

- runs before method dispatch once `config.password` is set —
  `rtsp_classic_airplay_auth(...) == 0` gates `conn->authorized`; otherwise
  the request is answered `401` with `WWW-Authenticate: Digest realm="raop",
  nonce="…"`;
- parses `realm`, `username`, `response`, `uri` out of the `Authorization`
  header (not the client's `nonce` — it uses its own stored nonce);
- computes the expected digest with MD5 as above and compares.

Shairport-sync's release notes are the historical evidence for which clients
speak this flow: they credit **Android** apps (AllConnect/Streambels) and old
iTunes. Notably they did **not** list iOS — but see the empirical result
below, which found modern iOS does participate.

## Empirical findings (iOS 26)

- **The no-gate probe was misleading.** Against the Phase 2 receiver (which
  advertised `pw` but had no 401 gate), an iPhone streamed with no credential
  and looked like "iOS ignores password protection." In fact a Digest client
  does not volunteer credentials; it responds to a challenge. There was no
  challenge, so there was nothing to answer.
- **With the gate, iOS 26 prompts and honors the password.** A real iPhone on
  iOS 26 showed a dialog, rejected a wrong password (`1111`), and accepted
  the right one (`1234`). Modern Apple senders *do* speak the classic AirPlay
  1 Digest flow; they only authenticate when challenged.

## Consequences for this receiver

- Advertise `pw=true` (boolean) only when a password is set; `pw=false`
  otherwise. The secret is never advertised.
- Gate at the RTSP dispatch point (before any method runs) with the Digest
  challenge, keeping `Apple-Challenge`/`Apple-Response` and the common
  response headers on 401s.
- Compare the digest response in constant time; the password never appears in
  logs, responses, or the advertisement.
- A password only protects *streaming*; it does not and should not protect the
  dashboard WebSocket (separate threat model, unauthenticated loopback).
