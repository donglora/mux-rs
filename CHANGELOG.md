# Changelog

## Unreleased

### Added

- Keepalive ping: detects unresponsive dongles (e.g. UART boards behind a
  CP2102 bridge where the serial port stays open across ESP32 resets). Pings
  every 5 seconds of idle, declares dongle lost after 2-second timeout.
- Radio state restoration after reconnect: saved SetConfig and StartRx are
  re-sent to the dongle so clients don't need to know a reset happened.

## 0.2.2 — 2026-04-08

### Fixed

- Bumped MSRV from 1.85 to 1.93 to satisfy `ucobs` dependency requirement.
  `cargo install donglora-mux` now works on toolchains >= 1.93.

## 0.2.1 — 2026-04-07

### Fixed

- **Exclusive serial port lock.** The mux now acquires an `flock`-based
  exclusive lock on the serial port before opening it. This prevents other
  processes (bridges, other mux instances) from directly opening the same
  device while the mux owns it. The lock is held for the duration of the
  dongle session and auto-released on disconnect or crash.

## 0.2.0 — 2026-04-07

### Changed

- Upgraded to `donglora-client` 0.2 (published crate, no more monorepo path dep).
- Wire-level protocol constants now imported from `donglora_client::protocol::*`.

### Fixed

- Repository URL now points to `github.com/donglora/mux-rs`.
- README links updated (removed stale monorepo paths).
- Added `rust-version`, `homepage`, and `exclude` to Cargo.toml.

## 0.1.1 — 2026-04-06

### Fixed

- Prevent multiple mux instances from running simultaneously via file-based
  exclusive lock (`fd-lock`); stale sockets from crashed instances are cleaned
  up automatically
- Clearer error message when TCP port is already in use

## 0.1.0 — 2026-04-06

Initial release.

### Features

- USB multiplexer daemon: share one DongLoRa dongle with multiple applications
- Unix domain socket and optional TCP listeners (same COBS-framed protocol)
- RxPacket broadcast to all connected clients
- Reference-counted StartRx/StopRx across clients
- Config locking: first client sets radio config, others must match or fail
- Auto-reconnect on USB hot-plug with ping verification
- Bounded per-client send queues (256 frames) with backpressure isolation
- Smart interception: redundant commands get synthetic responses without
  hitting the dongle
- Graceful shutdown on SIGINT/SIGTERM with socket cleanup
- TCP_NODELAY on accepted connections for low-latency frame delivery
- CLI: `--port`, `--socket`, `--tcp`, `--verbose` options
