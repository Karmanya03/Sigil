<div align="center">
  <img src="assets/SIGIL-logo.png" alt="Sigil Logo" width="400" />

  # Sigil

  **Pure-Rust Discord DAVE (Audio/Video E2EE) Protocol Framework.**<br>
  Keep your voice traffic off the books.

  <br>

  [![crates.io](https://img.shields.io/crates/v/sigil-rs?style=flat-square&color=c7102a)](https://crates.io/crates/sigil-rs)
  [![license](https://img.shields.io/crates/l/sigil-rs?style=flat-square&color=c7102a)](#license)
  [![written in](https://img.shields.io/badge/written%20in-Rust-c7102a?style=flat-square)](#)
  [![target](https://img.shields.io/badge/target-Discord%20Voice-c7102a?style=flat-square)](#)

  <br>

  [![E2EE](https://img.shields.io/badge/encryption-AES--128--GCM-9710c7?style=flat-square)](#)
  [![MLS](https://img.shields.io/badge/key%20exchange-MLS%20RFC%209420-9710c7?style=flat-square)](#)
  [![Ratchets](https://img.shields.io/badge/ratchet-HKDF--Expand-9710c7?style=flat-square)](#)
  [![Codecs](https://img.shields.io/badge/codecs-VP8%20%7C%20VP9%20%7C%20H264%20%7C%20H265%20%7C%20AV1%20%7C%20Opus-9710c7?style=flat-square)](#)

  <br>

  [![Dependencies](https://img.shields.io/badge/C%20dependencies-0-10c756?style=flat-square)](#)
  [![Integrations](https://img.shields.io/badge/integrations-Serenity%20%7C%20Songbird-10c756?style=flat-square)](#)

  <br>

  [**Quick Start**](#quick-start) &nbsp;·&nbsp;
  [**Integration Guide**](#integration-guide) &nbsp;·&nbsp;
  [**Architecture**](#architecture) &nbsp;·&nbsp;
  [**Sigil vs Davey**](#sigil-vs-davey) &nbsp;·&nbsp;
  [**Protocol Details**](#dave-protocol-details)
  
  <hr>
</div>

> *End-to-end encryption for Discord voice & video. No backdoors, no compromises, no C dependencies.*

**What is this?**<br>
Sigil is a pure-Rust implementation of Discord's [DAVE protocol](https://daveprotocol.com/) — the end-to-end encryption layer that ensures only your friends (and definitely not your ISP) can hear you screaming when you botch a raid. 

Discord recently rolled out DAVE so Voice & Video calls are completely protected. But actually implementing the math behind it? Absolute nightmare fuel. 

Sigil does all the heavy cryptographical lifting so you don't have to. You can just plug E2EE straight into your Discord bot without losing a single night of sleep over "MLS key epochs", "nonce expansions", or "truncated authentication tags." 

## Why Sigil?

Because your bot's voice traffic isn't a public podcast. Here is what Sigil brings to the table:

- **One struct to rule them all** — `SigilSession` handles all the MLS handshakes, key ratcheting, and Gateway events behind a single clean API. No PhD in cryptography required.
- **Codec magic** — VP8, VP9, H.264, H.265, AV1, Opus. WebRTC gets violently angry if you encrypt *everything*. Sigil automatically knows exactly which bytes to leave unencrypted so WebRTC doesn't crash and burn.
- **100% Protocol v1.1 compliant** — HKDF ratchets, ciphersuite 2, `0xFAFA` magic markers... the whole nine yards. It flawlessly speaks the exact language Discord's official clients expect.
- **Zero C dependencies** — no CMake, no OpenSSL, no `audiopus_sys` required for the core library. Just run `cargo build` and go touch grass.


## Architecture

```
┌─────────────────────────────────────────────────┐
│                  SigilSession                   │   ← start here
│  (high-level facade for bot integration)        │
└────────┬──────────┬───────────┬────────┬────────┘
         │          │           │        │
    ┌────▼────┐ ┌───▼───┐ ┌────▼───┐ ┌──▼──────┐
    │ gateway │ │  mls   │ │ crypto │ │  frame  │
    │─────────│ │────────│ │────────│ │─────────│
    │ opcodes │ │ group  │ │ ratchet│ │encryptor│
    │ handler │ │ creds  │ │ aes-gcm│ │decryptor│
    │ session │ │ keypkg │ │ uleb128│ │ payload │
    │         │ │ config │ │ codec  │ │         │
    └─────────┘ └────────┘ └────────┘ └─────────┘
```

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
sigil-rs = { git = "https://github.com/Karmanya03/Sigil" }
```

### Basic Usage

```rust
use sigil::{SigilSession, SigilError};
use sigil::crypto::codec::Codec;

fn main() -> Result<(), SigilError> {
    // Create a session with your bot's Discord user ID
    let mut session = SigilSession::new(123456789012345678)?;

    // Generate a key package (send this to the gateway)
    let key_package = session.generate_key_package()?;
    // → send key_package bytes to Discord via voice gateway opcode 22

    // Later, after receiving a Welcome message from the gateway:
    // session.join_group(&welcome_bytes)?;

    // Encrypt outgoing audio
    let my_key = [0x42u8; 16]; // from MLS key export
    let raw_opus = vec![0u8; 960]; // your encoded audio frame
    let encrypted = session.encrypt_frame(&my_key, &raw_opus, Codec::Opus)?;

    // Decrypt incoming audio from another user
    let their_key = [0x37u8; 16]; // their sender key
    let decrypted = session.decrypt_frame(&their_key, &encrypted)?;
    assert_eq!(decrypted, raw_opus);

    Ok(())
}
```

### With Cached Keys

When you have keys for known participants:

```rust
// Install sender keys for each participant
session.install_sender_key(sender_user_id, sender_key);

// Encrypt with your own cached key
let encrypted = session.encrypt_own_frame(&raw_frame, Codec::Opus)?;

// Decrypt using cached sender key
let decrypted = session.decrypt_from_sender(sender_user_id, &encrypted_frame)?;
```

### Lower-Level Access

You aren't locked into `SigilSession`. Every module is public:

```rust
use sigil::crypto::key_ratchet::KeyRatchet;
use sigil::frame::encryptor::FrameEncryptor;
use sigil::frame::decryptor::FrameDecryptor;
use sigil::gateway::handler::{dispatch, DaveEvent};
use sigil::mls::group::DaveGroup;
```

## Integration Guide

### Songbird / Serenity Bots

Sigil plays nicely with any Rust Discord framework. Here's the general flow:

```
Your Bot                    Discord Gateway              Sigil
  │                              │                         │
  │── join voice channel ──────► │                         │
  │                              │── DAVE Ready (op 21) ──►│
  │                              │                         │ process event
  │◄── key package ──────────────│◄── key pkg (op 22) ─────│
  │                              │                         │
  │                              │── Welcome (op 24) ─────►│
  │                              │                         │ join_group()
  │                              │                         │ export_sender_keys()
  │                              │                         │
  │── send audio ──────────────► │                         │
  │   (encrypt_own_frame)        │                         │
  │                              │                         │
  │◄── receive audio ────────────│                         │
  │   (decrypt_from_sender)      │                         │
```

1. **On voice connect**: create `SigilSession::new(bot_user_id)`
2. **On DAVE Ready (op 21)**: generate key package, send to gateway
3. **On Welcome (op 24)**: call `session.join_group(&welcome_bytes)`
4. **On epoch change**: call `session.export_sender_keys(&participant_ids)`
5. **On send**: call `session.encrypt_own_frame(&frame, codec)`
6. **On receive**: call `session.decrypt_from_sender(sender_id, &frame)`
7. **On disconnect**: call `session.disconnect()`

### Supported Codecs

| Codec | Unencrypted | Behavior |
|-------|-------------|----------|
| Opus  | Nothing     | Fully encrypted |
| VP8   | 1 or 10 bytes | Payload header (keyframe detection) |
| VP9   | Nothing     | Fully encrypted |
| H.264 | NAL headers | Iterates NAL units, keeps headers clear |
| H.265 | NAL headers | 2-byte NAL headers stay clear |
| AV1   | OBU headers | Iterates OBU headers, keeps metadata clear |

### DAVE Protocol Details

For the curious (or the paranoid):

- **Key Exchange**: [MLS RFC 9420](https://www.rfc-editor.org/rfc/rfc9420) with ciphersuite `MLS_128_DHKEMP256_AES128GCM_SHA256_P256`
- **Frame Encryption**: AES-128-GCM, truncated to 8-byte auth tags
- **Nonce**: 32-bit counter, expanded to 96-bit (8 zero bytes + 4 LE bytes)
- **Key Ratchet**: HKDF-Expand per generation, info = `"sigil-ratchet-{gen}"`
- **MLS Export**: label = `"Discord Secure Frames v0"`, context = LE u64 sender ID
- **Credentials**: MLS Basic credential with BE u64 Discord user snowflake
- **Magic**: every encrypted frame ends with `0xFAFA`
- **Group ID**: `b"sigil-dave"`

## Building

```bash
cargo check          # type-check
cargo test           # 12 tests across 4 suites
cargo clippy         # lint (currently 0 warnings)
cargo fmt --check    # formatting (currently 0 diffs)
cargo bench          # benchmark frame encryption
```

### Feature Flags

| Feature | What it enables | Needs |
|---------|----------------|-------|
| `voice-gateway` | Songbird + Serenity integration | CMake (for audiopus) |

The core library (`cargo build`) compiles without any native dependencies. The `voice-gateway` feature is opt-in for when you want direct Songbird/Serenity interop.

### Serenity + Songbird Integration (Music Bots)

If you're building a music bot with [Serenity](https://github.com/serenity-rs/serenity) and [Songbird](https://github.com/serenity-rs/songbird), you'll need the `voice-gateway` feature. This pulls in Songbird's audio driver, which depends on **Opus** (via `audiopus_sys`), which requires **CMake** to compile.

#### Prerequisites

**Windows:**
```powershell
# Option 1: winget
winget install Kitware.CMake

# Option 2: choco
choco install cmake --installargs 'ADD_CMAKE_TO_PATH=System'

# Option 3: scoop
scoop install cmake

# After installing, restart your terminal and verify:
cmake --version
```

**macOS:**
```bash
brew install cmake
```

**Linux (Debian/Ubuntu):**
```bash
sudo apt install cmake build-essential pkg-config libopus-dev
```

**Linux (Arch):**
```bash
sudo pacman -S cmake base-devel opus
```

#### Cargo Setup

In your **bot's** `Cargo.toml` (not Sigil's):

```toml
[dependencies]
sigil-rs = { git = "https://github.com/Karmanya03/Sigil", features = ["voice-gateway"] }
serenity = { version = "0.12", features = ["voice", "gateway"] }
songbird = { version = "0.4", features = ["driver", "gateway"] }
tokio = { version = "1", features = ["full"] }
```

#### Bot Skeleton

Here's a minimal example wiring Sigil into a Serenity + Songbird bot:

```rust
use std::sync::Arc;
use serenity::prelude::*;
use songbird::SerenityInit;
use sigil::{SigilSession, SigilError};
use sigil::crypto::codec::Codec;

struct Handler;

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: serenity::model::gateway::Ready) {
        println!("{} is connected!", ready.user.name);
    }
}

#[tokio::main]
async fn main() {
    let token = std::env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");

    let mut client = Client::builder(&token, GatewayIntents::non_privileged())
        .event_handler(Handler)
        .register_songbird()     // ← register Songbird
        .await
        .expect("Error creating client");

    client.start().await.unwrap();
}

// When joining a voice channel:
async fn join_and_encrypt(ctx: &Context, guild_id: u64, channel_id: u64) -> Result<(), SigilError> {
    let bot_user_id: u64 = 123456789012345678; // your bot's user ID

    // 1. Create a Sigil session
    let mut session = SigilSession::new(bot_user_id)?;

    // 2. Generate key package for DAVE enrollment
    let key_package = session.generate_key_package()?;
    // → send key_package to Discord via voice gateway opcode 22

    // 3. After receiving Welcome from gateway:
    // session.join_group(&welcome_bytes)?;
    // session.export_sender_keys(&participant_ids)?;

    // 4. Encrypt outgoing audio frames before sending
    let raw_opus_frame = vec![0u8; 960]; // your encoded audio
    let encrypted = session.encrypt_own_frame(&raw_opus_frame, Codec::Opus)?;

    // 5. Decrypt incoming audio frames after receiving
    let sender_id: u64 = 987654321098765432;
    let decrypted = session.decrypt_from_sender(sender_id, &encrypted)?;

    Ok(())
}
```

#### Troubleshooting CMake

| Problem | Fix |
|---------|-----|
| `cmake not found` | Install CMake and restart your terminal |
| `Could not find Opus` | Install `libopus-dev` (Linux) or let audiopus build it from source |
| `LINK : fatal error LNK1181` (Windows) | Make sure Visual Studio Build Tools are installed with C++ workload |
| `cc: error: unrecognized option '-m64'` (macOS ARM) | Run `rustup target add aarch64-apple-darwin` and build with `--target aarch64-apple-darwin` |

> **Don't need voice?** Skip `voice-gateway` entirely. The core Sigil library handles all E2EE logic without CMake, Opus, or any native dependencies. You'd wire it into your own voice transport layer instead.

## Project Structure

```
src/
├── session.rs            # SigilSession — start here
├── lib.rs                # crate root, re-exports
├── error.rs              # SigilError enum
├── types.rs              # shared constants (key sizes, magic, labels)
├── crypto/
│   ├── codec.rs          # unencrypted ranges per codec
│   ├── frame_crypto.rs   # AES-128-GCM encrypt/decrypt
│   ├── key_ratchet.rs    # HKDF-based key generation ratchet
│   └── uleb128.rs        # ULEB128 encoder/decoder
├── frame/
│   ├── encryptor.rs      # codec-aware frame encryption
│   ├── decryptor.rs      # codec-unaware frame decryption
│   └── payload.rs        # DAVE footer builder/parser
├── gateway/
│   ├── opcodes.rs        # DAVE opcodes 21-31 and payloads
│   ├── handler.rs        # DaveEvent dispatch
│   └── session.rs        # session state machine
└── mls/
    ├── config.rs         # ciphersuite + group config
    ├── credential.rs     # DaveIdentity (Basic credential)
    ├── group.rs          # MLS group lifecycle
    └── key_package.rs    # key package generation
```

## Sigil vs. Davey

[Davey](https://github.com/Snazzah/davey) is another excellent pure-Rust implementation of the DAVE protocol by Discord community developers. It powers the official `@discordjs/voice` package. 

Here's a quick comparison to help you choose:

| Feature | Sigil | Davey |
|---------|-------|-------|
| **Primary Use Case** | Direct Rust bot integration (e.g. Songbird/Serenity) | Multi-language SDK (Rust core + Node.js/Python via NAPI/PyO3) |
| **Architecture** | Highly modular (separate `crypto`, `mls`, `frame`, `gateway` layers) | Monolithic `session.rs` handling all logic |
| **API Surface** | High-level `SigilSession` facade | Granular, configurable `DaveSession` |
| **Integrations** | Built-in `voice-gateway` feature for Songbird/Serenity | Official Node.js/Python bindings |
| **Gateway Opcodes**| Includes raw payload structs and dispatcher for opcodes 21–31 | Focuses entirely on crypto/MLS logic |
| **AES-GCM Base** | Uses the standard high-level `aes-gcm` crate | Custom raw implementation using `aes` + `ghash` for low-level exactness |
| **Maturity** | Early-stage, structured codebase | Battle-tested, multi-contributor, adopted by `discord.js` |

**Which one should I use?**
Use **Sigil** if you are building a Rust-native Discord bot (using Serenity/Songbird/Twilight) and want a drop-in component that manages both the gateway E2EE lifecycle and the encryption in a clean, segregated way.

Use **Davey** if you need Node.js/Python bindings, require strict DAVE protocol edge-cases (like privacy validation codes, passthrough modes, re-init support), or prefer a battle-tested library that mirrors Discord's JS architecture.

## Contributing

PRs welcome. If you break `cargo clippy` or `cargo fmt --check`, your PR gets sent to the shadow realm.

## License

MIT — [Karmanya Ravindra](https://github.com/Karmanya03)
