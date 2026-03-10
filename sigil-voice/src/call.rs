use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use crate::driver::CoreDriver;
use crate::track::{Track, TrackHandle};

/// A thread-safe coordinator for a single guild's voice connection.
/// This is the primary interface for users to control playback.
pub struct Call {
    pub driver: Arc<CoreDriver>,
    pub receiver_rx: Arc<Mutex<Option<mpsc::Receiver<(u64, Vec<i16>)>>>>,
}

impl Call {
    pub fn new(mut driver: CoreDriver) -> Self {
        let (tx, rx) = mpsc::channel(512);
        driver.receiver_tx = Some(tx);
        let driver = Arc::new(driver);

        // Automatically start the mixing loop and receiver loop in the background
        let d_clone = driver.clone();
        tokio::spawn(async move {
            if let Err(e) = d_clone.start_mixing().await {
                tracing::error!("Mixing loop failed: {:?}", e);
            }
        });

        let d_clone_2 = driver.clone();
        tokio::spawn(async move {
            if let Err(e) = d_clone_2.start_receiver().await {
                tracing::error!("Receiver loop failed: {:?}", e);
            }
        });

        Self {
            driver,
            receiver_rx: Arc::new(Mutex::new(Some(rx))),
        }
    }

    /// Retrieve the receiver for incoming audio from other users.
    /// This can only be called once.
    pub async fn take_receiver(&self) -> Option<mpsc::Receiver<(u64, Vec<i16>)>> {
        let mut rx = self.receiver_rx.lock().await;
        rx.take()
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
