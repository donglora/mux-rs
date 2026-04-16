//! Per-client session state and bounded send queue.
//!
//! Each connected client gets a [`ClientSession`] that tracks its ID,
//! RX-interest flag, an optional TX-data cache (for the TX-loopback
//! fan-out spec §13.4), and a bounded channel for outgoing wire-encoded
//! frames.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;
use tracing::warn;

/// Bounded send queue capacity per client.
const SEND_QUEUE_CAP: usize = 256;

/// Emit a drop-count summary after this many drops for a given client.
const DROP_LOG_EVERY: u64 = 32;

/// Global client ID counter.
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(0);

pub type ClientId = u64;

/// Per-client state tracked by the mux daemon.
pub struct ClientSession {
    /// Unique client identifier.
    pub id: ClientId,
    /// Whether this client has called `RX_START`.
    pub rx_interested: bool,
    /// In-flight TX payloads keyed by the client's upstream tag. Needed
    /// so the mux can fan out a `TX_DONE(TRANSMITTED)` to other clients
    /// as an origin=LocalLoopback RX frame (spec §13.4) without asking
    /// the sender to re-transmit the bytes.
    pub tx_cache: HashMap<u16, Vec<u8>>,
    /// Sender half of the bounded channel to the client's writer task.
    tx: mpsc::Sender<Vec<u8>>,
    /// Running total of frames dropped for this client because its
    /// queue was full. Shared with the writer task so metrics survive
    /// the session.
    drops: Arc<AtomicU64>,
}

impl ClientSession {
    /// Create a new session. Returns the session and the receiver half
    /// for the client's writer task to drain.
    pub fn new() -> (Self, mpsc::Receiver<Vec<u8>>) {
        let id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(SEND_QUEUE_CAP);
        let session =
            Self { id, rx_interested: false, tx_cache: HashMap::new(), tx, drops: Arc::new(AtomicU64::new(0)) };
        (session, rx)
    }

    /// Human-readable label for logging.
    pub fn label(&self) -> String {
        format!("client-{}", self.id)
    }

    /// Running total of frames dropped for this client.
    pub fn drop_count(&self) -> u64 {
        self.drops.load(Ordering::Relaxed)
    }

    /// Best-effort enqueue of a wire frame for sending to this client.
    ///
    /// If the channel is full (slow client), the frame is dropped and
    /// counted. Drop counts are summarized every `DROP_LOG_EVERY` drops.
    pub fn enqueue(&self, frame: Vec<u8>) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.tx.try_send(frame) {
            let prev = self.drops.fetch_add(1, Ordering::Relaxed);
            let total = prev + 1;
            if total == 1 || total.is_multiple_of(DROP_LOG_EVERY) {
                warn!("{}: send queue full, dropped {total} frames so far", self.label());
            }
        }
        // Closed channel (client disconnected) is silently ignored.
    }
}
