# OpenAirPlay

A minimal AirPlay 1 (RAOP / AirTunes) audio receiver for Linux, written in
Rust. It aims to accept audio streams from stock Apple clients (iPhone, iPad,
Mac, iTunes) and play them on an ALSA device. mDNS/DNS-SD advertisement is
delegated to the system Avahi daemon.

See [`notes/design.md`](notes/design.md) for the protocol design and
[`notes/milestone-1.md`](notes/milestone-1.md) for the current milestone.

## Status

**Milestone 1 (skeleton) — complete.** The receiver runs an RTSP server that
logs requests, answers `OPTIONS` including the `Apple-Challenge` →
`Apple-Response` RSA signature, and advertises `_raop._tcp` via Avahi. Audio
handling (ANNOUNCE/SETUP/RECORD, decryption, ALAC, ALSA) is not yet
implemented — other methods return `501 Not Implemented`.

## Build & test

```sh
cargo build
cargo test
```

## Run

```sh
# Advertise as "Living Room" on the default RTSP port 5000.
./target/debug/openairplay --name "Living Room"

# Options
#   --name NAME     friendly name shown on the client (default OpenAirPlay)
#   --port PORT     RTSP TCP port (default 5000)
#   --mac AA:..:FF  MAC used in the service name and challenge signature
#                   (default: auto-detected from /sys/class/net)
#   --no-avahi      do not spawn avahi-publish-service

# Verbose logging:
RUST_LOG=debug ./target/debug/openairplay
```

Avahi advertisement requires `avahi-publish-service` (Debian/Ubuntu:
`avahi-utils`) and a running `avahi-daemon`. Without them the receiver still
serves RTSP; it just won't be discoverable, and you can connect by address
for testing.

## Notes

The embedded `src/airport.pem` is the well-known AirPort Express RSA private
key (as shipped by shairport-sync). It is what lets any third-party receiver
answer the `Apple-Challenge`; it is not a secret.
