# DongLoRa Mux (Rust)

USB multiplexer daemon — lets multiple applications share one DongLoRa
dongle simultaneously.

## Install

```sh
cargo install donglora-mux
```

## What It Does

- Owns the USB serial connection exclusively
- Exposes a Unix domain socket (and optional TCP) speaking the same
  COBS-framed protocol
- RxPacket frames broadcast to all connected clients
- StartRx/StopRx reference-counted across clients
- SetConfig locked once set (single client can change freely)
- Single-instance enforcement via file lock (stale sockets auto-cleaned)
- Automatic dongle reconnect on hot-plug
- No panics — enforced by clippy deny lints

## Running

```sh
donglora-mux                                    # start the mux daemon
donglora-mux --verbose                          # start with verbose logging
donglora-mux --tcp 5741 --port /dev/ttyACM0     # with options
```

## Depends On

- [`donglora-client`](https://crates.io/crates/donglora-client) — protocol
  types, COBS framing, device discovery
