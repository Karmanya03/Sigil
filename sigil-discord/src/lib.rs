//! # Sigil
//!
//! Pure-Rust implementation of Discord's DAVE (Audio/Video E2EE) protocol.
//!
//! ## Quick Start
//!
//! Use [`SigilSession`] for a batteries-included integration experience:
//!
//! ```rust,no_run
//! use sigil_discord::SigilSession;
//! use sigil_discord::crypto::codec::Codec;
//! ```
//!
//! ## Modules
//! - [`session`] — High-level session facade (start here)
//! - [`mls`]     — MLS group lifecycle, credentials, key packages
//! - [`crypto`]  — Key ratchet, AES-128-GCM frame encryption, ULEB128 codec
//! - [`gateway`] — DAVE voice gateway opcodes 21–31 and session state machine
//! - [`frame`]   — Codec-aware encryptor and codec-unaware decryptor

pub mod crypto;
pub mod error;
pub mod frame;
pub mod gateway;
pub mod mls;
pub mod session;
pub mod types;

pub use error::SigilError;
pub use session::SigilSession;
pub use types::*;
