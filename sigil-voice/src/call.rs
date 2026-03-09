use std::sync::Arc;
use tokio::sync::Mutex;
use crate::driver::CoreDriver;
use crate::track::{Track, TrackHandle};

/// A thread-safe coordinator for a single guild's voice connection.
/// This is the primary interface for users to control playback.
pub struct Call {
    pub driver: Arc<Mutex<CoreDriver>>,
}

impl Call {
    pub fn new(driver: CoreDriver) -> Self {
        let driver = Arc::new(Mutex::new(driver));
        
        // Automatically start the mixing loop in the background
        let d_clone = driver.clone();
        tokio::spawn(async move {
            let d = d_clone.lock().await;
            if let Err(e) = d.start_mixing().await {
                tracing::error!("Mixing loop failed: {:?}", e);
            }
        });

        Self { driver }
    }

    /// Play a new track. Returns a handle to control it.
    pub async fn play(&self, track: Track) -> TrackHandle {
        let d = self.driver.lock().await;
        d.add_track(track).await
    }

    /// Stop all playback in this guild.
    pub async fn stop(&self) {
        let d = self.driver.lock().await;
        d.stop().await;
    }

    /// Disconnect from the voice channel.
    pub async fn leave(&self) {
        // Here we would send the Leave event to the WS and close UDP
        // For now, stopping all tracks and dropping the driver is a start.
        self.stop().await;
    }
}
