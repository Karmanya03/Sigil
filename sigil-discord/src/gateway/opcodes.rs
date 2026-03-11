//! DAVE voice gateway opcodes 21-31.
//!
//! Defines the opcode enum and serde-serializable payload structs
//! for all DAVE-specific voice gateway messages.

use serde::{Deserialize, Serialize};

/// DAVE voice gateway opcodes (21-31).
///
/// ## Transport Types
///
/// Per Discord's Voice Opcodes Table:
/// - **JSON text**: OP 21, 22, 23, 24, 31
/// - **Binary**: OP 25, 26, 27, 28, 29, 30
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaveOpcode {
    /// Server -> Client: prepare for a protocol transition (JSON).
    PrepareTransition = 21,
    /// Server -> Client: execute a previously announced transition (JSON).
    ExecuteTransition = 22,
    /// Client -> Server: signal readiness for a transition (JSON).
    ReadyForTransition = 23,
    /// Server -> Client: prepare for a new epoch (JSON).
    PrepareEpoch = 24,
    /// Server -> Client: external sender package (binary).
    MlsExternalSender = 25,
    /// Client -> Server: key package (binary).
    MlsKeyPackage = 26,
    /// Server -> Client: proposals to append or revoke (binary).
    MlsProposals = 27,
    /// Client -> Server: commit + optional welcome (binary).
    MlsCommitWelcome = 28,
    /// Server -> Client: announce commit with transition ID (binary).
    MlsAnnounceCommitTransition = 29,
    /// Server -> Client: welcome message for pending member (binary).
    MlsWelcome = 30,
    /// Client -> Server: report invalid commit/welcome (JSON).
    MlsInvalidCommitWelcome = 31,
}

impl DaveOpcode {
    /// Try to convert a raw `u8` to a [`DaveOpcode`].
    ///
    /// Returns `None` if the value is not a recognized DAVE opcode.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            21 => Some(Self::PrepareTransition),
            22 => Some(Self::ExecuteTransition),
            23 => Some(Self::ReadyForTransition),
            24 => Some(Self::PrepareEpoch),
            25 => Some(Self::MlsExternalSender),
            26 => Some(Self::MlsKeyPackage),
            27 => Some(Self::MlsProposals),
            28 => Some(Self::MlsCommitWelcome),
            29 => Some(Self::MlsAnnounceCommitTransition),
            30 => Some(Self::MlsWelcome),
            31 => Some(Self::MlsInvalidCommitWelcome),
            _ => None,
        }
    }

    /// Returns `true` if this opcode is sent from the server to the client.
    pub fn is_server_to_client(&self) -> bool {
        matches!(
            self,
            Self::PrepareTransition
                | Self::ExecuteTransition
                | Self::PrepareEpoch
                | Self::MlsExternalSender
                | Self::MlsProposals
                | Self::MlsAnnounceCommitTransition
                | Self::MlsWelcome
        )
    }

    /// Returns `true` if this opcode uses binary WebSocket transport.
    ///
    /// Binary opcodes: 25, 26, 27, 28, 29, 30.
    /// JSON opcodes: 21, 22, 23, 24, 31.
    pub fn is_binary(&self) -> bool {
        matches!(
            self,
            Self::MlsExternalSender
                | Self::MlsKeyPackage
                | Self::MlsProposals
                | Self::MlsCommitWelcome
                | Self::MlsAnnounceCommitTransition
                | Self::MlsWelcome
        )
    }
}

// -- Payload structs (JSON-encoded opcodes) --

/// Payload for [`DaveOpcode::PrepareTransition`] (opcode 21, JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareTransition {
    /// The protocol version for this transition.
    pub protocol_version: u16,
    /// Unique transition identifier. 0 = (re)initialization.
    pub transition_id: u64,
}

/// Payload for [`DaveOpcode::ExecuteTransition`] (opcode 22, JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteTransition {
    /// The previously announced transition ID to execute.
    pub transition_id: u64,
}

/// Payload for [`DaveOpcode::ReadyForTransition`] (opcode 23, JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyForTransition {
    /// The transition ID the client is ready to execute.
    pub transition_id: u64,
}

/// Payload for [`DaveOpcode::PrepareEpoch`] (opcode 24, JSON).
///
/// JSON format: `{ "protocol_version": u16, "epoch": u64 }`
///
/// When `epoch == 1`, the client should generate and send a fresh
/// MLS KeyPackage (OP 26, binary).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareEpoch {
    /// The protocol version for the upcoming epoch.
    pub protocol_version: u16,
    /// The epoch identifier. 1 = new MLS group creation.
    pub epoch: u64,
}

// -- Payload structs (binary-encoded opcodes) --

/// Payload for [`DaveOpcode::MlsExternalSender`] (opcode 25, binary).
///
/// This is a binary opcode. The actual credential + public key are
/// parsed from raw bytes by `SigilSession::set_external_sender()`.
#[derive(Debug, Clone)]
pub struct MlsExternalSenderPayload {
    /// Raw credential bytes from the external sender.
    pub credential: Vec<u8>,
    /// Raw signature public key bytes.
    pub signature_key: Vec<u8>,
}

/// Payload for [`DaveOpcode::MlsKeyPackage`] (opcode 26, binary).
#[derive(Debug, Clone)]
pub struct MlsKeyPackagePayload {
    /// TLS-serialized MLS KeyPackage message.
    pub key_package_bytes: Vec<u8>,
}

/// Payload for [`DaveOpcode::MlsProposals`] (opcode 27, binary).
#[derive(Debug, Clone)]
pub struct MlsProposalsPayload {
    /// 0 = append, 1 = revoke.
    pub operation_type: u8,
    /// Raw proposal message bytes.
    pub data: Vec<u8>,
}

/// Payload for [`DaveOpcode::MlsCommitWelcome`] (opcode 28, binary).
#[derive(Debug, Clone)]
pub struct MlsCommitWelcomePayload {
    /// TLS-serialized MLS Commit message.
    pub commit_bytes: Vec<u8>,
    /// Optional TLS-serialized MLS Welcome message.
    pub welcome_bytes: Option<Vec<u8>>,
}

/// Payload for [`DaveOpcode::MlsAnnounceCommitTransition`] (opcode 29, binary).
///
/// Wire format: [seq(2)][op(1)][transition_id(2 LE uint16)][commit...]
///
/// Note: transition_id is uint16 on the wire, stored as u64 here
/// for consistency with the JSON opcodes.
#[derive(Debug, Clone)]
pub struct MlsAnnounceCommitTransition {
    /// Transition ID for the commit transition.
    pub transition_id: u64,
    /// TLS-serialized MLS Commit message.
    pub commit_bytes: Vec<u8>,
}

/// Payload for [`DaveOpcode::MlsWelcome`] (opcode 30, binary).
///
/// Wire format: [seq(2)][op(1)][transition_id(2 LE uint16)][welcome...]
///
/// Note: transition_id is uint16 on the wire, stored as u64 here
/// for consistency with the JSON opcodes.
#[derive(Debug, Clone)]
pub struct MlsWelcomePayload {
    /// Transition ID for the group transition.
    pub transition_id: u64,
    /// TLS-serialized MLS Welcome message.
    pub welcome_bytes: Vec<u8>,
}

/// Payload for [`DaveOpcode::MlsInvalidCommitWelcome`] (opcode 31, JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MlsInvalidCommitWelcome {
    /// Transition ID in which the invalid message was received.
    pub transition_id: u64,
}
