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
        DaveOpcode::PrepareTransition => {
            let data: PrepareTransition = serde_json::from_slice(payload)
                .map_err(|e| SigilError::Mls(format!("PrepareTransition deserialize: {}", e)))?;
            Ok(DaveEvent::PrepareTransition(data))
        }

        DaveOpcode::ExecuteTransition => {
            let data: ExecuteTransition = serde_json::from_slice(payload)
                .map_err(|e| SigilError::Mls(format!("ExecuteTransition deserialize: {}", e)))?;
            Ok(DaveEvent::ExecuteTransition(data))
        }

        DaveOpcode::PrepareEpoch => {
            let data: PrepareEpoch = serde_json::from_slice(payload)
                .map_err(|e| SigilError::Mls(format!("PrepareEpoch deserialize: {}", e)))?;
            Ok(DaveEvent::PrepareEpoch(data))
        }

        DaveOpcode::MlsExternalSender => {
            // Binary payload: skip sequence_number(2) + opcode(1) header
            // then parse ExternalSender struct
            // For now, store the raw bytes for the MLS layer to parse
            let data = if payload.len() > 3 {
                &payload[3..]
            } else {
                payload
            };
            Ok(DaveEvent::MlsExternalSender(MlsExternalSenderPayload {
                credential: Vec::new(), // will be parsed by MLS layer
                signature_key: data.to_vec(),
            }))
        }

        DaveOpcode::MlsProposals => {
            // Binary payload: skip sequence_number(2) + opcode(1)
            let data = if payload.len() > 3 {
                &payload[3..]
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
            // Binary: seq(2) + opcode(1) + transition_id(2) + commit
            let data = if payload.len() > 3 {
                &payload[3..]
            } else {
                payload
            };
            let transition_id = if data.len() >= 2 {
                u16::from_be_bytes([data[0], data[1]]) as u64
            } else {
                0
            };
            let commit_bytes = if data.len() > 2 {
                data[2..].to_vec()
            } else {
                Vec::new()
            };
            Ok(DaveEvent::MlsAnnounceCommitTransition(
                MlsAnnounceCommitTransition {
                    transition_id,
                    commit_bytes,
                },
            ))
        }

        DaveOpcode::MlsWelcome => {
            // Binary: seq(2) + opcode(1) + transition_id(2) + welcome
            let data = if payload.len() > 3 {
                &payload[3..]
            } else {
                payload
            };
            let transition_id = if data.len() >= 2 {
                u16::from_be_bytes([data[0], data[1]]) as u64
            } else {
                0
            };
            let welcome_bytes = if data.len() > 2 {
                data[2..].to_vec()
            } else {
                Vec::new()
            };
            Ok(DaveEvent::MlsWelcome(MlsWelcomePayload {
                transition_id,
                welcome_bytes,
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
