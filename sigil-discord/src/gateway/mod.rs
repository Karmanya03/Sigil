//! DAVE voice gateway opcode handling and session state machine.
//!
//! - [`DaveOpcode`] — opcodes 21–31 from the DAVE specification
//! - [`DaveSession`] — session state machine with nonce management
//! - [`handler`] — raw opcode dispatch to high-level events

pub mod handler;
pub mod opcodes;
pub mod session;

pub use opcodes::DaveOpcode;
pub use session::{DaveSession, SessionState};
