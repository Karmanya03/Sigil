use serenity::all::{GuildId, VoiceState, VoiceServerUpdateEvent};
use std::sync::Arc;
use tokio::sync::Mutex;
use std::collections::HashMap;
use crate::driver::CoreDriver;

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
/// Think of this as the drop-in replacement for the `Songbird` typemap instance.
pub struct SigilVoiceManager {
    user_id: u64,
    pending: Arc<Mutex<HashMap<GuildId, PendingConnection>>>,
    pub drivers: Arc<Mutex<HashMap<GuildId, Arc<Mutex<CoreDriver>>>>>,
}

impl SigilVoiceManager {
    /// Initialize a new Voice Manager tracking the host Bot's user ID.
    pub fn new(user_id: u64) -> Self {
        Self {
            user_id,
            pending: Arc::new(Mutex::new(HashMap::new())),
            drivers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Retrieve an active CoreDriver for a specific Guild, if it exists.
    pub async fn get_driver(&self, guild_id: GuildId) -> Option<Arc<Mutex<CoreDriver>>> {
        let drivers = self.drivers.lock().await;
        drivers.get(&guild_id).cloned()
    }

    /// Trap incoming Voice State patches (e.g. tracking when the bot physically enters the VC)
    pub async fn handle_voice_state(&self, state: &VoiceState) {
        let Some(user) = &state.user_id else { return };
        if user.get() == self.user_id {
            let Some(guild_id) = state.guild_id else { return };
            let mut p = self.pending.lock().await;
            let entry = p.entry(guild_id).or_default();
            entry.session_id = Some(state.session_id.clone());

            drop(p);
            self.check_and_connect(guild_id).await;
        }
    }

    /// Trap Voice Server negotiations (e.g. allocating the WS endpoint and connection token)
    pub async fn handle_voice_server(&self, server: &VoiceServerUpdateEvent) {
        let Some(guild_id) = server.guild_id else { return };
        let mut p = self.pending.lock().await;
        let entry = p.entry(guild_id).or_default();
        if let Some(endpoint) = &server.endpoint {
            entry.endpoint = Some(endpoint.clone());
        }
        entry.token = Some(server.token.clone());

        drop(p);
        self.check_and_connect(guild_id).await;
    }

    /// Internal orchestrator that examines if all 3 Voice routing elements are fulfilled.
    /// Once the triad completes, it fully connects the CoreDriver automatically!
    async fn check_and_connect(&self, guild_id: GuildId) {
        let args = {
            let mut p = self.pending.lock().await;
            let Some(entry) = p.get(&guild_id) else { return };
            if !entry.is_ready() {
                return;
            }
            (
                entry.endpoint.clone().unwrap(),
                entry.session_id.clone().unwrap(),
                entry.token.clone().unwrap(),
            )
        };

        let (endpoint, session_id, token) = args;
        let server_id_str = guild_id.get().to_string();
        let user_id_str = self.user_id.to_string();
        let sigil_drivers = self.drivers.clone();

        tokio::spawn(async move {
            tracing::info!("Initializing Sigil CoreDriver natively for {:?}", guild_id);
            match CoreDriver::connect(&endpoint, &server_id_str, &user_id_str, &session_id, &token).await {
                Ok(driver) => {
                    tracing::info!("Sigil successfully attached to Voice VC {:?}", guild_id);
                    let mut d = sigil_drivers.lock().await;
                    d.insert(guild_id, Arc::new(Mutex::new(driver)));
                }
                Err(e) => {
                    tracing::error!("CoreDriver completely failed to bootstrap for {:?}: {:?}", guild_id, e);
                }
            }
        });

        // Flush tracking
        let mut p = self.pending.lock().await;
        p.remove(&guild_id);
    }
}
