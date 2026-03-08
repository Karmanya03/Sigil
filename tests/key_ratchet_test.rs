use sigil::crypto::key_ratchet::KeyRatchet;

#[test]
fn generation_zero_returns_base() {
    let secret = [0x42u8; 16];
    let mut ratchet = KeyRatchet::new(secret);
    let key = ratchet.get(0).unwrap();
    assert_eq!(key, secret);
}

#[test]
fn forward_ratchet_produces_different_keys() {
    let secret = [0x42u8; 16];
    let mut ratchet = KeyRatchet::new(secret);
    let k0 = ratchet.get(0).unwrap();
    let k1 = ratchet.get(1).unwrap();
    let k2 = ratchet.get(2).unwrap();
    assert_ne!(k0, k1);
    assert_ne!(k1, k2);
    assert_ne!(k0, k2);
}

#[test]
fn cached_generation_returns_same_key() {
    let secret = [0xABu8; 16];
    let mut ratchet = KeyRatchet::new(secret);
    let k5_first = ratchet.get(5).unwrap();
    let k5_second = ratchet.get(5).unwrap();
    assert_eq!(k5_first, k5_second);
}

#[test]
fn erased_generation_fails() {
    let secret = [0xCDu8; 16];
    let mut ratchet = KeyRatchet::new(secret);
    let _ = ratchet.get(5).unwrap();
    ratchet.erase_before(3);
    assert!(ratchet.get(1).is_err());
}
