//! Core mux daemon — DongLoRa Protocol v2 tag-aware frame routing.
//!
//! Owns the USB serial connection and exposes Unix-domain + TCP listeners.
//! Every inbound client frame goes through a [`TagMapper`] that allocates a
//! fresh **device tag** and remembers the owning `(client, upstream_tag)` so
//! the dongle's reply can be routed back. Async frames from the dongle
//! (`tag == 0`: `RX`, async `ERR(EFRAME)`) fan out to every client.
//!
//! Additional spec-mandated behaviour:
//!
//! - **§13.2** — `SET_CONFIG` arbitration with `APPLIED` / `ALREADY_MATCHED`
//!   / `LOCKED_MISMATCH` results and owner tracking. Handled in
//!   [`crate::intercept`].
//! - **§13.4** — TX loopback: on `TX_DONE(TRANSMITTED)` the mux synthesizes
//!   an RX frame with `origin = LocalLoopback` and fans it out to every
//!   client that didn't send the original TX.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use donglora_protocol::{
    Command, ErrorCode, FrameDecoder, FrameResult, MAX_WIRE_FRAME, OkPayload, Owner, RxOrigin, RxPayload,
    SetConfigResultCode, TxDonePayload, TxResult, commands, encode_frame, events,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::intercept::{self, Decision, MuxState};
use crate::session::{ClientId, ClientSession};

/// Delay between dongle reconnect attempts.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Max consecutive dongle read errors before declaring disconnect.
const MAX_CONSECUTIVE_ERRORS: u32 = 3;

/// Keepalive cadence for the mux → dongle connection. Matches the
/// client-rs `KEEPALIVE_INTERVAL` with 2× margin on the 1 s inactivity
/// window (spec §3.4).
const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(500);

/// Queued inbound work for the dongle writer.
#[derive(Debug)]
enum WriterWork {
    /// Forward a client frame. Payload is the pre-wire `(type_id, payload)`
    /// — writer allocates a fresh device tag, records the mapping, and
    /// re-encodes.
    Forward { client_id: ClientId, type_id: u8, upstream_tag: u16, payload: Vec<u8> },
    /// Periodic mux-originated keepalive ping. No client owns this.
    Keepalive,
}

/// Maps mux-allocated device tags back to `(client, upstream_tag, cmd_type)`.
struct TagMapper {
    next: u16,
    pending: HashMap<u16, Pending>,
}

struct Pending {
    client_id: ClientId,
    upstream_tag: u16,
    cmd_type: u8,
}

impl TagMapper {
    fn new() -> Self {
        Self { next: 1, pending: HashMap::new() }
    }

    /// Allocate a fresh device tag. Skips 0 and wraps at `u16::MAX`.
    fn alloc(&mut self, client_id: ClientId, upstream_tag: u16, cmd_type: u8) -> u16 {
        // Linear probe for an unused tag. In practice the pending map is
        // small (tens of entries max) so this is cheap.
        loop {
            let tag = self.next;
            self.next = self.next.wrapping_add(1);
            if self.next == 0 {
                self.next = 1;
            }
            if tag == 0 || self.pending.contains_key(&tag) {
                continue;
            }
            self.pending.insert(tag, Pending { client_id, upstream_tag, cmd_type });
            return tag;
        }
    }

    /// Final-resolve and remove the mapping.
    fn take(&mut self, device_tag: u16) -> Option<Pending> {
        self.pending.remove(&device_tag)
    }

    /// Peek without removing (used for TX two-phase completion where the
    /// OK is observed but TX_DONE will still arrive under the same tag).
    fn peek(&self, device_tag: u16) -> Option<&Pending> {
        self.pending.get(&device_tag)
    }

    /// Drop every mapping owned by this client (on disconnect).
    fn drop_client(&mut self, client_id: ClientId) {
        self.pending.retain(|_, p| p.client_id != client_id);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.pending.len()
    }
}

/// Shared mux state.
type SharedState = Arc<Mutex<MuxInner>>;

struct MuxInner {
    sessions: HashMap<ClientId, ClientSession>,
    mux_state: MuxState,
    tag_mapper: TagMapper,
}

/// The multiplexer daemon.
pub struct MuxDaemon {
    serial_port: String,
    socket_path: String,
    tcp_addr: Option<(String, u16)>,
    shutdown: CancellationToken,
}

impl MuxDaemon {
    pub fn new(
        serial_port: String,
        socket_path: String,
        tcp_addr: Option<(String, u16)>,
        shutdown: CancellationToken,
    ) -> Self {
        Self { serial_port, socket_path, tcp_addr, shutdown }
    }

    /// Run the mux daemon. Returns when shutdown is signalled.
    pub async fn run(&self) -> anyhow::Result<()> {
        let state: SharedState = Arc::new(Mutex::new(MuxInner {
            sessions: HashMap::new(),
            mux_state: MuxState::new(),
            tag_mapper: TagMapper::new(),
        }));

        // Work queue: client handlers → dongle writer.
        let (work_tx, work_rx) = mpsc::channel::<WriterWork>(64);
        let work_rx = Arc::new(Mutex::new(work_rx));

        let sock_path = Path::new(&self.socket_path);
        if let Some(parent) = sock_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let lock_path = format!("{}.lock", self.socket_path);
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| anyhow::anyhow!("failed to open lock file {lock_path}: {e}"))?;
        let mut lock = fd_lock::RwLock::new(lock_file);
        let _lock_guard = match lock.try_write() {
            Ok(guard) => guard,
            Err(_) => anyhow::bail!("another donglora-mux is already running (lock held on {lock_path})"),
        };

        if sock_path.exists() {
            info!("removing stale socket {}", self.socket_path);
            tokio::fs::remove_file(sock_path).await.ok();
        }

        let unix_listener = UnixListener::bind(&self.socket_path)
            .map_err(|e| anyhow::anyhow!("failed to bind Unix socket {}: {e}", self.socket_path))?;
        info!("Unix socket listening on {}", self.socket_path);

        let tcp_listener = if let Some((ref host, port)) = self.tcp_addr {
            let addr = format!("{host}:{port}");
            let listener = TcpListener::bind(&addr).await.map_err(|e| {
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    anyhow::anyhow!("another donglora-mux is already listening on TCP {addr}")
                } else {
                    anyhow::anyhow!("failed to bind TCP {addr}: {e}")
                }
            })?;
            info!("TCP listening on {addr}");
            Some(listener)
        } else {
            None
        };

        tokio::spawn(accept_unix_clients(unix_listener, state.clone(), work_tx.clone(), self.shutdown.clone()));
        if let Some(listener) = tcp_listener {
            tokio::spawn(accept_tcp_clients(listener, state.clone(), work_tx.clone(), self.shutdown.clone()));
        }

        self.reconnect_loop(state, work_tx, work_rx).await;

        info!("shutting down...");
        if Path::new(&self.socket_path).exists() {
            tokio::fs::remove_file(&self.socket_path).await.ok();
        }
        info!("stopped.");
        Ok(())
    }

    async fn reconnect_loop(
        &self,
        state: SharedState,
        work_tx: mpsc::Sender<WriterWork>,
        work_rx: Arc<Mutex<mpsc::Receiver<WriterWork>>>,
    ) {
        loop {
            if self.shutdown.is_cancelled() {
                break;
            }
            info!("opening dongle on {}", self.serial_port);
            let (serial, _serial_lock_guard) = loop {
                if self.shutdown.is_cancelled() {
                    return;
                }
                match open_and_ping(&self.serial_port).await {
                    Ok(pair) => break pair,
                    Err(e) => {
                        debug!("could not open dongle: {e}");
                        tokio::select! {
                            () = self.shutdown.cancelled() => return,
                            () = tokio::time::sleep(RECONNECT_DELAY) => continue,
                        }
                    }
                }
            };

            info!("dongle connected");
            let (serial_read, serial_write) = tokio::io::split(serial);
            let dongle_lost = CancellationToken::new();

            let reader_handle =
                tokio::spawn(dongle_reader(serial_read, state.clone(), dongle_lost.clone(), self.shutdown.clone()));

            let writer_handle = tokio::spawn(dongle_writer(
                serial_write,
                work_rx.clone(),
                state.clone(),
                dongle_lost.clone(),
                self.shutdown.clone(),
            ));

            // Drive keepalive from a dedicated timer task so the writer
            // loop doesn't need to interleave.
            let keepalive_handle =
                tokio::spawn(keepalive_loop(work_tx.clone(), dongle_lost.clone(), self.shutdown.clone()));

            tokio::select! {
                () = self.shutdown.cancelled() => {},
                () = dongle_lost.cancelled() => {},
            }

            reader_handle.abort();
            writer_handle.abort();
            keepalive_handle.abort();
            let _ = reader_handle.await;
            let _ = writer_handle.await;
            let _ = keepalive_handle.await;

            // Drain any queued work. Mappings will get cleared on reconnect
            // because the lock owner's client may still be live.
            {
                let mut rx = work_rx.lock().await;
                while rx.try_recv().is_ok() {}
            }

            // Wake any clients that had commands in flight by clearing
            // the pending map. Their session handlers notice the writer
            // isn't producing replies and eventually time out — but the
            // cleaner thing is to let the dongle-writer reinstall pending
            // mappings as their clients retry.
            {
                let mut inner = state.lock().await;
                inner.tag_mapper.pending.clear();
            }

            if !self.shutdown.is_cancelled() {
                info!("reconnecting in 2 seconds...");
                tokio::select! {
                    () = self.shutdown.cancelled() => return,
                    () = tokio::time::sleep(RECONNECT_DELAY) => {},
                }
            }
        }
    }
}

// ── Dongle I/O ─────────────────────────────────────────────────────

/// Lock file path for exclusive serial port access.
fn serial_lock_path(port: &str) -> String {
    format!("/tmp/donglora-serial-{}.lock", port.replace('/', "_"))
}

/// Open the serial port with an exclusive lock and verify the dongle responds to ping.
async fn open_and_ping(port: &str) -> anyhow::Result<(tokio_serial::SerialStream, fd_lock::RwLock<std::fs::File>)> {
    let lock_path = serial_lock_path(port);
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| anyhow::anyhow!("failed to open serial lock file {lock_path}: {e}"))?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let guard = lock.try_write().map_err(|_| anyhow::anyhow!("serial port {port} is locked by another process"))?;
    std::mem::forget(guard);

    let builder = tokio_serial::new(port, 115_200).timeout(Duration::from_secs(2));
    let mut serial =
        tokio_serial::SerialStream::open(&builder).map_err(|e| anyhow::anyhow!("failed to open {port}: {e}"))?;

    // PING the dongle (tag=1) to verify it's alive.
    let mut wire = [0u8; MAX_WIRE_FRAME];
    let n = encode_frame(commands::TYPE_PING, 1, &[], &mut wire)
        .map_err(|e| anyhow::anyhow!("failed to encode PING: {e:?}"))?;
    AsyncWriteExt::write_all(&mut serial, &wire[..n])
        .await
        .map_err(|e| anyhow::anyhow!("failed to write ping: {e}"))?;

    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 256];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("ping timeout");
        }
        let n = tokio::select! {
            result = serial.read(&mut buf) => {
                result.map_err(|e| anyhow::anyhow!("serial read error: {e}"))?
            }
            () = tokio::time::sleep(remaining) => anyhow::bail!("ping timeout"),
        };
        if n == 0 {
            anyhow::bail!("serial port closed during ping");
        }
        let mut got = false;
        decoder.feed(&buf[..n], |res| {
            if let FrameResult::Ok { type_id, tag: _, payload: _ } = res
                && type_id == events::TYPE_OK
            {
                got = true;
            }
        });
        if got {
            info!("dongle responded to PING");
            return Ok((serial, lock));
        }
    }
}

async fn keepalive_loop(
    work_tx: mpsc::Sender<WriterWork>,
    dongle_lost: CancellationToken,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = dongle_lost.cancelled() => return,
            () = tokio::time::sleep(KEEPALIVE_INTERVAL) => {},
        }
        if work_tx.send(WriterWork::Keepalive).await.is_err() {
            return;
        }
    }
}

async fn dongle_reader(
    mut serial: tokio::io::ReadHalf<tokio_serial::SerialStream>,
    state: SharedState,
    dongle_lost: CancellationToken,
    shutdown: CancellationToken,
) {
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 512];
    let mut consecutive_errors: u32 = 0;

    loop {
        if shutdown.is_cancelled() || dongle_lost.is_cancelled() {
            return;
        }
        let read_result = tokio::select! {
            result = serial.read(&mut buf) => result,
            () = shutdown.cancelled() => return,
            () = dongle_lost.cancelled() => return,
        };
        match read_result {
            Ok(0) => {
                error!("serial port closed");
                dongle_lost.cancel();
                return;
            }
            Ok(n) => {
                consecutive_errors = 0;
                // Collect frames up-front because the decoder's callback
                // borrows the payload from its internal buffer.
                let mut works: Vec<(u8, u16, Vec<u8>)> = Vec::new();
                let mut frame_errs = 0usize;
                decoder.feed(&buf[..n], |res| match res {
                    FrameResult::Ok { type_id, tag, payload } => {
                        works.push((type_id, tag, payload.to_vec()));
                    }
                    FrameResult::Err(_) => frame_errs += 1,
                });
                for (type_id, tag, payload) in works {
                    dispatch_dongle_frame(type_id, tag, &payload, &state).await;
                }
                if frame_errs > 0 {
                    warn!("{frame_errs} corrupt frame(s) from dongle (CRC or COBS)");
                    // Forward synthetic async ERR(EFRAME) to all clients —
                    // §2.5 allows the device to emit these and we want clients
                    // to be able to observe framing issues on the link.
                    fanout_async_err(&state, ErrorCode::EFrame).await;
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    error!("dongle read error (giving up after {consecutive_errors}): {e}");
                    dongle_lost.cancel();
                    return;
                }
                warn!("dongle read glitch ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn fanout_async_err(state: &SharedState, code: ErrorCode) {
    let mut buf = [0u8; 2];
    if events::encode_err_payload(code, &mut buf).is_err() {
        return;
    }
    let mut wire = [0u8; MAX_WIRE_FRAME];
    let n = match encode_frame(events::TYPE_ERR, 0, &buf, &mut wire) {
        Ok(n) => n,
        Err(_) => return,
    };
    let frame = wire[..n].to_vec();
    let inner = state.lock().await;
    for session in inner.sessions.values() {
        session.enqueue(frame.clone());
    }
}

async fn dispatch_dongle_frame(type_id: u8, tag: u16, payload: &[u8], state: &SharedState) {
    // Async events (tag == 0): fan out to all clients verbatim.
    if tag == 0 {
        let mut wire = [0u8; MAX_WIRE_FRAME];
        let n = match encode_frame(type_id, 0, payload, &mut wire) {
            Ok(n) => n,
            Err(e) => {
                warn!("re-encode of async frame failed: {e:?}");
                return;
            }
        };
        let frame = wire[..n].to_vec();
        let inner = state.lock().await;
        for session in inner.sessions.values() {
            session.enqueue(frame.clone());
        }
        return;
    }

    // Tag-correlated: look up pending mapping.
    let mut inner = state.lock().await;
    // TX has a two-phase completion (OK -> TX_DONE). On the intermediate
    // OK, keep the mapping alive; remove only on the terminal frame.
    let pending_snapshot = inner.tag_mapper.peek(tag).map(|p| (p.client_id, p.upstream_tag, p.cmd_type));
    let Some((client_id, upstream_tag, cmd_type)) = pending_snapshot else {
        warn!("unsolicited frame type={type_id:#04x} tag={tag} — no pending mapping");
        return;
    };

    // Decide if this frame is terminal for the mapping.
    let terminal = !(cmd_type == commands::TYPE_TX && type_id == events::TYPE_OK);
    if terminal {
        inner.tag_mapper.take(tag);
    }

    // Lock-installation: on a successful SET_CONFIG APPLIED, record the
    // `(client, modulation)` lock so subsequent clients see synthesized
    // ALREADY_MATCHED / LOCKED_MISMATCH.
    if cmd_type == commands::TYPE_SET_CONFIG
        && type_id == events::TYPE_OK
        && let Ok(OkPayload::SetConfig(result)) = OkPayload::parse_for(cmd_type, payload)
        && result.result == SetConfigResultCode::Applied
        && result.owner == Owner::Mine
    {
        inner.mux_state.locked = Some((client_id, result.current));
        info!("SET_CONFIG applied by client {client_id} — lock installed");
    }

    // Drop from TX cache on terminal TX_DONE, regardless of outcome.
    let mut tx_loopback: Option<Vec<u8>> = None;
    if cmd_type == commands::TYPE_TX
        && type_id == events::TYPE_TX_DONE
        && let Some(session) = inner.sessions.get_mut(&client_id)
        && let Some(bytes) = session.tx_cache.remove(&upstream_tag)
        && let Ok(td) = TxDonePayload::decode(payload)
        && td.result == TxResult::Transmitted
    {
        tx_loopback = Some(bytes);
    }

    // Track RX interest on RX_START/RX_STOP OK replies so state matches.
    if type_id == events::TYPE_OK {
        match cmd_type {
            commands::TYPE_RX_START => {
                if let Some(s) = inner.sessions.get_mut(&client_id) {
                    s.rx_interested = true;
                }
            }
            commands::TYPE_RX_STOP => {
                if let Some(s) = inner.sessions.get_mut(&client_id) {
                    s.rx_interested = false;
                }
            }
            _ => {}
        }
    }

    // Re-encode with the upstream tag and route to the owning client.
    let mut wire = [0u8; MAX_WIRE_FRAME];
    let n = match encode_frame(type_id, upstream_tag, payload, &mut wire) {
        Ok(n) => n,
        Err(e) => {
            warn!("re-encode of downstream frame failed: {e:?}");
            return;
        }
    };
    let frame = wire[..n].to_vec();
    if let Some(session) = inner.sessions.get(&client_id) {
        session.enqueue(frame);
    }

    // TX loopback fan-out: synthesize an RX frame to every OTHER client.
    if let Some(data) = tx_loopback {
        let mut rx_data = heapless::Vec::<u8, { donglora_protocol::MAX_OTA_PAYLOAD }>::new();
        if rx_data.extend_from_slice(&data).is_ok() {
            let rx = RxPayload {
                rssi_tenths_dbm: 0,
                snr_tenths_db: 0,
                freq_err_hz: 0,
                timestamp_us: now_us(),
                crc_valid: true,
                packets_dropped: 0,
                origin: RxOrigin::LocalLoopback,
                data: rx_data,
            };
            let mut rxbuf = [0u8; RxPayload::METADATA_SIZE + donglora_protocol::MAX_OTA_PAYLOAD];
            if let Ok(n) = rx.encode(&mut rxbuf) {
                let mut wirebuf = [0u8; MAX_WIRE_FRAME];
                if let Ok(n2) = encode_frame(events::TYPE_RX, 0, &rxbuf[..n], &mut wirebuf) {
                    let loop_frame = wirebuf[..n2].to_vec();
                    for (id, session) in &inner.sessions {
                        if *id != client_id {
                            session.enqueue(loop_frame.clone());
                        }
                    }
                }
            }
        }
    }
}

fn now_us() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros().min(u128::from(u64::MAX)) as u64)
}

async fn dongle_writer(
    mut serial: tokio::io::WriteHalf<tokio_serial::SerialStream>,
    work_rx: Arc<Mutex<mpsc::Receiver<WriterWork>>>,
    state: SharedState,
    dongle_lost: CancellationToken,
    shutdown: CancellationToken,
) {
    loop {
        let work = {
            let mut rx = work_rx.lock().await;
            tokio::select! {
                w = rx.recv() => w,
                () = shutdown.cancelled() => return,
                () = dongle_lost.cancelled() => return,
            }
        };
        let Some(work) = work else { return };
        match work {
            WriterWork::Keepalive => {
                // Allocate a synthetic mapping so the reply is absorbed
                // cleanly. Use client_id = u64::MAX as the "synthetic"
                // marker; there's never a real client with that id.
                let device_tag = {
                    let mut inner = state.lock().await;
                    inner.tag_mapper.alloc(u64::MAX, 0, commands::TYPE_PING)
                };
                let mut wire = [0u8; MAX_WIRE_FRAME];
                let n = match encode_frame(commands::TYPE_PING, device_tag, &[], &mut wire) {
                    Ok(n) => n,
                    Err(e) => {
                        warn!("keepalive encode failed: {e:?}");
                        continue;
                    }
                };
                if let Err(e) = AsyncWriteExt::write_all(&mut serial, &wire[..n]).await {
                    error!("dongle write error (keepalive): {e}");
                    dongle_lost.cancel();
                    return;
                }
            }
            WriterWork::Forward { client_id, type_id, upstream_tag, payload } => {
                let device_tag = {
                    let mut inner = state.lock().await;
                    // If this is a TX, stash the payload so the reader
                    // can fan out on TX_DONE(TRANSMITTED).
                    if type_id == commands::TYPE_TX
                        && payload.len() > 1
                        && let Some(session) = inner.sessions.get_mut(&client_id)
                    {
                        session.tx_cache.insert(upstream_tag, payload[1..].to_vec());
                    }
                    inner.tag_mapper.alloc(client_id, upstream_tag, type_id)
                };
                let mut wire = [0u8; MAX_WIRE_FRAME];
                let n = match encode_frame(type_id, device_tag, &payload, &mut wire) {
                    Ok(n) => n,
                    Err(e) => {
                        warn!("forward encode failed: {e:?}");
                        continue;
                    }
                };
                if let Err(e) = AsyncWriteExt::write_all(&mut serial, &wire[..n]).await {
                    error!("dongle write error: {e}");
                    dongle_lost.cancel();
                    return;
                }
            }
        }
    }
}

// ── Client management ──────────────────────────────────────────────

async fn accept_unix_clients(
    listener: UnixListener,
    state: SharedState,
    work_tx: mpsc::Sender<WriterWork>,
    shutdown: CancellationToken,
) {
    loop {
        let accept_result = tokio::select! {
            result = listener.accept() => result,
            () = shutdown.cancelled() => return,
        };
        match accept_result {
            Ok((stream, _addr)) => {
                let (read_half, write_half) = stream.into_split();
                spawn_client(read_half, write_half, state.clone(), work_tx.clone(), shutdown.clone());
            }
            Err(e) => warn!("Unix accept error: {e}"),
        }
    }
}

async fn accept_tcp_clients(
    listener: TcpListener,
    state: SharedState,
    work_tx: mpsc::Sender<WriterWork>,
    shutdown: CancellationToken,
) {
    loop {
        let accept_result = tokio::select! {
            result = listener.accept() => result,
            () = shutdown.cancelled() => return,
        };
        match accept_result {
            Ok((stream, addr)) => {
                debug!("TCP connection from {addr}");
                let _ = stream.set_nodelay(true);
                let (read_half, write_half) = stream.into_split();
                spawn_client(read_half, write_half, state.clone(), work_tx.clone(), shutdown.clone());
            }
            Err(e) => warn!("TCP accept error: {e}"),
        }
    }
}

fn spawn_client<R, W>(
    read_half: R,
    write_half: W,
    state: SharedState,
    work_tx: mpsc::Sender<WriterWork>,
    shutdown: CancellationToken,
) where
    R: AsyncReadExt + Unpin + Send + 'static,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    let (session, send_rx) = ClientSession::new();
    let client_id = session.id;
    let label = session.label();

    let state_clone = state.clone();
    let work_tx_clone = work_tx.clone();

    tokio::spawn(async move {
        {
            let mut inner = state_clone.lock().await;
            inner.sessions.insert(client_id, session);
            info!("{label} connected ({} total)", inner.sessions.len());
        }
        let writer_shutdown = shutdown.clone();
        let writer_handle = tokio::spawn(client_writer(write_half, send_rx, client_id, writer_shutdown));
        client_reader(read_half, client_id, work_tx_clone.clone(), state_clone.clone(), shutdown).await;
        writer_handle.abort();
        let _ = writer_handle.await;
        remove_client(client_id, &state_clone, &label, &work_tx_clone).await;
    });
}

async fn client_reader<R: AsyncReadExt + Unpin>(
    mut read_half: R,
    client_id: ClientId,
    work_tx: mpsc::Sender<WriterWork>,
    state: SharedState,
    shutdown: CancellationToken,
) {
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 4096];

    loop {
        let read_result = tokio::select! {
            result = read_half.read(&mut buf) => result,
            () = shutdown.cancelled() => {
                debug!("client-{client_id} reader exit: mux shutdown");
                return;
            }
        };
        match read_result {
            Ok(0) => {
                debug!("client-{client_id} reader exit: EOF");
                return;
            }
            Ok(n) => {
                let mut to_process: Vec<(u8, u16, Vec<u8>)> = Vec::new();
                decoder.feed(&buf[..n], |res| match res {
                    FrameResult::Ok { type_id, tag, payload } => {
                        to_process.push((type_id, tag, payload.to_vec()));
                    }
                    FrameResult::Err(_) => {
                        // Corrupt upstream frame — best-effort reply with
                        // async ERR(EFRAME) (tag=0).
                    }
                });
                for (type_id, upstream_tag, payload) in to_process {
                    if !handle_client_frame(type_id, upstream_tag, payload, client_id, &work_tx, &state).await {
                        return;
                    }
                }
            }
            Err(e) => {
                debug!("client-{client_id} reader exit: read error: {e}");
                return;
            }
        }
    }
}

/// Returns `true` to keep reading, `false` on terminal error.
async fn handle_client_frame(
    type_id: u8,
    upstream_tag: u16,
    payload: Vec<u8>,
    client_id: ClientId,
    work_tx: &mpsc::Sender<WriterWork>,
    state: &SharedState,
) -> bool {
    // Decide whether to intercept or forward.
    let decision = {
        let mut inner = state.lock().await;
        let MuxInner { mux_state, sessions, .. } = &mut *inner;
        intercept::decide(type_id, &payload, client_id, mux_state, sessions)
    };
    match decision {
        Decision::Synthesize { type_id: out_type, payload: out_payload } => {
            let mut wire = [0u8; MAX_WIRE_FRAME];
            let n = match encode_frame(out_type, upstream_tag, &out_payload, &mut wire) {
                Ok(n) => n,
                Err(e) => {
                    warn!("synthesized frame encode failed: {e:?}");
                    return true;
                }
            };
            let inner = state.lock().await;
            if let Some(session) = inner.sessions.get(&client_id) {
                session.enqueue(wire[..n].to_vec());
            }
            true
        }
        Decision::Drop => true,
        Decision::Forward => {
            // Unknown/malformed commands: short-circuit with a synthesized
            // ERR(EUNKNOWN_CMD) that echoes the upstream tag, so the
            // client's per-tag correlation still resolves. Done here so
            // the mux's dispatcher stays symmetric with the spec.
            if Command::parse(type_id, &payload).is_err() {
                let (err_type, err_payload) = intercept::synthesize_err(ErrorCode::EUnknownCmd);
                let mut wire = [0u8; MAX_WIRE_FRAME];
                if let Ok(n) = encode_frame(err_type, upstream_tag, &err_payload, &mut wire) {
                    let inner = state.lock().await;
                    if let Some(session) = inner.sessions.get(&client_id) {
                        session.enqueue(wire[..n].to_vec());
                    }
                }
                return true;
            }
            let send = work_tx.send(WriterWork::Forward { client_id, type_id, upstream_tag, payload }).await;
            send.is_ok()
        }
    }
}

async fn client_writer<W: AsyncWriteExt + Unpin>(
    mut write_half: W,
    mut send_rx: mpsc::Receiver<Vec<u8>>,
    client_id: ClientId,
    shutdown: CancellationToken,
) {
    loop {
        let frame = tokio::select! {
            f = send_rx.recv() => match f {
                Some(f) => f,
                None => {
                    debug!("client-{client_id} writer exit: queue closed");
                    return;
                }
            },
            () = shutdown.cancelled() => {
                debug!("client-{client_id} writer exit: shutdown");
                return;
            }
        };
        if let Err(e) = AsyncWriteExt::write_all(&mut write_half, &frame).await {
            debug!("client-{client_id} writer exit: {e}");
            return;
        }
    }
}

async fn remove_client(client_id: ClientId, state: &SharedState, label: &str, work_tx: &mpsc::Sender<WriterWork>) {
    let mut inner = state.lock().await;
    let (was_interested, drops) =
        inner.sessions.get(&client_id).map_or((false, 0), |s| (s.rx_interested, s.drop_count()));
    inner.sessions.remove(&client_id);
    inner.tag_mapper.drop_client(client_id);

    // Release the config lock if this client held it.
    if let Some((owner, _)) = inner.mux_state.locked
        && owner == client_id
    {
        inner.mux_state.locked = None;
        info!("config lock released (owner {label} disconnected)");
    }

    if drops > 0 {
        info!("{label} disconnected ({} remain, {drops} dropped frames)", inner.sessions.len());
    } else {
        info!("{label} disconnected ({} remain)", inner.sessions.len());
    }

    // If last RX-interested client is gone, synthesize an RX_STOP.
    if was_interested && MuxState::rx_interest_count(&inner.sessions) == 0 {
        drop(inner);
        let _ = work_tx
            .send(WriterWork::Forward {
                client_id: u64::MAX,
                type_id: commands::TYPE_RX_STOP,
                upstream_tag: 0,
                payload: vec![],
            })
            .await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn tag_mapper_skips_zero() {
        let mut m = TagMapper::new();
        // Start at 1; after many allocations we should never see 0.
        let tag1 = m.alloc(1, 100, commands::TYPE_PING);
        assert_ne!(tag1, 0);
        m.take(tag1);
        for _ in 0..1024 {
            let t = m.alloc(1, 100, commands::TYPE_PING);
            assert_ne!(t, 0);
            m.take(t);
        }
    }

    #[test]
    fn tag_mapper_take_returns_mapping() {
        let mut m = TagMapper::new();
        let tag = m.alloc(42, 7, commands::TYPE_TX);
        let p = m.take(tag).unwrap();
        assert_eq!(p.client_id, 42);
        assert_eq!(p.upstream_tag, 7);
        assert_eq!(p.cmd_type, commands::TYPE_TX);
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn tag_mapper_peek_keeps_mapping() {
        let mut m = TagMapper::new();
        let tag = m.alloc(42, 7, commands::TYPE_TX);
        let p = m.peek(tag).unwrap();
        assert_eq!(p.client_id, 42);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn tag_mapper_drop_client_removes_only_that_clients_tags() {
        let mut m = TagMapper::new();
        let t_a1 = m.alloc(1, 10, commands::TYPE_PING);
        let t_a2 = m.alloc(1, 11, commands::TYPE_PING);
        let t_b = m.alloc(2, 12, commands::TYPE_PING);
        m.drop_client(1);
        assert!(m.take(t_a1).is_none());
        assert!(m.take(t_a2).is_none());
        assert!(m.take(t_b).is_some());
    }
}
