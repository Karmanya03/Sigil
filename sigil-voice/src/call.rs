use std::sync::Arc;
use tokio::sync::Mutex;
use crate::driver::CoreDriver;
use crate::track::{Track, TrackHandle};

/// A thread-safe coordinator for a single guild's voice connection.
/// This is the primary interface for users to control playback.
pub struct Call {
    pub driver: Arc<CoreDriver>,
}

impl Call {
    pub fn new(driver: CoreDriver) -> Self {
        let driver = Arc::new(driver);
        
        // Automatically start the mixing loop in the background
        let d_clone = driver.clone();
        tokio::spawn(async move {
            if let Err(e) = d_clone.start_mixing().await {
                tracing::error!("Mixing loop failed: {:?}", e);
            }
        });

        Self { driver }
    }

    /// Play a new track. Returns a handle to control it.
    pub async fn play(&self, track: Track) -> TrackHandle {
        self.driver.add_track(track).await
    }

    /// Stop all playback in this guild.
    pub async fn stop(&self) {
        self.driver.stop().await;
    }

    /// Disconnect from the voice channel.
    pub async fn leave(&self) {
        // Here we would send the Leave event to the WS and close UDP
        // For now, stopping all tracks and dropping the driver is a start.
        self.stop().await;
    }
}
