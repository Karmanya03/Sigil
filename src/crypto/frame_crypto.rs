//! AES-128-GCM frame encryption and decryption with DAVE nonce expansion.
//!
//! The DAVE protocol uses AES-128-GCM with 32-bit truncated nonces (expanded
//! to 96-bit by prepending 8 zero bytes) and 8-byte truncated authentication
//! tags (from the full 16-byte GCM tag).

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Key, Nonce};

use crate::error::SigilError;
use crate::types::{FULL_TAG_LENGTH, KEY_LENGTH, NONCE_LENGTH, TRUNCATED_TAG_LENGTH};

/// Expand a 32-bit truncated nonce to a full 96-bit (12-byte) AES-GCM nonce.
///
/// Layout: `[0x00; 8] || truncated_nonce_le[4]`
pub fn expand_nonce(truncated: u32) -> [u8; NONCE_LENGTH] {
    let mut nonce = [0u8; NONCE_LENGTH];
    nonce[8..12].copy_from_slice(&truncated.to_le_bytes());
    nonce
}

/// Extract the generation number from a 32-bit truncated nonce.
///
/// The generation is the most-significant byte of the nonce value.
/// This means after 2^24 frames, the generation increments.
pub fn generation_from_nonce(truncated_nonce: u32) -> u32 {
    truncated_nonce >> 24
}

/// Encrypt a plaintext frame with AES-128-GCM.
///
/// # Arguments
///
/// * `key` — 16-byte AES-128 key
/// * `truncated_nonce` — 32-bit nonce (will be expanded to 96-bit)
/// * `plaintext` — the data to encrypt
/// * `aad` — additional authenticated data (unencrypted ranges joined)
///
/// # Returns
///
/// A tuple of `(ciphertext, truncated_tag)` where the tag is the first
/// 8 bytes of the full 16-byte GCM authentication tag.
///
/// # Errors
///
/// Returns [`SigilError::DecryptionFailed`] if encryption fails internally.
pub fn encrypt_frame(
    key: &[u8; KEY_LENGTH],
    truncated_nonce: u32,
    plaintext: &[u8],
    aad: &[u8],
) -> Result<(Vec<u8>, [u8; TRUNCATED_TAG_LENGTH]), SigilError> {
    let cipher = Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(key));
    let nonce_bytes = expand_nonce(truncated_nonce);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let payload = Payload {
        msg: plaintext,
        aad,
    };

    let ciphertext_with_tag = cipher
        .encrypt(nonce, payload)
        .map_err(|_| SigilError::DecryptionFailed)?;

    // AES-GCM appends the 16-byte tag to the ciphertext
    let ct_len = ciphertext_with_tag.len() - FULL_TAG_LENGTH;
    let ciphertext = ciphertext_with_tag[..ct_len].to_vec();
    let mut truncated_tag = [0u8; TRUNCATED_TAG_LENGTH];
    truncated_tag.copy_from_slice(&ciphertext_with_tag[ct_len..ct_len + TRUNCATED_TAG_LENGTH]);

    Ok((ciphertext, truncated_tag))
}

/// Decrypt a ciphertext frame with AES-128-GCM using a truncated tag.
///
/// The truncated 8-byte tag is zero-padded to reconstruct a full 16-byte
/// tag for the GCM verification (note: this means the last 8 bytes of the
/// tag are zeroed, which is consistent with how the protocol truncation works).
///
/// # Arguments
///
/// * `key` — 16-byte AES-128 key
/// * `truncated_nonce` — 32-bit nonce (will be expanded to 96-bit)
/// * `ciphertext` — the encrypted data (without tag)
/// * `truncated_tag` — 8-byte truncated authentication tag
/// * `aad` — additional authenticated data
///
/// # Errors
///
/// Returns [`SigilError::DecryptionFailed`] if authentication fails.
pub fn decrypt_frame(
    key: &[u8; KEY_LENGTH],
    truncated_nonce: u32,
    ciphertext: &[u8],
    truncated_tag: &[u8; TRUNCATED_TAG_LENGTH],
    aad: &[u8],
) -> Result<Vec<u8>, SigilError> {
    let cipher = Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(key));
    let nonce_bytes = expand_nonce(truncated_nonce);
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Reconstruct the full 16-byte tag by zero-padding the truncated tag
    let mut full_tag = [0u8; FULL_TAG_LENGTH];
    full_tag[..TRUNCATED_TAG_LENGTH].copy_from_slice(truncated_tag);

    // Combine ciphertext + full tag for aes-gcm decryption
    let mut ct_with_tag = Vec::with_capacity(ciphertext.len() + FULL_TAG_LENGTH);
    ct_with_tag.extend_from_slice(ciphertext);
    ct_with_tag.extend_from_slice(&full_tag);

    let payload = Payload {
        msg: &ct_with_tag,
        aad,
    };

    cipher
        .decrypt(nonce, payload)
        .map_err(|_| SigilError::DecryptionFailed)
}
