//! Shared types and constants for the DAVE protocol.

/// Discord user snowflake ID.
pub type UserId = u64;

/// MLS epoch counter.
pub type Epoch = u64;

/// Voice gateway transition identifier.
pub type TransitionId = u64;

/// Current supported DAVE protocol version.
pub const DAVE_PROTOCOL_VERSION: u16 = 1;

/// MLS-Exporter label for per-sender key derivation.
pub const SENDER_KEY_LABEL: &[u8] = b"Discord Secure Frames v0";

/// AES-128 key length in bytes.
pub const KEY_LENGTH: usize = 16;

/// Truncated AES-GCM authentication tag length (64-bit).
pub const TRUNCATED_TAG_LENGTH: usize = 8;

/// Full AES-GCM tag length before truncation.
pub const FULL_TAG_LENGTH: usize = 16;

/// Full AES-GCM nonce length (96-bit).
pub const NONCE_LENGTH: usize = 12;

/// Truncated nonce length (32-bit).
pub const TRUNCATED_NONCE_LENGTH: usize = 4;

/// Frame footer magic marker.
pub const MAGIC_MARKER: [u8; 2] = [0xFA, 0xFA];

/// Max generation before mandatory key rotation (2^24 frames).
pub const MAX_GENERATION_FRAMES: u32 = 1 << 24;

/// Duration in seconds to retain old epoch keys during transitions.
pub const KEY_RETENTION_SECS: u64 = 10;
