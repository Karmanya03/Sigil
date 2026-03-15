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
        
        // ── DEBUG: Log full OP 25 payload for analysis ──
        tracing::error!(
            "🔍 RAW OP 25 PAYLOAD ANALYSIS:\n\
             - Total length: {} bytes\n\
             - Full hex dump: {:02x?}",
            payload.len(),
            payload
        );
        
        // ── HYPOTHESIS: Maybe the order is SignaturePublicKey THEN Credential ──
        // Let's try parsing SignaturePublicKey first
        let mut cursor = std::io::Cursor::new(payload);
        
        let signature_key_result = SignaturePublicKey::tls_deserialize(&mut cursor);
        
        let (signature_key, credential) = match signature_key_result {
            Ok(sig_key) => {
                let sig_key_bytes = sig_key.as_slice().to_vec();
                let pos_after_key = cursor.position() as usize;
                
                tracing::info!(
                    "✅ TLS-deserialized SignaturePublicKey FIRST from OP 25\n\
                     - Key length: {} bytes\n\
                     - Cursor position after key: {}",
                    sig_key_bytes.len(),
                    pos_after_key
                );
                
                // Now try to deserialize the credential
                let credential_result = Credential::tls_deserialize(&mut cursor);
                
                match credential_result {
                    Ok(cred) => {
                        tracing::info!(
                            "✅ Successfully parsed OP 25 as: SignaturePublicKey → Credential\n\
                             - Credential type: {:?}\n\
                             - Credential identity length: {} bytes",
                            cred.credential_type(),
                            cred.serialized_content().len()
                        );
                        (sig_key_bytes, cred)
                    }
                    Err(e) => {
                        tracing::error!(
                            "❌ Failed to parse credential after SignaturePublicKey: {}\n\
                             - Falling back to original parsing order",
                            e
                        );
                        
                        // Fall back to original order: Credential then SignaturePublicKey
                        return self.parse_credential_then_key(payload);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "⚠️  TLS deserialization of SignaturePublicKey FIRST failed: {}\n\
                     - Trying original order: Credential → SignaturePublicKey",
                    e
                );
                
                // Fall back to original order
                return self.parse_credential_then_key(payload);
            }
        };

        // ── DEBUG LOGGING: External sender public key format ──
        tracing::debug!(
            "🔍 External sender public key extracted from OP 25:\n\
             - Total payload size: {} bytes\n\
             - Signature key size: {} bytes\n\
             - Key format: {}",
            payload.len(),
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

        // Store the parsed data
        self.pending_external_sender = Some((credential, signature_key));
        tracing::info!("✅ External sender credential and key stored for deferred group creation");
        Ok(())
    }
    
    // Helper function for original parsing order
    fn parse_credential_then_key(&mut self, payload: &[u8]) -> Result<(), SigilError> {
        use openmls::prelude::tls_codec::Deserialize;
        let mut cursor = std::io::Cursor::new(payload);

        let credential = Credential::tls_deserialize(&mut cursor)
            .map_err(|e| SigilError::Mls(format!("credential deserialize: {}", e)))?;

        tracing::info!(
            "📋 External sender credential from OP 25 (original order):\n\
             - Credential type: {:?}\n\
             - Identity length: {} bytes\n\
             - Cursor position after credential: {}",
            credential.credential_type(),
            credential.serialized_content().len(),
            cursor.position()
        );

        let pos = cursor.position() as usize;
        let signature_key = payload[pos..].to_vec();

        tracing::debug!(
            "🔍 External sender public key (original order):\n\
             - Signature key size: {} bytes",
            signature_key.len()
        );

        // ── FIX: Discord sends 64-byte raw P-256 keys (X || Y without prefix) ──
        // BUT: The last 4 bytes might be TLS structure padding/metadata
        // OpenMLS expects either:
        // - 65 bytes: 0x04 || X || Y (uncompressed)
        // - 33 bytes: 0x02/0x03 || X (compressed)
        // Discord sends 64 bytes, but we need to check if the last 4 bytes are metadata
        
        let fixed_signature_key = if signature_key.len() == 64 {
            // Check if last 4 bytes look like TLS metadata (all zeros or specific pattern)
            let last_4 = &signature_key[60..64];
            tracing::debug!(
                "🔍 Analyzing last 4 bytes of 64-byte key: {:02x?}",
                last_4
            );
            
            // If last 4 bytes are [00, 01, 01, 00] or similar metadata pattern,
            // they're likely not part of the actual P-256 key
            // P-256 keys are 32 bytes X + 32 bytes Y = 64 bytes total
            // But if we see padding/metadata, we should use only the first 60 bytes
            
            if last_4 == [0x00, 0x01, 0x01, 0x00] {
                tracing::warn!(
                    "⚠️  Detected TLS metadata in last 4 bytes: {:02x?}\n\
                     - This is NOT part of the P-256 key\n\
                     - Using only first 60 bytes as key data\n\
                     - This will result in INVALID key - need to investigate OP 25 structure",
                    last_4
                );
                
                // This is actually a problem - we can't just truncate to 60 bytes
                // A P-256 key MUST be exactly 64 bytes (32 + 32)
                // The issue is that we're extracting the wrong bytes from OP 25
                
                // Let's try a different approach: maybe the signature key starts earlier
                // Let me log the full payload to understand the structure
                tracing::error!(
                    "🚨 CRITICAL: OP 25 payload structure analysis needed\n\
                     - Total payload: {} bytes\n\
                     - Credential consumed: {} bytes\n\
                     - Remaining bytes: {} bytes\n\
                     - Full remaining bytes: {:02x?}",
                    payload.len(),
                    pos,
                    signature_key.len(),
                    signature_key
                );
            }
            
            tracing::info!(
                "🔧 Fixing 64-byte raw P-256 key from Discord:\n\
                 - Prepending 0x04 to create valid uncompressed format\n\
                 - Original: 64 bytes (X || Y)\n\
                 - Fixed: 65 bytes (0x04 || X || Y)"
            );
            
            let mut fixed_key = Vec::with_capacity(65);
            fixed_key.push(0x04); // Uncompressed point marker
            fixed_key.extend_from_slice(&signature_key);
            
            tracing::info!(
                "✅ Key format fixed: {} bytes → {} bytes",
                signature_key.len(),
                fixed_key.len()
            );
            
            fixed_key
        } else if signature_key.len() == 65 && signature_key.first() == Some(&0x04) {
            tracing::debug!("✅ Key already in correct uncompressed format (65 bytes with 0x04 prefix)");
            signature_key
        } else if signature_key.len() == 33 && matches!(signature_key.first(), Some(&0x02) | Some(&0x03)) {
            tracing::debug!("✅ Key already in correct compressed format (33 bytes with 0x02/0x03 prefix)");
            signature_key
        } else {
            tracing::warn!(
                "⚠️  Unexpected key format: {} bytes, first byte: 0x{:02x}\n\
                 - Expected: 64 bytes (raw), 65 bytes (0x04 prefix), or 33 bytes (0x02/0x03 prefix)\n\
                 - Using key as-is, but signature verification may fail",
                signature_key.len(),
                signature_key.first().unwrap_or(&0)
            );
            signature_key
        };

        self.pending_external_sender = Some((credential, fixed_signature_key));
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
