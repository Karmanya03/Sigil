# 🜏 Sigil

> End-to-end encryption for Discord voice & video. No backdoors, no compromises, no C dependencies.

Sigil is a pure-Rust implementation of Discord's [DAVE protocol](https://daveprotocol.com/) — the encryption layer that keeps your voice and video calls private. It handles everything from MLS key exchange to frame-level AES-128-GCM encryption, so you can plug E2EE into your Discord bot without losing sleep over key management.

## Why Sigil?

Because your bot's voice traffic doesn't need to be an open book. Sigil gives you:

- **One struct to rule them all** — `SigilSession` wraps MLS, key derivation, frame crypto, and gateway events into a single cohesive API. No PhD required.
- **Codec-aware encryption** — VP8, VP9, H.264, H.265, AV1, Opus. Each codec has byte ranges that *must* stay unencrypted for WebRTC to function. Sigil handles all of it.
- **Protocol v1.1 compliant** — ciphersuite 2, truncated 8-byte tags, `0xFAFA` magic markers, HKDF ratchets, the whole nine yards.
- **Zero C dependencies** — no CMake, no OpenSSL, no `audiopus_sys`. Just `cargo build` and go.

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
sigil = { git = "https://github.com/Karmanya03/Sigil" }
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

The core library compiles without any native dependencies. The `voice-gateway` feature is opt-in for when you want direct Songbird/Serenity interop.

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

## Contributing

PRs welcome. If you break `cargo clippy` or `cargo fmt --check`, your PR gets sent to the shadow realm.

## License

MIT — [Karmanya Ravindra](https://github.com/Karmanya03)
