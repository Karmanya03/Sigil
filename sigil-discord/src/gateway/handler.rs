//! Opcode dispatch to high-level [`DaveEvent`] variants.
//!
//! Deserializes raw gateway payloads into structured events for the
//! session state machine to process.

use tracing::debug;

use crate::error::SigilError;
use crate::gateway::opcodes::*;

/// High-level events produced by dispatching raw DAVE gateway opcodes.
#[derive(Debug)]
pub enum DaveEvent {
    /// Server requests preparation for a protocol transition.
    PrepareTransition(PrepareTransition),
    /// Server signals execution of a previously announced transition.
    ExecuteTransition(ExecuteTransition),
    /// Server prepares a new epoch.
    PrepareEpoch(PrepareEpoch),
    /// Server sends the external sender credential+key for MLS group.
    MlsExternalSender(MlsExternalSenderPayload),
    /// Server sends MLS proposals to append or revoke.
    MlsProposals(MlsProposalsPayload),
    /// Server announces a commit transition with commit bytes.
    MlsAnnounceCommitTransition(MlsAnnounceCommitTransition),
    /// Server sends a Welcome message for the pending member.
    MlsWelcome(MlsWelcomePayload),
}

/// Dispatch a raw gateway opcode + payload into a [`DaveEvent`].
///
/// Only server→client opcodes are dispatched. Client→server opcodes
/// (ReadyForTransition, MlsKeyPackage, MlsCommitWelcome,
/// MlsInvalidCommitWelcome) are not expected to be received.
///
/// # Arguments
///
/// * `opcode` — raw opcode byte
/// * `payload` — raw payload bytes (JSON or binary)
///
/// # Errors
///
/// - [`SigilError::UnknownOpcode`] if the opcode is not recognized
/// - [`SigilError::Mls`] if JSON deserialization fails
pub fn dispatch(opcode: u8, payload: &[u8]) -> Result<DaveEvent, SigilError> {
    let op = DaveOpcode::from_u8(opcode).ok_or(SigilError::UnknownOpcode(opcode))?;

    debug!(?op, payload_len = payload.len(), "dispatching DAVE opcode");

    match op {
        // ─── Fix OP 21 (PrepareTransition): binary, not JSON ───
        DaveOpcode::PrepareTransition => {
            // Format: [seq(2)][op(1)][transition_id(4)][protocol_version(1)]
            if payload.len() >= 7 {
                let transition_id = u32::from_be_bytes([payload[3], payload[4], payload[5], payload[6]]);
                let protocol_version = if payload.len() > 7 { payload[7] as u16 } else { 0 };
                Ok(DaveEvent::PrepareTransition(PrepareTransition { 
                    transition_id: transition_id as u64, 
                    protocol_version,
                }))
            } else {
                Err(SigilError::Mls("PrepareTransition too short".into()))
            }
        }

        DaveOpcode::ExecuteTransition => {
            let data: ExecuteTransition = serde_json::from_slice(payload)
                .map_err(|e| SigilError::Mls(format!("ExecuteTransition deserialize: {}", e)))?;
            Ok(DaveEvent::ExecuteTransition(data))
        }

        // ─── Fix OP 24 (PrepareEpoch): binary, not JSON ───
        DaveOpcode::PrepareEpoch => {
            // Format: [seq(2)][op(1)][epoch(4 bytes LE)]
            if payload.len() >= 7 {
                let epoch = u32::from_le_bytes([payload[3], payload[4], payload[5], payload[6]]);
                Ok(DaveEvent::PrepareEpoch(PrepareEpoch { 
                    epoch: epoch as u64,
                    protocol_version: 1, // Default protocol version for binary format
                }))
            } else {
                Err(SigilError::Mls("PrepareEpoch too short".into()))
            }
        }

        // ─── Fix OP 25 (MlsExternalSender): skip 3, not 11 ───
        DaveOpcode::MlsExternalSender => {
            // Format: [seq(2)][op(1)][credential_bytes] — NO transition_id
            let data = if payload.len() > 3 {
                &payload[3..]     // ← was [11..], now correctly [3..]
            } else {
                payload
            };
            Ok(DaveEvent::MlsExternalSender(MlsExternalSenderPayload {
                credential: data.to_vec(),
                signature_key: Vec::new(),
            }))
        }

        // ─── Fix OP 27 (MlsProposals): skip 3, not 11 ───
        DaveOpcode::MlsProposals => {
            // Binary: seq(2) + opcode(1) + type(1) + proposal
            let data = if payload.len() > 3 {
                &payload[3..]     // ← was [11..], now correctly [3..]
            } else {
                payload
            };
            let operation_type = if data.is_empty() { 0 } else { data[0] };
            let proposal_data = if data.len() > 1 {
                data[1..].to_vec()
            } else {
                Vec::new()
            };
            Ok(DaveEvent::MlsProposals(MlsProposalsPayload {
                operation_type,
                data: proposal_data,
            }))
        }

        DaveOpcode::MlsAnnounceCommitTransition => {
            // Binary: seq(2) + opcode(1) + transition_id(8) + commit
            let (tid, data) = if payload.len() >= 11 {
                let tid = u64::from_be_bytes([
                    payload[3], payload[4], payload[5], payload[6],
                    payload[7], payload[8], payload[9], payload[10],
                ]);
                (tid, &payload[11..])
            } else {
                (0, payload)
            };
            Ok(DaveEvent::MlsAnnounceCommitTransition(
                MlsAnnounceCommitTransition {
                    transition_id: tid,
                    commit_bytes: data.to_vec(),
                },
            ))
        }

        DaveOpcode::MlsWelcome => {
            // Binary: seq(2) + opcode(1) + transition_id(8) + welcome
            let (tid, data) = if payload.len() >= 11 {
                let tid = u64::from_be_bytes([
                    payload[3], payload[4], payload[5], payload[6],
                    payload[7], payload[8], payload[9], payload[10],
                ]);
                (tid, &payload[11..])
            } else {
                (0, payload)
            };
            Ok(DaveEvent::MlsWelcome(MlsWelcomePayload {
                transition_id: tid,
                welcome_bytes: data.to_vec(),
            }))
        }

        // Client → Server opcodes: not expected to be received
        DaveOpcode::ReadyForTransition
        | DaveOpcode::MlsKeyPackage
        | DaveOpcode::MlsCommitWelcome
        | DaveOpcode::MlsInvalidCommitWelcome => Err(SigilError::InvalidState(format!(
            "received client-to-server opcode {:?} on dispatch path",
            op
        ))),
    }
}
