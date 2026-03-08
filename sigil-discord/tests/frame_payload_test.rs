use sigil_discord::crypto::codec::UnencryptedRange;
use sigil_discord::frame::payload::{build_footer, parse_footer};
use sigil_discord::types::*;
use sigil_discord::TRUNCATED_TAG_LENGTH;

#[test]
fn footer_roundtrip_no_ranges() {
    let tag = [0xAA; TRUNCATED_TAG_LENGTH];
    let nonce = 42u32;
    let ranges: Vec<UnencryptedRange> = vec![];

    let footer = build_footer(&tag, nonce, &ranges);

    let mut frame = vec![0u8; 64]; // fake frame data
    frame.extend_from_slice(&footer);

    let (parsed_tag, parsed_nonce, parsed_ranges, data_end) = parse_footer(&frame).unwrap();
    assert_eq!(parsed_tag, tag);
    assert_eq!(parsed_nonce, nonce);
    assert!(parsed_ranges.is_empty());
    assert_eq!(data_end, 64);
}

#[test]
fn footer_roundtrip_with_ranges() {
    let tag = [0xBB; TRUNCATED_TAG_LENGTH];
    let nonce = 1024u32;
    let ranges = vec![
        UnencryptedRange {
            offset: 0,
            length: 10,
        },
        UnencryptedRange {
            offset: 50,
            length: 2,
        },
    ];

    let footer = build_footer(&tag, nonce, &ranges);
    let mut frame = vec![0u8; 128];
    frame.extend_from_slice(&footer);

    let (parsed_tag, parsed_nonce, parsed_ranges, _) = parse_footer(&frame).unwrap();
    assert_eq!(parsed_tag, tag);
    assert_eq!(parsed_nonce, nonce);
    assert_eq!(parsed_ranges.len(), 2);
    assert_eq!(parsed_ranges[0].offset, 0);
    assert_eq!(parsed_ranges[0].length, 10);
    assert_eq!(parsed_ranges[1].offset, 50);
    assert_eq!(parsed_ranges[1].length, 2);
}

#[test]
fn invalid_magic_detected() {
    let frame = vec![0x00, 0x00, 0x00]; // no magic
    assert!(parse_footer(&frame).is_err());
}
