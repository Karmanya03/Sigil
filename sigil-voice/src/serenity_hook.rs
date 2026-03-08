use serenity::client::Context;
use serenity::model::id::{GuildId, ChannelId};
use serenity::model::gateway::Ready;
use serenity::model::voice::{VoiceStateEvent, VoiceServerUpdateEvent};
use tracing::{info, debug, warn};

/// Trait to allow easy hook-ins for `serenity` event dispatchers.
/// Note: You must route both `VoiceStateUpdate` and `VoiceServerUpdate` here
/// for the `sigil-voice` driver to harvest tokens and endpoints.
#[serenity::async_trait]
pub trait SigilVoiceClient {
    async fn sigil_join(&self, ctx: &Context, guild_id: GuildId, channel_id: ChannelId);
    async fn handle_voice_state(&self, ctx: &Context, state: &VoiceStateEvent);
    async fn handle_voice_server(&self, ctx: &Context, server: &VoiceServerUpdateEvent);
}

// In a real implementation, we would maintain a `parking_lot::RwLock` global map
// binding `GuildId` to a channel of endpoints and tokens to trigger the `CoreDriver`
// spawn when a voice connect executes. 
// For this scaffolding, we lay out the required Serenity structures.
