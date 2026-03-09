<div align="center">
  <img src="assets/SIGIL-logo.png" alt="Sigil Logo" width="400" />

  # Sigil

  **Pure-Rust Discord DAVE (Audio/Video E2EE) Protocol Framework.**<br>
  Keep your voice traffic off the books.

  <br>

  [![crates.io](https://img.shields.io/crates/v/sigil-discord?style=flat-square&color=c7102a)](https://crates.io/crates/sigil-discord)
  [![license](https://img.shields.io/crates/l/sigil-discord?style=flat-square&color=c7102a)](#license)
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

### Sigil-Voice Custom Driver

Why use Songbird when you can pipe audio directly through Sigil?

We've built `sigil-voice`—a completely standalone Discord Voice v8 async driver that seamlessly handles WebSocket handshakes, UDP Hole Punching, Audio Pipeline routing (PCM -> Opus -> DAVE -> UDP AES-GCM), and native `yt-dlp` music streaming. 

**It 100% replaces Songbird.**

### Under the Hood Features
* **Serenity Hooking (`SigilVoiceManager`)**: Exposes a `SigilVoiceClient` trait that seamlessly drops into your Serenity bot to harvest `endpoint`, `token`, and `session_id` directly from Discord events, automatically spawning `CoreDriver` instances on VC join.
* **Unified Background WS Loop**: Natively handles the DAVE OP 21-31 state transitions inside a detached Tokio thread via `Arc<Mutex<SigilSession>>`.
* **Precise `seq_ack` Heartbeats**: Captures sequence acknowledgments from text payloads and executes perfect outbound OP 3 pings at the requested server interval.
* **Transport Encryption Nonces**: Strictly isolated RTP AADs with properly appended, mathematically independent 96-bit AES-GCM padded nonces to ensure UDP block cipher integrity.
* **Silence Frame Termination**: Automatically writes `0xF8 0xFF 0xFE` Opus termination sequences when an audio channel drops out, guaranteeing Discord clients experience no audio artifacting.
* **Native Opus Media Injection**: Drops the fake mock bytes for real Opus C-bindings `0.3.0-rc.0` to correctly convert `ffmpeg`/`yt-dlp` 48kHz PCM stdout pipes.

If you're building a music bot with [Serenity](https://github.com/serenity-rs/serenity), simply add `sigil-voice` instead:

#### Cargo Setup

```toml
[dependencies]
sigil-discord = { git = "https://github.com/Karmanya03/Sigil" }
sigil-voice = { git = "https://github.com/Karmanya03/Sigil" }
serenity = { version = "0.12", default-features = false, features = ["client", "gateway", "model", "rustls_backend"] }
tokio = { version = "1", features = ["full"] }
```

#### Advanced Multi-Track Mixing

Sigil-Voice can play multiple tracks simultaneously in the same voice channel. You can control each one independently.

```rust
use sigil_voice::source::YtDlpSource;
use sigil_voice::driver::CoreDriver;
use sigil_voice::call::Call;
use sigil_voice::track::TrackEvent;

async fn play_complex_scene(call: &Call) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 1. Play background music at low volume
    let bgm = YtDlpSource::create_track("https://youtube.com/watch?v=background-lofi").await?;
    let bgm_handle = call.play(bgm).await;
    bgm_handle.set_volume(0.2).await;

    // 2. Play an effect over it
    let sfx = YtDlpSource::create_track("https://youtube.com/watch?v=explosion-sfx").await?;
    let mut sfx_handle = call.play(sfx).await;

    // 3. React to events (e.g. knowing when the SFX ends)
    let mut events = sfx_handle.take_event_receiver().unwrap();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if let TrackEvent::End = event {
                println!("The explosion finished playing!");
            }
        }
    });

    Ok(())
}
```

#### Complete Bot Setup

Sigil Voice fully replaces Songbird. Here's a complete working setup:

```rust
use sigil_voice::serenity_hook::{SigilVoiceManager, SigilVoiceManagerKey};
use sigil_voice::source::YtDlpSource;
use serenity::async_trait;
use serenity::model::event::VoiceServerUpdateEvent;
use serenity::model::prelude::*;
use serenity::prelude::*;
use std::sync::Arc;

struct Bot;

#[async_trait]
impl EventHandler for Bot {
    // ✅ Required: Wire these two events so Sigil can bootstrap the voice connection
    async fn voice_state_update(&self, ctx: Context, _old: Option<VoiceState>, new: VoiceState) {
        let data = ctx.data.read().await;
        if let Some(mgr) = data.get::<SigilVoiceManagerKey>() {
            mgr.handle_voice_state(&new).await;
        }
    }

    async fn voice_server_update(&self, ctx: Context, event: VoiceServerUpdateEvent) {
        let data = ctx.data.read().await;
        if let Some(mgr) = data.get::<SigilVoiceManagerKey>() {
            mgr.handle_voice_server(&event).await;
        }
    }
}

#[tokio::main]
async fn main() {
    let bot_user_id: u64 = /* your bot user ID */;
    let manager = Arc::new(SigilVoiceManager::new(bot_user_id));

    let mut client = Client::builder(&token, GatewayIntents::non_privileged() | GatewayIntents::GUILD_VOICE_STATES)
        .event_handler(Bot)
        .await
        .unwrap();

    // Register the manager in the TypeMap
    client.data.write().await.insert::<SigilVoiceManagerKey>(manager);
    client.start().await.unwrap();
}
```

#### Voice Commands — Join, Leave & Play

> ⚠️ **Important**: Do **not** use `guild_id.connect(&ctx, ...)` — that uses Serenity's internal voice manager. Sigil provides its own `manager.join()` that sends the correct Gateway OP 4 packet.

```rust
#[command]
async fn join(ctx: &Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).unwrap();
    let channel_id = guild
        .voice_states
        .get(&msg.author.id)
        .and_then(|vs| vs.channel_id)
        .ok_or("You must be in a voice channel!")?;

    let data = ctx.data.read().await;
    let manager = data.get::<SigilVoiceManagerKey>().expect("Manager not found");

    // Sends Discord Gateway OP 4 to physically move the bot into the channel
    manager.join(&ctx.shard, guild.id, channel_id).await;

    msg.reply(ctx, "✅ Joined your voice channel!").await?;
    Ok(())
}

#[command]
async fn leave(ctx: &Context, msg: &Message) -> CommandResult {
    let guild_id = msg.guild_id.ok_or("Must be in a guild")?;
    let data = ctx.data.read().await;
    let manager = data.get::<SigilVoiceManagerKey>().expect("Manager not found");

    // Sends OP 4 with channel_id: null and cleans up the local driver
    manager.leave(&ctx.shard, guild_id).await;

    msg.reply(ctx, "👋 Left the voice channel.").await?;
    Ok(())
}

#[command]
async fn play(ctx: &Context, msg: &Message, mut args: Args) -> CommandResult {
    let url = args.single::<String>()?;
    let guild_id = msg.guild_id.ok_or("Must be in a guild")?;

    let data = ctx.data.read().await;
    let manager = data.get::<SigilVoiceManagerKey>().expect("Manager not found");

    // Wait for the CoreDriver to bootstrap (triggered by the voice events above)
    let call = loop {
        if let Some(c) = manager.get_call(guild_id).await {
            break c;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    };

    let track = YtDlpSource::create_track(&url).await?;
    let handle = call.play(track).await;

    msg.reply(ctx, "🎶 Now playing!").await?;
    Ok(())
}

#[command]
async fn stop(ctx: &Context, msg: &Message) -> CommandResult {
    let guild_id = msg.guild_id.unwrap();
    let data = ctx.data.read().await;
    let manager = data.get::<SigilVoiceManagerKey>().unwrap();

    if let Some(call) = manager.get_call(guild_id).await {
        call.stop().await;
        msg.reply(ctx, "⏹️ Stopped.").await?;
    }
    Ok(())
}
```

> **Requirements:** `yt-dlp` and `ffmpeg` must be installed and on your `PATH`. Build with `$env:CMAKE_POLICY_VERSION_MINIMUM=3.5; cargo build` on Windows.


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
