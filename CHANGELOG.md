# Changelog

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
