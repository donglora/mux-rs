//! Smart multiplexing: SetConfig locking, StartRx/StopRx reference counting.
//!
//! The mux intercepts certain commands to avoid redundant dongle operations
//! when multiple clients are connected. Intercepted commands get synthetic
//! responses without hitting the physical dongle.

use std::collections::HashMap;

use donglora_client::{
    CMD_TAG_SET_CONFIG, CMD_TAG_START_RX, CMD_TAG_STOP_RX, ERROR_INVALID_CONFIG, RADIO_CONFIG_SIZE,
    RESP_TAG_ERROR, RESP_TAG_OK,
};
use tracing::{debug, warn};

use crate::session::ClientSession;

/// Mux-level state for interception logic.
pub struct MuxState {
    /// Raw 13-byte RadioConfig, locked once set by any client with others connected.
    pub locked_config: Option<[u8; RADIO_CONFIG_SIZE]>,
}

impl MuxState {
    pub fn new() -> Self {
        Self {
            locked_config: None,
        }
    }
}

/// Check if a command should be intercepted (synthetic response) vs forwarded to the dongle.
///
/// Returns `Some(response_bytes)` if intercepted, `None` to forward normally.
pub fn maybe_intercept(
    raw_cmd: &[u8],
    client_id: u64,
    sessions: &mut HashMap<u64, ClientSession>,
    locked_config: &Option<[u8; RADIO_CONFIG_SIZE]>,
) -> Option<Vec<u8>> {
    let cmd_tag = *raw_cmd.first()?;

    match cmd_tag {
        CMD_TAG_SET_CONFIG => intercept_set_config(raw_cmd, client_id, sessions, locked_config),
        CMD_TAG_START_RX => intercept_start_rx(client_id, sessions),
        CMD_TAG_STOP_RX => intercept_stop_rx(client_id, sessions),
        _ => None,
    }
}

fn intercept_set_config(
    raw_cmd: &[u8],
    client_id: u64,
    sessions: &HashMap<u64, ClientSession>,
    locked_config: &Option<[u8; RADIO_CONFIG_SIZE]>,
) -> Option<Vec<u8>> {
    let config_bytes = raw_cmd.get(1..1 + RADIO_CONFIG_SIZE)?;

    // Single client — allow free config changes (scanner mode)
    if sessions.len() <= 1 {
        return None;
    }

    // First SetConfig with multiple clients — forward and lock on success
    let Some(locked) = locked_config else {
        return None;
    };

    // Same config — reply Ok without hitting the dongle
    if config_bytes == locked.as_slice() {
        let label = sessions
            .get(&client_id)
            .map(ClientSession::label)
            .unwrap_or_else(|| format!("id-{client_id}"));
        debug!("{label}: SetConfig matches locked config — Ok");
        return Some(vec![RESP_TAG_OK]);
    }

    // Different config — reject
    let label = sessions
        .get(&client_id)
        .map(ClientSession::label)
        .unwrap_or_else(|| format!("id-{client_id}"));
    warn!("{label}: SetConfig rejected (conflicts with locked config)");
    Some(vec![RESP_TAG_ERROR, ERROR_INVALID_CONFIG])
}

fn intercept_start_rx(
    client_id: u64,
    sessions: &mut HashMap<u64, ClientSession>,
) -> Option<Vec<u8>> {
    let client = sessions.get(&client_id)?;

    // Already interested — no-op
    if client.rx_interested {
        return Some(vec![RESP_TAG_OK]);
    }

    // Others already receiving — just mark this client
    if rx_interest_count(sessions) > 0 {
        if let Some(c) = sessions.get_mut(&client_id) {
            c.rx_interested = true;
        }
        return Some(vec![RESP_TAG_OK]);
    }

    // First interested client — forward to dongle
    None
}

fn intercept_stop_rx(
    client_id: u64,
    sessions: &mut HashMap<u64, ClientSession>,
) -> Option<Vec<u8>> {
    let client = sessions.get(&client_id)?;

    // Wasn't interested — no-op
    if !client.rx_interested {
        return Some(vec![RESP_TAG_OK]);
    }

    // Mark as not interested
    if let Some(c) = sessions.get_mut(&client_id) {
        c.rx_interested = false;
    }

    // Others still interested — don't stop the dongle
    if rx_interest_count(sessions) > 0 {
        return Some(vec![RESP_TAG_OK]);
    }

    // Last interested client — forward StopRx to dongle
    None
}

/// Count how many sessions have rx_interested set.
pub fn rx_interest_count(sessions: &HashMap<u64, ClientSession>) -> usize {
    sessions.values().filter(|s| s.rx_interested).count()
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sessions(n: usize) -> HashMap<u64, ClientSession> {
        let mut map = HashMap::new();
        for _ in 0..n {
            let (session, _rx) = ClientSession::new();
            let id = session.id;
            map.insert(id, session);
        }
        map
    }

    fn first_id(sessions: &HashMap<u64, ClientSession>) -> u64 {
        *sessions.keys().min().unwrap()
    }

    fn second_id(sessions: &HashMap<u64, ClientSession>) -> u64 {
        let mut ids: Vec<u64> = sessions.keys().copied().collect();
        ids.sort();
        ids[1]
    }

    // ── SetConfig ──────────────────────────────���───────────────────

    #[test]
    fn set_config_single_client_forwards() {
        let mut sessions = make_sessions(1);
        let locked = None;
        let id = first_id(&sessions);
        let cmd = [CMD_TAG_SET_CONFIG, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(maybe_intercept(&cmd, id, &mut sessions, &locked).is_none());
    }

    #[test]
    fn set_config_first_with_multiple_forwards() {
        let mut sessions = make_sessions(2);
        let locked = None;
        let id = first_id(&sessions);
        let cmd = [CMD_TAG_SET_CONFIG, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
        assert!(maybe_intercept(&cmd, id, &mut sessions, &locked).is_none());
    }

    #[test]
    fn set_config_matching_returns_ok() {
        let mut sessions = make_sessions(2);
        let config = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
        let locked = Some(config);
        let id = first_id(&sessions);
        let mut cmd = vec![CMD_TAG_SET_CONFIG];
        cmd.extend_from_slice(&config);
        assert_eq!(maybe_intercept(&cmd, id, &mut sessions, &locked), Some(vec![RESP_TAG_OK]));
    }

    #[test]
    fn set_config_conflicting_returns_error() {
        let mut sessions = make_sessions(2);
        let locked = Some([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
        let id = first_id(&sessions);
        let mut cmd = vec![CMD_TAG_SET_CONFIG];
        cmd.extend_from_slice(&[99, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
        assert_eq!(
            maybe_intercept(&cmd, id, &mut sessions, &locked),
            Some(vec![RESP_TAG_ERROR, ERROR_INVALID_CONFIG])
        );
    }

    // ── StartRx ────────────────────────────────────────────────────

    #[test]
    fn start_rx_first_client_forwards() {
        let mut sessions = make_sessions(1);
        let locked = None;
        let id = first_id(&sessions);
        assert!(maybe_intercept(&[CMD_TAG_START_RX], id, &mut sessions, &locked).is_none());
    }

    #[test]
    fn start_rx_already_interested_returns_ok() {
        let mut sessions = make_sessions(1);
        let locked = None;
        let id = first_id(&sessions);
        sessions.get_mut(&id).unwrap().rx_interested = true;
        assert_eq!(
            maybe_intercept(&[CMD_TAG_START_RX], id, &mut sessions, &locked),
            Some(vec![RESP_TAG_OK])
        );
    }

    #[test]
    fn start_rx_others_interested_marks_and_returns_ok() {
        let mut sessions = make_sessions(2);
        let locked = None;
        let id1 = first_id(&sessions);
        let id2 = second_id(&sessions);
        sessions.get_mut(&id1).unwrap().rx_interested = true;
        let result = maybe_intercept(&[CMD_TAG_START_RX], id2, &mut sessions, &locked);
        assert_eq!(result, Some(vec![RESP_TAG_OK]));
        assert!(sessions.get(&id2).unwrap().rx_interested);
    }

    // ── StopRx ─────────────────────────────────────────────────────

    #[test]
    fn stop_rx_not_interested_returns_ok() {
        let mut sessions = make_sessions(1);
        let locked = None;
        let id = first_id(&sessions);
        assert_eq!(
            maybe_intercept(&[CMD_TAG_STOP_RX], id, &mut sessions, &locked),
            Some(vec![RESP_TAG_OK])
        );
    }

    #[test]
    fn stop_rx_others_remain_returns_ok() {
        let mut sessions = make_sessions(2);
        let locked = None;
        let id1 = first_id(&sessions);
        let id2 = second_id(&sessions);
        sessions.get_mut(&id1).unwrap().rx_interested = true;
        sessions.get_mut(&id2).unwrap().rx_interested = true;
        let result = maybe_intercept(&[CMD_TAG_STOP_RX], id1, &mut sessions, &locked);
        assert_eq!(result, Some(vec![RESP_TAG_OK]));
        assert!(!sessions.get(&id1).unwrap().rx_interested);
    }

    #[test]
    fn stop_rx_last_interested_forwards() {
        let mut sessions = make_sessions(1);
        let locked = None;
        let id = first_id(&sessions);
        sessions.get_mut(&id).unwrap().rx_interested = true;
        assert!(maybe_intercept(&[CMD_TAG_STOP_RX], id, &mut sessions, &locked).is_none());
        assert!(!sessions.get(&id).unwrap().rx_interested);
    }

    // ── Other commands ─────────────────────────────────────────────

    #[test]
    fn other_commands_not_intercepted() {
        let mut sessions = make_sessions(1);
        let locked = None;
        let id = first_id(&sessions);
        for tag in [0u8, 1, 5, 6, 7, 8] {
            assert!(maybe_intercept(&[tag], id, &mut sessions, &locked).is_none());
        }
    }
}
