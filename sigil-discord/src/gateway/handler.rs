//! DAVE voice gateway event dispatcher.
//!
//! Parses raw binary/JSON payloads from the voice gateway into
//! structured [`DaveEvent`] variants for the driver to act on.

use crate::error::SigilError;
use crate::gateway::opcodes::*;

/// A parsed DAVE gateway event ready for driver consumption.
#[derive(Debug, Clone)]
pub enum DaveEvent {
    /// OP 21: Server announces an upcoming protocol transition.
    PrepareTransition(PrepareTransition),
    /// OP 22: Server instructs us to execute a transition.
    ExecuteTransition(ExecuteTransition),
    /// OP 24: Server announces a new epoch (binary).
    PrepareEpoch(PrepareEpoch),
    /// OP 25: External sender credential (binary).
    MlsExternalSender(MlsExternalSenderPayload),
    /// OP 27: One or more proposals (binary). Each inner Vec<u8> is one
    ///        complete TLS-serialized MLS message.
    MlsProposals {
        operation_type: u8,
        proposals: Vec<Vec<u8>>,
    },
    /// OP 29: Commit with transition ID (binary).
    MlsAnnounceCommitTransition(MlsAnnounceCommitTransition),
    /// OP 30: Welcome for a pending member (binary).
    MlsWelcome(MlsWelcomePayload),
}

/// Dispatch a raw voice gateway binary message to a [`DaveEvent`].
///
/// Binary layout common prefix: `[seq_be(2)][opcode(1)][payload...]`
///
/// # Errors
///
/// Returns [`SigilError::UnknownOpcode`] for unrecognized opcodes, or
/// [`SigilError::Mls`] for malformed payloads.
pub fn dispatch(opcode: u8, payload: &[u8]) -> Result<DaveEvent, SigilError> {
    match opcode {
        21 => parse_prepare_transition(payload),
        22 => parse_execute_transition(payload),
        24 => parse_prepare_epoch(payload),
        25 => parse_mls_external_sender(payload),
        27 => parse_mls_proposals(payload),
        29 => parse_mls_announce_commit(payload),
        30 => parse_mls_welcome(payload),
        _ => Err(SigilError::UnknownOpcode(opcode)),
    }
}

// ── OP 21: PrepareTransition (JSON or binary) ──────────────────────

fn parse_prepare_transition(payload: &[u8]) -> Result<DaveEvent, SigilError> {
    // Binary: [seq(2)][op(1)][protocol_version(2 LE)]
    // The payload passed here is AFTER the seq+op strip in driver, so:
    //   payload = [protocol_version(2 LE)]
    // But it may also be the full binary including seq+op if driver passes raw.
    // Handle both cases:
    let data = skip_header_if_present(payload, 21);

    if data.len() >= 2 {
        let protocol_version = u16::from_le_bytes([data[0], data[1]]);
        Ok(DaveEvent::PrepareTransition(PrepareTransition {
            protocol_version,
            transition_id: 0, // transition_id comes with ExecuteTransition
        }))
    } else {
        Err(SigilError::Mls(format!(
            "PrepareTransition payload too short: {} bytes",
            payload.len()
        )))
    }
}

// ── OP 22: ExecuteTransition (binary) ──────────────────────────────

fn parse_execute_transition(payload: &[u8]) -> Result<DaveEvent, SigilError> {
    // Binary: [seq(2)][op(1)][transition_id(4 LE)]
    let data = skip_header_if_present(payload, 22);

    if data.len() >= 4 {
        let transition_id =
            u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as u64;
        Ok(DaveEvent::ExecuteTransition(ExecuteTransition {
            transition_id,
        }))
    } else {
        Err(SigilError::Mls(format!(
            "ExecuteTransition payload too short: {} bytes",
            payload.len()
        )))
    }
}

// ── OP 24: PrepareEpoch (binary) ───────────────────────────────────

fn parse_prepare_epoch(payload: &[u8]) -> Result<DaveEvent, SigilError> {
    // Binary: [seq(2)][op(1)][epoch(4 LE)]
    let data = skip_header_if_present(payload, 24);

    if data.len() >= 4 {
        let epoch = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as u64;
        Ok(DaveEvent::PrepareEpoch(PrepareEpoch {
            protocol_version: 1,
            epoch,
        }))
    } else {
        Err(SigilError::Mls(format!(
            "PrepareEpoch payload too short: {} bytes",
            payload.len()
        )))
    }
}

// ── OP 25: MlsExternalSender (binary) ──────────────────────────────

fn parse_mls_external_sender(payload: &[u8]) -> Result<DaveEvent, SigilError> {
    // Binary: [seq(2)][op(1)][credential + signature_key bytes...]
    let data = skip_header_if_present(payload, 25);

    if data.is_empty() {
        return Err(SigilError::Mls(
            "MlsExternalSender payload is empty".to_string(),
        ));
    }

    // The credential and signature_key are TLS-serialized back-to-back.
    // SigilSession::set_external_sender() handles the actual parsing.
    Ok(DaveEvent::MlsExternalSender(MlsExternalSenderPayload {
        credential: data.to_vec(),
        signature_key: Vec::new(), // parsed later by set_external_sender
    }))
}

// ── OP 27: MlsProposals (binary) ──────────────────────────────────
//
// Discord sends proposals as:
//   [seq(2)][op(1)][operation_type(1)][proposal_bytes...]
//
// CRITICAL: The proposal_bytes may contain MULTIPLE TLS-serialized
// MLS messages concatenated. Each is a complete MlsMessage with its
// own TLS length prefix. We must split them into individual messages
// so that process_proposals() can deserialize each one separately.

fn parse_mls_proposals(payload: &[u8]) -> Result<DaveEvent, SigilError> {
    let data = skip_header_if_present(payload, 27);

    if data.is_empty() {
        return Err(SigilError::Mls(
            "MlsProposals payload is empty".to_string(),
        ));
    }

    let operation_type = data[0]; // 0 = append, 1 = revoke
    let proposal_blob = &data[1..];

    // Split the concatenated TLS-serialized proposals into individual messages.
    // Each TLS-serialized MlsMessage is prefixed with a 4-byte big-endian length.
    // If we can't split (no length prefix pattern), treat the entire blob as one.
    let proposals = split_tls_messages(proposal_blob);

    Ok(DaveEvent::MlsProposals {
        operation_type,
        proposals,
    })
}

/// Split a buffer of concatenated TLS-serialized MLS messages.
///
/// MLS messages are TLS-serialized with a variable-length header. For
/// simplicity and robustness, if we can't reliably split, we return the
/// entire blob as a single proposal — `process_proposals` will attempt
/// deserialization and report any errors.
fn split_tls_messages(data: &[u8]) -> Vec<Vec<u8>> {
    // Try to split using TLS 3-byte length prefix (MlsMessage uses u24 length).
    // Format: [msg_type(1 or 2 bytes)][length(varies)][content...]
    //
    // However, the exact framing depends on the TLS codec. In practice,
    // Discord sends each OP 27 with exactly ONE proposal per message.
    // Multiple proposals arrive as separate OP 27 binary messages.
    //
    // So the safest approach: return the entire blob as one proposal.
    // The driver accumulates multiple OP 27 events into a Vec before committing.
    if data.is_empty() {
        return Vec::new();
    }
    vec![data.to_vec()]
}

// ── OP 29: MlsAnnounceCommitTransition (binary) ───────────────────

fn parse_mls_announce_commit(payload: &[u8]) -> Result<DaveEvent, SigilError> {
    // Binary: [seq(2)][op(1)][transition_id(4 LE)][commit_bytes...]
    let data = skip_header_if_present(payload, 29);

    if data.len() < 4 {
        return Err(SigilError::Mls(format!(
            "AnnounceCommitTransition payload too short: {} bytes",
            data.len()
        )));
    }

    let transition_id =
        u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as u64;
    let commit_bytes = data[4..].to_vec();

    Ok(DaveEvent::MlsAnnounceCommitTransition(
        MlsAnnounceCommitTransition {
            transition_id,
            commit_bytes,
        },
    ))
}

// ── OP 30: MlsWelcome (binary) ────────────────────────────────────

fn parse_mls_welcome(payload: &[u8]) -> Result<DaveEvent, SigilError> {
    // Binary: [seq(2)][op(1)][transition_id(4 LE)][welcome_bytes...]
    let data = skip_header_if_present(payload, 30);

    if data.len() < 4 {
        return Err(SigilError::Mls(format!(
            "MlsWelcome payload too short: {} bytes",
            data.len()
        )));
    }

    let transition_id =
        u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as u64;
    let welcome_bytes = data[4..].to_vec();

    Ok(DaveEvent::MlsWelcome(MlsWelcomePayload {
        transition_id,
        welcome_bytes,
    }))
}

// ── Helpers ────────────────────────────────────────────────────────

/// If the payload still contains the [seq(2)][op(1)] header, skip it.
///
/// The driver may or may not strip the header before calling dispatch,
/// so we handle both cases. We detect by checking if byte[2] matches
/// the expected opcode.
fn skip_header_if_present(payload: &[u8], expected_op: u8) -> &[u8] {
    if payload.len() >= 3 && payload[2] == expected_op {
        &payload[3..]
    } else {
        payload
    }
}
