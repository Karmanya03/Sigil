/// Sigil Voice — Serenity Integration
///
/// Implements [`VoiceGatewayManager`] so Serenity automatically delivers all voice events
/// (VOICE_STATE_UPDATE and VOICE_SERVER_UPDATE) directly to Sigil without the user having
/// to manually route events in their `EventHandler`.
use futures::channel::mpsc::UnboundedSender;
use serenity::async_trait;
use serenity::all::{ChannelId, GuildId, ShardRunnerMessage, UserId};
use serenity::gateway::VoiceGatewayManager;
use serenity::model::voice::VoiceState;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::call::Call;
use crate::driver::CoreDriver;

// ──────────────────────────────────────────────────────────────────────────────
// Pending connection state
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct PendingConnection {
    session_id: Option<String>,
    endpoint: Option<String>,
    token: Option<String>,
    channel_id: Option<ChannelId>,
}

impl PendingConnection {
    fn is_ready(&self) -> bool {
        self.session_id.is_some() && self.endpoint.is_some() && self.token.is_some()
    }
}

#[derive(Default, Clone)]
struct ActiveConnectionInfo {
    endpoint: String,
    token: String,
    session_id: String,
}

// ──────────────────────────────────────────────────────────────────────────────
// SigilVoiceManager
// ──────────────────────────────────────────────────────────────────────────────

pub struct SigilVoiceManager {
    user_id: Mutex<u64>,
    shards: Arc<Mutex<HashMap<u32, UnboundedSender<ShardRunnerMessage>>>>,
    pending: Arc<Mutex<HashMap<GuildId, PendingConnection>>>,
    pub calls: Arc<Mutex<HashMap<GuildId, Arc<Call>>>>,
    active_info: Arc<Mutex<HashMap<GuildId, ActiveConnectionInfo>>>,
    connecting: Arc<Mutex<HashMap<GuildId, bool>>>,
    last_channel: Arc<Mutex<HashMap<GuildId, ChannelId>>>,
    reconnect_count: Arc<Mutex<HashMap<GuildId, u8>>>,
    transitioning: Arc<Mutex<HashSet<GuildId>>>,

    /// One-shot notifier fired by `state_update` when it sees the bot leave
    /// (channel_id = None). The watchdog recovery waits on this so we know
    /// Discord has actually processed the disconnect before we rejoin.
    ///
    /// ROOT CAUSE FIX: previously we used a blind 1.5 s sleep from when we
    /// *sent* the leave OP4. Discord takes ~800 ms to confirm it, meaning
    /// the voice server only had ~700 ms of cleanup time — not enough.
    /// Now we wait for the explicit confirmation, then apply the full
    /// rejoin delay from that point.
    leave_notifiers: Arc<Mutex<HashMap<GuildId, Arc<Notify>>>>,

    /// Per-guild pre-connect grace delay (ms). Set by the watchdog before it
    /// triggers a fresh join; consumed inside check_and_connect before
    /// CoreDriver::connect() is called, giving the voice server extra time to
    /// de-register the old session (scales with attempt: 1 s / 2 s / 4 s).
    pre_connect_delay_ms: Arc<Mutex<HashMap<GuildId, u64>>>,
}

/// Maximum consecutive auto-reconnect attempts before giving up.
const MAX_RECONNECT_ATTEMPTS: u8 = 3;

/// How long after Call insertion to check if the driver died immediately.
const LIVENESS_CHECK_DELAY_MS: u64 = 300;

/// Base rejoin delay applied AFTER Discord confirms leave.
/// Scales exponentially: attempt 1 → 2 s, attempt 2 → 4 s, attempt 3 → 8 s.
const BASE_REJOIN_DELAY_MS: u64 = 2_000;

/// Maximum seconds to wait for Discord's leave confirmation before proceeding.
const LEAVE_CONFIRM_TIMEOUT_SECS: u64 = 4;

/// Transition delay for a clean $join command (no stale session to wait out).
const JOIN_TRANSITION_DELAY_MS: u64 = 1_500;

impl Default for SigilVoiceManager {
    fn default() -> Self {
        Self {
            user_id: Mutex::new(0),
            shards: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            calls: Arc::new(Mutex::new(HashMap::new())),
            active_info: Arc::new(Mutex::new(HashMap::new())),
            connecting: Arc::new(Mutex::new(HashMap::new())),
            last_channel: Arc::new(Mutex::new(HashMap::new())),
            reconnect_count: Arc::new(Mutex::new(HashMap::new())),
            transitioning: Arc::new(Mutex::new(HashSet::new())),
            leave_notifiers: Arc::new(Mutex::new(HashMap::new())),
            pre_connect_delay_ms: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl SigilVoiceManager {
    // ── Public API ──────────────────────────────────────────────────────────

    pub async fn join(&self, guild_id: GuildId, channel_id: ChannelId) {
        self.teardown_guild(guild_id).await;
        self.update_voice_state(guild_id, None).await;

        self.transitioning.lock().await.insert(guild_id);
        tokio::time::sleep(std::time::Duration::from_millis(JOIN_TRANSITION_DELAY_MS)).await;
        self.transitioning.lock().await.remove(&guild_id);

        self.last_channel.lock().await.insert(guild_id, channel_id);
        self.reconnect_count.lock().await.remove(&guild_id);
        self.update_voice_state(guild_id, Some(channel_id)).await;
    }

    pub async fn leave(&self, guild_id: GuildId) {
        self.update_voice_state(guild_id, None).await;
        self.teardown_guild(guild_id).await;
        self.last_channel.lock().await.remove(&guild_id);
        self.reconnect_count.lock().await.remove(&guild_id);
        self.transitioning.lock().await.remove(&guild_id);
        // Cancel any in-flight watchdog recovery
        self.leave_notifiers.lock().await.remove(&guild_id);
        self.pre_connect_delay_ms.lock().await.remove(&guild_id);
    }

    pub async fn get_call(&self, guild_id: GuildId) -> Option<Arc<Call>> {
        self.calls.lock().await.get(&guild_id).cloned()
    }

    pub async fn is_connected(&self, guild_id: GuildId) -> bool {
        if let Some(call) = self.calls.lock().await.get(&guild_id) {
            call.driver.is_ws_alive()
        } else {
            false
        }
    }

    // ── Internal helpers ────────────────────────────────────────────────────

    async fn teardown_guild(&self, guild_id: GuildId) {
        if let Some(old_call) = self.calls.lock().await.remove(&guild_id) {
            tracing::warn!("♻️ Tearing down old Call for guild={}", guild_id);
            old_call.driver.request_shutdown();
            old_call.stop().await;
        }
        self.pending.lock().await.remove(&guild_id);
        self.active_info.lock().await.remove(&guild_id);
        self.connecting.lock().await.remove(&guild_id);
    }

    async fn update_voice_state(&self, guild_id: GuildId, channel_id: Option<ChannelId>) {
        let channel_val = match channel_id {
            Some(c) => serde_json::Value::String(c.get().to_string()),
            None => serde_json::Value::Null,
        };
        let payload = serde_json::json!({
            "op": 4,
            "d": {
                "guild_id": guild_id.get().to_string(),
                "channel_id": channel_val,
                "self_mute": false,
                "self_deaf": false
            }
        });
        let json = match serde_json::to_string(&payload) {
            Ok(j) => j,
            Err(e) => { tracing::error!("Failed to serialize OP 4: {:?}", e); return; }
        };

        let shards = self.shards.lock().await;
        let num_shards = shards.len() as u64;
        let target_shard = if num_shards <= 1 { 0u32 } else {
            ((guild_id.get() >> 22) % num_shards) as u32
        };

        if let Some(sender) = shards.get(&target_shard) {
            tracing::info!("Sending OP 4 to shard {} [guild={}, channel={:?}]",
                target_shard, guild_id, channel_id);
            if sender.unbounded_send(
                ShardRunnerMessage::Message(WsMessage::Text(json.into()))
            ).is_err() {
                tracing::warn!("Shard {} sender is closed", target_shard);
            }
        } else {
            tracing::error!("No shard sender for shard {} (guild={})", target_shard, guild_id);
        }
    }

    /// Static helper: send OP4 from inside a spawned task (no &self).
    fn send_op4_via_shards(
        shards: &HashMap<u32, UnboundedSender<ShardRunnerMessage>>,
        guild_id: GuildId,
        channel_id: Option<ChannelId>,
    ) -> bool {
        let channel_val = match channel_id {
            Some(c) => serde_json::Value::String(c.get().to_string()),
            None => serde_json::Value::Null,
        };
        let payload = serde_json::json!({
            "op": 4,
            "d": {
                "guild_id": guild_id.get().to_string(),
                "channel_id": channel_val,
                "self_mute": false,
                "self_deaf": false
            }
        });
        let json = match serde_json::to_string(&payload) {
            Ok(j) => j,
            Err(_) => return false,
        };
        let num_shards = shards.len() as u64;
        let shard_id = if num_shards <= 1 { 0u32 } else {
            ((guild_id.get() >> 22) % num_shards) as u32
        };
        shards.get(&shard_id)
            .map(|s| s.unbounded_send(
                ShardRunnerMessage::Message(WsMessage::Text(json.into()))
            ).is_ok())
            .unwrap_or(false)
    }

    async fn check_and_connect(&self, guild_id: GuildId) {
        if self.transitioning.lock().await.contains(&guild_id) {
            tracing::debug!(
                "check_and_connect suppressed — guild={} is in leave→join transition",
                guild_id
            );
            return;
        }

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

        let (endpoint, session_id, token) = args;

        {
            let mut conn = self.connecting.lock().await;
            if conn.get(&guild_id).copied().unwrap_or(false) {
                tracing::error!("🚨 DUPLICATE CONNECT BLOCKED for guild={}", guild_id);
                return;
            }
            conn.insert(guild_id, true);
        }

        let same_creds = {
            let active = self.active_info.lock().await;
            active.get(&guild_id).map(|info|
                info.endpoint == endpoint
                    && info.token == token
                    && info.session_id == session_id
            ).unwrap_or(false)
        };

        if same_creds {
            let still_alive = self.calls.lock().await
                .get(&guild_id).map(|c| c.driver.is_ws_alive()).unwrap_or(false);
            if still_alive {
                tracing::debug!(
                    "Duplicate connect with same credentials — skipping [guild={}]",
                    guild_id
                );
                self.pending.lock().await.remove(&guild_id);
                self.connecting.lock().await.remove(&guild_id);
                return;
            }
        }

        self.pending.lock().await.remove(&guild_id);

        if let Some(old_call) = self.calls.lock().await.remove(&guild_id) {
            tracing::warn!("♻️ Tearing down old Call for guild={}", guild_id);
            old_call.driver.request_shutdown();
            old_call.stop().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let server_id_str = guild_id.get().to_string();
        let user_id_str = self.user_id.lock().await.to_string();
        let calls_clone           = self.calls.clone();
        let active_clone          = self.active_info.clone();
        let conn_clone            = self.connecting.clone();
        let reconnect_clone       = self.reconnect_count.clone();
        let shards_clone          = self.shards.clone();
        let last_channel_clone    = self.last_channel.clone();
        let transitioning_clone   = self.transitioning.clone();
        let pending_clone         = self.pending.clone();
        let leave_notifiers_clone = self.leave_notifiers.clone();
        let pre_connect_delay_clone = self.pre_connect_delay_ms.clone();

        let new_info = ActiveConnectionInfo {
            endpoint: endpoint.clone(),
            token: token.clone(),
            session_id: session_id.clone(),
        };

        tokio::spawn(async move {
            tracing::warn!("🚀 Bootstrapping CoreDriver for guild={} — CONNECT ATTEMPT", guild_id);

            // ── Pre-connect grace delay (reconnect path only) ──────────────
            // Set by the watchdog before triggering this join.  Scales with
            // attempt number so the voice server has enough time to fully
            // de-register the previous session before we Identify on the new
            // one.  First-time joins leave this at 0 (no delay).
            let pre_delay = pre_connect_delay_clone.lock().await.remove(&guild_id).unwrap_or(0);
            if pre_delay > 0 {
                tracing::info!("⏳ Pre-connect grace: {}ms [guild={}]", pre_delay, guild_id);
                tokio::time::sleep(std::time::Duration::from_millis(pre_delay)).await;
            }

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                CoreDriver::connect(&endpoint, &server_id_str, &user_id_str, &session_id, &token)
            ).await;

            conn_clone.lock().await.remove(&guild_id);

            match result {
                Ok(Ok(driver)) => {
                    if !driver.is_ws_alive() {
                        tracing::error!(
                            "❌ CoreDriver WS died during connect [guild={}] — not inserting",
                            guild_id
                        );
                        driver.request_shutdown();
                        active_clone.lock().await.remove(&guild_id);
                        return;
                    }

                    tracing::info!("✅ CoreDriver ready for guild={}", guild_id);
                    let call = Arc::new(Call::new(driver));
                    calls_clone.lock().await.insert(guild_id, call.clone());
                    active_clone.lock().await.insert(guild_id, new_info);
                    tracing::info!("📞 Call inserted for guild={} — ready for playback", guild_id);

                    // NOTE: reconnect_count is NOT reset here.
                    // It is only reset inside the liveness watchdog AFTER the
                    // connection survives the LIVENESS_CHECK_DELAY_MS window.
                    // Previously it was reset here, which allowed a connection
                    // that died 80ms later (4006) to wipe the counter and cause
                    // the "attempt 1/3 forever" loop seen in the logs.

                    // ── Liveness watchdog ──────────────────────────────────
                    let calls_w          = calls_clone.clone();
                    let active_w         = active_clone.clone();
                    let shards_w         = shards_clone.clone();
                    let last_channel_w   = last_channel_clone.clone();
                    let reconnect_w      = reconnect_clone.clone();
                    let transitioning_w  = transitioning_clone.clone();
                    let pending_w        = pending_clone.clone();
                    let leave_notifiers_w = leave_notifiers_clone.clone();
                    let pre_connect_w    = pre_connect_delay_clone.clone();

                    tokio::spawn(async move {
                        tokio::time::sleep(
                            std::time::Duration::from_millis(LIVENESS_CHECK_DELAY_MS)
                        ).await;

                        let (needs_teardown, close_code) = {
                            let calls = calls_w.lock().await;
                            if let Some(c) = calls.get(&guild_id) {
                                let dead = !c.driver.is_ws_alive()
                                    && c.driver.frames_sent.load(
                                        std::sync::atomic::Ordering::Relaxed) == 0;
                                (dead, c.driver.ws_close_code())
                            } else {
                                (false, 0u16)
                            }
                        };

                        if !needs_teardown {
                            // Connection is healthy — safe to clear the counter
                            tracing::info!(
                                "✅ Liveness check passed [guild={}] — resetting reconnect counter",
                                guild_id
                            );
                            reconnect_w.lock().await.remove(&guild_id);
                            return;
                        }

                        tracing::error!(
                            "❌ Liveness watchdog: driver died with 0 frames \
                             [guild={}, close_code={}] — tearing down",
                            guild_id, close_code
                        );

                        if let Some(dead) = calls_w.lock().await.remove(&guild_id) {
                            dead.driver.request_shutdown();
                        }
                        active_w.lock().await.remove(&guild_id);
                        pending_w.lock().await.remove(&guild_id);

                        match close_code {
                            4006 | 4014 | 4015 => {
                                // ── Reconnect budget ───────────────────────
                                let attempt = {
                                    let mut rc = reconnect_w.lock().await;
                                    let c = rc.entry(guild_id).or_insert(0);
                                    *c += 1;
                                    *c
                                };

                                if attempt > MAX_RECONNECT_ATTEMPTS {
                                    tracing::error!(
                                        "❌ Max reconnect attempts ({}/{}) exceeded \
                                         after code {} [guild={}] — use $join",
                                        attempt - 1, MAX_RECONNECT_ATTEMPTS,
                                        close_code, guild_id
                                    );
                                    reconnect_w.lock().await.remove(&guild_id);
                                    return;
                                }

                                // Exponential: 2 s → 4 s → 8 s
                                let rejoin_delay =
                                    BASE_REJOIN_DELAY_MS * (1u64 << (attempt - 1).min(3));

                                tracing::warn!(
                                    "🔄 WS closed with {} (attempt {}/{}, delay={}ms) — \
                                     full leave→join cycle [guild={}]",
                                    close_code, attempt, MAX_RECONNECT_ATTEMPTS,
                                    rejoin_delay, guild_id
                                );

                                let channel_id = match last_channel_w.lock().await
                                    .get(&guild_id).copied()
                                {
                                    Some(c) => c,
                                    None => {
                                        tracing::warn!(
                                            "No last_channel — cannot auto-reconnect \
                                             after {} [guild={}]. Use $join.",
                                            close_code, guild_id
                                        );
                                        return;
                                    }
                                };

                                // ── Step 1: set transitioning guard ───────
                                transitioning_w.lock().await.insert(guild_id);

                                // ── Step 2: arm leave notifier ─────────────
                                // state_update() fires this when Discord sends
                                // VoiceStateUpdate(channel=None), proving the
                                // voice server has processed the disconnect.
                                let leave_notify = Arc::new(Notify::new());
                                leave_notifiers_w.lock().await
                                    .insert(guild_id, leave_notify.clone());

                                // ── Step 3: send OP4 leave ─────────────────
                                {
                                    let shards = shards_w.lock().await;
                                    if !SigilVoiceManager::send_op4_via_shards(
                                        &shards, guild_id, None
                                    ) {
                                        tracing::warn!(
                                            "Shard closed — cannot send OP4 leave [guild={}]",
                                            guild_id
                                        );
                                        transitioning_w.lock().await.remove(&guild_id);
                                        leave_notifiers_w.lock().await.remove(&guild_id);
                                        return;
                                    }
                                    tracing::info!(
                                        "📡 OP4 leave sent (step 1/3) [guild={}]",
                                        guild_id
                                    );
                                }

                                // ── Step 4: wait for Discord's leave confirmation
                                //
                                // KEY FIX: We do NOT start the rejoin countdown
                                // until Discord explicitly confirms the bot has
                                // left (VoiceStateUpdate channel=None).  Only
                                // then do we know the voice server has begun
                                // cleaning up the old session.  Starting the
                                // clock from the OP4 send (old behaviour) meant
                                // we were leaving only ~700 ms for the voice
                                // server to clean up after Discord's own ~800 ms
                                // confirmation delay — not enough time, causing
                                // immediate 4006 on every reconnect attempt.
                                let confirmed = tokio::time::timeout(
                                    std::time::Duration::from_secs(LEAVE_CONFIRM_TIMEOUT_SECS),
                                    leave_notify.notified()
                                ).await;

                                match confirmed {
                                    Ok(_) => tracing::info!(
                                        "✅ Leave confirmed — waiting {}ms \
                                         before rejoin [guild={}]",
                                        rejoin_delay, guild_id
                                    ),
                                    Err(_) => tracing::warn!(
                                        "⚠️ Leave not confirmed in {}s — \
                                         proceeding anyway [guild={}]",
                                        LEAVE_CONFIRM_TIMEOUT_SECS, guild_id
                                    ),
                                }

                                // Full scaled delay starts AFTER leave is confirmed
                                tokio::time::sleep(
                                    std::time::Duration::from_millis(rejoin_delay)
                                ).await;

                                // ── Step 5: set pre-connect grace delay ────
                                // 1 s / 2 s / 4 s — consumed inside the next
                                // check_and_connect before CoreDriver::connect
                                let pre_ms =
                                    1_000u64 * (1u64 << (attempt - 1).min(3));
                                pre_connect_w.lock().await.insert(guild_id, pre_ms);
                                tracing::info!(
                                    "⏳ Pre-connect grace set to {}ms [guild={}]",
                                    pre_ms, guild_id
                                );

                                // ── Step 6: clear guards + stale pending ───
                                transitioning_w.lock().await.remove(&guild_id);
                                pending_w.lock().await.remove(&guild_id);

                                // ── Step 7: send OP4 join ──────────────────
                                {
                                    let shards = shards_w.lock().await;
                                    if SigilVoiceManager::send_op4_via_shards(
                                        &shards, guild_id, Some(channel_id)
                                    ) {
                                        tracing::info!(
                                            "📡 OP4 join sent (step 3/3) — \
                                             awaiting fresh credentials [guild={}]",
                                            guild_id
                                        );
                                    } else {
                                        tracing::warn!(
                                            "Shard closed — cannot send OP4 join \
                                             [guild={}]",
                                            guild_id
                                        );
                                    }
                                }
                                // Fresh VoiceStateUpdate + VoiceServerUpdate will
                                // now flow through state_update → server_update →
                                // check_and_connect, completing the reconnect.
                            }
                            _ => {
                                tracing::error!(
                                    "Non-recoverable close {} — use $join [guild={}]",
                                    close_code, guild_id
                                );
                            }
                        }
                    });
                }
                Ok(Err(e)) => {
                    tracing::error!("❌ CoreDriver::connect failed [guild={}]: {:?}", guild_id, e);
                    active_clone.lock().await.remove(&guild_id);
                }
                Err(_) => {
                    tracing::error!("❌ CoreDriver::connect timed out [guild={}] (30s)", guild_id);
                    active_clone.lock().await.remove(&guild_id);
                }
            }
        });
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// VoiceGatewayManager
// ──────────────────────────────────────────────────────────────────────────────

#[async_trait]
impl VoiceGatewayManager for SigilVoiceManager {
    async fn initialise(&self, _shard_count: u32, user_id: UserId) {
        *self.user_id.lock().await = user_id.get();
        tracing::info!("SigilVoiceManager initialised [user_id={}]", user_id);
    }

    async fn register_shard(&self, shard_id: u32, sender: UnboundedSender<ShardRunnerMessage>) {
        tracing::info!("SigilVoiceManager: shard {} registered", shard_id);
        self.shards.lock().await.insert(shard_id, sender);
    }

    async fn deregister_shard(&self, shard_id: u32) {
        tracing::info!("SigilVoiceManager: shard {} deregistered", shard_id);
        self.shards.lock().await.remove(&shard_id);
    }

    async fn server_update(
        &self,
        guild_id: GuildId,
        endpoint: &Option<String>,
        token: &str,
    ) {
        let Some(ep) = endpoint else {
            tracing::warn!(
                "VoiceServerUpdate guild={} — no endpoint, session destroyed",
                guild_id
            );
            self.teardown_guild(guild_id).await;
            return;
        };

        if let Some(call) = self.calls.lock().await.get(&guild_id) {
            if call.driver.is_ws_alive() {
                tracing::warn!(
                    "🔄 VoiceServerUpdate while live — session invalidated [guild={}]",
                    guild_id
                );
            }
        }

        tracing::warn!("VoiceServerUpdate [guild={}, endpoint={}] — CALLER TRACE", guild_id, ep);
        tracing::warn!(
            "VoiceServerUpdate backtrace: {:?}",
            std::backtrace::Backtrace::force_capture()
        );

        {
            let mut p = self.pending.lock().await;
            let entry = p.entry(guild_id).or_default();
            entry.endpoint = Some(ep.clone());
            entry.token = Some(token.to_string());
        }

        self.check_and_connect(guild_id).await;
    }

    async fn state_update(&self, guild_id: GuildId, voice_state: &VoiceState) {
        let our_id = *self.user_id.lock().await;
        if voice_state.user_id.get() != our_id {
            return;
        }

        if voice_state.channel_id.is_none() {
            tracing::info!("Bot disconnected from voice in guild={}", guild_id);

            // FIX: Signal the watchdog's leave notifier BEFORE teardown.
            // The watchdog is sleeping waiting for this signal; firing it here
            // tells it "Discord has confirmed the disconnect — start the rejoin
            // countdown now" rather than guessing based on a fixed sleep.
            if let Some(notify) = self.leave_notifiers.lock().await.remove(&guild_id) {
                notify.notify_waiters();
            }

            self.teardown_guild(guild_id).await;
            return;
        }

        tracing::info!(
            "VoiceStateUpdate [guild={}, channel={:?}, session={}]",
            guild_id, voice_state.channel_id, voice_state.session_id
        );

        if let Some(ch) = voice_state.channel_id {
            self.last_channel.lock().await.insert(guild_id, ch);
        }

        {
            let mut p = self.pending.lock().await;
            let entry = p.entry(guild_id).or_default();
            entry.session_id = Some(voice_state.session_id.clone());
            entry.channel_id = voice_state.channel_id;
        }

        self.check_and_connect(guild_id).await;
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// TypeMap key
// ──────────────────────────────────────────────────────────────────────────────

pub struct SigilVoiceManagerKey;
impl serenity::prelude::TypeMapKey for SigilVoiceManagerKey {
    type Value = Arc<SigilVoiceManager>;
}
