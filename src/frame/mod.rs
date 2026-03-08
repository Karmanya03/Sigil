//! DAVE frame encryptor, decryptor, and payload footer handling.
//!
//! - [`FrameEncryptor`] — codec-aware sender pipeline
//! - [`FrameDecryptor`] — codec-unaware receiver pipeline
//! - [`payload`] — footer builder and parser

pub mod decryptor;
pub mod encryptor;
pub mod payload;

pub use decryptor::FrameDecryptor;
pub use encryptor::FrameEncryptor;
