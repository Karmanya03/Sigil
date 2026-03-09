use std::sync::Arc;
use tokio::sync::Mutex;

pub trait AudioSource: Send {
    /// Read exactly 20ms of PCM data (960 samples per channel, stereo = 1920 i16s).
    fn read_frame(&mut self) -> Option<Vec<i16>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayState {
    Playing,
    Paused,
    Stopped,
}

pub struct Track {
    pub source: Box<dyn AudioSource>,
    pub state: PlayState,
    pub volume: f32,
    pub loops: usize, // 0 = infinite, 1 = play once, etc.
}

pub struct TrackHandle {
    pub(crate) inner: Arc<Mutex<Track>>,
}

impl TrackHandle {
    pub fn new(track: Track) -> Self {
        Self {
            inner: Arc::new(Mutex::new(track)),
        }
    }

    pub fn inner(&self) -> Arc<Mutex<Track>> {
        self.inner.clone()
    }

    pub async fn play(&self) {
        let mut t = self.inner.lock().await;
        t.state = PlayState::Playing;
    }

    pub async fn pause(&self) {
        let mut t = self.inner.lock().await;
        t.state = PlayState::Paused;
    }

    pub async fn stop(&self) {
        let mut t = self.inner.lock().await;
        t.state = PlayState::Stopped;
    }

    pub async fn set_volume(&self, volume: f32) {
        let mut t = self.inner.lock().await;
        t.volume = volume;
    }

    pub async fn get_state(&self) -> PlayState {
        self.inner.lock().await.state
    }
}

pub struct ChannelSource {
    pub receiver: tokio::sync::mpsc::Receiver<Vec<i16>>,
}

impl AudioSource for ChannelSource {
    fn read_frame(&mut self) -> Option<Vec<i16>> {
        self.receiver.try_recv().ok()
    }
}
