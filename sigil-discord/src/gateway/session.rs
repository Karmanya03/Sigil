//! DAVE session state machine with nonce management and key retention.
//!
//! Tracks the lifecycle of a DAVE voice connection: disconnected → pending
//! → established (with epoch) → transitioning between epochs.

use std::collections::HashMap;

use crate::crypto::key_ratchet::KeyRatchet;
use crate::types::{Epoch, TransitionId, UserId};

/// The current state of a DAVE session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// Not connected to any DAVE session.
    Disconnected,
    /// Waiting for the session to be established.
    Pending,
    /// Session is active at a specific epoch.
    Established {
        /// Current MLS epoch.
        epoch: Epoch,
    },
    /// Transitioning between epochs.
    Transitioning {
        /// The epoch we are transitioning from.
        from: Epoch,
        /// The epoch we are transitioning to.
        to: Epoch,
        /// The transition identifier.
        tid: TransitionId,
    },
}

/// DAVE session state machine for a single voice connection.
///
/// Manages the protocol state, per-sender key ratchets, and the
/// monotonically increasing send nonce.
pub struct DaveSession {
    /// Current session state.
    pub state: SessionState,
    /// Our user ID.
    pub user_id: UserId,
    /// Negotiated protocol version.
    pub protocol_version: u16,
    /// Per-sender key ratchets for the current epoch.
    sender_ratchets: HashMap<UserId, KeyRatchet>,
    /// Key ratchets from the previous epoch, retained temporarily for
    /// out-of-order frame decryption during transitions.
    previous_ratchets: HashMap<UserId, KeyRatchet>,
    /// Monotonically increasing send nonce counter.
    send_nonce: u32,
}

impl DaveSession {
    /// Create a new session in the [`Disconnected`](SessionState::Disconnected) state.
    pub fn new(user_id: UserId) -> Self {
        Self {
            state: SessionState::Disconnected,
            user_id,
            protocol_version: crate::types::DAVE_PROTOCOL_VERSION,
            sender_ratchets: HashMap::new(),
            previous_ratchets: HashMap::new(),
            send_nonce: 0,
        }
    }

    /// Transition the session to the [`Established`](SessionState::Established) state.
    ///
    /// Moves the current ratchets to `previous_ratchets` for key retention,
    /// installs the new ratchets, and resets the send nonce to 0.
    pub fn establish(&mut self, epoch: Epoch, ratchets: HashMap<UserId, KeyRatchet>) {
        // Rotate: current → previous
        self.previous_ratchets = std::mem::take(&mut self.sender_ratchets);

        // Install new ratchets
        self.sender_ratchets = ratchets;

        // Reset nonce
        self.send_nonce = 0;

        self.state = SessionState::Established { epoch };
    }

    /// Get and auto-increment the send nonce.
    ///
    /// The nonce is a 32-bit counter that wraps around at `u32::MAX`.
    pub fn next_nonce(&mut self) -> u32 {
        let nonce = self.send_nonce;
        self.send_nonce = self.send_nonce.wrapping_add(1);
        nonce
    }

    /// Clear the previous epoch's key ratchets.
    ///
    /// Called after the key retention period (typically 10 seconds) has
    /// elapsed following an epoch transition.
    pub fn expire_previous_keys(&mut self) {
        self.previous_ratchets.clear();
    }

    /// Reset the session back to [`Disconnected`](SessionState::Disconnected),
    /// clearing all ratchets and nonce state.
    pub fn reset(&mut self) {
        self.state = SessionState::Disconnected;
        self.sender_ratchets.clear();
        self.previous_ratchets.clear();
        self.send_nonce = 0;
    }

    /// Get a mutable reference to the key ratchet for a sender in the current epoch.
    pub fn sender_ratchet(&mut self, sender_id: UserId) -> Option<&mut KeyRatchet> {
        self.sender_ratchets.get_mut(&sender_id)
    }

    /// Get a mutable reference to the key ratchet for a sender in the previous epoch.
    ///
    /// Used during transitions to decrypt out-of-order frames.
    pub fn previous_ratchet(&mut self, sender_id: UserId) -> Option<&mut KeyRatchet> {
        self.previous_ratchets.get_mut(&sender_id)
    }

    /// Returns the current send nonce without incrementing.
    pub fn current_nonce(&self) -> u32 {
        self.send_nonce
    }
}
