pub mod audio;
pub mod driver;
pub mod gateway;
pub mod serenity_hook;
pub mod source;
pub mod udp;
pub mod track;
pub mod call;

/// Enable/disable DAVE E2EE encryption.
/// 
/// **IMPORTANT**: Set this based on your users' Discord client versions:
/// - `true`  = DAVE enabled (E2EE) - Only works with Discord Canary/PTB clients
/// - `false` = DAVE disabled (regular audio) - Works with ALL Discord clients (stable, PTB, Canary)
/// 
/// **To enable DAVE**: Change this to `true` and rebuild with `cargo build --release`
/// **To disable DAVE**: Change this to `false` and rebuild with `cargo build --release`
pub const ENABLE_DAVE: bool = false;
