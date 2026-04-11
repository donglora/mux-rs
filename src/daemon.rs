//! Core mux daemon: dongle I/O, client management, reconnect loop.
//!
//! The daemon owns the USB serial connection and exposes Unix domain socket
//! and optional TCP listeners. It routes commands from clients to the dongle
//! and broadcasts/routes responses back.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use donglora_client::protocol::{
    CMD_TAG_SET_CONFIG, CMD_TAG_START_RX, CMD_TAG_STOP_RX, RADIO_CONFIG_SIZE, RESP_TAG_OK, RESP_TAG_RX_PACKET,
};
use donglora_client::{FrameReader, encode_frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{Mutex, mpsc, oneshot};

/// A queued command: (client_id, raw_cmd_bytes).
type QueuedCmd = (u64, Vec<u8>);

/// Shared command receiver, wrapped for use across reconnect cycles.
type SharedCmdRx = Arc<Mutex<mpsc::Receiver<QueuedCmd>>>;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::intercept::{self, MuxState};
use crate::session::ClientSession;

/// Command timeout waiting for dongle response.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Delay between dongle reconnect attempts.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Max consecutive dongle read errors before declaring disconnect.
const MAX_CONSECUTIVE_ERRORS: u32 = 3;

/// Sentinel client ID for synthetic commands (e.g. auto StopRx).
const SYNTHETIC_CLIENT_ID: u64 = u64::MAX;

/// Keepalive ping interval: if no commands flow for this long, ping the dongle.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// How long to wait for a keepalive Pong before declaring the dongle lost.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(2);

/// Shared mux state, protected by async mutex.
type SharedState = Arc<Mutex<MuxInner>>;

struct MuxInner {
    sessions: HashMap<u64, ClientSession>,
    mux_state: MuxState,
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
        let state: SharedState =
            Arc::new(Mutex::new(MuxInner { sessions: HashMap::new(), mux_state: MuxState::new() }));

        // Command channel: client handlers → dongle writer
        let (cmd_tx, cmd_rx) = mpsc::channel::<(u64, Vec<u8>)>(64);
        let cmd_rx = Arc::new(Mutex::new(cmd_rx));

        // Pending response slot: dongle writer installs, dongle reader resolves
        let pending_response: Arc<Mutex<Option<oneshot::Sender<Vec<u8>>>>> = Arc::new(Mutex::new(None));

        // Prepare socket directory
        let sock_path = Path::new(&self.socket_path);
        if let Some(parent) = sock_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        // Acquire an exclusive lock to prevent multiple mux instances.
        // The lock is held for the process lifetime and auto-released on exit (even SIGKILL).
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
            // Lock acquired but socket file exists — stale from a crashed instance.
            info!("removing stale socket {}", self.socket_path);
            tokio::fs::remove_file(sock_path).await.ok();
        }

        // Start Unix socket server
        let unix_listener = UnixListener::bind(&self.socket_path)
            .map_err(|e| anyhow::anyhow!("failed to bind Unix socket {}: {e}", self.socket_path))?;
        info!("Unix socket listening on {}", self.socket_path);

        // Start TCP server (optional)
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

        // Spawn client accept tasks
        tokio::spawn(accept_unix_clients(unix_listener, state.clone(), cmd_tx.clone(), self.shutdown.clone()));
        if let Some(listener) = tcp_listener {
            tokio::spawn(accept_tcp_clients(listener, state.clone(), cmd_tx.clone(), self.shutdown.clone()));
        }

        // Connect/reconnect loop
        self.reconnect_loop(state, cmd_tx, cmd_rx, pending_response).await;

        // Cleanup
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
        cmd_tx: mpsc::Sender<(u64, Vec<u8>)>,
        cmd_rx: SharedCmdRx,
        pending_response: Arc<Mutex<Option<oneshot::Sender<Vec<u8>>>>>,
    ) {
        let mut saved_config: Option<[u8; RADIO_CONFIG_SIZE]> = None;
        let mut had_rx_interest = false;

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

            let reader_handle = tokio::spawn(dongle_reader(
                serial_read,
                state.clone(),
                pending_response.clone(),
                dongle_lost.clone(),
                self.shutdown.clone(),
            ));

            let writer_handle = tokio::spawn(dongle_writer(
                serial_write,
                cmd_rx.clone(),
                state.clone(),
                pending_response.clone(),
                dongle_lost.clone(),
                self.shutdown.clone(),
            ));

            // Re-establish radio state from previous session
            if let Some(config) = saved_config.take() {
                info!("restoring radio config after reconnect");
                let mut cmd = vec![CMD_TAG_SET_CONFIG];
                cmd.extend_from_slice(&config);
                let _ = cmd_tx.send((SYNTHETIC_CLIENT_ID, cmd)).await;
                if had_rx_interest {
                    let _ = cmd_tx.send((SYNTHETIC_CLIENT_ID, vec![CMD_TAG_START_RX])).await;
                }
            }

            // Wait for dongle loss or shutdown
            tokio::select! {
                () = self.shutdown.cancelled() => {},
                () = dongle_lost.cancelled() => {},
            }

            reader_handle.abort();
            writer_handle.abort();
            let _ = reader_handle.await;
            let _ = writer_handle.await;

            // Drain pending commands
            {
                let mut rx = cmd_rx.lock().await;
                while rx.try_recv().is_ok() {}
            }

            // Save radio state for restoration after reconnect
            {
                let mut inner = state.lock().await;
                saved_config = inner.mux_state.locked_config.take();
                had_rx_interest = intercept::rx_interest_count(&inner.sessions) > 0;
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
///
/// Returns the serial stream and the lock. The caller must keep `_serial_lock` alive
/// for the duration of the dongle session. Dropping it releases the exclusive flock,
/// allowing other processes to claim the port.
async fn open_and_ping(
    port: &str,
) -> anyhow::Result<(tokio_serial::SerialStream, fd_lock::RwLock<std::fs::File>)> {
    // Acquire exclusive flock before opening the serial port. This prevents bridges
    // or other processes from directly opening the same device while the mux owns it.
    let lock_path = serial_lock_path(port);
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| anyhow::anyhow!("failed to open serial lock file {lock_path}: {e}"))?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    // Acquire exclusive flock (non-blocking). Fails if another process holds it.
    let guard = lock
        .try_write()
        .map_err(|_| anyhow::anyhow!("serial port {port} is locked by another process"))?;
    // Forget the guard so its Drop doesn't call flock(UNLOCK). The underlying flock
    // stays held as long as the file descriptor (owned by `lock`) remains open.
    // When `lock` is dropped at the end of the dongle session, the fd closes and
    // the OS releases the flock automatically.
    std::mem::forget(guard);

    let builder = tokio_serial::new(port, 115_200).timeout(Duration::from_secs(2));
    let mut serial =
        tokio_serial::SerialStream::open(&builder).map_err(|e| anyhow::anyhow!("failed to open {port}: {e}"))?;

    let ping_frame = encode_frame(&[0]);
    AsyncWriteExt::write_all(&mut serial, &ping_frame)
        .await
        .map_err(|e| anyhow::anyhow!("failed to write ping: {e}"))?;

    let mut reader = FrameReader::new();
    let mut buf = [0u8; 64];
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
            () = tokio::time::sleep(remaining) => {
                anyhow::bail!("ping timeout");
            }
        };

        if n == 0 {
            anyhow::bail!("serial port closed during ping");
        }

        for frame in reader.feed(&buf[..n]) {
            if frame.first() == Some(&0) {
                info!("dongle responded to Ping");
                return Ok((serial, lock));
            }
        }
    }
}

async fn dongle_reader(
    mut serial: tokio::io::ReadHalf<tokio_serial::SerialStream>,
    state: SharedState,
    pending_response: Arc<Mutex<Option<oneshot::Sender<Vec<u8>>>>>,
    dongle_lost: CancellationToken,
    shutdown: CancellationToken,
) {
    let mut reader = FrameReader::new();
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
                resolve_pending_with_error(&pending_response).await;
                dongle_lost.cancel();
                return;
            }
            Ok(n) => {
                consecutive_errors = 0;
                for frame in reader.feed(&buf[..n]) {
                    dispatch_frame(&frame, &state, &pending_response).await;
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    error!("dongle read error (giving up after {consecutive_errors}): {e}");
                    resolve_pending_with_error(&pending_response).await;
                    dongle_lost.cancel();
                    return;
                }
                warn!("dongle read glitch ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn dispatch_frame(
    frame: &[u8],
    state: &SharedState,
    pending_response: &Arc<Mutex<Option<oneshot::Sender<Vec<u8>>>>>,
) {
    let Some(&tag) = frame.first() else {
        return;
    };

    if tag == RESP_TAG_RX_PACKET {
        let cobs_frame = encode_frame(frame);
        let inner = state.lock().await;
        for session in inner.sessions.values() {
            session.enqueue(cobs_frame.clone());
        }
    } else {
        let mut slot = pending_response.lock().await;
        if let Some(sender) = slot.take() {
            let _ = sender.send(frame.to_vec());
        } else {
            warn!("unsolicited non-RxPacket response (tag {tag}) — dropped");
        }
    }
}

async fn resolve_pending_with_error(pending_response: &Arc<Mutex<Option<oneshot::Sender<Vec<u8>>>>>) {
    let mut slot = pending_response.lock().await;
    if let Some(sender) = slot.take() {
        let _ = sender.send(vec![5, 1]); // Error(RadioBusy)
    }
}

/// Send a Ping and wait for Pong. Returns `true` if the dongle responded.
async fn keepalive_ping(
    serial: &mut tokio::io::WriteHalf<tokio_serial::SerialStream>,
    pending_response: &Arc<Mutex<Option<oneshot::Sender<Vec<u8>>>>>,
) -> bool {
    let (resp_tx, resp_rx) = oneshot::channel();
    {
        let mut slot = pending_response.lock().await;
        *slot = Some(resp_tx);
    }

    let ping_frame = encode_frame(&[0]);
    if AsyncWriteExt::write_all(serial, &ping_frame).await.is_err() {
        return false;
    }

    match tokio::time::timeout(KEEPALIVE_TIMEOUT, resp_rx).await {
        Ok(Ok(_)) => true,
        _ => {
            let mut slot = pending_response.lock().await;
            *slot = None;
            false
        }
    }
}

async fn dongle_writer(
    mut serial: tokio::io::WriteHalf<tokio_serial::SerialStream>,
    cmd_rx: SharedCmdRx,
    state: SharedState,
    pending_response: Arc<Mutex<Option<oneshot::Sender<Vec<u8>>>>>,
    dongle_lost: CancellationToken,
    shutdown: CancellationToken,
) {
    let keepalive = tokio::time::sleep(KEEPALIVE_INTERVAL);
    tokio::pin!(keepalive);

    loop {
        // Pull next command, or fire keepalive if idle too long
        let cmd = {
            let mut rx = cmd_rx.lock().await;
            tokio::select! {
                cmd = rx.recv() => cmd,
                () = &mut keepalive => None,
                () = shutdown.cancelled() => return,
                () = dongle_lost.cancelled() => return,
            }
        };

        keepalive.as_mut().reset(tokio::time::Instant::now() + KEEPALIVE_INTERVAL);

        let (client_id, raw_cmd) = match cmd {
            Some(c) => c,
            None => {
                debug!("keepalive ping");
                if !keepalive_ping(&mut serial, &pending_response).await {
                    error!("keepalive ping failed - dongle lost");
                    dongle_lost.cancel();
                    return;
                }
                continue;
            }
        };

        // Check for interception
        let intercepted = {
            let mut inner = state.lock().await;
            let locked = inner.mux_state.locked_config;
            intercept::maybe_intercept(&raw_cmd, client_id, &mut inner.sessions, &locked)
        };
        if let Some(synthetic) = intercepted {
            let inner = state.lock().await;
            if let Some(session) = inner.sessions.get(&client_id) {
                session.enqueue(encode_frame(&synthetic));
            }
            continue;
        }

        // Install oneshot for the response
        let (resp_tx, resp_rx) = oneshot::channel();
        {
            let mut slot = pending_response.lock().await;
            *slot = Some(resp_tx);
        }

        // Send command to dongle
        let cobs_frame = encode_frame(&raw_cmd);
        if let Err(e) = AsyncWriteExt::write_all(&mut serial, &cobs_frame).await {
            error!("dongle write error: {e}");
            resolve_pending_with_error(&pending_response).await;
            dongle_lost.cancel();
            return;
        }

        // Wait for response
        let response = match tokio::time::timeout(COMMAND_TIMEOUT, resp_rx).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(_)) => {
                warn!("response channel dropped");
                vec![5, 1]
            }
            Err(_) => {
                error!("dongle response timeout");
                let mut slot = pending_response.lock().await;
                *slot = None;
                vec![5, 1]
            }
        };

        // Track state on successful forward
        let cmd_tag = raw_cmd.first().copied().unwrap_or(0xFF);
        let resp_tag = response.first().copied().unwrap_or(0xFF);
        {
            let mut inner = state.lock().await;
            if cmd_tag == CMD_TAG_SET_CONFIG && resp_tag == RESP_TAG_OK {
                if let Some(config_bytes) = raw_cmd.get(1..1 + RADIO_CONFIG_SIZE) {
                    let mut locked = [0u8; RADIO_CONFIG_SIZE];
                    locked.copy_from_slice(config_bytes);
                    inner.mux_state.locked_config = Some(locked);
                    info!("radio config locked");
                }
            } else if cmd_tag == CMD_TAG_START_RX && resp_tag == RESP_TAG_OK {
                if let Some(session) = inner.sessions.get_mut(&client_id) {
                    session.rx_interested = true;
                }
            } else if cmd_tag == CMD_TAG_STOP_RX
                && resp_tag == RESP_TAG_OK
                && let Some(session) = inner.sessions.get_mut(&client_id)
            {
                session.rx_interested = false;
            }
        }

        // Route response to commanding client
        if client_id != SYNTHETIC_CLIENT_ID {
            let inner = state.lock().await;
            if let Some(session) = inner.sessions.get(&client_id) {
                session.enqueue(encode_frame(&response));
            }
        }
    }
}

// ── Client management ──────────────────────────────────────────────

async fn accept_unix_clients(
    listener: UnixListener,
    state: SharedState,
    cmd_tx: mpsc::Sender<(u64, Vec<u8>)>,
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
                spawn_client(read_half, write_half, state.clone(), cmd_tx.clone(), shutdown.clone());
            }
            Err(e) => warn!("Unix accept error: {e}"),
        }
    }
}

async fn accept_tcp_clients(
    listener: TcpListener,
    state: SharedState,
    cmd_tx: mpsc::Sender<(u64, Vec<u8>)>,
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
                // Low-latency: disable Nagle (COBS frames are small)
                let _ = stream.set_nodelay(true);
                let (read_half, write_half) = stream.into_split();
                spawn_client(read_half, write_half, state.clone(), cmd_tx.clone(), shutdown.clone());
            }
            Err(e) => warn!("TCP accept error: {e}"),
        }
    }
}

fn spawn_client<R, W>(
    read_half: R,
    write_half: W,
    state: SharedState,
    cmd_tx: mpsc::Sender<(u64, Vec<u8>)>,
    shutdown: CancellationToken,
) where
    R: AsyncReadExt + Unpin + Send + 'static,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    let (session, send_rx) = ClientSession::new();
    let client_id = session.id;
    let label = session.label();

    let state_clone = state.clone();
    let cmd_tx_clone = cmd_tx.clone();

    tokio::spawn(async move {
        {
            let mut inner = state_clone.lock().await;
            inner.sessions.insert(client_id, session);
            info!("{label} connected ({} total)", inner.sessions.len());
        }

        let writer_shutdown = shutdown.clone();
        let writer_handle = tokio::spawn(client_writer(write_half, send_rx, writer_shutdown));

        client_reader(read_half, client_id, cmd_tx_clone.clone(), shutdown).await;

        writer_handle.abort();
        let _ = writer_handle.await;

        remove_client(client_id, &state_clone, &label, &cmd_tx_clone).await;
    });
}

async fn client_reader<R: AsyncReadExt + Unpin>(
    mut read_half: R,
    client_id: u64,
    cmd_tx: mpsc::Sender<(u64, Vec<u8>)>,
    shutdown: CancellationToken,
) {
    let mut reader = FrameReader::new();
    let mut buf = [0u8; 4096];

    loop {
        let read_result = tokio::select! {
            result = read_half.read(&mut buf) => result,
            () = shutdown.cancelled() => return,
        };

        match read_result {
            Ok(0) => return,
            Ok(n) => {
                for frame in reader.feed(&buf[..n]) {
                    if cmd_tx.send((client_id, frame)).await.is_err() {
                        return;
                    }
                }
            }
            Err(_) => return,
        }
    }
}

async fn client_writer<W: AsyncWriteExt + Unpin>(
    mut write_half: W,
    mut send_rx: mpsc::Receiver<Vec<u8>>,
    shutdown: CancellationToken,
) {
    loop {
        let frame = tokio::select! {
            f = send_rx.recv() => {
                match f {
                    Some(f) => f,
                    None => return,
                }
            }
            () = shutdown.cancelled() => return,
        };

        if AsyncWriteExt::write_all(&mut write_half, &frame).await.is_err() {
            return;
        }
    }
}

async fn remove_client(client_id: u64, state: &SharedState, label: &str, cmd_tx: &mpsc::Sender<(u64, Vec<u8>)>) {
    let mut inner = state.lock().await;

    let was_interested = inner.sessions.get(&client_id).is_some_and(|s| s.rx_interested);

    inner.sessions.remove(&client_id);
    info!("{label} disconnected ({} remain)", inner.sessions.len());

    // If last RX-interested client gone, send synthetic StopRx
    if was_interested && intercept::rx_interest_count(&inner.sessions) == 0 {
        info!("last RX-interested client gone — sending StopRx");
        // Drop the lock before sending to avoid deadlock
        drop(inner);
        let _ = cmd_tx.send((SYNTHETIC_CLIENT_ID, vec![CMD_TAG_STOP_RX])).await;
        return;
    }

    // Reset locked config when all clients are gone
    if inner.sessions.is_empty() && inner.mux_state.locked_config.is_some() {
        info!("all clients gone — config lock released");
        inner.mux_state.locked_config = None;
    }
}
