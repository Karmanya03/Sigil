//! Unified error type for all Sigil operations.

use crate::types::{Epoch, TransitionId, UserId};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SigilError {
    // ── MLS ──
    #[error("MLS operation failed: {0}")]
    Mls(String),

    #[error("MLS group not established")]
    GroupNotEstablished,

    #[error("Invalid external sender")]
    InvalidExternalSender,

    #[error("Duplicate credential in group for user {0}")]
    DuplicateCredential(UserId),

    // ── Key Ratchet ──
    #[error("Generation {generation} already erased (current: {current})")]
    GenerationErased { generation: u32, current: u32 },

    #[error("Key ratchet exhausted at generation {0}")]
    RatchetExhausted(u32),

    // ── Frame Crypto ──
    #[error("Decryption failed: invalid authentication tag")]
    DecryptionFailed,

    #[error("Nonce reuse detected: nonce {0} already consumed")]
    NonceReuse(u32),

    #[error("Invalid frame: missing 0xFAFA magic marker")]
    InvalidMagic,

    #[error("Frame too short: need {need} bytes, got {got}")]
    FrameTooShort { need: usize, got: usize },

    // ── Gateway ──
    #[error("Unknown DAVE opcode: {0}")]
    UnknownOpcode(u8),

    #[error("Epoch mismatch: expected {expected}, got {got}")]
    EpochMismatch { expected: Epoch, got: Epoch },

    #[error("No sender key for user {0} in epoch {1}")]
    NoSenderKey(UserId, Epoch),

    #[error("Transition {0} timed out")]
    TransitionTimeout(TransitionId),

    #[error("Unexpected gateway state: {0}")]
    InvalidState(String),

    // ── Codec ──
    #[error("ULEB128 decode overflow")]
    Uleb128Overflow,

    #[error("Unsupported codec: {0}")]
    UnsupportedCodec(String),
}
