# Licensing, provenance & attribution

This document records where openairplay (the AirPlay 1 / RAOP receiver) took
inspiration and code from, the licenses of those sources (verified against the
upstream files, not assumed), the recommended license for this project, and the
caveats — especially around the embedded AirPort Express RSA private key.
**This is not legal advice; if distribution or anything commercial matters, get
a real legal opinion, particularly regarding the AirPort key.**

## Sources drawn on, and how

| Source | How it was used | Nature |
|---|---|---|
| **shairport-sync** (James Laird, Mike Brady) | Primary protocol reference: the RTP/RAOP wire details (`rtp.c`), the resend-request encoding (byte-exact against its layout), the `_raop._tcp` TXT records, the coarse frame "stuffing" drift approach, and the copy of the AirPort private key (`src/airport.pem`, taken verbatim from its `common.c`). | Reference + translated logic + the embedded key |
| **`src/airport.pem`** — the AirPort Express **RSA private key** | Embedded verbatim as a PEM constant; used to answer the `Apple-Challenge` (`Apple-Response`) and to decrypt the AES session key from the SDP. | Third-party, Apple-derived (see caveat) |
| **`alac` crate** (Ed Barnard) | ALAC (Apple Lossless) audio decoding. | Dependency, MIT/Apache-2.0 |
| RAOP reverse-engineering docs (xmms2 RAOP technical note, the Airtunes2 spec, Wikipedia) | Protocol/spec reference only. | Documentation, no code |
| Rust crates: `rsa`, `aes`, `cbc`, `sha1`, `base64`, `alsa`, `tokio`, `zbus` | Normal dependencies (RSA/AES, hashing, ALSA, async, mDNS/D-Bus). | Dependencies, permissive licenses |

Everything else is original Rust written for this project, informed by the
protocol but not copied: the RTSP/SDP parsing, the session/server structure, the
jitter buffer + retransmit logic, the NTP timing / latency-correct start, the
ALSA player, and the Avahi D-Bus advertisement.

## Verified upstream licenses

- **shairport-sync — MIT.** Its source headers (e.g. `rtsp.c`, `common.c`)
  carry the MIT text, © James Laird 2013 and Mike Brady 2014-2026. Its
  top-level `COPYING` says "refer to the individual source files for licenses"
  because it bundles some third-party components under other licenses, but the
  core code referenced here is MIT.
- **`alac` crate — MIT/Apache-2.0** (dual), by Ed Barnard. (Apple's own
  reference ALAC decoder, which such ports descend from, is Apache-2.0.)
- **The AirPort Express RSA private key** is **Apple's**. It was extracted from
  an AirPort Express (by James Laird, whose `shairport` is the ancestor of
  shairport-sync) and is now shipped by essentially every third-party AirPlay 1
  receiver. No open-source project's license grants rights to it — it is Apple's
  key, redistributed for interoperability.

## The AirPort key caveat (matters more than the OSS license)

`src/airport.pem` is the **actual private key** extracted from Apple's AirPort
Express. Its legal exposure is Apple's copyright / trade-secret rights and the
DMCA §1201 anti-circumvention provisions — **not** which open-source license
this project picks. A license only governs *our* code; it cannot grant rights to
Apple's key.

If anything, this is *more* sensitive than a set of reverse-engineered
constants: it is a cryptographic private key whose extraction and redistribution
was the subject of the original AirPort key saga. Every third-party AirPlay 1
receiver embeds it, and Apple has not pursued them, but it is a genuine gray
area. Using openairplay to receive audio streamed to your own device is the
intended interop use. The key is kept isolated in one file (`src/airport.pem`,
loaded by `src/crypto.rs`) and attributed; treat it as external Apple data, not
as original project code.

## What this means for openairplay's own code

- The protocol *facts* (RTP layout, TXT keys, the `Apple-Response` construction)
  are generally not copyrightable, and where expressive logic was translated,
  the source (shairport-sync) is MIT — so it is MIT-compatible.
- The embedded key is Apple's; the MIT grant on this project does not, and
  cannot, extend to it. It should be quarantined and attributed (see `NOTICE.md`).

## Recommended license: MIT

MIT for this project's own code is the natural, defensible choice: it matches
shairport-sync (the primary reference) and the Rust crates, and `Cargo.toml`
already declares `license = "MIT"`.

To make that real and honest, the repo should carry:

1. A top-level **`LICENSE`** file with the MIT text (currently missing — the
   `Cargo.toml` field alone is not the license grant).
2. A **`NOTICE.md`** that:
   - attributes protocol reference to shairport-sync (MIT, Laird/Brady);
   - quarantines the AirPort Express RSA private key (`src/airport.pem`) as
     third-party Apple material — not original to this project, embedded solely
     for interoperability, with the caveat above.

## Copyright status of AI-assisted code

openairplay was written with heavy AI assistance under human direction.

- Under current US Copyright Office guidance, output with *no* human authorship
  is generally not copyrightable; human-authored, -selected, -arranged, or
  -modified portions do qualify. Copyrightability of purely machine-generated
  passages is thin or absent.
- This project had substantial **human authorship and direction**: the
  architecture, the milestone planning, the hardware testing that drove each
  fix, and the review. That is a real human-authored layer, so it is not
  "purely AI-generated".
- You can still license the repo. For parts that are copyrightable, MIT grants
  permission as usual; for parts that are not, the license simply has nothing to
  grant (they are effectively public domain), which is harmless — and MIT's
  warranty disclaimer still applies either way.
- If you wanted to explicitly disclaim copyright instead, CC0 or the Unlicense
  are options, but MIT is the conventional, low-friction choice and is
  recommended here.

## Summary

- **Project license:** MIT (add a `LICENSE` file + `NOTICE.md`).
- **shairport-sync:** MIT — protocol reference and the key copy; MIT-compatible.
- **`alac` crate:** MIT/Apache-2.0 — dependency.
- **AirPort Express RSA private key (`src/airport.pem`):** third-party Apple
  material; the real caveat is Apple/DMCA, independent of the OSS license chosen.
- **AI-assisted authorship:** licensing is fine; substantial human direction
  gives a real authored layer, and MIT governs what is protectable.
- Not legal advice.

## Comparison with openairplay2

The AirPlay 2 receiver reaches the same conclusion (MIT + a NOTICE) for the same
reasons; there the Apple-derived material is the FairPlay `fp-setup` tables
rather than the AirPort RSA key. See that project's `notes/licensing.md`.
