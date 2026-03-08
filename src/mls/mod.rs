//! MLS group lifecycle, credentials, and key packages for DAVE.
//!
//! Implements RFC 9420 MLS with ciphersuite 2
//! (MLS_128_DHKEMP256_AES128GCM_SHA256_P256) for Discord's DAVE protocol.

pub mod config;
pub mod credential;
pub mod group;
pub mod key_package;

pub use config::{DAVE_CIPHERSUITE, build_group_config, crypto_provider};
pub use credential::DaveIdentity;
pub use group::DaveGroup;
pub use key_package::generate_key_package;
