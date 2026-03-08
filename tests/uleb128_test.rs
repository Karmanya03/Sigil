use sigil::crypto::uleb128;

#[test]
fn roundtrip_small() {
    let encoded = uleb128::encode(42);
    let (decoded, consumed) = uleb128::decode_forward(&encoded).unwrap();
    assert_eq!(decoded, 42);
    assert_eq!(consumed, 1);
}

#[test]
fn roundtrip_multibyte() {
    let encoded = uleb128::encode(300);
    let (decoded, _) = uleb128::decode_forward(&encoded).unwrap();
    assert_eq!(decoded, 300);
}

#[test]
fn roundtrip_large() {
    let val = 0xDEAD_BEEF_u64;
    let encoded = uleb128::encode(val);
    let (decoded, _) = uleb128::decode_forward(&encoded).unwrap();
    assert_eq!(decoded, val);
}

#[test]
fn decode_reverse_works() {
    let encoded = uleb128::encode(624485);
    let mut pos = encoded.len();
    let decoded = uleb128::decode_reverse(&encoded, &mut pos).unwrap();
    assert_eq!(decoded, 624485);
    assert_eq!(pos, 0);
}

#[test]
fn encode_zero() {
    let encoded = uleb128::encode(0);
    assert_eq!(encoded, vec![0]);
}
