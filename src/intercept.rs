//! Mux-level command interception (spec §13.2 / §13.4).
//!
//! When multiple clients share a single dongle, certain commands must be
//! handled by the mux rather than blindly forwarded. The interesting
//! cases:
//!
//! - **`SET_CONFIG`**: the first successful call from any client
//!   "locks" the config. Subsequent calls from that same client pass
//!   through; calls from *other* clients receive a synthesized
//!   `OK(SetConfigResult)` — `ALREADY_MATCHED` if params byte-match the
//!   lock, `LOCKED_MISMATCH` otherwise — so other clients never clobber
//!   the lock owner's configuration.
//! - **`RX_START` / `RX_STOP`**: reference-counted. The mux keeps RX
//!   running until the last interested client stops.

use std::collections::HashMap;

use donglora_protocol::{
    Command, Modulation, OkPayload, Owner, SetConfigResult, SetConfigResultCode,
    events::{TYPE_ERR, TYPE_OK},
};
use tracing::debug;

use crate::session::{ClientId, ClientSession};

/// What to do with a client command.
///
/// - [`Decision::Forward`] — pass it through to the dongle (after the
///   caller rewrites the tag to a mux-allocated device tag).
/// - [`Decision::Synthesize`] — reply directly to the client with the
///   attached `(type_id, payload_bytes)`; do not touch the dongle.
/// - [`Decision::Drop`] — silently swallow (reserved for corrupt input).
#[derive(Debug, Clone)]
pub enum Decision {
    Forward,
    Synthesize {
        type_id: u8,
        payload: Vec<u8>,
    },
    #[allow(dead_code)] // reserved for future shape-rejection paths
    Drop,
}

/// Mux-level state tracked across all client sessions.
pub struct MuxState {
    /// Active config lock. When `Some`, identifies the client that most
    /// recently applied `SET_CONFIG` and the exact modulation they set.
    pub locked: Option<(ClientId, Modulation)>,
}

impl MuxState {
    pub fn new() -> Self {
        Self { locked: None }
    }

    /// Number of clients that have called `RX_START`.
    pub fn rx_interest_count(sessions: &HashMap<ClientId, ClientSession>) -> usize {
        sessions.values().filter(|s| s.rx_interested).count()
    }
}

/// Decide what to do with a decoded command.
///
/// `type_id` + `payload` together come from the FrameDecoder on the
/// client's incoming frame (pre-tag-rewrite). The returned `Decision`
/// says whether the mux should absorb it locally or forward it to the
/// dongle.
pub fn decide(
    type_id: u8,
    payload: &[u8],
    client_id: ClientId,
    state: &mut MuxState,
    sessions: &mut HashMap<ClientId, ClientSession>,
) -> Decision {
    // Parse the command — if it's unparseable, let it through and let
    // the firmware return ERR(EUNKNOWN_CMD) with the echoed tag. Keeps
    // the mux forward-compatible with minor-version command additions.
    let Ok(cmd) = Command::parse(type_id, payload) else {
        return Decision::Forward;
    };

    match cmd {
        Command::SetConfig(modulation) => decide_set_config(modulation, client_id, state, sessions),
        Command::RxStart => decide_rx_start(client_id, sessions),
        Command::RxStop => decide_rx_stop(client_id, sessions),
        _ => Decision::Forward,
    }
}

fn decide_set_config(
    modulation: Modulation,
    client_id: ClientId,
    state: &mut MuxState,
    sessions: &HashMap<ClientId, ClientSession>,
) -> Decision {
    // Single client, no lock: unconditionally forward.
    if sessions.len() <= 1 {
        return Decision::Forward;
    }

    match &state.locked {
        None => {
            // First SetConfig with multiple clients — forward; the writer
            // will install the lock when APPLIED comes back.
            Decision::Forward
        }
        Some((owner, current)) if *owner == client_id => {
            // The lock owner is reconfiguring — forward.
            let _ = current;
            Decision::Forward
        }
        Some((_, current)) => {
            // Another client holds the lock. Synthesize a response.
            let matches = modulations_match(&modulation, current);
            let code = if matches { SetConfigResultCode::AlreadyMatched } else { SetConfigResultCode::LockedMismatch };
            let result = SetConfigResult { result: code, owner: Owner::Other, current: *current };
            let mut buf = [0u8; donglora_protocol::MAX_SETCONFIG_OK_PAYLOAD];
            let n = match OkPayload::SetConfig(result).encode(&mut buf) {
                Ok(n) => n,
                Err(_) => return Decision::Forward, // unreachable in practice
            };
            let label = sessions.get(&client_id).map_or_else(|| format!("id-{client_id}"), ClientSession::label);
            debug!("{label}: SetConfig {:?} (owner=OTHER) — synthesized", code);
            Decision::Synthesize { type_id: TYPE_OK, payload: buf[..n].to_vec() }
        }
    }
}

fn decide_rx_start(client_id: ClientId, sessions: &mut HashMap<ClientId, ClientSession>) -> Decision {
    let Some(client) = sessions.get(&client_id) else {
        return Decision::Forward;
    };

    if client.rx_interested {
        // Already in RX — synthesize empty OK.
        return Decision::Synthesize { type_id: TYPE_OK, payload: vec![] };
    }

    if MuxState::rx_interest_count(sessions) > 0 {
        // Others already have RX running on the dongle — just mark this
        // client as interested and synthesize OK.
        if let Some(c) = sessions.get_mut(&client_id) {
            c.rx_interested = true;
        }
        return Decision::Synthesize { type_id: TYPE_OK, payload: vec![] };
    }

    // First interested client — actually turn RX on.
    Decision::Forward
}

fn decide_rx_stop(client_id: ClientId, sessions: &mut HashMap<ClientId, ClientSession>) -> Decision {
    let Some(client) = sessions.get(&client_id) else {
        return Decision::Forward;
    };
    if !client.rx_interested {
        // Wasn't listening — nothing to do.
        return Decision::Synthesize { type_id: TYPE_OK, payload: vec![] };
    }
    if let Some(c) = sessions.get_mut(&client_id) {
        c.rx_interested = false;
    }
    if MuxState::rx_interest_count(sessions) > 0 {
        // Others still interested — don't shut RX off.
        return Decision::Synthesize { type_id: TYPE_OK, payload: vec![] };
    }
    Decision::Forward
}

fn modulations_match(a: &Modulation, b: &Modulation) -> bool {
    // Byte-wise comparison via encode (spec definition of "matches" is
    // "same params on the wire").
    let mut ba = [0u8; donglora_protocol::MAX_SETCONFIG_OK_PAYLOAD];
    let mut bb = [0u8; donglora_protocol::MAX_SETCONFIG_OK_PAYLOAD];
    let na = match a.encode(&mut ba) {
        Ok(n) => n,
        Err(_) => return false,
    };
    let nb = match b.encode(&mut bb) {
        Ok(n) => n,
        Err(_) => return false,
    };
    ba[..na] == bb[..nb]
}

/// Synthesize an `ERR(code)` payload.
#[must_use]
pub fn synthesize_err(code: donglora_protocol::ErrorCode) -> (u8, Vec<u8>) {
    let mut buf = [0u8; 2];
    // This never fails: buf is large enough.
    let _ = donglora_protocol::events::encode_err_payload(code, &mut buf);
    (TYPE_ERR, buf.to_vec())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use donglora_protocol::{LoRaBandwidth, LoRaCodingRate, LoRaConfig, LoRaHeaderMode};

    fn lora(freq: u32) -> Modulation {
        Modulation::LoRa(LoRaConfig {
            freq_hz: freq,
            sf: 7,
            bw: LoRaBandwidth::Khz62,
            cr: LoRaCodingRate::Cr4_5,
            preamble_len: 16,
            sync_word: 0x1424,
            tx_power_dbm: 20,
            header_mode: LoRaHeaderMode::Explicit,
            payload_crc: true,
            iq_invert: false,
        })
    }

    fn make_sessions(n: usize) -> HashMap<ClientId, ClientSession> {
        let mut map = HashMap::new();
        for _ in 0..n {
            let (session, _rx) = ClientSession::new();
            map.insert(session.id, session);
        }
        map
    }

    fn first_id(sessions: &HashMap<ClientId, ClientSession>) -> ClientId {
        *sessions.keys().min().unwrap()
    }
    fn second_id(sessions: &HashMap<ClientId, ClientSession>) -> ClientId {
        let mut ids: Vec<_> = sessions.keys().copied().collect();
        ids.sort_unstable();
        ids[1]
    }

    fn encode_set_config(modulation: Modulation) -> Vec<u8> {
        let mut buf = [0u8; 64];
        let n = Command::SetConfig(modulation).encode_payload(&mut buf).unwrap();
        buf[..n].to_vec()
    }

    #[test]
    fn set_config_single_client_forwards() {
        let mut sessions = make_sessions(1);
        let mut state = MuxState::new();
        let id = first_id(&sessions);
        let payload = encode_set_config(lora(910_525_000));
        assert!(matches!(
            decide(donglora_protocol::commands::TYPE_SET_CONFIG, &payload, id, &mut state, &mut sessions),
            Decision::Forward
        ));
    }

    #[test]
    fn set_config_first_multi_client_forwards() {
        let mut sessions = make_sessions(2);
        let mut state = MuxState::new();
        let id = first_id(&sessions);
        let payload = encode_set_config(lora(910_525_000));
        assert!(matches!(
            decide(donglora_protocol::commands::TYPE_SET_CONFIG, &payload, id, &mut state, &mut sessions),
            Decision::Forward
        ));
    }

    #[test]
    fn set_config_matching_synthesizes_already_matched() {
        let mut sessions = make_sessions(2);
        let id_a = first_id(&sessions);
        let id_b = second_id(&sessions);
        let mut state = MuxState::new();
        state.locked = Some((id_a, lora(910_525_000)));
        let payload = encode_set_config(lora(910_525_000));
        match decide(donglora_protocol::commands::TYPE_SET_CONFIG, &payload, id_b, &mut state, &mut sessions) {
            Decision::Synthesize { type_id, payload } => {
                assert_eq!(type_id, TYPE_OK);
                let parsed = OkPayload::parse_for(donglora_protocol::commands::TYPE_SET_CONFIG, &payload).unwrap();
                let OkPayload::SetConfig(result) = parsed else { panic!("expected SetConfig") };
                assert_eq!(result.result, SetConfigResultCode::AlreadyMatched);
                assert_eq!(result.owner, Owner::Other);
            }
            other => panic!("unexpected decision: {other:?}"),
        }
    }

    #[test]
    fn set_config_conflict_synthesizes_locked_mismatch() {
        let mut sessions = make_sessions(2);
        let id_a = first_id(&sessions);
        let id_b = second_id(&sessions);
        let mut state = MuxState::new();
        state.locked = Some((id_a, lora(910_525_000)));
        let payload = encode_set_config(lora(915_000_000));
        match decide(donglora_protocol::commands::TYPE_SET_CONFIG, &payload, id_b, &mut state, &mut sessions) {
            Decision::Synthesize { type_id, payload } => {
                assert_eq!(type_id, TYPE_OK);
                let parsed = OkPayload::parse_for(donglora_protocol::commands::TYPE_SET_CONFIG, &payload).unwrap();
                let OkPayload::SetConfig(result) = parsed else { panic!("expected SetConfig") };
                assert_eq!(result.result, SetConfigResultCode::LockedMismatch);
                assert_eq!(result.owner, Owner::Other);
                // `current` should reflect the locked modulation (910.525 MHz).
                if let Modulation::LoRa(c) = result.current {
                    assert_eq!(c.freq_hz, 910_525_000);
                } else {
                    panic!("expected LoRa");
                }
            }
            other => panic!("unexpected decision: {other:?}"),
        }
    }

    #[test]
    fn set_config_owner_reconfig_forwards() {
        let mut sessions = make_sessions(2);
        let id_a = first_id(&sessions);
        let mut state = MuxState::new();
        state.locked = Some((id_a, lora(910_525_000)));
        let payload = encode_set_config(lora(915_000_000));
        assert!(matches!(
            decide(donglora_protocol::commands::TYPE_SET_CONFIG, &payload, id_a, &mut state, &mut sessions),
            Decision::Forward
        ));
    }

    #[test]
    fn rx_start_first_forwards() {
        let mut sessions = make_sessions(1);
        let mut state = MuxState::new();
        let id = first_id(&sessions);
        assert!(matches!(
            decide(donglora_protocol::commands::TYPE_RX_START, &[], id, &mut state, &mut sessions),
            Decision::Forward
        ));
    }

    #[test]
    fn rx_start_when_others_listening_synthesizes_ok() {
        let mut sessions = make_sessions(2);
        let mut state = MuxState::new();
        let id_a = first_id(&sessions);
        let id_b = second_id(&sessions);
        sessions.get_mut(&id_a).unwrap().rx_interested = true;
        let res = decide(donglora_protocol::commands::TYPE_RX_START, &[], id_b, &mut state, &mut sessions);
        assert!(matches!(res, Decision::Synthesize { type_id: TYPE_OK, .. }));
        assert!(sessions.get(&id_b).unwrap().rx_interested);
    }

    #[test]
    fn rx_start_already_interested_synthesizes_ok() {
        let mut sessions = make_sessions(1);
        let mut state = MuxState::new();
        let id = first_id(&sessions);
        sessions.get_mut(&id).unwrap().rx_interested = true;
        let res = decide(donglora_protocol::commands::TYPE_RX_START, &[], id, &mut state, &mut sessions);
        assert!(matches!(res, Decision::Synthesize { type_id: TYPE_OK, .. }));
    }

    #[test]
    fn rx_stop_not_interested_synthesizes_ok() {
        let mut sessions = make_sessions(1);
        let mut state = MuxState::new();
        let id = first_id(&sessions);
        let res = decide(donglora_protocol::commands::TYPE_RX_STOP, &[], id, &mut state, &mut sessions);
        assert!(matches!(res, Decision::Synthesize { type_id: TYPE_OK, .. }));
    }

    #[test]
    fn rx_stop_with_others_listening_synthesizes_ok_and_clears() {
        let mut sessions = make_sessions(2);
        let mut state = MuxState::new();
        let id_a = first_id(&sessions);
        let id_b = second_id(&sessions);
        sessions.get_mut(&id_a).unwrap().rx_interested = true;
        sessions.get_mut(&id_b).unwrap().rx_interested = true;
        let res = decide(donglora_protocol::commands::TYPE_RX_STOP, &[], id_a, &mut state, &mut sessions);
        assert!(matches!(res, Decision::Synthesize { type_id: TYPE_OK, .. }));
        assert!(!sessions.get(&id_a).unwrap().rx_interested);
        assert!(sessions.get(&id_b).unwrap().rx_interested);
    }

    #[test]
    fn rx_stop_last_forwards_and_clears() {
        let mut sessions = make_sessions(1);
        let mut state = MuxState::new();
        let id = first_id(&sessions);
        sessions.get_mut(&id).unwrap().rx_interested = true;
        let res = decide(donglora_protocol::commands::TYPE_RX_STOP, &[], id, &mut state, &mut sessions);
        assert!(matches!(res, Decision::Forward));
        assert!(!sessions.get(&id).unwrap().rx_interested);
    }

    #[test]
    fn other_commands_forward() {
        let mut sessions = make_sessions(1);
        let mut state = MuxState::new();
        let id = first_id(&sessions);
        // PING, GET_INFO forward unchanged.
        assert!(matches!(
            decide(donglora_protocol::commands::TYPE_PING, &[], id, &mut state, &mut sessions),
            Decision::Forward
        ));
        assert!(matches!(
            decide(donglora_protocol::commands::TYPE_GET_INFO, &[], id, &mut state, &mut sessions),
            Decision::Forward
        ));
    }

    #[test]
    fn synthesize_err_encodes_wire_value() {
        let (type_id, payload) = synthesize_err(donglora_protocol::ErrorCode::EBusy);
        assert_eq!(type_id, TYPE_ERR);
        let code = donglora_protocol::events::decode_err_payload(&payload).unwrap();
        assert_eq!(code, donglora_protocol::ErrorCode::EBusy);
    }
}
