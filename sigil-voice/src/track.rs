use std::sync::Arc;
use tokio::sync::Mutex;

pub trait AudioSource: Send {
    /// Read exactly 20ms of PCM data (960 samples per channel, stereo = 1920 i16s).
    fn read_frame(&mut self) -> Option<Vec<i16>>;
}

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PlayState {
    Playing = 0,
    Paused = 1,
    Stopped = 2,
    Errored = 3,
}

impl From<u8> for PlayState {
    fn from(v: u8) -> Self {
        match v {
            1 => PlayState::Paused,
            2 => PlayState::Stopped,
            3 => PlayState::Errored,
            _ => PlayState::Playing,
        }
    }
}

pub enum TrackEvent {
    End,
    Loop,
    Error(String),
}

pub struct Track {
    pub source: Box<dyn AudioSource>,
    pub state: Arc<AtomicU8>,
    pub volume: f32,
    pub loops: usize,
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<TrackEvent>>,
}

pub struct TrackHandle {
    pub(crate) inner: Arc<Mutex<Track>>,
    pub(crate) state: Arc<AtomicU8>,
    pub event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<TrackEvent>>,
}

impl Clone for TrackHandle {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            state: self.state.clone(),
            event_rx: None, // Only the original handle can have the receiver
        }
    }
}

impl TrackHandle {
    pub fn new(mut track: Track) -> Self {
        let state = track.state.clone();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        track.event_tx = Some(tx);
        Self {
            inner: Arc::new(Mutex::new(track)),
            state,
            event_rx: Some(rx),
        }
    }

    pub fn inner(&self) -> Arc<Mutex<Track>> {
        self.inner.clone()
    }

    pub fn get_state_atomic(&self) -> PlayState {
        PlayState::from(self.state.load(Ordering::Relaxed))
    }

    pub fn take_event_receiver(&mut self) -> Option<tokio::sync::mpsc::UnboundedReceiver<TrackEvent>> {
        self.event_rx.take()
    }

    pub async fn play(&self) {
        let t = self.inner.lock().await;
        t.state.store(PlayState::Playing as u8, Ordering::SeqCst);
    }

    pub async fn pause(&self) {
        let t = self.inner.lock().await;
        t.state.store(PlayState::Paused as u8, Ordering::SeqCst);
    }

    pub async fn stop(&self) {
        let t = self.inner.lock().await;
        t.state.store(PlayState::Stopped as u8, Ordering::SeqCst);
    }

    pub async fn set_volume(&self, volume: f32) {
        let mut t = self.inner.lock().await;
        t.volume = volume;
    }

    pub async fn get_state(&self) -> PlayState {
        PlayState::from(self.state.load(Ordering::Relaxed))
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
