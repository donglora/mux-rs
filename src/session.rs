//! Per-client session state and bounded send queue.
//!
//! Each connected client gets a [`ClientSession`] that tracks its ID,
//! RX interest flag, and a bounded channel for outgoing COBS frames.

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;
use tracing::warn;

/// Bounded send queue capacity per client.
const SEND_QUEUE_CAP: usize = 256;

/// Global client ID counter.
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(0);

/// Per-client state tracked by the mux daemon.
pub struct ClientSession {
    /// Unique client identifier.
    pub id: u64,
    /// Whether this client has called StartRx.
    pub rx_interested: bool,
    /// Sender half of the bounded channel to the client's writer task.
    tx: mpsc::Sender<Vec<u8>>,
}

impl ClientSession {
    /// Create a new session. Returns the session and the receiver half
    /// for the client's writer task to drain.
    pub fn new() -> (Self, mpsc::Receiver<Vec<u8>>) {
        let id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(SEND_QUEUE_CAP);
        let session = Self { id, rx_interested: false, tx };
        (session, rx)
    }

    /// Human-readable label for logging.
    pub fn label(&self) -> String {
        format!("client-{}", self.id)
    }

    /// Best-effort enqueue of a COBS frame for sending to this client.
    ///
    /// If the channel is full (slow client), the frame is dropped with a warning.
    /// This prevents backpressure from one slow client blocking the entire mux.
    pub fn enqueue(&self, frame: Vec<u8>) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.tx.try_send(frame) {
            warn!("{}: send queue full, dropping frame", self.label());
        }
        // Closed channel (client disconnected) is silently ignored — the session
        // will be cleaned up when the client handler task exits.
    }
}
