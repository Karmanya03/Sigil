//! Codec-aware DAVE frame encryptor.
//!
//! The encryptor splits a raw media frame into encrypted and unencrypted
//! ranges based on codec rules, encrypts the encrypted portions via
//! AES-128-GCM, interleaves them back, and appends the DAVE footer.

use crate::crypto::codec::Codec;
use crate::crypto::frame_crypto;
use crate::error::SigilError;
use crate::frame::payload;
use crate::types::KEY_LENGTH;

/// Codec-aware DAVE frame encryptor.
///
/// Knows which codec the outgoing stream uses in order to determine
/// which byte ranges must be left unencrypted.
pub struct FrameEncryptor {
    /// The codec of the outgoing media stream.
    pub codec: Codec,
}

impl FrameEncryptor {
    /// Create a new encryptor for the given codec.
    pub fn new(codec: Codec) -> Self {
        Self { codec }
    }

    /// Encrypt a raw media frame into a DAVE protocol frame.
    ///
    /// # Pipeline
    ///
    /// 1. Determine unencrypted ranges for this codec/frame
    /// 2. Split frame into unencrypted (AAD) and encrypted (plaintext) parts
    /// 3. Encrypt the plaintext with AES-128-GCM
    /// 4. Interleave unencrypted + ciphertext back into original positions
    /// 5. Append DAVE footer (tag, nonce, ranges, suppl_size, magic)
    ///
    /// # Arguments
    ///
    /// * `key` — 16-byte AES-128 key for this sender+generation
    /// * `nonce` — 32-bit truncated nonce (auto-incremented by caller)
    /// * `raw_frame` — the original encoded media frame
    ///
    /// # Errors
    ///
    /// Returns [`SigilError`] if codec range detection or encryption fails.
    pub fn encrypt(
        &self,
        key: &[u8; KEY_LENGTH],
        nonce: u32,
        raw_frame: &[u8],
    ) -> Result<Vec<u8>, SigilError> {
        // 1. Determine which ranges stay unencrypted
        let ranges = self.codec.unencrypted_ranges(raw_frame)?;

        // 2. Split the frame
        let (plaintext, aad) = split_frame(raw_frame, &ranges);

        // 3. Encrypt
        let (ciphertext, truncated_tag) =
            frame_crypto::encrypt_frame(key, nonce, &plaintext, &aad)?;

        // 4. Interleave back
        let interleaved = interleave(raw_frame.len(), &ciphertext, &ranges, raw_frame);

        // 5. Build and append footer
        let footer = payload::build_footer(&truncated_tag, nonce, &ranges);

        let mut dave_frame = Vec::with_capacity(interleaved.len() + footer.len());
        dave_frame.extend_from_slice(&interleaved);
        dave_frame.extend_from_slice(&footer);

        Ok(dave_frame)
    }
}

/// Split a raw frame into plaintext (encrypted ranges) and AAD (unencrypted ranges).
///
/// The plaintext contains all byte ranges that are NOT in `unencrypted_ranges`,
/// concatenated in order. The AAD contains all bytes within `unencrypted_ranges`,
/// concatenated in order.
fn split_frame(
    frame: &[u8],
    unencrypted_ranges: &[crate::crypto::codec::UnencryptedRange],
) -> (Vec<u8>, Vec<u8>) {
    let mut plaintext = Vec::new();
    let mut aad = Vec::new();

    // Build a sorted list of unencrypted byte positions
    let mut is_unencrypted = vec![false; frame.len()];
    for range in unencrypted_ranges {
        let end = (range.offset + range.length).min(frame.len());
        is_unencrypted[range.offset..end].fill(true);
    }

    for (i, &byte) in frame.iter().enumerate() {
        if is_unencrypted[i] {
            aad.push(byte);
        } else {
            plaintext.push(byte);
        }
    }

    (plaintext, aad)
}

/// Interleave unencrypted ranges (from original frame) and ciphertext
/// back into a frame of the original length.
///
/// Unencrypted positions keep their original bytes; encrypted positions
/// are replaced by the corresponding ciphertext bytes in order.
fn interleave(
    frame_len: usize,
    ciphertext: &[u8],
    unencrypted_ranges: &[crate::crypto::codec::UnencryptedRange],
    original_frame: &[u8],
) -> Vec<u8> {
    let mut result = vec![0u8; frame_len];
    let mut is_unencrypted = vec![false; frame_len];

    for range in unencrypted_ranges {
        let end = (range.offset + range.length).min(frame_len);
        for i in range.offset..end {
            is_unencrypted[i] = true;
            result[i] = original_frame[i];
        }
    }

    let mut ct_idx = 0;
    for i in 0..frame_len {
        if !is_unencrypted[i] && ct_idx < ciphertext.len() {
            result[i] = ciphertext[ct_idx];
            ct_idx += 1;
        }
    }

    result
}
