//! DAVE identity credential: Basic MLS credential with Discord user ID.
//!
//! The credential identity is the big-endian 64-bit representation of the
//! Discord user snowflake ID, as specified by the DAVE protocol.

use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;

use crate::error::SigilError;
use crate::mls::config::DAVE_CIPHERSUITE;
use crate::types::UserId;

/// A DAVE participant identity consisting of an MLS Basic credential,
/// the associated signature keypair, and the Discord user ID.
pub struct DaveIdentity {
    /// Discord user snowflake ID.
    pub user_id: UserId,
    /// The MLS credential with the associated public signature key.
    pub credential_with_key: CredentialWithKey,
    /// The P-256 signature keypair for signing MLS messages.
    pub signature_keys: SignatureKeyPair,
}

impl DaveIdentity {
    /// Create a new DAVE identity for the given user ID.
    ///
    /// Generates a P-256 signature keypair, creates a Basic credential
    /// with the big-endian u64 user ID as the identity bytes, and stores
    /// the keypair in the provider's key store.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::Mls`] if key generation or storage fails.
    pub fn new(user_id: UserId, provider: &OpenMlsRustCrypto) -> Result<Self, SigilError> {
        // Create Basic credential with BE u64 identity
        let identity = user_id.to_be_bytes().to_vec();
        let basic_credential = BasicCredential::new(identity);
        let credential: Credential = basic_credential.into();

        // Generate P-256 signature keypair
        let signature_scheme = DAVE_CIPHERSUITE.signature_algorithm();
        let signature_keys = SignatureKeyPair::new(signature_scheme)
            .map_err(|e| SigilError::Mls(format!("keypair generation: {}", e)))?;

        // Store in the provider's key store
        signature_keys
            .store(provider.storage())
            .map_err(|e| SigilError::Mls(format!("key store: {}", e)))?;

        let credential_with_key = CredentialWithKey {
            credential,
            signature_key: signature_keys.public().into(),
        };

        Ok(Self {
            user_id,
            credential_with_key,
            signature_keys,
        })
    }

    /// Extract the Discord user ID from a Basic credential's identity bytes.
    ///
    /// The identity is expected to be an 8-byte big-endian u64.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::Mls`] if the identity is not exactly 8 bytes.
    pub fn user_id_from_credential(credential: &Credential) -> Result<UserId, SigilError> {
        let identity = credential.serialized_content();
        if identity.len() != 8 {
            return Err(SigilError::Mls(format!(
                "expected 8-byte identity, got {} bytes",
                identity.len()
            )));
        }
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(identity);
        Ok(u64::from_be_bytes(bytes))
    }
}
