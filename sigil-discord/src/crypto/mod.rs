//! Cryptographic primitives for the DAVE protocol.
//!
//! Provides key ratcheting, AES-128-GCM frame encryption/decryption,
//! ULEB128 encoding, and codec-specific unencrypted range handling.

pub mod codec;
pub mod frame_crypto;
pub mod key_ratchet;
pub mod uleb128;

pub use frame_crypto::{decrypt_frame, encrypt_frame};
pub use key_ratchet::KeyRatchet;
