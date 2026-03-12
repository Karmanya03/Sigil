/// Sigil Voice — Serenity Integration
///
/// Implements [`VoiceGatewayManager`] so Serenity automatically delivers all voice events
/// (VOICE_STATE_UPDATE and VOICE_SERVER_UPDATE) directly to Sigil without the user having
/// to manually route events in their `EventHandler`.
///
/// # Setup
///
/// ```rust,no_run
/// use sigil_voice::serenity_hook::{SigilVoiceManager, SigilVoiceManagerKey};
/// use serenity::all::GatewayIntents;
/// use serenity::async_trait;
/// use serenity::prelude::*;
/// use std::sync::Arc;
///
/// struct MyHandler;
///
/// #[async_trait]
/// impl EventHandler for MyHandler {}
///
/// #[tokio::main]
/// async fn main() {
///     let token = std::env::var("DISCORD_TOKEN").unwrap();
///     let sigil = Arc::new(SigilVoiceManager::default());
///
///     let mut client = serenity::Client::builder(&token, GatewayIntents::non_privileged()
///         | GatewayIntents::GUILD_VOICE_STATES)
///         .event_handler(MyHandler)
///         .voice_manager_arc(sigil.clone())
///         .await.unwrap();
///
///     client.data.write().await.insert::<SigilVoiceManagerKey>(sigil);
///     client.start().await.unwrap();
/// }
/// ```
///
/// # Joining/Leaving
///
/// ```rust,no_run
/// use sigil_voice::serenity_hook::SigilVoiceManagerKey;
/// use serenity::all::{ChannelId, GuildId};
/// use serenity::prelude::*;
///
/// async fn example(ctx: &Context, guild_id: GuildId, channel_id: ChannelId) {
///     let data = ctx.data.read().await;
///     let mgr = data.get::<SigilVoiceManagerKey>().unwrap();
///     mgr.join(guild_id, channel_id).await;
/// }
/// ```
use futures::channel::mpsc::UnboundedSender;
use serenity::async_trait;
use serenity::all::{ChannelId, GuildId, ShardRunnerMessage, UserId};
use serenity::gateway::VoiceGatewayManager;
use serenity::model::voice::VoiceState;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::call::Call;
use crate::driver::CoreDriver;

// ──────────────────────────────────────────────────────────────────────────────
// Pending connection state
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct PendingConnection {
    session_id: Option<String>,
    endpoint:   Option<String>,
    token:      Option<String>,
    channel_id: Option<ChannelId>,
}

impl PendingConnection {
    fn is_ready(&self) -> bool {
        self.session_id.is_some() && self.endpoint.is_some() && self.token.is_some()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Connection state tracker — prevents duplicate connects
// ──────────────────────────────────────────────────────────────────────────────

/// Tracks the credentials of the current active connection so we can detect
/// when Discord invalidates the session and sends fresh credentials.
#[derive(Default, Clone)]
struct ActiveConnectionInfo {
    endpoint:   String,
    token:      String,
    session_id: String,
}

// ──────────────────────────────────────────────────────────────────────────────
// SigilVoiceManager
// ──────────────────────────────────────────────────────────────────────────────

/// Drop-in replacement for Songbird. Register once with `.voice_manager_arc()` on the
/// `ClientBuilder` and Serenity handles all the plumbing automatically.
pub struct SigilVoiceManager {
    /// bot's own user_id — set by `initialise()`
    user_id: Mutex<u64>,
    /// shard_id → UnboundedSender (for sending Gateway OP 4)
    shards: Mutex<HashMap<u32, UnboundedSender<ShardRunnerMessage>>>,
    /// guild_id → pending session info
    pending: Arc<Mutex<HashMap<GuildId, PendingConnection>>>,
    /// guild_id → active Call handle
    pub calls: Arc<Mutex<HashMap<GuildId, Arc<Call>>>>,
    /// guild_id → credentials of the currently active connection
    active_info: Arc<Mutex<HashMap<GuildId, ActiveConnectionInfo>>>,
    /// guild_id → true if a CoreDriver::connect() is currently in-flight
    connecting: Arc<Mutex<HashMap<GuildId, bool>>>,
}

impl Default for SigilVoiceManager {
    fn default() -> Self {
        Self {
            user_id:     Mutex::new(0),
            shards:      Mutex::new(HashMap::new()),
            pending:     Arc::new(Mutex::new(HashMap::new())),
            calls:       Arc::new(Mutex::new(HashMap::new())),
            active_info: Arc::new(Mutex::new(HashMap::new())),
            connecting:  Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl SigilVoiceManager {
    // ── Public API ─────────────────────────────────────────────────────────

    /// Move the bot into `channel_id` in `guild_id`.
    /// Sends Gateway OP 4 via the shard's messenger.
    pub async fn join(&self, guild_id: GuildId, channel_id: ChannelId) {
        self.update_voice_state(guild_id, Some(channel_id)).await;
    }

    /// Disconnect the bot from voice in `guild_id`.
    pub async fn leave(&self, guild_id: GuildId) {
        self.update_voice_state(guild_id, None).await;
        self.teardown_guild(guild_id).await;
    }

    /// Retrieve the active `Call` for a guild, if one exists.
    pub async fn get_call(&self, guild_id: GuildId) -> Option<Arc<Call>> {
        self.calls.lock().await.get(&guild_id).cloned()
    }

    /// Check if the voice WebSocket for a guild is still alive.
    pub async fn is_connected(&self, guild_id: GuildId) -> bool {
        if let Some(call) = self.calls.lock().await.get(&guild_id) {
            call.driver.is_ws_alive()
        } else {
            false
        }
    }

    // ── Internal helpers ────────────────────────────────────────────────────

    /// Full teardown for a guild — stops tracks, removes call, clears state.
    async fn teardown_guild(&self, guild_id: GuildId) {
        // Stop and remove the active call
        if let Some(old_call) = self.calls.lock().await.remove(&guild_id) {
            tracing::warn!("♻️ Tearing down old Call for guild={}", guild_id);
            old_call.driver.request_shutdown();
            old_call.stop().await;
        }
        self.pending.lock().await.remove(&guild_id);
        self.active_info.lock().await.remove(&guild_id);
        self.connecting.lock().await.remove(&guild_id);
    }

    /// Send Gateway OP 4 (Voice State Update) to Discord via shards map.
    async fn update_voice_state(&self, guild_id: GuildId, channel_id: Option<ChannelId>) {
        let channel_val = match channel_id {
            Some(c) => serde_json::Value::String(c.get().to_string()),
            None    => serde_json::Value::Null,
        };
        let payload = serde_json::json!({
            "op": 4,
            "d": {
                "guild_id":   guild_id.get().to_string(),
                "channel_id": channel_val,
                "self_mute":  false,
                "self_deaf":  false
            }
        });
        let json = match serde_json::to_string(&payload) {
            Ok(j) => j,
            Err(e) => {
                tracing::error!("Failed to serialize OP 4: {:?}", e);
                return;
            }
        };

        let shards = self.shards.lock().await;
        for (shard_id, sender) in shards.iter() {
            tracing::info!(
                "Sending OP 4 to shard {} [guild={}, channel={:?}]",
                shard_id, guild_id, channel_id
            );
            let msg = ShardRunnerMessage::Message(WsMessage::Text(json.clone().into()));
            if sender.unbounded_send(msg).is_err() {
                tracing::warn!("Shard {} sender is closed", shard_id);
            }
        }
    }

    /// Once we have session_id + endpoint + token, bootstrap the CoreDriver.
    ///
    /// FIX: Reordered to set the `connecting` guard BEFORE the `active_info`
    /// dedup check, closing the TOCTOU race that allowed two concurrent calls
    /// to both pass the check and send duplicate Identify (OP 0) payloads,
    /// causing Discord to close with code=4005 "Already authenticated".
    async fn check_and_connect(&self, guild_id: GuildId) {
        // ── Extract pending args (and drop the lock immediately) ───────────
        let args = {
            let p = self.pending.lock().await;
            let Some(entry) = p.get(&guild_id) else { return };
            if !entry.is_ready() {
                tracing::debug!(
                    "Pending for guild {} — session={} endpoint={} token={}",
                    guild_id,
                    entry.session_id.is_some(),
                    entry.endpoint.is_some(),
                    entry.token.is_some(),
                );
                return;
            }
            (
                entry.endpoint.clone().unwrap(),
                entry.session_id.clone().unwrap(),
                entry.token.clone().unwrap(),
            )
        };
        // pending lock is dropped here

        let (endpoint, session_id, token) = args;

        // FIX #1: Set the connecting guard FIRST, before the active_info
        // dedup check. Previously this was done AFTER, which left a TOCTOU
        // window where two concurrent check_and_connect() calls (one from
        // VoiceStateUpdate, one from VoiceServerUpdate) could both pass the
        // dedup check before either had set the guard — producing two
        // Identify (OP 0) payloads and causing Discord close code=4005.
        {
            let mut conn = self.connecting.lock().await;
            if conn.get(&guild_id).copied().unwrap_or(false) {
                tracing::warn!("Connect already in-flight for guild={} — skipping", guild_id);
                return;
            }
            conn.insert(guild_id, true);
        }
        // connecting lock is dropped here — guard is now set

        // ── Deduplicate: skip if already connected with same credentials ──
        // FIX #2: This check now runs AFTER the connecting guard is set.
        // Also: we drop the active_info lock before acquiring the calls lock
        // to prevent potential lock-order deadlock.
        {
            let active = self.active_info.lock().await;
            if let Some(info) = active.get(&guild_id) {
                if info.endpoint == endpoint
                    && info.token == token
                    && info.session_id == session_id
                {
                    // Drop active_info lock before acquiring calls lock
                    drop(active);
                    let still_alive = self.calls.lock().await
                        .get(&guild_id)
                        .map(|c| c.driver.is_ws_alive())
                        .unwrap_or(false);
                    if still_alive {
                        tracing::debug!(
                            "Duplicate connect attempt with same credentials — skipping [guild={}]",
                            guild_id
                        );
                        self.pending.lock().await.remove(&guild_id);
                        // FIX: Clear the connecting guard we set above
                        self.connecting.lock().await.remove(&guild_id);
                        return;
                    }
                }
            }
        }
        // active_info lock is dropped here

        // Flush pending so duplicate events don't re-trigger a connect
        self.pending.lock().await.remove(&guild_id);

        // ── Tear down any existing connection (reconnect scenario) ─────────
        if let Some(old_call) = self.calls.lock().await.remove(&guild_id) {
            tracing::warn!("♻️ Tearing down old Call for guild={} (session invalidated)", guild_id);
            old_call.driver.request_shutdown();
            old_call.stop().await;
            // Brief delay so OS-level resources (sockets, tasks) settle
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let server_id_str = guild_id.get().to_string();
        let user_id_str   = self.user_id.lock().await.to_string();
        let calls_clone   = self.calls.clone();
        let active_clone  = self.active_info.clone();
        let conn_clone    = self.connecting.clone();

        let new_info = ActiveConnectionInfo {
            endpoint:   endpoint.clone(),
            token:      token.clone(),
            session_id: session_id.clone(),
        };

        tokio::spawn(async move {
            tracing::info!("🚀 Bootstrapping CoreDriver for guild={}", guild_id);

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                CoreDriver::connect(&endpoint, &server_id_str, &user_id_str, &session_id, &token)
            ).await;

            // Clear the connecting guard FIRST — always, even on failure
            conn_clone.lock().await.remove(&guild_id);

            match result {
                Ok(Ok(driver)) => {
                    // FIX #3: Verify the WS is still alive before inserting
                    // the Call. The WS background task may have received a
                    // close frame (e.g. 4005) during or immediately after
                    // connect(). Inserting a dead Call causes start_mixing()
                    // to immediately exit with 0 frames → silence.
                    if !driver.is_ws_alive() {
                        tracing::error!(
                            "❌ CoreDriver WS died immediately after connect [guild={}] — not inserting Call",
                            guild_id
                        );
                        driver.request_shutdown();
                        active_clone.lock().await.remove(&guild_id);
                        return;
                    }
                    tracing::info!("✅ CoreDriver ready for guild={}", guild_id);
                    let call = Arc::new(Call::new(driver));
                    calls_clone.lock().await.insert(guild_id, call);
                    active_clone.lock().await.insert(guild_id, new_info);
                    tracing::info!("📞 Call inserted for guild={} — ready for playback", guild_id);
                }
                Ok(Err(e)) => {
                    tracing::error!("❌ CoreDriver::connect failed for guild={}: {:?}", guild_id, e);
                    active_clone.lock().await.remove(&guild_id);
                }
                Err(_) => {
                    tracing::error!("❌ CoreDriver::connect timed out for guild={} (30s)", guild_id);
                    active_clone.lock().await.remove(&guild_id);
                }
            }
        });
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// VoiceGatewayManager implementation — Serenity calls these automatically
// ──────────────────────────────────────────────────────────────────────────────

#[async_trait]
impl VoiceGatewayManager for SigilVoiceManager {
    /// Called once at startup with the bot's UserId and shard count.
    async fn initialise(&self, _shard_count: u32, user_id: UserId) {
        *self.user_id.lock().await = user_id.get();
        tracing::info!("SigilVoiceManager initialised [user_id={}]", user_id);
    }

    /// Called when a shard connects/reconnects — provides the channel to send gateway messages.
    async fn register_shard(&self, shard_id: u32, sender: UnboundedSender<ShardRunnerMessage>) {
        tracing::info!("SigilVoiceManager: shard {} registered", shard_id);
        self.shards.lock().await.insert(shard_id, sender);
    }

    /// Called when a shard disconnects.
    async fn deregister_shard(&self, shard_id: u32) {
        tracing::info!("SigilVoiceManager: shard {} deregistered", shard_id);
        self.shards.lock().await.remove(&shard_id);
    }

    /// Called by Serenity for every VOICE_SERVER_UPDATE.
    /// Contains the voice gateway endpoint and authentication token.
    async fn server_update(&self, guild_id: GuildId, endpoint: &Option<String>, token: &str) {
        let Some(ep) = endpoint else {
            tracing::warn!("VoiceServerUpdate for guild={} had no endpoint — session destroyed", guild_id);
            self.teardown_guild(guild_id).await;
            return;
        };

        // Detect if this is a reconnect (new credentials while already connected)
        if let Some(call) = self.calls.lock().await.get(&guild_id) {
            if call.driver.is_ws_alive() {
                tracing::warn!(
                    "🔄 New VoiceServerUpdate while connected — session was invalidated [guild={}]",
                    guild_id
                );
            }
        }

        tracing::info!("VoiceServerUpdate [guild={}, endpoint={}]", guild_id, ep);

        {
            let mut p = self.pending.lock().await;
            let entry = p.entry(guild_id).or_default();
            entry.endpoint = Some(ep.clone());
            entry.token    = Some(token.to_string());
        }
        // pending lock dropped before check_and_connect

        self.check_and_connect(guild_id).await;
    }

    /// Called by Serenity for every VOICE_STATE_UPDATE for our bot.
    /// Contains the session_id needed to authenticate with the voice gateway.
    async fn state_update(&self, guild_id: GuildId, voice_state: &VoiceState) {
        // We only care about our own bot's state
        let our_id = *self.user_id.lock().await;
        if voice_state.user_id.get() != our_id {
            return;
        }

        // Bot left voice
        if voice_state.channel_id.is_none() {
            tracing::info!("Bot disconnected from voice in guild={}", guild_id);
            self.teardown_guild(guild_id).await;
            return;
        }

        tracing::info!(
            "VoiceStateUpdate [guild={}, channel={:?}, session={}]",
            guild_id, voice_state.channel_id, voice_state.session_id
        );

        {
            let mut p = self.pending.lock().await;
            let entry = p.entry(guild_id).or_default();
            entry.session_id = Some(voice_state.session_id.clone());
            entry.channel_id = voice_state.channel_id;
        }
        // pending lock dropped before check_and_connect

        self.check_and_connect(guild_id).await;
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// TypeMap key
// ──────────────────────────────────────────────────────────────────────────────

/// Key for accessing `SigilVoiceManager` from `ctx.data`.
///
/// ```rust,no_run
/// use sigil_voice::serenity_hook::SigilVoiceManagerKey;
/// use serenity::all::{ChannelId, GuildId};
/// use serenity::prelude::*;
///
/// async fn example(ctx: &Context, guild_id: GuildId, channel_id: ChannelId) {
///     let data = ctx.data.read().await;
///     let mgr = data.get::<SigilVoiceManagerKey>().unwrap();
///     mgr.join(guild_id, channel_id).await;
/// }
/// ```
pub struct SigilVoiceManagerKey;
impl serenity::prelude::TypeMapKey for SigilVoiceManagerKey {
    type Value = Arc<SigilVoiceManager>;
}
