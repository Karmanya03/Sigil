//! DAVE frame footer builder and parser.
//!
//! The footer is appended to every encrypted DAVE frame and contains:
//!
//! ```text
//! [interleaved media frame]
//! [truncated tag (8 bytes)]
//! [ULEB128 nonce]
//! [ULEB128 unencrypted range offset/length pairs]
//! [supplemental size (1 byte)]
//! [magic marker 0xFAFA (2 bytes)]
//! ```
//!
//! The supplemental size covers everything from the tag through the magic marker.

use crate::crypto::codec::UnencryptedRange;
use crate::crypto::uleb128;
use crate::error::SigilError;
use crate::types::{MAGIC_MARKER, TRUNCATED_TAG_LENGTH};

/// Build a DAVE frame footer.
///
/// Returns the raw bytes to be appended after the interleaved media frame data.
///
/// # Layout
///
/// `[tag(8)] [nonce(ULEB128)] [range_pairs(ULEB128)...] [suppl_size(1)] [0xFAFA]`
pub fn build_footer(
    truncated_tag: &[u8; TRUNCATED_TAG_LENGTH],
    truncated_nonce: u32,
    ranges: &[UnencryptedRange],
) -> Vec<u8> {
    let mut footer = Vec::new();

    // 1. Truncated authentication tag (8 bytes)
    footer.extend_from_slice(truncated_tag);

    // 2. ULEB128-encoded nonce
    footer.extend_from_slice(&uleb128::encode(truncated_nonce as u64));

    // 3. ULEB128-encoded unencrypted range pairs (offset, length)
    for range in ranges {
        footer.extend_from_slice(&uleb128::encode(range.offset as u64));
        footer.extend_from_slice(&uleb128::encode(range.length as u64));
    }

    // 4. Supplemental size: covers tag + nonce + ranges + this byte + magic
    //    = footer.len() (so far) + 1 (this size byte) + 2 (magic)
    let suppl_size = (footer.len() + 1 + 2) as u8;
    footer.push(suppl_size);

    // 5. Magic marker
    footer.extend_from_slice(&MAGIC_MARKER);

    footer
}

/// Parse a DAVE frame footer from a complete frame (media data + footer).
///
/// Returns `(truncated_tag, truncated_nonce, unencrypted_ranges, frame_data_end_index)`.
///
/// `frame_data_end_index` is the byte offset where the interleaved media frame
/// data ends (i.e., the start of the footer within the buffer).
///
/// # Errors
///
/// - [`SigilError::InvalidMagic`] if the last 2 bytes are not `0xFAFA`
/// - [`SigilError::FrameTooShort`] if the frame is too small to contain a valid footer
pub fn parse_footer(
    frame: &[u8],
) -> Result<
    (
        [u8; TRUNCATED_TAG_LENGTH],
        u32,
        Vec<UnencryptedRange>,
        usize,
    ),
    SigilError,
> {
    let len = frame.len();

    // Minimum footer: 8 (tag) + 1 (nonce) + 0 (ranges) + 1 (suppl) + 2 (magic) = 12
    let min_footer = TRUNCATED_TAG_LENGTH + 1 + 1 + 2;
    if len < min_footer {
        return Err(SigilError::FrameTooShort {
            need: min_footer,
            got: len,
        });
    }

    // 1. Verify magic marker at the very end
    if frame[len - 2] != MAGIC_MARKER[0] || frame[len - 1] != MAGIC_MARKER[1] {
        return Err(SigilError::InvalidMagic);
    }

    // 2. Read supplemental size (1 byte before magic)
    let suppl_size = frame[len - 3] as usize;

    if suppl_size > len {
        return Err(SigilError::FrameTooShort {
            need: suppl_size,
            got: len,
        });
    }

    // The footer starts at: len - suppl_size
    let footer_start = len - suppl_size;
    let frame_data_end = footer_start;

    // Parse the footer content: [tag(8)] [nonce(uleb128)] [ranges(uleb128 pairs)] [suppl_size(1)] [magic(2)]
    let footer = &frame[footer_start..];

    // 3. Extract truncated tag (first 8 bytes of footer)
    if footer.len() < TRUNCATED_TAG_LENGTH {
        return Err(SigilError::FrameTooShort {
            need: TRUNCATED_TAG_LENGTH,
            got: footer.len(),
        });
    }
    let mut tag = [0u8; TRUNCATED_TAG_LENGTH];
    tag.copy_from_slice(&footer[..TRUNCATED_TAG_LENGTH]);

    // 4. Decode nonce (ULEB128) after tag
    let after_tag = &footer[TRUNCATED_TAG_LENGTH..];
    // The range data + nonce is everything between tag and [suppl_size, magic]
    // which is: after_tag[..after_tag.len() - 3] (excluding suppl_size byte + magic)
    let payload_end = after_tag.len() - 3; // 1 (suppl) + 2 (magic)
    let payload_data = &after_tag[..payload_end];

    let (nonce_val, nonce_consumed) = uleb128::decode_forward(payload_data)?;
    let truncated_nonce = nonce_val as u32;

    // 5. Decode unencrypted range pairs from remaining payload data
    let mut ranges = Vec::new();
    let range_data = &payload_data[nonce_consumed..];
    let mut pos = 0;
    while pos < range_data.len() {
        let (offset_val, consumed1) = uleb128::decode_forward(&range_data[pos..])?;
        pos += consumed1;

        if pos >= range_data.len() {
            break;
        }

        let (length_val, consumed2) = uleb128::decode_forward(&range_data[pos..])?;
        pos += consumed2;

        ranges.push(UnencryptedRange {
            offset: offset_val as usize,
            length: length_val as usize,
        });
    }

    Ok((tag, truncated_nonce, ranges, frame_data_end))
}
