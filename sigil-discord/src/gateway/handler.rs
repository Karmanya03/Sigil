//! DAVE voice gateway event dispatcher.
//!
//! Parses raw binary/JSON payloads from the voice gateway into
//! structured [`DaveEvent`] variants for the driver to act on.
//!
//! ## Opcode Transport Types
//!
//! Per Discord's Voice Opcodes Table:
//! - **JSON text**: OP 21, 22, 23, 24, 31
//! - **Binary**: OP 25, 26, 27, 28, 29, 30
//!
//! Only binary opcodes are dispatched through this module. JSON opcodes
//! (21, 22, 24) are handled directly in the driver's text message handler.

use crate::error::SigilError;
use crate::gateway::opcodes::*;

/// A parsed DAVE gateway event ready for driver consumption.
#[derive(Debug, Clone)]
pub enum DaveEvent {
    /// OP 25: External sender credential (binary).
    MlsExternalSender(MlsExternalSenderPayload),
    /// OP 27: One or more proposals (binary). Each inner Vec<u8> is one
    ///        complete TLS-serialized MLS message.
    ///
    /// `transition_id` must be echoed back in the OP 28 CommitWelcome
    /// response so Discord can correlate the commit with the proposal batch.
    MlsProposals {
        /// The transition ID from the OP 27 payload — echo in OP 28.
        transition_id: u64,
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
/// Only binary opcodes (25, 27, 29, 30) are handled here. JSON opcodes
/// (21, 22, 24) are handled in the driver's text message handler.
///
/// # Errors
///
/// Returns [`SigilError::UnknownOpcode`] for unrecognized opcodes, or
/// [`SigilError::Mls`] for malformed payloads.
pub fn dispatch(opcode: u8, payload: &[u8]) -> Result<DaveEvent, SigilError> {
    match opcode {
        25 => parse_mls_external_sender(payload),
        27 => parse_mls_proposals(payload),
        29 => parse_mls_announce_commit(payload),
        30 => parse_mls_welcome(payload),
        _ => Err(SigilError::UnknownOpcode(opcode)),
    }
}

// ── OP 25: MlsExternalSender (binary) ──────────────────────────────────────

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

// ── OP 27: MlsProposals (binary) ───────────────────────────────────────────
//
// Per the DAVE protocol, the binary format after the common 3-byte header is:
//
//   struct {
//     uint16 sequence_number;          // ← common header, stripped
//     uint8  opcode = 27;              // ← common header, stripped
//     uint16 transition_id;            // MUST be echoed in OP 28 response
//     ProposalsOperationType operation_type;   // 0 = append, 1 = revoke
//     select (operation_type) {
//       case append: MLSMessage proposal_messages<V>;
//       case revoke: ProposalRef proposal_refs<V>;
//     }
//   }
//
// The transition_id is little-endian u16, consistent with OP 29/30.
// Discord requires OP 28 to carry the same transition_id so it can
// match the commit to the proposal batch. Without it Discord closes
// the voice WS with code 4005.
//
// IMPORTANT: The two transition_id bytes immediately follow the header.
// Previous parsing consumed data[0] as operation_type, which meant
// we fed the first byte of transition_id + all remaining bytes as the
// MLS message — causing tls_deserialize to fail with UnknownValue because
// the MLS message was offset by 2 bytes.

fn parse_mls_proposals(payload: &[u8]) -> Result<DaveEvent, SigilError> {
    let data = skip_header_if_present(payload, 27);

    // Need at least: transition_id(2) + operation_type(1) = 3 bytes
    if data.len() < 3 {
        return Err(SigilError::Mls(format!(
            "MlsProposals payload too short: {} bytes (need ≥3)",
            data.len()
        )));
    }

    // transition_id: u16 little-endian — echo back in OP 28
    let transition_id = u16::from_le_bytes([data[0], data[1]]) as u64;

    // operation_type: 0 = append, 1 = revoke
    let operation_type = data[2];

    // Remaining bytes are the actual MLS message(s)
    let proposal_blob = &data[3..];

    let proposals = if proposal_blob.is_empty() {
        Vec::new()
    } else {
        vec![proposal_blob.to_vec()]
    };

    Ok(DaveEvent::MlsProposals {
        transition_id,
        operation_type,
        proposals,
    })
}

// ── OP 29: MlsAnnounceCommitTransition (binary) ────────────────────────────
//
// Per the DAVE protocol whitepaper:
//
//   struct {
//     uint16 sequence_number;
//     uint8 opcode = 29;
//     uint16 transition_id;          // ← uint16, NOT uint32!
//     MLSMessage commit_message;
//   }

fn parse_mls_announce_commit(payload: &[u8]) -> Result<DaveEvent, SigilError> {
    let data = skip_header_if_present(payload, 29);

    // Need at least 2 bytes for transition_id
    if data.len() < 2 {
        return Err(SigilError::Mls(format!(
            "AnnounceCommitTransition payload too short: {} bytes",
            data.len()
        )));
    }

    // transition_id is uint16 little-endian (2 bytes)
    let transition_id = u16::from_le_bytes([data[0], data[1]]) as u64;
    let commit_bytes = data[2..].to_vec();

    if commit_bytes.is_empty() {
        return Err(SigilError::Mls(
            "AnnounceCommitTransition has empty commit data".to_string(),
        ));
    }

    Ok(DaveEvent::MlsAnnounceCommitTransition(
        MlsAnnounceCommitTransition {
            transition_id,
            commit_bytes,
        },
    ))
}

// ── OP 30: MlsWelcome (binary) ─────────────────────────────────────────────
//
// Per the DAVE protocol whitepaper:
//
//   struct {
//     uint16 sequence_number;
//     uint8 opcode = 30;
//     uint16 transition_id;          // ← uint16, NOT uint32!
//     Welcome welcome_message;
//   }

fn parse_mls_welcome(payload: &[u8]) -> Result<DaveEvent, SigilError> {
    let data = skip_header_if_present(payload, 30);

    // Need at least 2 bytes for transition_id
    if data.len() < 2 {
        return Err(SigilError::Mls(format!(
            "MlsWelcome payload too short: {} bytes",
            data.len()
        )));
    }

    // transition_id is uint16 little-endian (2 bytes)
    let transition_id = u16::from_le_bytes([data[0], data[1]]) as u64;
    let welcome_bytes = data[2..].to_vec();

    if welcome_bytes.is_empty() {
        return Err(SigilError::Mls(
            "MlsWelcome has empty welcome data".to_string(),
        ));
    }

    Ok(DaveEvent::MlsWelcome(MlsWelcomePayload {
        transition_id,
        welcome_bytes,
    }))
}

// ── Helpers ─────────────────────────────────────────────────────────────────

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
