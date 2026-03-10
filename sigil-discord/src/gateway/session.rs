//! Gateway-level DAVE session state machine.
//!
//! Tracks the MLS epoch lifecycle, manages the per-sender key ratchets,
//! and provides nonce management for frame encryption.

use std::collections::HashMap;

use crate::crypto::key_ratchet::KeyRatchet;
use crate::types::{Epoch, UserId};

/// The state of the DAVE gateway session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// No MLS group is active.
    Disconnected,
    /// MLS group is being negotiated (proposals pending).
    Negotiating { epoch: Epoch },
    /// MLS group is established and keys are ready.
    Established { epoch: Epoch },
}

/// Gateway-level DAVE session managing epoch transitions and nonce state.
///
/// This is a lightweight state machine that sits between the MLS group
/// (managed by `SigilSession`) and the mixing loop (in `driver.rs`).
/// It tracks:
/// - Current session state (disconnected / negotiating / established)
/// - The monotonic 32-bit send nonce (auto-incrementing per frame)
/// - Per-sender key ratchets for generation advancement
pub struct DaveSession {
    /// Our Discord user ID.
    pub user_id: UserId,
    /// Current session state.
    pub state: SessionState,
    /// Monotonic 32-bit nonce counter for outgoing frames.
    /// Resets to 0 on each new epoch (new key material).
    send_nonce: u32,
    /// Per-sender key ratchets for the current epoch.
    ratchets: HashMap<UserId, KeyRatchet>,
}

impl DaveSession {
    /// Create a new disconnected session.
    pub fn new(user_id: UserId) -> Self {
        Self {
            user_id,
            state: SessionState::Disconnected,
            send_nonce: 0,
            ratchets: HashMap::new(),
        }
    }

    /// Transition to negotiating state for a given epoch.
    pub fn begin_negotiation(&mut self, epoch: Epoch) {
        self.state = SessionState::Negotiating { epoch };
    }

    /// Establish the session with new key ratchets.
    ///
    /// **CRITICAL**: Resets `send_nonce` to 0 because the key material
    /// has changed. The receiving side derives `generation = nonce >> 24`,
    /// so a fresh epoch must start at nonce 0 to align with generation 0
    /// of the new ratchet.
    pub fn establish(&mut self, epoch: Epoch, ratchets: HashMap<UserId, KeyRatchet>) {
        self.state = SessionState::Established { epoch };
        self.send_nonce = 0; // MUST reset on new epoch
        // Merge new ratchets (don't clobber existing ones for other senders
        // that haven't changed)
        for (uid, ratchet) in ratchets {
            self.ratchets.insert(uid, ratchet);
        }
    }

    /// Get the next nonce and auto-increment.
    ///
    /// The nonce is a monotonic 32-bit counter that wraps around.
    /// Generation is derived as `nonce >> 24`, meaning after 2^24
    /// frames (~5.6 hours at 50fps), the generation advances and
    /// the key ratchet produces a new AES key.
    pub fn next_nonce(&mut self) -> u32 {
        let n = self.send_nonce;
        self.send_nonce = self.send_nonce.wrapping_add(1);
        n
    }

    /// Peek at the current nonce without incrementing.
    pub fn current_nonce(&self) -> u32 {
        self.send_nonce
    }

    /// Reset the nonce to 0 (e.g., on epoch change or reconnect).
    pub fn reset_nonce(&mut self) {
        self.send_nonce = 0;
    }

    /// Get a mutable reference to a sender's key ratchet.
    pub fn ratchet_mut(&mut self, sender_id: UserId) -> Option<&mut KeyRatchet> {
        self.ratchets.get_mut(&sender_id)
    }

    /// Get a reference to a sender's key ratchet.
    pub fn ratchet(&self, sender_id: UserId) -> Option<&KeyRatchet> {
        self.ratchets.get(&sender_id)
    }

    /// Check if a ratchet exists for the given sender.
    pub fn has_ratchet(&self, sender_id: UserId) -> bool {
        self.ratchets.contains_key(&sender_id)
    }

    /// Full reset: go back to disconnected, clear all state.
    pub fn reset(&mut self) {
        self.state = SessionState::Disconnected;
        self.send_nonce = 0;
        self.ratchets.clear();
    }
}
