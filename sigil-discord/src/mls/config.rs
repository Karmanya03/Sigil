//! MLS ciphersuite, crypto provider, and group configuration for DAVE.
//!
//! DAVE uses ciphersuite 2: `MLS_128_DHKEMP256_AES128GCM_SHA256_P256`.

use openmls::prelude::*;
use openmls_rust_crypto::OpenMlsRustCrypto;

use crate::error::SigilError;

/// DAVE MLS ciphersuite: P-256 + AES-128-GCM + SHA-256.
pub const DAVE_CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256;

/// Create a new OpenMLS crypto provider instance.
///
/// Returns the default Rust-native crypto backend.
pub fn crypto_provider() -> OpenMlsRustCrypto {
    OpenMlsRustCrypto::default()
}

/// Build an MLS group creation configuration with the external sender extension.
///
/// The external sender is the Discord voice gateway server, which needs the
/// ability to create Remove proposals for disconnected members. The DAVE
/// protocol requires the gateway's credential and signature public key to be
/// registered as an external sender in the group context extensions.
///
/// # Arguments
///
/// * `external_sender_credential` — the Basic credential of the voice gateway
/// * `external_sender_pubkey` — the P-256 signature public key of the gateway
///
/// # Errors
///
/// Returns [`SigilError::Mls`] if the extension or configuration builder fails.
pub fn build_group_config(
    external_sender_credential: Credential,
    external_sender_pubkey: Vec<u8>,
) -> Result<MlsGroupCreateConfig, SigilError> {
    // Build the ExternalSender entry for the Discord voice gateway
    let external_sender = ExternalSender::new(
        external_sender_pubkey.into(), // SignaturePublicKey implements From<Vec<u8>>
        external_sender_credential,
    );

    // Register the gateway as an allowed external sender via group context extensions
    let extensions = Extensions::single(Extension::ExternalSenders(vec![
        external_sender,
    ])).map_err(|e| SigilError::Mls(format!("group context extensions: {}", e)))?;

    let config = MlsGroupCreateConfig::builder()
        .ciphersuite(DAVE_CIPHERSUITE)
        .with_group_context_extensions(extensions)
        .build();

    Ok(config)
}
