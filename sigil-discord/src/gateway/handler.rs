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
// Per the DAVE protocol whitepaper, the binary format is:
//
//   struct {
//     uint16 sequence_number;
//     uint8 opcode = 27;
//     ProposalsOperationType operation_type;
//     select (operation_type) {
//       case append: MLSMessage proposal_messages<V>;
//       case revoke: ProposalRef proposal_refs<V>;
//     }
//   }
//
// Discord sends one OP 27 per proposal, or may batch by sending
// multiple OP 27 messages. Each OP 27 contains one complete
// TLS-serialized MLS message after the operation_type byte.

fn parse_mls_proposals(payload: &[u8]) -> Result<DaveEvent, SigilError> {
    let data = skip_header_if_present(payload, 27);

    if data.is_empty() {
        return Err(SigilError::Mls(
            "MlsProposals payload is empty".to_string(),
        ));
    }

    let operation_type = data[0]; // 0 = append, 1 = revoke
    let proposal_blob = &data[1..];

    // Each OP 27 message contains one complete proposal.
    // Multiple proposals arrive as separate OP 27 binary messages.
    // The driver accumulates them before committing.
    let proposals = if proposal_blob.is_empty() {
        Vec::new()
    } else {
        vec![proposal_blob.to_vec()]
    };

    Ok(DaveEvent::MlsProposals {
        operation_type,
        proposals,
    })
}

// ── OP 29: MlsAnnounceCommitTransition (binary) ───────────────────
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

// ── OP 30: MlsWelcome (binary) ────────────────────────────────────
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
