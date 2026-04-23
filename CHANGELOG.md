# Changelog

## 1.0.1 — 2026-04-22

### Dependencies

- Bumped `donglora-client` to `1.0.1` — adds the WCH CH340K USB-UART
  bridge (`1a86:7522`) to the port-discovery list. The Elecrow
  ThinkNode-M2 board is now detected automatically; previously mux
  refused to attach to it because the bridge VID:PID wasn't
  recognized.

## 1.0.0 — 2026-04-22

### Breaking

- **Upgraded to DongLoRa Protocol v2 wire format.** Wire-incompatible with 0.x
  clients and firmware. Now depends directly on `donglora-protocol` for
  frame encode/decode.
- **Tag-aware dispatch.** Each inbound client frame is re-tagged with a
  mux-allocated device tag; responses are routed back to the original
  client using the tag mapping. No more single-oneshot response slot —
  multiple clients can have commands in flight concurrently.

### Added

- **Full `§13.2` multi-client `SET_CONFIG` semantics.** Tracks the
  `(client, modulation)` lock owner. Cross-client calls synthesize:
  - `OK(APPLIED / MINE, params)` when the calling client owns the lock
    (forwarded to dongle).
  - `OK(ALREADY_MATCHED / OTHER, current)` when another client holds
    the lock and the requested params byte-match.
  - `OK(LOCKED_MISMATCH / OTHER, current)` when another client holds
    the lock and the requested params differ.
  - Lock is released automatically when the owning client disconnects.
- **TX loopback fan-out (spec §13.4).** On `TX_DONE(TRANSMITTED)`, the
  mux synthesizes an `RX` frame with `origin = LocalLoopback` and sends
  it to every connected client except the transmitter.
- **Async `ERR(EFRAME)` fan-out.** If the dongle emits async framing
  errors (tag = 0), they're broadcast to all clients rather than
  swallowed.
- **Per-client TX cache** on `ClientSession` so the loopback fan-out
  doesn't need the sender to re-buffer.
- **`TagMapper`** with skip-zero allocation and per-client bulk drop on
  disconnect.

### Removed

- The single `pending_response: oneshot::Sender` slot in the daemon
  (fundamentally incompatible with v1.0's per-tag concurrency).
- The separate 5-second keepalive timer — replaced with a 500 ms
  internal cadence matching the spec §3.4 inactivity window.

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
