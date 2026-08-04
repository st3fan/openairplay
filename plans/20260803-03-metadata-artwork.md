# Track metadata and artwork events (requested by radio)

Goal: surface the **track metadata and cover art the sender already sends**
so embedding hosts can display them — the AirPlay 1 counterpart of
[openairplay2's milestone](https://github.com/st3fan/openairplay2/blob/main/plans/20260802-01-metadata-artwork.md)
(shipped there as PR #23). The consumer is the same:
[st3fan/radio](https://github.com/st3fan/radio), whose `radiod` daemon
embeds both receivers and whose dashboard shows a `— NO TRACK INFO —`
placeholder where title/artist/album and cover art should be.

**The API must be the same shape as openairplay2's**, deliberately: radiod
handles both receivers in one event task, and two identical `Event`
variants mean one mapping, not two. Where the wire differs (it does — see
below), the difference stays inside the library.

## Background: where the data already is

RAOP senders push now-playing info over the RTSP control connection via
`SET_PARAMETER`, distinguished by `Content-Type` — the same three payload
kinds as AirPlay 2, since this part of the protocol never changed:

- `application/x-dmap-tagged` — **track metadata** as a DMAP/DAAP binary
  payload: 4-byte tag codes with big-endian u32 lengths, nested in an
  `mlit` (dmap.listingitem) container. The tags the consumer cares about:
  - `minm` (dmap.itemname) → **title**
  - `asar` (daap.songartist) → **artist**
  - `asal` (daap.songalbum) → **album**
- `image/jpeg` / `image/png` (also seen: `image/none` with an empty body,
  meaning "no art") — **cover art**, raw bytes, tens to a few hundred KB.
- `text/parameters` with `progress: start/current/end` — playback position
  as RTP timestamps. (The `volume:` flavor of `text/parameters` is already
  handled in [session.rs](../openairplay1/src/session.rs).)

Senders transmit these at session start and on every track change — push,
not poll. Same conclusion as in openairplay2: no polling accessor, the
library already owns an event channel and `Receiver::run` consumes the
receiver, so events land in radiod's shared state and its dashboard picks
them up on its existing cycle.

AirPlay 1 senders additionally put an `RTP-Info: rtptime=…` header on
metadata `SET_PARAMETER`s so a receiver can align the display with the
audio it is currently playing. We ignore it (out of scope, below).

## The requested API

Two additive `Event` variants — **byte-identical to openairplay2's**. The
enum is already `#[non_exhaustive]`, so this is a compatible change for
existing embedders (including this repo's own binary, which has a `_` arm).

```rust
/// SET_PARAMETER application/x-dmap-tagged. Fields the payload did not
/// carry are None; a new event replaces the previous one wholesale.
Event::Metadata {
    title: Option<String>,   // minm
    artist: Option<String>,  // asar
    album: Option<String>,   // asal
},

/// SET_PARAMETER image/*. `data` empty (or content_type image/none)
/// means "the sender cleared the artwork".
Event::Artwork {
    content_type: String,    // "image/jpeg" | "image/png"
    data: Vec<u8>,
},
```

Contract details the consumer depends on (unchanged from openairplay2):

- **Ordering:** both events arrive only between `SessionStarted` and
  `SessionEnded` for their session. The library *enforces* this because the
  wire does not: `SessionStarted` is emitted inside `handle_setup`, and a
  sender may push metadata earlier on the same connection. So the session
  **latches** the most recent metadata/artwork that arrives while no session
  is active and emits them immediately after `SessionStarted`, rather than
  dropping them (losing the first track) or emitting them early (breaking
  the contract). The existing `Volume` event is *not* gated this way — leave
  it as is; its contract predates this change. The consumer clears its own
  display state on `SessionEnded`, so no explicit clear events are needed —
  but `image/none` is still forwarded as the empty-artwork case, since it
  happens mid-track.
- **Replacement semantics:** each `Metadata` event is a complete statement,
  not a delta. Title-only payload → artist/album `None` and the consumer
  blanks them. That matches how DMAP arrives (one `mlit` per track change).
- **Duplicates are fine** — the consumer is idempotent.
- **Unknown DMAP tags are skipped silently**; tag-code + length walking
  only, not a general DAAP implementation. The three wanted tags sit
  *inside* `mlit`.
- **Strings are UTF-8** (lossy conversion acceptable).
- **Artwork is delivered as-is** — no decoding or resizing in the library.

## Fit with the current code (library-side survey)

Checked against the code as of 2026-08-03. Three differences from
openairplay2 matter:

- **There is no dispatch gap here.** openairplay2 had to plumb the
  `Content-Type` header into `Session::set_parameter`, which took only the
  body. In this library [server.rs](../openairplay1/src/server.rs) already
  passes the whole `Request` to `Session::handle_other`, so the header is in
  hand: the `"SET_PARAMETER"` arm just branches on
  `request.headers.get("Content-Type")` — `text/parameters` (or absent, for
  safety) → the existing volume scan, `application/x-dmap-tagged` →
  metadata, `image/*` → artwork, anything else → debug-log and 200 OK as
  today. Strip any `; charset=…` parameter and match case-insensitively.
- **`MAX_BODY` must go up, or artwork kills the connection.** RTSP here is
  plaintext, and [rtsp.rs](../openairplay1/src/rtsp.rs) reads each body to
  its exact `Content-Length` — good — but under a **1 MB** cap
  (`MAX_BODY`), and exceeding it returns `Err`, which propagates out of
  `handle_connection` and **drops the RTSP connection**, i.e. kills the
  stream. openairplay2's cap is 8 MB. Raise this one to 8 MB to match, so a
  large JPEG is a normal request rather than a teardown. (The hard error
  above the cap stays: at 8 MB it is garbage-sender protection, not a
  legitimate payload.)
- **The `md` TXT record already advertises metadata — no bitmask fix
  needed.** openairplay2 lost its first hardware capture to cleared feature
  bits 15/16/17. The AirPlay 1 equivalent is the `md` TXT record, and
  [avahi.rs](../openairplay1/src/avahi.rs) already ships `md=0,1,2` (text,
  artwork, progress), matching shairport-sync's classic mode. So nothing to
  change — but this is exactly the thing that silently produces *zero*
  metadata on the wire, so the hardware capture below is what confirms it,
  and `txt_records()` is the first place to look if the capture is empty.

Otherwise the shape carries over unchanged:

- **The DMAP walker is a new private module** (`dmap.rs`), hand-rolled tag
  code + big-endian-u32 length walking, no new dependencies. It can be
  ported essentially verbatim from openairplay2 — the payloads are the same
  format from the same senders. It should be a straight `&[u8] ->
  ParsedMetadata` function with no I/O, so it unit-tests inline.
- **No public-API wire types leak:** both variants carry plain
  `String`/`Vec<u8>`, consistent with the invariant that the documented API
  stays free of RAOP wire types.
- **The binary logs the new events**: `openairplay1-receiver`'s event task
  ([main.rs](../openairplay1-receiver/src/main.rs)) gains `Metadata` /
  `Artwork` arms at info level (`now playing: Artist — Title (Album)`,
  `artwork: image/jpeg, N bytes`) so hardware runs are readable without
  `RUST_LOG=debug`. It stays a pure consumer of the public API.
- **`log_request` gets slightly better**: it already prints non-text bodies
  as `N bytes of <content-type>`, which is right for DMAP and images. No
  change needed; just don't let a debug path try to print a JPEG.

## Testing

Mirrors openairplay2's approach, adapted to this repo's layout:

- **Unit tests in [session.rs](../openairplay1/src/session.rs)** driving
  `handle_other` directly and asserting on the event channel with `try_recv`
  — the pattern the existing volume test already uses. Cases: content-type
  branching (DMAP → `Metadata`, `image/jpeg` → `Artwork`, `text/parameters`
  → `Volume` still, unknown type → 200 with no event), replacement
  semantics, `image/none`/empty body → `Artwork` with empty `data`, and the
  latch: metadata sent *before* SETUP is emitted right after
  `SessionStarted`, in order, exactly once.
- **Unit tests for `dmap.rs`** with hand-built payloads (`mlit` wrapping
  `minm`/`asar`/`asal`), plus the malformed cases: truncated length, length
  running past the buffer, unknown tags interleaved, empty body, non-UTF-8
  bytes. None of these may panic or error out of the session — metadata is
  decoration, never worth a teardown.
- **One integration test** in `openairplay1/tests/` (alongside
  [handshake.rs](../openairplay1/tests/handshake.rs)) driving the real
  server over a real socket through the `#[doc(hidden)]` modules: complete
  ANNOUNCE → SETUP → RECORD, then send a `SET_PARAMETER` with a DMAP body
  and one with an `image/png` body, and assert both events arrive on the
  event channel with the right content — proving the header plumbing end to
  end. Include one body larger than the old 1 MB cap so the `MAX_BODY`
  regression is covered by a test rather than by hardware.
- **Hardware validation** against a real sender (iPhone/Mac, Music.app), as
  every wire-touching change here requires: title/artist/album on each track
  change, `image/jpeg` artwork, and specifically *when* metadata arrives
  relative to SETUP — the latch design assumes it can be early, and
  openairplay2's capture found the opposite for its sender (~1 s after the
  pipeline started). Worth recording which way AirPlay 1 senders behave.

## Scope

In scope: the two `Event` variants, `Content-Type` branching in the
`SET_PARAMETER` arm of `handle_other`, a private `dmap.rs` walker, the
latch-until-`SessionStarted` gating, the `MAX_BODY` raise to 8 MB, the
binary's log lines, and the tests above.

Out of scope (per the consumer): `progress:` forwarding; the `RTP-Info`
`rtptime` on metadata requests (aligning display to playback position); a
sender-displayed-name field on `SessionStarted`; any artwork-serving
endpoint (radiod's job); changing the `Volume` event's ungated timing.

## Phases

One implementation phase — contained: one new module, three touched files
in the library, one in the binary, additive API.

1. **Metadata and artwork events** — `dmap.rs` walker, `Content-Type`
   branching in `Session::handle_other`, the two `Event` variants with
   latch-until-`SessionStarted` gating, `MAX_BODY` 1 MB → 8 MB, binary log
   arms, unit + integration tests.

## Acceptance criteria

- `Event::Metadata` and `Event::Artwork` delivered per the contract above
  (replacement semantics, only between `SessionStarted` and `SessionEnded`,
  `image/none`/empty forwarded as artwork-cleared), with the **same variant
  shapes as openairplay2** so radiod maps both libraries with one match.
- A several-hundred-KB artwork body no longer risks dropping the RTSP
  connection.
- Existing behavior unchanged: volume path, session flow, single-session
  gate, and every existing test still green; no new dependencies; library
  still ALSA-free (`cargo tree -p openairplay1`) and macOS-green
  (`cargo test -p openairplay1`); clippy and fmt clean.
- Malformed metadata payloads never kill a session.
- Hardware validation against a real sender: title, artist, album, and cover
  art observed in the event stream at session start and on track change,
  including when metadata arrives relative to SETUP.

## Status

Approved; phase 1 implemented (PR #23) — unit + integration tests green,
clippy/fmt/rustdoc clean, no new dependencies, library still ALSA-free.

Two things the implementation confirmed:

- The `MAX_BODY` raise is load-bearing, not hygiene: reverting the constant
  makes the new integration test fail with an EOF from the server, i.e. the
  old 1 MB cap really did drop the RTSP connection on a large artwork body.
- Nothing had to change in `avahi.rs` — `md=0,1,2` was already advertised, so
  there is no AirPlay 1 counterpart to openairplay2's features-bitmask fix.

Outstanding: hardware validation against a real sender (see the acceptance
criteria), including whether AirPlay 1 senders push metadata before or after
SETUP. openairplay2's sender pushed ~1 s *after*, leaving its latch idle; if
that holds here too, the latch stays as armor for other senders.
