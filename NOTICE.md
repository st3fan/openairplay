# NOTICE

openairplay is licensed under the MIT License (see `LICENSE`). This file records
third-party material and attributions. A fuller discussion, including the
verified upstream licenses and the AirPort-key/DMCA caveats, is in
[`notes/licensing.md`](notes/licensing.md). **This is not legal advice.**

## Protocol reference — shairport-sync (MIT)

The AirPlay 1 / RAOP protocol handling was developed with reference to
[shairport-sync](https://github.com/mikebrady/shairport-sync), © James Laird
and Mike Brady, MIT-licensed. Specifically: the RTP wire details and the
resend-request encoding were verified against its source, the `_raop._tcp` TXT
records follow its layout, the coarse frame "stuffing" drift approach is
adapted from it, and `src/airport.pem` is copied verbatim from its `common.c`.
shairport-sync's core is MIT; some components it bundles carry other licenses
(none of those were used here).

## AirPort Express RSA private key — third-party, Apple's

`src/airport.pem` is the RSA **private key** extracted from Apple's AirPort
Express. **It is not original to this project.** It was extracted by James Laird
(whose `shairport` is the ancestor of shairport-sync) and is embedded by
essentially every third-party AirPlay 1 receiver; this copy comes from
shairport-sync.

Its legal status is governed by Apple's copyright / trade-secret rights and the
DMCA anti-circumvention provisions, **not** by this project's MIT license — the
MIT grant does not, and cannot, extend to Apple's key. It is embedded solely to
interoperate with Apple senders (to answer the `Apple-Challenge` and decrypt the
AES session key for audio streamed to your own device).

## ALAC decoding — the `alac` crate (MIT/Apache-2.0)

Apple Lossless (ALAC) decoding uses the `alac` crate by Ed Barnard, dual-licensed
MIT/Apache-2.0.

## Dependencies

Other third-party Rust crates (`rsa`, `aes`, `cbc`, `sha1`, `base64`, `alsa`,
`tokio`, `zbus`, …) are under their own licenses (predominantly MIT /
Apache-2.0); see `Cargo.toml` and their respective repositories.
