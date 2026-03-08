//! Codec-specific unencrypted range handling for DAVE frame encryption.
//!
//! The DAVE protocol encrypting transform is codec-aware: certain byte ranges
//! in media frames must remain unencrypted so they pass through WebRTC's
//! codec-specific packetizer and depacketizer unmodified.

use crate::error::SigilError;

/// A byte range within a media frame that must remain unencrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnencryptedRange {
    /// Byte offset from the start of the frame.
    pub offset: usize,
    /// Number of unencrypted bytes starting at `offset`.
    pub length: usize,
}

/// Supported media codecs for DAVE frame encryption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Opus audio — fully encrypted.
    Opus,
    /// VP8 video — 1 or 10 bytes unencrypted depending on keyframe flag.
    Vp8,
    /// VP9 video — fully encrypted per DAVE spec.
    Vp9,
    /// H.264 video — NAL unit iteration to determine unencrypted ranges.
    H264,
    /// H.265 / HEVC — NAL unit iteration to determine unencrypted ranges.
    H265,
    /// AV1 video — OBU header iteration to determine unencrypted ranges.
    Av1,
}

impl Codec {
    /// Determine which byte ranges of `frame` must remain unencrypted
    /// for this codec.
    ///
    /// The returned ranges are ordered by ascending offset.
    ///
    /// # Errors
    ///
    /// Returns [`SigilError::UnsupportedCodec`] if the frame cannot be
    /// parsed for the given codec.
    pub fn unencrypted_ranges(&self, frame: &[u8]) -> Result<Vec<UnencryptedRange>, SigilError> {
        match self {
            Codec::Opus => Ok(Vec::new()),
            Codec::Vp8 => Self::vp8_ranges(frame),
            Codec::Vp9 => Ok(Vec::new()),
            Codec::H264 => Self::h264_ranges(frame),
            Codec::H265 => Self::h265_ranges(frame),
            Codec::Av1 => Self::av1_ranges(frame),
        }
    }

    /// VP8: parse the payload header to determine unencrypted range.
    ///
    /// ```text
    /// Byte 0:  |Size0|H| VER |P|
    ///          P=0 → keyframe → 10 bytes unencrypted
    ///          P=1 → delta   → 1 byte unencrypted
    /// ```
    fn vp8_ranges(frame: &[u8]) -> Result<Vec<UnencryptedRange>, SigilError> {
        if frame.is_empty() {
            return Err(SigilError::UnsupportedCodec(
                "VP8 frame is empty".to_string(),
            ));
        }

        // P flag is bit 0 of the first byte (LSB). P=0 means keyframe.
        let is_keyframe = (frame[0] & 0x01) == 0;

        if is_keyframe {
            // Keyframe: first 10 bytes are unencrypted (full uncompressed header)
            let len = frame.len().min(10);
            Ok(vec![UnencryptedRange {
                offset: 0,
                length: len,
            }])
        } else {
            // Delta frame: only the first byte is unencrypted
            Ok(vec![UnencryptedRange {
                offset: 0,
                length: 1,
            }])
        }
    }

    /// H.264: iterate NAL units to determine unencrypted ranges.
    ///
    /// Scans for start codes (0x000001 or 0x00000001) to find NAL unit
    /// boundaries. For each NAL unit, the NAL header (1 byte for H.264)
    /// plus any non-VCL NAL unit data are left unencrypted, while VCL
    /// NAL unit payloads are encrypted.
    fn h264_ranges(frame: &[u8]) -> Result<Vec<UnencryptedRange>, SigilError> {
        let mut ranges = Vec::new();
        let nalu_positions = Self::find_nalu_positions(frame);

        if nalu_positions.is_empty() {
            // No start codes found — treat entire frame as a single NAL unit
            // with a 1-byte header if possible
            if frame.is_empty() {
                return Ok(Vec::new());
            }

            let nal_type = frame[0] & 0x1F;
            if Self::is_h264_vcl(nal_type) {
                // VCL NAL: only header byte unencrypted
                ranges.push(UnencryptedRange {
                    offset: 0,
                    length: 1,
                });
            } else {
                // Non-VCL: entire NAL unencrypted
                ranges.push(UnencryptedRange {
                    offset: 0,
                    length: frame.len(),
                });
            }
            return Ok(ranges);
        }

        for i in 0..nalu_positions.len() {
            let (start_code_offset, nalu_data_offset) = nalu_positions[i];
            let nalu_end = if i + 1 < nalu_positions.len() {
                nalu_positions[i + 1].0
            } else {
                frame.len()
            };

            if nalu_data_offset >= nalu_end {
                continue;
            }

            let nal_header_byte = frame[nalu_data_offset];
            let nal_type = nal_header_byte & 0x1F;

            // Start code itself is always left unencrypted
            let start_code_len = nalu_data_offset - start_code_offset;

            if Self::is_h264_vcl(nal_type) {
                // VCL NAL: start code + 1 byte NAL header unencrypted
                ranges.push(UnencryptedRange {
                    offset: start_code_offset,
                    length: start_code_len + 1,
                });
            } else {
                // Non-VCL NAL: entire NAL unit unencrypted
                ranges.push(UnencryptedRange {
                    offset: start_code_offset,
                    length: nalu_end - start_code_offset,
                });
            }
        }

        Ok(ranges)
    }

    /// H.265: iterate NAL units to determine unencrypted ranges.
    ///
    /// Similar to H.264 but with a 2-byte NAL unit header.
    /// NAL unit type is in bits 1..6 of the first header byte.
    fn h265_ranges(frame: &[u8]) -> Result<Vec<UnencryptedRange>, SigilError> {
        let mut ranges = Vec::new();
        let nalu_positions = Self::find_nalu_positions(frame);

        if nalu_positions.is_empty() {
            if frame.len() < 2 {
                return Ok(vec![UnencryptedRange {
                    offset: 0,
                    length: frame.len(),
                }]);
            }

            let nal_type = (frame[0] >> 1) & 0x3F;
            if Self::is_h265_vcl(nal_type) {
                ranges.push(UnencryptedRange {
                    offset: 0,
                    length: 2,
                });
            } else {
                ranges.push(UnencryptedRange {
                    offset: 0,
                    length: frame.len(),
                });
            }
            return Ok(ranges);
        }

        for i in 0..nalu_positions.len() {
            let (start_code_offset, nalu_data_offset) = nalu_positions[i];
            let nalu_end = if i + 1 < nalu_positions.len() {
                nalu_positions[i + 1].0
            } else {
                frame.len()
            };

            if nalu_data_offset + 1 >= nalu_end {
                // NAL unit too short for header
                ranges.push(UnencryptedRange {
                    offset: start_code_offset,
                    length: nalu_end - start_code_offset,
                });
                continue;
            }

            let nal_type = (frame[nalu_data_offset] >> 1) & 0x3F;
            let start_code_len = nalu_data_offset - start_code_offset;

            if Self::is_h265_vcl(nal_type) {
                // VCL NAL: start code + 2 byte NAL header unencrypted
                ranges.push(UnencryptedRange {
                    offset: start_code_offset,
                    length: start_code_len + 2,
                });
            } else {
                // Non-VCL: entire NAL unit unencrypted
                ranges.push(UnencryptedRange {
                    offset: start_code_offset,
                    length: nalu_end - start_code_offset,
                });
            }
        }

        Ok(ranges)
    }

    /// AV1: iterate OBU (Open Bitstream Unit) headers.
    ///
    /// For each OBU:
    /// - 1-byte OBU header is unencrypted
    /// - 1-byte optional extension header is unencrypted (if extension flag set)
    /// - Optional LEB128 payload size is unencrypted
    /// - OBU payload is encrypted
    fn av1_ranges(frame: &[u8]) -> Result<Vec<UnencryptedRange>, SigilError> {
        let mut ranges = Vec::new();
        let mut pos = 0;

        while pos < frame.len() {
            let obu_header_offset = pos;
            let obu_header = frame[pos];
            pos += 1;

            // Bit layout of OBU header:
            // |obu_type(4)|obu_extension_flag(1)|obu_has_size_field(1)|obu_reserved(1)|reserved(1)|
            // Actually: forbidden(1) | obu_type(4) | extension_flag(1) | has_size(1) | reserved(1)
            let extension_flag = (obu_header >> 2) & 0x01;
            let has_size_field = (obu_header >> 1) & 0x01;

            let mut header_len: usize = 1;

            // Optional extension header
            if extension_flag == 1 && pos < frame.len() {
                pos += 1;
                header_len += 1;
            }

            // Optional LEB128 size field
            let obu_payload_size: usize = if has_size_field == 1 {
                let size_start = pos;
                let mut size: u64 = 0;
                let mut shift: u32 = 0;
                loop {
                    if pos >= frame.len() {
                        break;
                    }
                    let byte = frame[pos];
                    pos += 1;
                    size |= ((byte & 0x7F) as u64) << shift;
                    if byte & 0x80 == 0 {
                        break;
                    }
                    shift += 7;
                    if shift >= 64 {
                        return Err(SigilError::Uleb128Overflow);
                    }
                }
                header_len += pos - size_start;
                size as usize
            } else {
                // No size field: rest of frame is this OBU's payload
                frame.len() - pos
            };

            // The header portion is unencrypted
            ranges.push(UnencryptedRange {
                offset: obu_header_offset,
                length: header_len,
            });

            // Skip over the OBU payload (which will be encrypted)
            pos += obu_payload_size;
        }

        Ok(ranges)
    }

    /// Find NAL unit positions in an H.264/H.265 byte stream.
    ///
    /// Returns a list of (start_code_offset, nalu_data_offset) pairs.
    /// Recognizes both 3-byte (0x000001) and 4-byte (0x00000001) start codes.
    fn find_nalu_positions(frame: &[u8]) -> Vec<(usize, usize)> {
        let mut positions = Vec::new();
        let mut i = 0;

        while i + 2 < frame.len() {
            if frame[i] == 0x00 && frame[i + 1] == 0x00 {
                if frame[i + 2] == 0x01 {
                    // 3-byte start code
                    positions.push((i, i + 3));
                    i += 3;
                    continue;
                } else if i + 3 < frame.len() && frame[i + 2] == 0x00 && frame[i + 3] == 0x01 {
                    // 4-byte start code
                    positions.push((i, i + 4));
                    i += 4;
                    continue;
                }
            }
            i += 1;
        }

        positions
    }

    /// Check if an H.264 NAL unit type is VCL (Video Coding Layer).
    /// VCL types are 1-5 (coded slice types).
    fn is_h264_vcl(nal_type: u8) -> bool {
        (1..=5).contains(&nal_type)
    }

    /// Check if an H.265 NAL unit type is VCL (Video Coding Layer).
    /// VCL types are 0-31 in H.265/HEVC.
    fn is_h265_vcl(nal_type: u8) -> bool {
        nal_type <= 31
    }
}
