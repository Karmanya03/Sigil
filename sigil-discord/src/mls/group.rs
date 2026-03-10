//! Full MLS group lifecycle for DAVE sessions.
//!
//! Manages group creation, joining via Welcome, processing proposals
//! and commits, exporting per-sender keys, and epoch tracking.

use openmls::prelude::tls_codec::{
    DeserializeBytes as TlsDeserializeBytes, Serialize as TlsSerialize,
};
use openmls::prelude::*;
use openmls_rust_crypto::OpenMlsRustCrypto;

use crate::error::SigilError;
use crate::mls::credential::DaveIdentity;
use crate::types::{Epoch, KEY_LENGTH, SENDER_KEY_LABEL, UserId};

/// An MLS group for a DAVE voice session.
///
/// Wraps the underlying `MlsGroup` and tracks the current epoch and
/// the local identity.
pub struct DaveGroup {
    /// The underlying OpenMLS group.
    mls_group: MlsGroup,
    /// The current MLS epoch number.
    pub current_epoch: Epoch,
    /// Reference back to our identity (user_id only, keys stored in provider).
    identity_user_id: UserId,
}

/// The group ID used for all Sigil DAVE groups.
const GROUP_ID: &[u8] = b"sigil-dave";

impl DaveGroup {
    /// Create a new MLS group as the initial member.
    ///
    /// # Errors
    ///
    pub fn merge_own_pending_commit(
    &mut self,
    provider: &OpenMlsRustCrypto,
) -> Result<(), SigilError> {
    self.mls_group.merge_pending_commit(provider)
        .map_err(|e| SigilError::Mls(format!("merge_pending_commit: {:?}", e)))?;
    self.current_epoch = self.mls_group.epoch().as_u64();
    Ok(())
}
    /// Returns [`SigilError::Mls`] if group creation fails.
    pub fn create(
        identity: &DaveIdentity,
        provider: &OpenMlsRustCrypto,
        config: &MlsGroupCreateConfig,
    ) -> Result<Self, SigilError> {
        let group_id = GroupId::from_slice(GROUP_ID);

        let mls_group = MlsGroup::new_with_group_id(
            provider,
            &identity.signature_keys,
            config,
            group_id,
            identity.credential_with_key.clone(),
        )
        .map_err(|e| SigilError::Mls(format!("group creation: {}", e)))?;

        let current_epoch = mls_group.epoch().as_u64();

        Ok(Self {
            mls_group,
            current_epoch,
            identity_user_id: identity.user_id,
        })
    }

    /// Join an existing group via a Welcome message.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::Mls`] if the Welcome cannot be processed.
    pub fn join_from_welcome(
        identity: &DaveIdentity,
        provider: &OpenMlsRustCrypto,
        _config: &MlsGroupCreateConfig,
        welcome: MlsMessageIn,
    ) -> Result<Self, SigilError> {
        let welcome_msg = welcome
            .into_welcome()
            .ok_or_else(|| SigilError::Mls("message is not a Welcome".to_string()))?;

        let mls_group = StagedWelcome::new_from_welcome(
            provider,
            &MlsGroupJoinConfig::default(),
            welcome_msg,
            None, // no ratchet tree needed, included in Welcome
        )
        .map_err(|e| SigilError::Mls(format!("staged welcome: {}", e)))?
        .into_group(provider)
        .map_err(|e| SigilError::Mls(format!("welcome into group: {}", e)))?;

        let current_epoch = mls_group.epoch().as_u64();

        Ok(Self {
            mls_group,
            current_epoch,
            identity_user_id: identity.user_id,
        })
    }

    /// Process incoming proposals from the delivery service.
    ///
    /// Each proposal is deserialized from bytes and processed by the group.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::Mls`] if deserialization or processing fails.
    pub fn process_proposals(
        &mut self,
        proposals_bytes: &[Vec<u8>],
        provider: &OpenMlsRustCrypto,
    ) -> Result<(), SigilError> {
        for proposal_data in proposals_bytes {
            let mls_msg_in = MlsMessageIn::tls_deserialize_exact_bytes(proposal_data)
                .map_err(|e| SigilError::Mls(format!("proposal deserialize: {}", e)))?;

            let protocol_message: ProtocolMessage = mls_msg_in
                .try_into_protocol_message()
                .map_err(|e| SigilError::Mls(format!("not a protocol message: {}", e)))?;

            self.mls_group
                .process_message(provider, protocol_message)
                .map_err(|e| SigilError::Mls(format!("process proposal: {}", e)))?;
        }
        Ok(())
    }

    /// Create a commit for pending proposals.
    ///
    /// Returns the serialized commit message and an optional Welcome message
    /// (present when the commit adds new members).
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::Mls`] if commit creation fails.
    pub fn commit_pending(
        &mut self,
        provider: &OpenMlsRustCrypto,
        signer: &openmls_basic_credential::SignatureKeyPair,
    ) -> Result<(Vec<u8>, Option<Vec<u8>>), SigilError> {
        let (commit, welcome, _group_info) = self
            .mls_group
            .commit_to_pending_proposals(provider, signer)
            .map_err(|e| SigilError::Mls(format!("commit pending: {}", e)))?;

        let commit_bytes = commit
            .tls_serialize_detached()
            .map_err(|e| SigilError::Mls(format!("commit serialize: {}", e)))?;

        let welcome_bytes = welcome
            .map(|w| {
                w.tls_serialize_detached()
                    .map_err(|e| SigilError::Mls(format!("welcome serialize: {}", e)))
            })
            .transpose()?;

        Ok((commit_bytes, welcome_bytes))
    }

    /// Process an incoming commit message and advance the epoch.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::Mls`] if the commit is invalid or processing fails.
    pub fn process_commit(
        &mut self,
        commit_bytes: &[u8],
        provider: &OpenMlsRustCrypto,
    ) -> Result<(), SigilError> {
        let mls_msg_in = MlsMessageIn::tls_deserialize_exact_bytes(commit_bytes)
            .map_err(|e| SigilError::Mls(format!("commit deserialize: {}", e)))?;

        let protocol_message: ProtocolMessage = mls_msg_in
            .try_into_protocol_message()
            .map_err(|e| SigilError::Mls(format!("not a protocol message: {}", e)))?;

        let processed = self
            .mls_group
            .process_message(provider, protocol_message)
            .map_err(|e| SigilError::Mls(format!("process commit: {}", e)))?;

        // If it's a staged commit, merge it
        if let ProcessedMessageContent::StagedCommitMessage(staged_commit) =
            processed.into_content()
        {
            self.mls_group
                .merge_staged_commit(provider, *staged_commit)
                .map_err(|e| SigilError::Mls(format!("merge commit: {}", e)))?;
        }

        self.current_epoch = self.mls_group.epoch().as_u64();
        Ok(())
    }

    /// Export the per-sender encryption key for a given sender user ID.
    ///
    /// Uses MLS-Exporter with:
    /// - Label: `"Discord Secure Frames v0"`
    /// - Context: little-endian u64 sender ID
    /// - Length: 16 bytes
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::Mls`] if the export fails.
    pub fn export_sender_key(
        &self,
        sender_id: UserId,
        provider: &OpenMlsRustCrypto,
    ) -> Result<[u8; KEY_LENGTH], SigilError> {
        let context = sender_id.to_le_bytes();

        // SENDER_KEY_LABEL is &[u8] = b"Discord Secure Frames v0" (from types.rs)
        // This MUST match Discord's expected label exactly!
        // export_secret expects &str, so convert
        let label = std::str::from_utf8(SENDER_KEY_LABEL)
            .map_err(|e| SigilError::Mls(format!("label conversion: {}", e)))?;

        let exported = self
            .mls_group
            .export_secret(provider, label, &context, KEY_LENGTH)
            .map_err(|e| SigilError::Mls(format!("export sender key: {}", e)))?;

        let mut key = [0u8; KEY_LENGTH];
        key.copy_from_slice(&exported[..KEY_LENGTH]);
        Ok(key)
    }

    /// Returns the epoch authenticator for the current epoch.
    ///
    /// This is used for out-of-band epoch verification between participants.
    pub fn epoch_authenticator(&self) -> &[u8] {
        self.mls_group.epoch_authenticator().as_slice()
    }

    /// Returns `true` if this member is the only member of the group.
    pub fn is_sole_member(&self) -> bool {
        self.mls_group.members().count() == 1
    }

    /// Returns the Discord user ID associated with this group member.
    pub fn user_id(&self) -> UserId {
        self.identity_user_id
    }

    /// Returns a reference to the underlying OpenMLS group.
    pub fn mls_group(&self) -> &MlsGroup {
        &self.mls_group
    }

    /// Returns a mutable reference to the underlying OpenMLS group.
    pub fn mls_group_mut(&mut self) -> &mut MlsGroup {
        &mut self.mls_group
    }
}
