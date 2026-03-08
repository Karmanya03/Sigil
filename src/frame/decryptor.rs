//! Codec-unaware DAVE frame decryptor.
//!
//! The decryptor parses the DAVE footer to learn the nonce, tag, and
//! unencrypted ranges, then extracts ciphertext and AAD, decrypts,
//! and reassembles the original raw media frame.

use crate::crypto::frame_crypto;
use crate::error::SigilError;
use crate::frame::payload;
use crate::types::KEY_LENGTH;

/// Codec-unaware DAVE frame decryptor.
///
/// Does not need to know the codec — all information needed for decryption
/// is stored in the DAVE frame footer (unencrypted ranges, nonce, tag).
pub struct FrameDecryptor;

impl FrameDecryptor {
    /// Decrypt a DAVE protocol frame back into the original raw media frame.
    ///
    /// # Pipeline
    ///
    /// 1. Parse footer to extract tag, nonce, unencrypted ranges, data boundary
    /// 2. Split interleaved data into ciphertext and AAD using the ranges
    /// 3. Decrypt ciphertext with AES-128-GCM
    /// 4. Reassemble: place unencrypted bytes and plaintext back into original positions
    ///
    /// # Arguments
    ///
    /// * `key` — 16-byte AES-128 key for this sender+generation
    /// * `dave_frame` — the complete DAVE protocol frame (media data + footer)
    ///
    /// # Errors
    ///
    /// Returns [`SigilError`] if footer parsing or decryption fails.
    pub fn decrypt(key: &[u8; KEY_LENGTH], dave_frame: &[u8]) -> Result<Vec<u8>, SigilError> {
        // 1. Parse the footer
        let (tag, nonce, ranges, data_end) = payload::parse_footer(dave_frame)?;

        let interleaved = &dave_frame[..data_end];

        // 2. Extract ciphertext and AAD from interleaved data
        let (ciphertext, aad) = extract_parts(interleaved, &ranges);

        // 3. Decrypt
        let plaintext = frame_crypto::decrypt_frame(key, nonce, &ciphertext, &tag, &aad)?;

        // 4. Reassemble: unencrypted bytes in their original positions,
        //    decrypted bytes fill the encrypted positions
        let raw_frame = reassemble(interleaved.len(), &plaintext, &ranges, interleaved);

        Ok(raw_frame)
    }
}

/// Extract ciphertext (encrypted positions) and AAD (unencrypted positions)
/// from the interleaved frame data.
fn extract_parts(
    interleaved: &[u8],
    ranges: &[crate::crypto::codec::UnencryptedRange],
) -> (Vec<u8>, Vec<u8>) {
    let mut ciphertext = Vec::new();
    let mut aad = Vec::new();

    let mut is_unencrypted = vec![false; interleaved.len()];
    for range in ranges {
        let end = (range.offset + range.length).min(interleaved.len());
        is_unencrypted[range.offset..end].fill(true);
    }

    for (i, &byte) in interleaved.iter().enumerate() {
        if is_unencrypted[i] {
            aad.push(byte);
        } else {
            ciphertext.push(byte);
        }
    }

    (ciphertext, aad)
}

/// Reassemble the original frame from decrypted plaintext and unencrypted bytes.
fn reassemble(
    frame_len: usize,
    plaintext: &[u8],
    ranges: &[crate::crypto::codec::UnencryptedRange],
    interleaved: &[u8],
) -> Vec<u8> {
    let mut result = vec![0u8; frame_len];
    let mut is_unencrypted = vec![false; frame_len];

    for range in ranges {
        let end = (range.offset + range.length).min(frame_len);
        for i in range.offset..end {
            is_unencrypted[i] = true;
            result[i] = interleaved[i];
        }
    }

    let mut pt_idx = 0;
    for i in 0..frame_len {
        if !is_unencrypted[i] && pt_idx < plaintext.len() {
            result[i] = plaintext[pt_idx];
            pt_idx += 1;
        }
    }

    result
}
