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
    // ── DEBUG LOGGING: External sender configuration ──
    tracing::debug!(
        "🔧 Building MLS group config with external sender:\n\
         - Credential type: {:?}\n\
         - Credential identity length: {} bytes\n\
         - Public key length: {} bytes\n\
         - Ciphersuite: {:?} (ECDSA P-256 + SHA-256)",
        external_sender_credential.credential_type(),
        external_sender_credential.serialized_content().len(),
        external_sender_pubkey.len(),
        DAVE_CIPHERSUITE
    );

    // Log the public key being converted to SignaturePublicKey
    if !external_sender_pubkey.is_empty() {
        let preview_len = external_sender_pubkey.len().min(8);
        tracing::debug!(
            "   - Converting {} bytes to SignaturePublicKey via From<Vec<u8>>",
            external_sender_pubkey.len()
        );
        tracing::debug!(
            "   - Key first {} bytes: {:02x?}",
            preview_len,
            &external_sender_pubkey[..preview_len]
        );
    }

    // Build the ExternalSender entry for the Discord voice gateway
    // NOTE: SignaturePublicKey::from(Vec<u8>) is used here.
    // This conversion may need investigation if signature verification fails.
    // OpenMLS expects raw P-256 public key bytes (65 bytes uncompressed or 33 bytes compressed).
    let signature_pubkey: SignaturePublicKey = external_sender_pubkey.into();
    
    // ── VERIFICATION: Log SignaturePublicKey details after conversion ──
    tracing::debug!(
        "   - SignaturePublicKey created:\n\
         - Converted key length: {} bytes\n\
         - Key bytes: {:02x?}",
        signature_pubkey.as_slice().len(),
        signature_pubkey.as_slice()
    );
    
    let external_sender = ExternalSender::new(
        signature_pubkey.clone(),
        external_sender_credential.clone(),
    );

    tracing::debug!("   - ExternalSender created successfully");

    // Register the gateway as an allowed external sender via group context extensions
    let extensions = Extensions::single(Extension::ExternalSenders(vec![
        external_sender.clone(),
    ])).map_err(|e| SigilError::Mls(format!("group context extensions: {}", e)))?;

    tracing::debug!("   - External senders extension created successfully");
    
    // ── ENHANCED LOGGING: Serialize and log the external senders extension ──
    use openmls::prelude::tls_codec::Serialize as TlsSerialize;
    match extensions.iter().find(|ext| matches!(ext, Extension::ExternalSenders(_))) {
        Some(Extension::ExternalSenders(senders)) => {
            tracing::debug!(
                "   - External senders extension contains {} sender(s)",
                senders.len()
            );
            
            // Serialize the extension to verify it contains expected data
            match extensions.tls_serialize_detached() {
                Ok(serialized) => {
                    tracing::debug!(
                        "   - Extension serialized successfully: {} bytes\n\
                         - First 32 bytes: {:02x?}",
                        serialized.len(),
                        &serialized[..serialized.len().min(32)]
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "⚠️  Failed to serialize external senders extension: {}",
                        e
                    );
                }
            }
            
            // Verify each sender in the extension
            for (idx, _sender) in senders.iter().enumerate() {
                tracing::debug!(
                    "   - External sender [{}]: registered in extension",
                    idx
                );
            }
        }
        _ => {
            tracing::warn!("⚠️  External senders extension not found in Extensions");
        }
    }

    let config = MlsGroupCreateConfig::builder()
        .ciphersuite(DAVE_CIPHERSUITE)
        .with_group_context_extensions(extensions)
        .build();

    // ── VERIFICATION: Log MLS group configuration details ──
    tracing::debug!(
        "   - MlsGroupCreateConfig built:\n\
         - Ciphersuite: {:?}\n\
         - Signature scheme (from ciphersuite): {:?}\n\
         - Group context extensions applied: {}",
        config.ciphersuite(),
        config.ciphersuite().signature_algorithm(),
        "ExternalSenders"
    );
    
    // ── INVESTIGATION: Check if additional extensions are needed ──
    // OpenMLS RFC 9420 specifies that external senders must be registered
    // in the group context extensions. The ciphersuite (MLS_128_DHKEMP256_AES128GCM_SHA256_P256)
    // implies ECDSA P-256 with SHA-256 for signatures.
    //
    // Potential additional extensions to investigate:
    // - RequiredCapabilities: Specifies required extensions/proposals/credentials
    // - ExternalPub: Public key for external joins (not needed for DAVE)
    // - ApplicationId: Application-specific identifier (not needed for DAVE)
    //
    // For DAVE, the ExternalSenders extension should be sufficient.
    // The signature scheme is determined by the ciphersuite and does not
    // require explicit configuration beyond the ciphersuite selection.
    
    tracing::debug!(
        "   - Signature verification will use:\n\
         - Algorithm: ECDSA P-256 (from ciphersuite)\n\
         - Hash: SHA-256 (from ciphersuite)\n\
         - No additional signature scheme configuration needed"
    );

    tracing::info!("✅ MLS group config built with external sender registered");

    Ok(config)
}
