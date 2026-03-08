//! High-level DAVE session facade for Discord bot integration.
//!
//! [`SigilSession`] is the single entry point that bot developers should use.
//! It orchestrates MLS group management, key derivation, frame encryption/decryption,
//! and gateway event handling behind a clean, ergonomic API.
//!
//! # Example
//!
//! ```rust,no_run
//! use sigil::session::SigilSession;
//! use sigil::crypto::codec::Codec;
//!
//! // Create a session for your bot's user ID
//! let mut session = SigilSession::new(123456789012345678).unwrap();
//!
//! // Encrypt an outgoing audio frame
//! let key = [0u8; 16]; // from MLS key export
//! let raw_opus = vec![0u8; 960];
//! let encrypted = session.encrypt_frame(&key, &raw_opus, Codec::Opus).unwrap();
//!
//! // Decrypt an incoming frame
//! let decrypted = session.decrypt_frame(&key, &encrypted).unwrap();
//! assert_eq!(decrypted, raw_opus);
//! ```

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
use crate::mls::group::DaveGroup;
use crate::types::{Epoch, KEY_LENGTH, UserId};

use openmls::prelude::*;
use openmls_rust_crypto::OpenMlsRustCrypto;

/// High-level DAVE session that wraps MLS, crypto, frame, and gateway
/// into a single cohesive API for Discord bot integration.
///
/// This is the main struct you interact with. It manages:
/// - MLS identity and group lifecycle
/// - Per-sender key derivation and rotation
/// - Frame encryption/decryption with codec awareness
/// - Gateway event processing
/// - Nonce management with automatic incrementing
pub struct SigilSession {
    /// Our Discord user ID.
    pub user_id: UserId,
    /// The openmls crypto provider instance.
    provider: OpenMlsRustCrypto,
    /// Our MLS identity (keypair + credential).
    identity: DaveIdentity,
    /// The MLS group (None until joined/created).
    group: Option<DaveGroup>,
    /// The underlying gateway session state machine.
    gateway_session: DaveSession,
    /// Per-sender encryption keys for the current epoch.
    sender_keys: HashMap<UserId, [u8; KEY_LENGTH]>,
    /// Our own encryption key for the current epoch.
    own_key: Option<[u8; KEY_LENGTH]>,
}

impl SigilSession {
    /// Create a new DAVE session for the given Discord user ID.
    ///
    /// Generates an MLS identity (P-256 keypair + Basic credential) and
    /// initializes the session in a disconnected state.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::Mls`] if identity generation fails.
    pub fn new(user_id: UserId) -> Result<Self, SigilError> {
        let provider = crypto_provider();
        let identity = DaveIdentity::new(user_id, &provider)?;

        Ok(Self {
            user_id,
            provider,
            identity,
            group: None,
            gateway_session: DaveSession::new(user_id),
            sender_keys: HashMap::new(),
            own_key: None,
        })
    }

    // ─── MLS Group Lifecycle ───────────────────────────────────────────

    /// Create a new MLS group for a voice channel.
    ///
    /// Call this when your bot joins a voice channel and no existing
    /// group is present (i.e., you're the first DAVE participant).
    ///
    /// # Arguments
    ///
    /// * `gateway_credential` — the Discord gateway's MLS credential
    /// * `gateway_pubkey` — the gateway's P-256 signature public key
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::Mls`] if group creation fails.
    pub fn create_group(
        &mut self,
        gateway_credential: Credential,
        gateway_pubkey: Vec<u8>,
    ) -> Result<(), SigilError> {
        let config = build_group_config(gateway_credential, gateway_pubkey)?;
        let group = DaveGroup::create(&self.identity, &self.provider, &config)?;
        self.group = Some(group);
        Ok(())
    }

    /// Join an existing MLS group via a Welcome message.
    ///
    /// Call this when you receive a Welcome message from the gateway
    /// after another member adds you to the group.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::Mls`] if the Welcome is invalid.
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

    /// Generate a key package for other group members to add us.
    ///
    /// Send the returned bytes to the gateway so other members can
    /// create Add proposals for us.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::Mls`] if key package generation fails.
    pub fn generate_key_package(&self) -> Result<Vec<u8>, SigilError> {
        use openmls::prelude::tls_codec::Serialize;
        let kp = crate::mls::key_package::generate_key_package(&self.identity, &self.provider)?;
        kp.tls_serialize_detached()
            .map_err(|e| SigilError::Mls(format!("key package serialize: {}", e)))
    }

    // ─── Key Management ────────────────────────────────────────────────

    /// Export encryption keys for all known senders in the current epoch.
    ///
    /// Call this after the MLS group epoch advances (after processing a commit).
    /// It uses MLS-Exporter with label `"Discord Secure Frames v0"` and
    /// the sender's user ID as context.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::GroupNotEstablished`] if no group exists.
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

        // Cache our own key
        if let Some(key) = keys.get(&self.user_id) {
            self.own_key = Some(*key);
        }

        self.sender_keys = keys.clone();
        Ok(keys)
    }

    /// Install a pre-derived sender key directly (e.g. from a key ratchet).
    ///
    /// Useful when you already have the key from a ratchet advancement
    /// rather than MLS export.
    pub fn install_sender_key(&mut self, sender_id: UserId, key: [u8; KEY_LENGTH]) {
        if sender_id == self.user_id {
            self.own_key = Some(key);
        }
        self.sender_keys.insert(sender_id, key);
    }

    /// Install a key ratchet for a sender and derive the initial key.
    ///
    /// Creates a [`KeyRatchet`] with the given base secret and installs
    /// it in the gateway session for automatic generation advancement.
    pub fn install_ratchet(
        &mut self,
        sender_id: UserId,
        base_secret: [u8; KEY_LENGTH],
    ) -> Result<(), SigilError> {
        let ratchet = KeyRatchet::new(base_secret);
        let key = ratchet.base_secret();
        self.sender_keys.insert(sender_id, *key);
        if sender_id == self.user_id {
            self.own_key = Some(*key);
        }

        let mut ratchets = HashMap::new();
        ratchets.insert(sender_id, ratchet);

        // Merge with existing session ratchets if established
        if let SessionState::Established { epoch } = self.gateway_session.state {
            self.gateway_session.establish(epoch, ratchets);
        }

        Ok(())
    }

    // ─── Frame Encryption/Decryption ───────────────────────────────────

    /// Encrypt a raw media frame for sending.
    ///
    /// Automatically handles codec-specific unencrypted ranges and
    /// nonce management. The nonce auto-increments on each call.
    ///
    /// # Arguments
    ///
    /// * `key` — 16-byte AES-128 sender key
    /// * `raw_frame` — the unencrypted media frame
    /// * `codec` — the media codec (determines which bytes stay in the clear)
    ///
    /// # Errors
    ///
    /// Returns [`SigilError`] if encryption fails.
    pub fn encrypt_frame(
        &mut self,
        key: &[u8; KEY_LENGTH],
        raw_frame: &[u8],
        codec: Codec,
    ) -> Result<Vec<u8>, SigilError> {
        let nonce = self.gateway_session.next_nonce();
        let encryptor = FrameEncryptor::new(codec);
        encryptor.encrypt(key, nonce, raw_frame)
    }

    /// Encrypt a frame using our own cached key.
    ///
    /// Convenience method that uses the key previously exported/installed
    /// for our own user ID.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::NoSenderKey`] if no own key is cached.
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
    ///
    /// The frame footer contains all metadata needed for decryption
    /// (nonce, tag, unencrypted ranges), so no codec info is required.
    ///
    /// # Arguments
    ///
    /// * `key` — 16-byte AES-128 sender key for the frame's sender
    /// * `dave_frame` — the complete encrypted DAVE frame
    ///
    /// # Errors
    ///
    /// Returns [`SigilError`] if decryption or authentication fails.
    pub fn decrypt_frame(
        &self,
        key: &[u8; KEY_LENGTH],
        dave_frame: &[u8],
    ) -> Result<Vec<u8>, SigilError> {
        FrameDecryptor::decrypt(key, dave_frame)
    }

    /// Decrypt an incoming frame using a cached sender key.
    ///
    /// Looks up the key for the given sender ID from the internal cache.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::NoSenderKey`] if no key is cached for this sender.
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

    // ─── Gateway Events ────────────────────────────────────────────────

    /// Process a raw gateway event (opcode + payload).
    ///
    /// Dispatches the event through the DAVE handler and returns
    /// a structured [`DaveEvent`] for further processing.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError`] if the opcode is unknown or payload is malformed.
    pub fn handle_gateway_event(
        &self,
        opcode: u8,
        payload: &[u8],
    ) -> Result<DaveEvent, SigilError> {
        dispatch(opcode, payload)
    }

    /// Process an incoming commit and advance the epoch.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError`] if no group exists or commit processing fails.
    pub fn process_commit(&mut self, commit_bytes: &[u8]) -> Result<Epoch, SigilError> {
        let group = self.group.as_mut().ok_or(SigilError::GroupNotEstablished)?;
        group.process_commit(commit_bytes, &self.provider)?;
        Ok(group.current_epoch)
    }

    // ─── State Accessors ───────────────────────────────────────────────

    /// Returns `true` if the MLS group is established.
    pub fn is_established(&self) -> bool {
        self.group.is_some()
    }

    /// Returns the current MLS epoch, or `None` if no group is active.
    pub fn current_epoch(&self) -> Option<Epoch> {
        self.group.as_ref().map(|g| g.current_epoch)
    }

    /// Returns the current gateway session state.
    pub fn session_state(&self) -> &SessionState {
        &self.gateway_session.state
    }

    /// Returns a reference to the underlying MLS identity.
    pub fn identity(&self) -> &DaveIdentity {
        &self.identity
    }

    /// Returns a reference to the openmls crypto provider.
    pub fn provider(&self) -> &OpenMlsRustCrypto {
        &self.provider
    }

    /// Returns a reference to the underlying gateway session.
    pub fn gateway_session(&self) -> &DaveSession {
        &self.gateway_session
    }

    /// Returns a mutable reference to the gateway session.
    pub fn gateway_session_mut(&mut self) -> &mut DaveSession {
        &mut self.gateway_session
    }

    /// Returns a reference to the underlying MLS group, if established.
    pub fn mls_group(&self) -> Option<&DaveGroup> {
        self.group.as_ref()
    }

    /// Full reset: disconnect, clear keys, remove group.
    pub fn disconnect(&mut self) {
        self.gateway_session.reset();
        self.sender_keys.clear();
        self.own_key = None;
        self.group = None;
    }
}
