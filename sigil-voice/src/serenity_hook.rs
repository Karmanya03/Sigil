use crate::call::Call;
use crate::driver::CoreDriver;
use serenity::all::{ChannelId, GuildId, VoiceServerUpdateEvent, VoiceState};
use serenity::gateway::ShardMessenger;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Default)]
pub struct PendingConnection {
    pub session_id: Option<String>,
    pub endpoint: Option<String>,
    pub token: Option<String>,
}

impl PendingConnection {
    pub fn is_ready(&self) -> bool {
        self.session_id.is_some() && self.endpoint.is_some() && self.token.is_some()
    }
}

/// The main gateway state manager for routing Serenity Voice events to Sigil's driver.
/// Acts as a drop-in replacement for the `Songbird` typemap instance.
pub struct SigilVoiceManager {
    user_id: u64,
    pending: Arc<Mutex<HashMap<GuildId, PendingConnection>>>,
    pub calls: Arc<Mutex<HashMap<GuildId, Arc<Call>>>>,
}

impl SigilVoiceManager {
    /// Initialize a new Voice Manager tracking the host Bot's user ID.
    pub fn new(user_id: u64) -> Self {
        Self {
            user_id,
            pending: Arc::new(Mutex::new(HashMap::new())),
            calls: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// **Primary API** — Send Gateway OP 4 (Voice State Update) through the Discord WebSocket
    /// to physically move the bot into a voice channel.  Discord responds with the OP 4
    /// `VoiceStateUpdate` and `VoiceServerUpdate` events that `SigilVoiceManager` traps in order
    /// to bootstrap the `CoreDriver` automatically.
    ///
    /// # Example
    /// ```rust,no_run
    /// manager.join(&ctx.shard, guild_id, channel_id).await;
    /// ```
    pub async fn join(&self, shard: &ShardMessenger, guild_id: GuildId, channel_id: ChannelId) {
        // Tear down any existing session for this guild first
        {
            let mut calls = self.calls.lock().await;
            calls.remove(&guild_id);
        }
        {
            let mut pending = self.pending.lock().await;
            pending.remove(&guild_id);
        }

        tracing::info!(
            "Sending OP 4 VoiceStateUpdate to join guild={} channel={}",
            guild_id,
            channel_id
        );

        // Discord Gateway OP 4: Update Voice State
        // https://discord.com/developers/docs/topics/gateway-events#update-voice-state
        let payload = serde_json::json!({
            "op": 4,
            "d": {
                "guild_id": guild_id.get().to_string(),
                "channel_id": channel_id.get().to_string(),
                "self_mute": false,
                "self_deaf": false
            }
        });

        match serde_json::to_string(&payload) {
            Ok(json) => {
                use serenity::all::ShardRunnerMessage;
                shard.send_to_shard(ShardRunnerMessage::Message(
                    tokio_tungstenite::tungstenite::Message::Text(json.into()),
                ));
            }
            Err(e) => tracing::error!("Failed to serialize OP 4 join payload: {}", e),
        }
    }

    /// **Primary API** — Disconnect the bot from any voice channel in the given guild.
    /// Sends Gateway OP 4 with `channel_id: null` and cleans up the local `CoreDriver` session.
    ///
    /// # Example
    /// ```rust,no_run
    /// manager.leave(&ctx.shard, guild_id).await;
    /// ```
    pub async fn leave(&self, shard: &ShardMessenger, guild_id: GuildId) {
        tracing::info!("Sending OP 4 VoiceStateUpdate to leave guild={}", guild_id);

        let payload = serde_json::json!({
            "op": 4,
            "d": {
                "guild_id": guild_id.get().to_string(),
                "channel_id": null,
                "self_mute": false,
                "self_deaf": false
            }
        });

        match serde_json::to_string(&payload) {
            Ok(json) => {
                use serenity::all::ShardRunnerMessage;
                shard.send_to_shard(ShardRunnerMessage::Message(
                    tokio_tungstenite::tungstenite::Message::Text(json.into()),
                ));
            }
            Err(e) => tracing::error!("Failed to serialize OP 4 leave payload: {}", e),
        }

        // Cleanup local state
        let mut calls = self.calls.lock().await;
        calls.remove(&guild_id);
        drop(calls);
        let mut pending = self.pending.lock().await;
        pending.remove(&guild_id);
    }

    /// Retrieve an active Call handle for a specific Guild, if it exists.
    pub async fn get_call(&self, guild_id: GuildId) -> Option<Arc<Call>> {
        let calls = self.calls.lock().await;
        calls.get(&guild_id).cloned()
    }

    /// Wire this into your Serenity `EventHandler::voice_state_update`.
    /// Traps the bot's own VoiceStateUpdate so we get the session_id.
    pub async fn handle_voice_state(&self, state: &VoiceState) {
        if state.user_id.get() != self.user_id {
            return;
        }

        let Some(guild_id) = state.guild_id else {
            return;
        };

        // Bot left voice — leave() already cleaned up, nothing more to do
        if state.channel_id.is_none() {
            tracing::info!("Bot left voice in guild {}", guild_id);
            return;
        }

        tracing::info!(
            "Bot joined channel {:?} in guild {}, session_id={}",
            state.channel_id,
            guild_id,
            state.session_id
        );

        let mut p = self.pending.lock().await;
        let entry = p.entry(guild_id).or_default();
        entry.session_id = Some(state.session_id.clone());
        drop(p);

        self.check_and_connect(guild_id).await;
    }

    /// Wire this into your Serenity `EventHandler::voice_server_update`.
    /// Traps the server endpoint and token so we can open the voice WS connection.
    pub async fn handle_voice_server(&self, server: &VoiceServerUpdateEvent) {
        let Some(guild_id) = server.guild_id else {
            return;
        };

        let Some(endpoint) = &server.endpoint else {
            tracing::warn!("VoiceServerUpdate with no endpoint for guild {}", guild_id);
            return;
        };

        tracing::info!(
            "Got VoiceServerUpdate for guild {}: endpoint={}",
            guild_id,
            endpoint
        );

        let mut p = self.pending.lock().await;
        let entry = p.entry(guild_id).or_default();
        entry.endpoint = Some(endpoint.clone());
        entry.token = Some(server.token.clone());
        drop(p);

        self.check_and_connect(guild_id).await;
    }

    /// Internal — once all three pieces are collected, spin up the `CoreDriver`.
    async fn check_and_connect(&self, guild_id: GuildId) {
        let args = {
            let p = self.pending.lock().await;
            let Some(entry) = p.get(&guild_id) else {
                return;
            };
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

        // Flush pending immediately so duplicate events don't trigger a second connect
        {
            let mut p = self.pending.lock().await;
            p.remove(&guild_id);
        }

        let (endpoint, session_id, token) = args;
        let server_id_str = guild_id.get().to_string();
        let user_id_str = self.user_id.to_string();
        let sigil_calls = self.calls.clone();

        tokio::spawn(async move {
            tracing::info!("Bootstrapping Sigil CoreDriver for guild={}", guild_id);
            match CoreDriver::connect(&endpoint, &server_id_str, &user_id_str, &session_id, &token)
                .await
            {
                Ok(driver) => {
                    tracing::info!("✅ CoreDriver connected for guild={}", guild_id);
                    let call = Arc::new(Call::new(driver));
                    let mut c = sigil_calls.lock().await;
                    c.insert(guild_id, call);
                }
                Err(e) => {
                    tracing::error!("❌ CoreDriver failed for guild={}: {:?}", guild_id, e);
                }
            }
        });
    }
}

/// TypeMap key so the manager can live in Serenity's `ctx.data`.
///
/// # Example
/// ```rust,no_run
/// ctx.data.read().await.get::<SigilVoiceManagerKey>().unwrap()
/// ```
pub struct SigilVoiceManagerKey;
impl serenity::prelude::TypeMapKey for SigilVoiceManagerKey {
    type Value = Arc<SigilVoiceManager>;
}
