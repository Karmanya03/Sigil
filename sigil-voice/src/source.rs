use std::process::Stdio;
use tokio::process::Command;
use tokio::io::{AsyncReadExt, BufReader};
use tracing::{info, error};

/// A utility to spawn `yt-dlp` to resolve a URL into a direct playable audio stream,
/// and then pipe it into `ffmpeg` to convert it to 48kHz, 16-bit, stereo PCM.
pub struct YtDlpSource;

impl YtDlpSource {
    /// Runs `yt-dlp -f bestaudio -g <url>` to discover the direct media link.
    pub async fn get_direct_url(youtube_url: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let output = Command::new("yt-dlp")
            .args(&["-f", "bestaudio", "-g", youtube_url])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("yt-dlp failed: {}", stderr);
            return Err("yt-dlp failed to resolve URL".into());
        }

        let link = String::from_utf8(output.stdout)?;
        Ok(link.trim().to_string())
    }

    /// Spawns `ffmpeg` extracting raw PCM data (48kHz, Stereo, 16-bit LE) straight to a tokio channel,
    /// which can be perfectly consumed by `CoreDriver::play_pcm_stream`.
    pub async fn spawn_ffmpeg_stream(
        direct_url: &str,
        pcm_tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut child = Command::new("ffmpeg")
            // Reconnect flags for network stream resilience
            .args(&["-reconnect", "1", "-reconnect_streamed", "1", "-reconnect_delay_max", "5"])
            .args(&["-i", direct_url])
            .args(&["-f", "s16le", "-ar", "48000", "-ac", "2"]) // Raw PCM, 48kHz, Stereo
            .arg("-") // Pipe to stdout
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let stdout = child.stdout.take().expect("Failed to open ffmpeg stdout");
        let mut reader = BufReader::new(stdout);

        // 960 samples * 2 channels * 2 bytes = 3840 bytes per 20ms frame
        let mut chunk = vec![0u8; 3840];

        tokio::spawn(async move {
            loop {
                let mut bytes_read = 0;
                while bytes_read < 3840 {
                    match reader.read(&mut chunk[bytes_read..]).await {
                        Ok(0) => {
                            info!("ffmpeg stream ended");
                            return; // EOF
                        }
                        Ok(n) => bytes_read += n,
                        Err(e) => {
                            error!("ffmpeg read error: {}", e);
                            return;
                        }
                    }
                }

                // Convert bytes to i16
                let pcm_frame: Vec<i16> = chunk
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect();

                if pcm_tx.send(pcm_frame).await.is_err() {
                    info!("PCM receiver dropped, stopping ffmpeg stream");
                    break;
                }
            }
        });

        Ok(())
    }

    /// High-level helper: Resolves a URL, spawns ffmpeg, and returns a playable `Track`.
    pub async fn create_track(youtube_url: &str) -> Result<crate::track::Track, Box<dyn std::error::Error + Send + Sync>> {
        let direct_url = Self::get_direct_url(youtube_url).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        Self::spawn_ffmpeg_stream(&direct_url, tx).await?;
        
        use crate::track::{Track, PlayState, ChannelSource};
        use std::sync::atomic::AtomicU8;
        Ok(Track {
            source: Box::new(ChannelSource { receiver: rx }),
            state: Arc::new(AtomicU8::new(PlayState::Playing as u8)),
            volume: 1.0,
            loops: 1,
            event_tx: None,
        })
    }
}
