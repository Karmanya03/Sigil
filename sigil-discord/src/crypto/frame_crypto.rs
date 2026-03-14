//! AES-128-GCM frame encryption and decryption with DAVE nonce expansion.
//!
//! The DAVE protocol uses AES-128-GCM with 32-bit truncated nonces (expanded
//! to 96-bit by prepending 8 zero bytes) and 8-byte truncated authentication
//! tags (from the full 16-byte GCM tag).

use aes_gcm::aead::{AeadInPlace, KeyInit, Payload};
use aes_gcm::aead::Aead;
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
pub fn generation_from_nonce(truncated_nonce: u32) -> u32 {
    truncated_nonce >> 24
}

/// Encrypt a plaintext frame with AES-128-GCM.
///
/// Returns `(ciphertext, truncated_tag)` where the tag is the first 8 bytes
/// of the full 16-byte GCM authentication tag.
pub fn encrypt_frame(
    key: &[u8; KEY_LENGTH],
    truncated_nonce: u32,
    plaintext: &[u8],
    aad: &[u8],
) -> Result<(Vec<u8>, [u8; TRUNCATED_TAG_LENGTH]), SigilError> {
    let cipher = Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(key));
    let nonce_bytes = expand_nonce(truncated_nonce);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let payload = Payload { msg: plaintext, aad };
    let ciphertext_with_tag = cipher
        .encrypt(nonce, payload)
        .map_err(|_| SigilError::DecryptionFailed)?;

    let ct_len = ciphertext_with_tag.len() - FULL_TAG_LENGTH;
    let ciphertext = ciphertext_with_tag[..ct_len].to_vec();
    let mut truncated_tag = [0u8; TRUNCATED_TAG_LENGTH];
    truncated_tag.copy_from_slice(&ciphertext_with_tag[ct_len..ct_len + TRUNCATED_TAG_LENGTH]);

    Ok((ciphertext, truncated_tag))
}

/// Decrypt a ciphertext frame with AES-128-GCM using a truncated tag.
///
/// Reconstructs the full 16-byte GCM tag by re-deriving it from the plaintext:
/// 1. Decrypt the ciphertext using AES-CTR (via `encrypt_in_place_detached` on
///    the ciphertext, which XORs with the same keystream to recover plaintext).
/// 2. Re-encrypt the recovered plaintext to get the full GCM tag.
/// 3. Verify the first TRUNCATED_TAG_LENGTH bytes match `truncated_tag`.
///
/// This correctly implements DAVE's 8-byte truncated tag scheme.
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

    // Step 1: Recover plaintext by XOR-ing ciphertext with the AES-CTR keystream.
    // AES-GCM uses CTR mode: ciphertext = plaintext XOR keystream.
    // encrypt_in_place_detached(ciphertext) = ciphertext XOR keystream = plaintext.
    let mut plaintext = ciphertext.to_vec();
    let _dummy_tag = cipher
        .encrypt_in_place_detached(nonce, aad, &mut plaintext)
        .map_err(|_| SigilError::DecryptionFailed)?;
    // plaintext now holds: ciphertext XOR keystream = original plaintext ✓

    // Step 2: Re-encrypt the recovered plaintext to get the authentic full tag.
    let payload = Payload { msg: &plaintext, aad };
    let ct_with_tag = cipher
        .encrypt(nonce, payload)
        .map_err(|_| SigilError::DecryptionFailed)?;

    // Step 3: Verify the truncated tag matches the first 8 bytes of the real tag.
    let ct_len = ct_with_tag.len() - FULL_TAG_LENGTH;
    let real_truncated = &ct_with_tag[ct_len..ct_len + TRUNCATED_TAG_LENGTH];

    // Constant-time comparison to prevent timing attacks
    use std::hint::black_box;
    let mut diff = 0u8;
    for (a, b) in truncated_tag.iter().zip(real_truncated.iter()) {
        diff |= black_box(a ^ b);
    }
    if diff != 0 {
        return Err(SigilError::DecryptionFailed);
    }

    Ok(plaintext)
}
