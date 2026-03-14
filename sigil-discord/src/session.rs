//! High-level DAVE session facade for Discord bot integration.
//!
//! [`SigilSession`] is the single entry point that bot developers should use.
//! It orchestrates MLS group management, key derivation, frame encryption/decryption,
//! and gateway event handling behind a clean, ergonomic API.

use std::collections::HashMap;

use crate::crypto::codec::Codec;
use crate::crypto::key_ratchet::KeyRatchet;
use crate::error::SigilError;
use crate::frame::decryptor::FrameDecryptor;
use crate::frame::encryptor::FrameEncryptor;
use crate::gateway::handler::{DaveEvent, dispatch};
use crate::gateway::session::{DaveSession, SessionState};
use crate::mls::config::{build_group_config, crypto_provider};
use crate::mls::credential::DaveIdentity;
use crate::mls::group::{DaveGroup, extract_group_id_from_proposals};
use crate::types::{Epoch, KEY_LENGTH, UserId};

use openmls::prelude::*;
use openmls_rust_crypto::OpenMlsRustCrypto;

/// High-level DAVE session that wraps MLS, crypto, frame, and gateway
/// into a single cohesive API for Discord bot integration.
pub struct SigilSession {
    /// Our Discord user ID.
    pub user_id: UserId,
    /// The openmls crypto provider instance.
    provider: OpenMlsRustCrypto,
    /// Our MLS identity (keypair + credential).
    identity: DaveIdentity,
    /// The MLS group (None until joined/created).
    group: Option<DaveGroup>,
    /// Pending external sender credential and key, stored by set_external_sender()
    /// and consumed by the first process_proposals() call to create the MLS group
    /// with the correct Discord-assigned group ID.
    pending_external_sender: Option<(Credential, Vec<u8>)>,
    /// The underlying gateway session state machine.
    gateway_session: DaveSession,
    /// Per-sender encryption keys for the current epoch.
    sender_keys: HashMap<UserId, [u8; KEY_LENGTH]>,
    /// Our own encryption key for the current epoch.
    own_key: Option<[u8; KEY_LENGTH]>,
}

impl SigilSession {
    /// Create a new DAVE session for the given Discord user ID.
    pub fn new(user_id: UserId) -> Result<Self, SigilError> {
        let provider = crypto_provider();
        let identity = DaveIdentity::new(user_id, &provider)?;

        Ok(Self {
            user_id,
            provider,
            identity,
            group: None,
            pending_external_sender: None,
            gateway_session: DaveSession::new(user_id),
            sender_keys: HashMap::new(),
            own_key: None,
        })
    }

    // --- MLS Group Lifecycle ---

    /// Create a new MLS group for a voice channel.
    pub fn create_group(
        &mut self,
        gateway_credential: Credential,
        gateway_pubkey: Vec<u8>,
        group_id: &[u8],
    ) -> Result<(), SigilError> {
        let config = build_group_config(gateway_credential, gateway_pubkey)?;
        let group = DaveGroup::create(&self.identity, &self.provider, &config, group_id)?;
        self.group = Some(group);
        Ok(())
    }

    /// Join an existing MLS group via a Welcome message.
    pub fn join_group(&mut self, welcome_bytes: &[u8]) -> Result<(), SigilError> {
        use openmls::prelude::tls_codec::DeserializeBytes;
        let welcome_msg = MlsMessageIn::tls_deserialize_exact_bytes(welcome_bytes)
            .map_err(|e| SigilError::Mls(format!("welcome deserialize: {}", e)))?;

        let config = MlsGroupCreateConfig::builder().build();
        let group =
            DaveGroup::join_from_welcome(&self.identity, &self.provider, &config, welcome_msg)?;
        self.group = Some(group);
        Ok(())
    }

    /// Set the external sender credentials received from the Voice Gateway (OP 25).
    ///
    /// Stores the credential and key in `pending_external_sender` for deferred
    /// group creation. The group is created on the first `process_proposals` call
    /// when the Discord-assigned group ID is available.
    pub fn set_external_sender(&mut self, payload: &[u8]) -> Result<(), SigilError> {
        use openmls::prelude::tls_codec::Deserialize;
        let mut cursor = std::io::Cursor::new(payload);

        let credential = Credential::tls_deserialize(&mut cursor)
            .map_err(|e| SigilError::Mls(format!("credential deserialize: {}", e)))?;

        let pos = cursor.position() as usize;
        
        // ── INVESTIGATION: Try TLS deserialization of SignaturePublicKey ──
        // Discord may be sending the signature key as a TLS-serialized SignaturePublicKey
        // rather than raw bytes. Let's try both approaches.
        
        let signature_key_result = SignaturePublicKey::tls_deserialize(&mut cursor);
        
        let signature_key = match signature_key_result {
            Ok(sig_key) => {
                tracing::info!(
                    "✅ Successfully TLS-deserialized SignaturePublicKey from OP 25\n\
                     - Consumed {} bytes from position {} to {}",
                    cursor.position() as usize - pos,
                    pos,
                    cursor.position()
                );
                
                // Convert back to Vec<u8> for storage
                // We need to store the raw bytes, not the TLS-serialized form
                sig_key.as_slice().to_vec()
            }
            Err(e) => {
                tracing::warn!(
                    "⚠️  TLS deserialization of SignaturePublicKey failed: {}\n\
                     - Falling back to raw bytes extraction",
                    e
                );
                
                // Fallback: treat remaining bytes as raw signature key
                payload[pos..].to_vec()
            }
        };

        // ── DEBUG LOGGING: External sender public key format ──
        tracing::debug!(
            "🔍 External sender public key extracted from OP 25:\n\
             - Total payload size: {} bytes\n\
             - Credential size: {} bytes\n\
             - Signature key size: {} bytes\n\
             - Key format: {}",
            payload.len(),
            pos,
            signature_key.len(),
            match signature_key.len() {
                65 if signature_key.first() == Some(&0x04) => 
                    "Uncompressed P-256 (0x04 || X || Y)",
                33 if matches!(signature_key.first(), Some(&0x02) | Some(&0x03)) => 
                    "Compressed P-256",
                _ => "Unknown/Invalid (expected 65 or 33 bytes)",
            }
        );

        if !signature_key.is_empty() {
            let preview_len = signature_key.len().min(8);
            tracing::debug!(
                "   - First {} bytes: {:02x?}",
                preview_len,
                &signature_key[..preview_len]
            );
            tracing::debug!(
                "   - Last {} bytes: {:02x?}",
                preview_len,
                &signature_key[signature_key.len().saturating_sub(preview_len)..]
            );
        }

        // Validate P-256 key length
        if signature_key.len() != 65 && signature_key.len() != 33 {
            tracing::warn!(
                "⚠️  External sender public key has unexpected length: {} bytes \
                 (expected 65 for uncompressed or 33 for compressed P-256)",
                signature_key.len()
            );
        }

        // Validate key format prefix
        if signature_key.len() == 65 && signature_key.first() != Some(&0x04) {
            tracing::warn!(
                "⚠️  65-byte key does not start with 0x04 (uncompressed marker), \
                 got 0x{:02x}",
                signature_key.first().unwrap_or(&0)
            );
        } else if signature_key.len() == 33 
            && !matches!(signature_key.first(), Some(&0x02) | Some(&0x03)) 
        {
            tracing::warn!(
                "⚠️  33-byte key does not start with 0x02/0x03 (compressed marker), \
                 got 0x{:02x}",
                signature_key.first().unwrap_or(&0)
            );
        }

        self.pending_external_sender = Some((credential, signature_key));
        tracing::info!("✅ External sender credential and key stored for deferred group creation");
        Ok(())
    }

    /// Process incoming OP 27 proposals (Append / Revoke) from the Voice server.
    ///
    /// `operations` is a slice of raw MLS proposal byte vectors.
    ///
    /// On the first call when no group exists yet, extracts the group ID from
    /// the proposals and creates the MLS group using the pending external sender
    /// credential stored by `set_external_sender`.
    ///
    /// Returns `Ok(true)` when Discord sent non-empty proposals that could not
    /// be deserialized (e.g. custom type 16) AND none were stored as pending.
    /// The caller must check `has_pending_proposals()`:
    /// - If true  → call `commit_and_welcome()` to commit the processable ones.
    /// - If false AND return value is true → the driver should still send a
    ///   self-update commit (OP 28) with the correct transition_id so Discord
    ///   advances the epoch and delivers the epoch key.
    pub fn process_proposals(&mut self, operations: &[Vec<u8>]) -> Result<bool, SigilError> {
        if self.group.is_none() {
            if let Some((cred, key)) = self.pending_external_sender.take() {
                let group_id = extract_group_id_from_proposals(operations)
                    .ok_or(SigilError::GroupNotEstablished)?;
                let config = build_group_config(cred, key)?;
                let group = DaveGroup::create(
                    &self.identity,
                    &self.provider,
                    &config,
                    group_id.as_slice(),
                )?;
                self.group = Some(group);
            }
        }
        let group = self.group.as_mut().ok_or(SigilError::GroupNotEstablished)?;
        let needs_commit = group.process_proposals(operations, &self.provider)?;
        Ok(needs_commit)
    }

    /// Check if the MLS group has pending proposals waiting to be committed.
    pub fn has_pending_proposals(&self) -> bool {
        self.group
            .as_ref()
            .map(|g| g.has_pending_proposals())
            .unwrap_or(false)
    }

    /// Resolve pending proposals by committing and issuing a Welcome buffer.
    ///
    /// **FIX (critical)**: After creating the commit, we call `merge_pending_commit()`
    /// on the underlying MLS group so that the local group state advances to the
    /// new epoch. Without this, `export_secret()` would still return the OLD epoch's
    /// exporter secret, causing a key mismatch where our encrypted frames use a
    /// stale key that receivers (who processed our commit) cannot decrypt.
    pub fn commit_and_welcome(&mut self) -> Result<(Vec<u8>, Option<Vec<u8>>), SigilError> {
        let group = self.group.as_mut().ok_or(SigilError::GroupNotEstablished)?;
        let signer = &self.identity.signature_keys;
        let (commit_bytes, welcome_bytes) = group.commit_pending(&self.provider, signer)?;

        group.merge_own_pending_commit(&self.provider)?;

        Ok((commit_bytes, welcome_bytes))
    }

    /// Generate a key package for the Voice Gateway to add us to a group.
    pub fn generate_key_package(&self) -> Result<Vec<u8>, SigilError> {
        use openmls::prelude::tls_codec::Serialize;
        let kp = crate::mls::key_package::generate_key_package(&self.identity, &self.provider)?;
        kp.tls_serialize_detached()
            .map_err(|e| SigilError::Mls(format!("key package serialize: {}", e)))
    }

    // --- Key Management ---

    /// Export encryption keys for the given senders and install ratchets.
    ///
    /// **FIX**: After exporting, we call `gateway_session.establish()` which:
    /// - Resets `send_nonce` to 0 (critical for epoch transitions)
    /// - Rotates current ratchets to `previous_ratchets` (for out-of-order decryption)
    /// - Installs fresh ratchets for the new epoch
    pub fn export_sender_keys(
        &mut self,
        sender_ids: &[UserId],
    ) -> Result<HashMap<UserId, [u8; KEY_LENGTH]>, SigilError> {
        let group = self.group.as_ref().ok_or(SigilError::GroupNotEstablished)?;

        let mut keys = HashMap::new();
        for &sender_id in sender_ids {
            let key = group.export_sender_key(sender_id, &self.provider)?;
            keys.insert(sender_id, key);
        }

        if let Some(key) = keys.get(&self.user_id) {
            self.own_key = Some(*key);
        }

        self.sender_keys = keys.clone();

        let epoch = group.current_epoch;
        let mut ratchets = HashMap::new();
        for (&sid, &base_secret) in &keys {
            ratchets.insert(sid, KeyRatchet::new(base_secret));
        }
        self.gateway_session.establish(epoch, ratchets);

        Ok(keys)
    }

    /// Install a pre-derived sender key directly.
    pub fn install_sender_key(&mut self, sender_id: UserId, key: [u8; KEY_LENGTH]) {
        if sender_id == self.user_id {
            self.own_key = Some(key);
        }
        self.sender_keys.insert(sender_id, key);
    }

    /// Install a key ratchet for a sender and derive the initial key.
    pub fn install_ratchet(
        &mut self,
        sender_id: UserId,
        base_secret: [u8; KEY_LENGTH],
    ) -> Result<(), SigilError> {
        let ratchet = KeyRatchet::new(base_secret);
        let key = *ratchet.base_secret();
        self.sender_keys.insert(sender_id, key);
        if sender_id == self.user_id {
            self.own_key = Some(key);
        }

        let mut ratchets = HashMap::new();
        ratchets.insert(sender_id, ratchet);

        if let SessionState::Established { epoch } = self.gateway_session.state {
            self.gateway_session.establish(epoch, ratchets);
        }

        Ok(())
    }

    // --- Frame Encryption/Decryption ---

    /// Encrypt a raw media frame for sending.
    ///
    /// Uses the key ratchet to derive the correct key for the current
    /// generation (nonce >> 24). For the first ~16M frames per epoch,
    /// generation is 0 and the base secret is used directly.
    pub fn encrypt_frame(
        &mut self,
        key: &[u8; KEY_LENGTH],
        raw_frame: &[u8],
        codec: Codec,
    ) -> Result<Vec<u8>, SigilError> {
        let nonce = self.gateway_session.next_nonce();
        let generation = nonce >> 24;

        let actual_key = if generation > 0 {
            if let Some(ratchet) = self.gateway_session.ratchet_mut(self.user_id) {
                ratchet.get(generation)?
            } else {
                *key
            }
        } else {
            *key
        };

        let encryptor = FrameEncryptor::new(codec);
        encryptor.encrypt(&actual_key, nonce, raw_frame)
    }

    /// Encrypt a frame using our own cached key.
    pub fn encrypt_own_frame(
        &mut self,
        raw_frame: &[u8],
        codec: Codec,
    ) -> Result<Vec<u8>, SigilError> {
        let key = self
            .own_key
            .ok_or(SigilError::NoSenderKey(self.user_id, 0))?;
        self.encrypt_frame(&key, raw_frame, codec)
    }

    /// Decrypt an incoming DAVE frame.
    pub fn decrypt_frame(
        &self,
        key: &[u8; KEY_LENGTH],
        dave_frame: &[u8],
    ) -> Result<Vec<u8>, SigilError> {
        FrameDecryptor::decrypt(key, dave_frame)
    }

    /// Decrypt an incoming frame using a cached sender key.
    pub fn decrypt_from_sender(
        &self,
        sender_id: UserId,
        dave_frame: &[u8],
    ) -> Result<Vec<u8>, SigilError> {
        let key = self
            .sender_keys
            .get(&sender_id)
            .ok_or(SigilError::NoSenderKey(sender_id, 0))?;
        self.decrypt_frame(key, dave_frame)
    }

    // --- Gateway Events ---

    /// Dispatch a raw gateway event (opcode + payload) into a DaveEvent.
    pub fn handle_gateway_event(
        &self,
        opcode: u8,
        payload: &[u8],
    ) -> Result<DaveEvent, SigilError> {
        dispatch(opcode, payload)
    }

    /// Process an incoming commit (OP 29) and advance the epoch.
    pub fn process_commit(&mut self, commit_bytes: &[u8]) -> Result<Epoch, SigilError> {
        let group = self.group.as_mut().ok_or(SigilError::GroupNotEstablished)?;
        group.process_commit(commit_bytes, &self.provider)?;
        Ok(group.current_epoch)
    }

    // --- State Accessors ---

    pub fn is_established(&self) -> bool {
        self.group.is_some()
    }

    /// Returns the Discord user IDs of all current MLS group members.
    ///
    /// Used to export sender keys for ALL participants so incoming audio
    /// from every user can be decrypted — not just the bot's own frames.
    pub fn group_member_ids(&self) -> Vec<UserId> {
        self.group
            .as_ref()
            .map(|g| g.member_user_ids())
            .unwrap_or_default()
    }

    pub fn has_own_key(&self) -> bool {
        self.own_key.is_some()
    }

    pub fn current_epoch(&self) -> Option<Epoch> {
        self.group.as_ref().map(|g| g.current_epoch)
    }

    pub fn session_state(&self) -> &SessionState {
        &self.gateway_session.state
    }

    pub fn identity(&self) -> &DaveIdentity {
        &self.identity
    }

    pub fn provider(&self) -> &OpenMlsRustCrypto {
        &self.provider
    }

    pub fn gateway_session(&self) -> &DaveSession {
        &self.gateway_session
    }

    pub fn gateway_session_mut(&mut self) -> &mut DaveSession {
        &mut self.gateway_session
    }

    pub fn mls_group(&self) -> Option<&DaveGroup> {
        self.group.as_ref()
    }

    /// Full reset: disconnect, clear keys, remove group.
    pub fn disconnect(&mut self) {
        self.gateway_session.reset();
        self.sender_keys.clear();
        self.own_key = None;
        self.group = None;
        self.pending_external_sender = None;
    }
}
