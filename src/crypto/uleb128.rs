//! ULEB128 (Unsigned Little-Endian Base 128) encoder/decoder.
//!
//! Used by the DAVE protocol to compactly encode nonces and
//! unencrypted range offset/length pairs in frame footers.

use crate::error::SigilError;

/// Encode a `u64` value as ULEB128 bytes.
///
/// # Examples
/// ```
/// # use sigil::crypto::uleb128;
/// assert_eq!(uleb128::encode(0), vec![0]);
/// assert_eq!(uleb128::encode(42), vec![42]);
/// assert_eq!(uleb128::encode(300), vec![0xAC, 0x02]);
/// ```
pub fn encode(mut value: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
    buf
}

/// Decode a ULEB128-encoded value from the start of `buf`.
///
/// Returns the decoded value and the number of bytes consumed.
///
/// # Errors
///
/// Returns [`SigilError::Uleb128Overflow`] if the encoded value
/// would overflow a `u64`.
pub fn decode_forward(buf: &[u8]) -> Result<(u64, usize), SigilError> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;

    for (i, &byte) in buf.iter().enumerate() {
        if shift >= 64 {
            return Err(SigilError::Uleb128Overflow);
        }

        let low_bits = (byte & 0x7F) as u64;

        // Check for overflow before shifting
        if shift > 0 {
            result |= low_bits
                .checked_shl(shift)
                .ok_or(SigilError::Uleb128Overflow)?;
        } else {
            result = low_bits;
        }

        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }

        shift += 7;
    }

    Err(SigilError::Uleb128Overflow)
}

/// Decode a ULEB128-encoded value from the end of `buf`, reading backwards.
///
/// `pos` is an in/out parameter: on entry it points one past the last byte
/// of the encoded value. On return it points to the first byte of the
/// encoded value (i.e. the position is decremented by the number of
/// bytes consumed).
///
/// # Errors
///
/// Returns [`SigilError::Uleb128Overflow`] if the encoded value
/// would overflow a `u64`.
pub fn decode_reverse(buf: &[u8], pos: &mut usize) -> Result<u64, SigilError> {
    if *pos == 0 {
        return Err(SigilError::Uleb128Overflow);
    }

    // First, find the start of this ULEB128 sequence by scanning backwards.
    // In ULEB128, continuation bytes have the high bit set (0x80).
    // The final byte (MSB group) does NOT have the high bit set.
    // When stored, the least-significant group is first.
    // Reading backwards from pos-1: the byte at pos-1 is the LAST encoded byte
    // (most-significant group, no continuation bit).
    // Walk backwards while the byte at (current - 1) has the continuation bit set.

    let end = *pos; // one past last byte
    let mut start = end - 1;

    // Walk backwards to find the start byte (which has continuation bit set)
    // Actually, the encoding is: LSB group first, MSB group last (no 0x80 bit).
    // So buf[start] is the MSB group (no 0x80). Bytes before it have 0x80.
    while start > 0 && (buf[start - 1] & 0x80) != 0 {
        start -= 1;
    }

    // Now decode forward from start..end
    let slice = &buf[start..end];
    let (value, consumed) = decode_forward(slice)?;

    if consumed != end - start {
        return Err(SigilError::Uleb128Overflow);
    }

    *pos = start;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_u64_max() {
        let val = u64::MAX;
        let encoded = encode(val);
        let (decoded, _) = decode_forward(&encoded).expect("decode should succeed");
        assert_eq!(decoded, val);
    }
}
