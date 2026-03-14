//! DAVE-compliant MLS key package generation.
//!
//! Generates key packages locked to ciphersuite 2 and Basic credentials
//! only, as required by the DAVE protocol.

use openmls::prelude::*;
use openmls_rust_crypto::OpenMlsRustCrypto;

use crate::error::SigilError;
use crate::mls::config::DAVE_CIPHERSUITE;
use crate::mls::credential::DaveIdentity;

/// Generate an MLS key package for the given identity.
///
/// The key package is restricted to:
/// - Ciphersuite 2 (`MLS_128_DHKEMP256_AES128GCM_SHA256_P256`)
/// - Basic credential type only
///
/// Returns the generated [`KeyPackage`] (the public part of the bundle;
/// the private keys are stored in the provider automatically).
///
/// # Errors
///
/// Returns [`SigilError::Mls`] if key package generation fails.
pub fn generate_key_package(
    identity: &DaveIdentity,
    provider: &OpenMlsRustCrypto,
) -> Result<KeyPackage, SigilError> {
    // ── FIX: Accept all credential types to support Discord's custom credential type 16449 ──
    // Discord sends credential type Other(16449) for external senders, not Basic.
    // OpenMLS will reject proposals if the sender's credential type is not in the
    // accepted credential types list. By setting this to None (default), we accept
    // all credential types, which allows Discord's external sender proposals to be verified.
    let capabilities = Capabilities::new(
        None,                           // protocol versions (default)
        Some(&[DAVE_CIPHERSUITE]),      // only ciphersuite 2
        None,                           // extensions (default)
        None,                           // proposals (default)
        None,                           // accept all credential types (was: Some(&[CredentialType::Basic]))
    );

    let extensions = Extensions::default();

    let key_package_bundle = KeyPackage::builder()
        .key_package_extensions(extensions)
        .leaf_node_capabilities(capabilities)
        .build(
            DAVE_CIPHERSUITE,
            provider,
            &identity.signature_keys,
            identity.credential_with_key.clone(),
        )
        .map_err(|e| SigilError::Mls(format!("key package generation: {}", e)))?;

    Ok(key_package_bundle.key_package().clone())
}
