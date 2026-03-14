//! Full MLS group lifecycle for DAVE sessions.
//!
//! Manages group creation, joining via Welcome, processing proposals
//! and commits, exporting per-sender keys, and epoch tracking.

use openmls::prelude::tls_codec::{
    DeserializeBytes as TlsDeserializeBytes, Serialize as TlsSerialize,
};
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;

use crate::error::SigilError;
use crate::mls::credential::DaveIdentity;
use crate::types::{Epoch, KEY_LENGTH, SENDER_KEY_LABEL, UserId};

// ── GROUP_ID const removed ── //
// Discord embeds its own group ID inside every OP 27 proposal. We must
// extract it from the proposal bytes and pass it to `DaveGroup::create()`.
// See `extract_group_id_from_proposals()` below.

// ---------------------------------------------------------------------------
// Free-standing helper
// ---------------------------------------------------------------------------

/// Parse the MLS group ID out of the first parseable non-empty proposal.
///
/// Discord's SFU creates the group ID and embeds it in every MLS message
/// header. We must read it from the OP 27 payload and use it when
/// constructing our local `MlsGroup`; otherwise every `process_message()`
/// call fails with a group-ID mismatch error.
///
/// # Returns
///
/// `Some(GroupId)` from the first proposal that deserialises successfully,
/// or `None` if the slice is empty or no message can be parsed.
pub fn extract_group_id_from_proposals(proposals_bytes: &[Vec<u8>]) -> Option<GroupId> {
    for data in proposals_bytes {
        if data.is_empty() {
            continue;
        }
        if let Ok(msg) = MlsMessageIn::tls_deserialize_exact_bytes(data) {
            // In openmls 0.8, MlsMessageIn does not have group_id() directly.
            // Convert to ProtocolMessage first, which does expose group_id().
            if let Ok(pm) = msg.try_into_protocol_message() {
                return Some(pm.group_id().clone());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// DaveGroup
// ---------------------------------------------------------------------------

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

impl DaveGroup {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    /// Create a new MLS group as the initial member.
    ///
    /// `group_id` **must** be extracted from Discord's OP 27 proposal bytes
    /// via [`extract_group_id_from_proposals`] before calling this.  Passing
    /// an arbitrary or empty ID will cause every subsequent
    /// `process_message()` call to fail with a group-ID mismatch.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::Mls`] if group creation fails.
    pub fn create(
        identity: &DaveIdentity,
        provider: &OpenMlsRustCrypto,
        config: &MlsGroupCreateConfig,
        group_id: &[u8], // ← Discord's actual group ID, extracted from OP 27
    ) -> Result<Self, SigilError> {
        let group_id = GroupId::from_slice(group_id);

        tracing::info!(
            "DAVE: creating MLS group with group_id = {:02x?}",
            group_id.as_slice()
        );

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
            None,
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

    // -----------------------------------------------------------------------
    // Group-ID recovery (quick-hack path)
    // -----------------------------------------------------------------------

    /// Tear down and recreate the local MLS group using `group_id`.
    ///
    /// Use this as a last-resort recovery when `process_proposals` detects
    /// that Discord's group ID does not match the one we created the group
    /// with.  Prefer the deferred-creation path in `driver.rs` (call
    /// `extract_group_id_from_proposals` before `DaveGroup::create`) instead
    /// of relying on this method.
    ///
    /// After calling this, retry `process_proposals` with the same data.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::Mls`] if recreating the group fails.
    pub fn recreate_with_group_id(
        &mut self,
        identity: &DaveIdentity,
        provider: &OpenMlsRustCrypto,
        config: &MlsGroupCreateConfig,
        group_id: GroupId,
    ) -> Result<(), SigilError> {
        tracing::warn!(
            "DAVE: recreating MLS group — old={:02x?}  new={:02x?}",
            self.mls_group.group_id().as_slice(),
            group_id.as_slice()
        );

        let new_group = MlsGroup::new_with_group_id(
            provider,
            &identity.signature_keys,
            config,
            group_id,
            identity.credential_with_key.clone(),
        )
        .map_err(|e| SigilError::Mls(format!("recreate group: {}", e)))?;

        self.mls_group = new_group;
        self.current_epoch = self.mls_group.epoch().as_u64();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Epoch management
    // -----------------------------------------------------------------------

    /// Merge our own pending commit so the local group state advances
    /// to the new epoch.
    pub fn merge_own_pending_commit(
        &mut self,
        provider: &OpenMlsRustCrypto,
    ) -> Result<(), SigilError> {
        self.mls_group
            .merge_pending_commit(provider)
            .map_err(|e| SigilError::Mls(format!("merge_pending_commit: {:?}", e)))?;
        self.current_epoch = self.mls_group.epoch().as_u64();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Proposals
    // -----------------------------------------------------------------------

    /// Process incoming proposals from the delivery service.
    ///
    /// Each proposal is deserialized individually. If a proposal uses an
    /// unknown MLS extension type (e.g., Discord's custom proposal type 16),
    /// it is skipped gracefully instead of failing the entire batch.
    ///
    /// After `process_message()` succeeds, the result is checked for
    /// `ProcessedMessageContent::ProposalMessage`. If found, it is stored
    /// via `store_pending_proposal(provider.storage(), ...)` so that
    /// `has_pending_proposals()` returns `true` and
    /// `commit_to_pending_proposals()` actually has something to commit.
    ///
    /// # Return value
    ///
    /// Returns `Ok(true)` if Discord sent proposals that could not be
    /// deserialized (e.g. custom type 16). The caller **must** respond with
    /// an empty commit via [`Self::commit_empty`] so that the MLS epoch
    /// advances and Discord delivers the epoch key. Without this commit,
    /// Discord never sends the key and DAVE times out.
    ///
    /// Returns `Ok(false)` when every proposal was either processed
    /// successfully or was genuinely empty/padding — no empty commit needed.
    pub fn process_proposals(
        &mut self,
        proposals_bytes: &[Vec<u8>],
        provider: &OpenMlsRustCrypto,
    ) -> Result<bool, SigilError> {
        let mut processed_count = 0u32;
        let mut skipped_count = 0u32;
        // Tracks non-empty proposals that failed to deserialize.
        // These are Discord custom types (e.g. type 16) that OpenMLS does not
        // know about. Even though we cannot store them as pending proposals,
        // Discord still expects us to send a commit to advance the epoch and
        // unlock the epoch key delivery. The caller uses this flag to decide
        // whether to call commit_empty().
        let mut had_unrecognized_nonempty = false;

        for (i, proposal_data) in proposals_bytes.iter().enumerate() {
            if proposal_data.is_empty() {
                tracing::debug!("Skipping empty proposal at index {}", i);
                skipped_count += 1;
                continue;
            }

            let mls_msg_in = match MlsMessageIn::tls_deserialize_exact_bytes(proposal_data) {
                Ok(msg) => msg,
                Err(e) => {
                    tracing::debug!(
                        "Skipping unrecognized proposal at index {} (deserialize): {} \
                         [first bytes: {:02x?}] — will need empty commit to unblock epoch",
                        i,
                        e,
                        &proposal_data[..proposal_data.len().min(8)]
                    );
                    skipped_count += 1;
                    // Non-empty but undeserializable → Discord custom type.
                    // Signal to the caller that an empty commit is required.
                    had_unrecognized_nonempty = true;
                    continue;
                }
            };

            let protocol_message = match mls_msg_in.try_into_protocol_message() {
                Ok(pm) => pm,
                Err(e) => {
                    tracing::debug!("Skipping non-protocol proposal at index {}: {}", i, e);
                    skipped_count += 1;
                    had_unrecognized_nonempty = true;
                    continue;
                }
            };

            // ── PRE-VERIFICATION DEBUGGING: Log group's external senders before process_message() ──
            // This helps diagnose whether external senders are properly registered
            let group_context = self.mls_group.export_group_context();
            let external_senders = group_context
                .extensions()
                .iter()
                .find_map(|ext| {
                    if let Extension::ExternalSenders(senders) = ext {
                        Some(senders)
                    } else {
                        None
                    }
                });
            
            if let Some(senders) = external_senders {
                tracing::debug!(
                    "🔍 Pre-verification check for proposal at index {}:\n\
                     - Registered external senders in group: {}\n\
                     - Group has external senders extension: YES",
                    i,
                    senders.len()
                );
                
                // Log each registered external sender (serialize to see contents)
                for (idx, ext_sender) in senders.iter().enumerate() {
                    // Serialize the external sender to inspect its contents
                    use openmls::prelude::tls_codec::Serialize as TlsSerialize;
                    match ext_sender.tls_serialize_detached() {
                        Ok(serialized) => {
                            tracing::debug!(
                                "   - External sender [{}]:\n\
                                 - Serialized length: {} bytes\n\
                                 - First 16 bytes: {:02x?}",
                                idx,
                                serialized.len(),
                                &serialized[..serialized.len().min(16)]
                            );
                        }
                        Err(e) => {
                            tracing::debug!(
                                "   - External sender [{}]: Failed to serialize: {}",
                                idx,
                                e
                            );
                        }
                    }
                }
            } else {
                tracing::debug!(
                    "🔍 Pre-verification check for proposal at index {}:\n\
                     - Group has NO external senders extension",
                    i
                );
            }

            match self.mls_group.process_message(provider, protocol_message) {
                Ok(processed_msg) => {
                    match processed_msg.into_content() {
                        ProcessedMessageContent::ProposalMessage(staged_proposal) => {
                            let _ = self
                                .mls_group
                                .store_pending_proposal(provider.storage(), *staged_proposal);
                            processed_count += 1;
                            tracing::debug!(
                                "Stored pending proposal at index {} (total pending: {})",
                                i,
                                processed_count
                            );
                        }
                        ProcessedMessageContent::StagedCommitMessage(_) => {
                            tracing::debug!(
                                "Received commit as proposal at index {} — unexpected but not fatal",
                                i
                            );
                            processed_count += 1;
                        }
                        other => {
                            tracing::debug!(
                                "Proposal at index {} yielded unexpected content type: {:?}",
                                i,
                                std::mem::discriminant(&other)
                            );
                        }
                    }
                }
                Err(e) => {
                    // ── ENHANCED ERROR LOGGING for signature verification failures ──
                    let error_msg = format!("{}", e);
                    
                    if error_msg.contains("Verifying the signature failed") 
                        || error_msg.contains("signature") 
                    {
                        tracing::error!(
                            "❌ SIGNATURE VERIFICATION FAILED at proposal index {}:\n\
                             - Error: {}\n\
                             - Proposal data length: {} bytes\n\
                             - First 16 bytes: {:02x?}\n\
                             - This is the BUG CONDITION: external sender proposal signature verification failure",
                            i,
                            e,
                            proposal_data.len(),
                            &proposal_data[..proposal_data.len().min(16)]
                        );
                        
                        // Log current group state for debugging
                        tracing::error!(
                            "   - Current MLS group epoch: {}\n\
                             - Group ID: {:02x?}\n\
                             - Number of group members: {}",
                            self.current_epoch,
                            self.mls_group.group_id().as_slice(),
                            self.mls_group.members().count()
                        );
                        
                        // ── DETAILED SIGNATURE DEBUGGING ──
                        tracing::error!(
                            "   🔍 SIGNATURE VERIFICATION DETAILS:\n\
                             - Extracting external sender information for comparison..."
                        );
                        
                        // Log the group's external senders for comparison
                        let group_context = self.mls_group.export_group_context();
                        if let Some(Extension::ExternalSenders(senders)) = group_context
                            .extensions()
                            .iter()
                            .find(|ext| matches!(ext, Extension::ExternalSenders(_)))
                        {
                            tracing::error!(
                                "   - Expected external sender(s) registered in group: {}",
                                senders.len()
                            );
                            
                            // Serialize each external sender to inspect contents
                            use openmls::prelude::tls_codec::Serialize as TlsSerialize;
                            for (idx, ext_sender) in senders.iter().enumerate() {
                                match ext_sender.tls_serialize_detached() {
                                    Ok(serialized) => {
                                        tracing::error!(
                                            "   - Expected external sender [{}]:\n\
                                             - Serialized length: {} bytes\n\
                                             - Full serialized data: {:02x?}",
                                            idx,
                                            serialized.len(),
                                            serialized
                                        );
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "   - Expected external sender [{}]: Failed to serialize: {}",
                                            idx,
                                            e
                                        );
                                    }
                                }
                            }
                        } else {
                            tracing::error!(
                                "   ⚠️  NO external senders registered in group context!\n\
                                 - This means set_external_sender() was not called or failed\n\
                                 - Or the external senders extension was not properly applied"
                            );
                        }
                        
                        // Log the ciphersuite and signature algorithm
                        tracing::error!(
                            "   - Ciphersuite: {:?}\n\
                             - Signature algorithm: {:?}\n\
                             - Expected signature scheme: ECDSA P-256 with SHA-256",
                            self.mls_group.ciphersuite(),
                            self.mls_group.ciphersuite().signature_algorithm()
                        );
                        
                        // Log diagnostic information about the proposal message
                        tracing::error!(
                            "   📋 PROPOSAL MESSAGE DIAGNOSTICS:\n\
                             - Message type: Protocol Message\n\
                             - Proposal data (first 64 bytes hex): {}",
                            proposal_data.iter()
                                .take(64)
                                .map(|b| format!("{:02x}", b))
                                .collect::<Vec<_>>()
                                .join(" ")
                        );
                        
                        tracing::error!(
                            "   🔧 DEBUGGING RECOMMENDATIONS:\n\
                             - Check if external sender public key format is correct (65 bytes uncompressed or 33 bytes compressed)\n\
                             - Verify credential in OP 25 matches credential in OP 27 proposal\n\
                             - Confirm SignaturePublicKey conversion in set_external_sender() is correct\n\
                             - Verify no serenity/poise interference with DAVE opcodes\n\
                             - Check if TLS deserialization of SignaturePublicKey in set_external_sender() succeeded"
                        );
                    } else {
                        tracing::debug!(
                            "Skipping proposal at index {} that failed processing: {}",
                            i,
                            e
                        );
                    }
                    
                    skipped_count += 1;
                    had_unrecognized_nonempty = true;
                    continue;
                }
            }
        }

        tracing::info!(
            "process_proposals: {} processed, {} skipped out of {} total \
             (needs_empty_commit={})",
            processed_count,
            skipped_count,
            proposals_bytes.len(),
            had_unrecognized_nonempty && processed_count == 0,
        );

        // Only signal "needs empty commit" when we have NO processable pending
        // proposals AND Discord sent us something non-empty that we couldn't
        // handle. If we stored real proposals, commit_pending() covers it.
        Ok(had_unrecognized_nonempty && processed_count == 0)
    }

    /// Check if there are pending proposals waiting to be committed.
    pub fn has_pending_proposals(&self) -> bool {
        self.mls_group.pending_proposals().next().is_some()
    }

    // -----------------------------------------------------------------------
    // Commits
    // -----------------------------------------------------------------------

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
        signer: &SignatureKeyPair,
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

    /// Create an **empty** commit (no pending proposals).
    ///
    /// Discord's DAVE handshake sends a custom proposal type (type 16) that
    /// OpenMLS cannot deserialize. Because we skip it, no proposals are stored
    /// and `commit_to_pending_proposals` would be a no-op.
    ///
    /// However, Discord still requires us to send *a* commit so that:
    /// 1. The MLS epoch advances on both sides.
    /// 2. Discord knows we have processed (or acknowledged) the proposal batch.
    /// 3. Discord then delivers the epoch key, unblocking DAVE E2EE.
    ///
    /// Call this when `process_proposals` returns `Ok(true)` AND
    /// `has_pending_proposals()` is `false`.
    ///
    /// After sending the commit bytes over the wire the caller must also call
    /// `merge_own_pending_commit` to advance the local epoch.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::Mls`] if the empty commit cannot be created or
    /// serialized.
    pub fn commit_empty(
        &mut self,
        provider: &OpenMlsRustCrypto,
        signer: &SignatureKeyPair,
    ) -> Result<Vec<u8>, SigilError> {
        tracing::info!(
            "DAVE: creating empty commit to advance epoch past unrecognized proposals \
             (current epoch={})",
            self.current_epoch
        );

        // commit_to_pending_proposals with an empty pending list produces a
        // valid self-update commit — exactly what Discord needs to advance the
        // epoch and deliver the key.
        let (commit, _welcome, _group_info) = self
            .mls_group
            .commit_to_pending_proposals(provider, signer)
            .map_err(|e| SigilError::Mls(format!("empty commit: {}", e)))?;

        let commit_bytes = commit
            .tls_serialize_detached()
            .map_err(|e| SigilError::Mls(format!("empty commit serialize: {}", e)))?;

        tracing::info!(
            "DAVE: empty commit created ({} bytes) — merge after sending",
            commit_bytes.len()
        );

        Ok(commit_bytes)
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

        if let ProcessedMessageContent::StagedCommitMessage(staged_commit) =
            processed.into_content()
        {
            self.mls_group
                .merge_staged_commit(provider, *staged_commit)
                .map_err(|e| SigilError::Mls(format!("merge commit: {}", e)))?;
            self.current_epoch = self.mls_group.epoch().as_u64();
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Key export
    // -----------------------------------------------------------------------

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

        let label = std::str::from_utf8(SENDER_KEY_LABEL)
            .map_err(|e| SigilError::Mls(format!("label conversion: {}", e)))?;

        let exported = self
            .mls_group
            .export_secret(provider.crypto(), label, &context, KEY_LENGTH)
            .map_err(|e| SigilError::Mls(format!("export sender key: {}", e)))?;

        let mut key = [0u8; KEY_LENGTH];
        key.copy_from_slice(&exported[..KEY_LENGTH]);
        Ok(key)
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    /// Returns the current MLS epoch number.
    pub fn epoch(&self) -> Epoch {
        self.current_epoch
    }

    /// Returns the epoch authenticator for the current epoch.
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

    /// Extract all Discord user IDs from the current MLS group members.
    ///
    /// Each member's Basic credential encodes the user ID as a big-endian u64.
    /// Members whose credentials cannot be decoded are silently skipped.
    pub fn member_user_ids(&self) -> Vec<UserId> {
        self.mls_group
            .members()
            .filter_map(|member| {
                let identity = member.credential.serialized_content();
                if identity.len() == 8 {
                    let mut bytes = [0u8; 8];
                    bytes.copy_from_slice(identity);
                    Some(u64::from_be_bytes(bytes))
                } else {
                    tracing::warn!(
                        "Skipping member with unexpected credential length: {} bytes",
                        identity.len()
                    );
                    None
                }
            })
            .collect()
    }
}
