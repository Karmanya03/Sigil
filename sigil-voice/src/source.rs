use std::process::Stdio;
use tokio::process::Command;
use tokio::io::{AsyncReadExt, BufReader};
use tracing::{info, error};
use std::sync::Arc;

/// A utility to spawn `yt-dlp` to resolve a URL or search query into a direct playable audio
/// stream, then pipe it into `ffmpeg` to produce raw 48kHz stereo 16-bit PCM.
pub struct YtDlpSource;

impl YtDlpSource {
    /// Resolve a YouTube URL *or* a free-text search query into a direct media URL.
    ///
    /// Plain URLs are passed through unchanged.  Anything that doesn't start with `http`
    /// is treated as a YouTube search and prefixed with `ytsearch1:`.
    pub async fn get_direct_url(query: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // If not a URL, treat as a YouTube search query
        let resolved_query = if query.starts_with("http://") || query.starts_with("https://") {
            query.to_string()
        } else {
            format!("ytsearch1:{}", query)
        };

        let output = Command::new("yt-dlp")
            .args(&["-f", "bestaudio", "-g", "--no-playlist", &resolved_query])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("yt-dlp failed: {}", stderr);
            return Err(format!("yt-dlp failed: {}", stderr.trim()).into());
        }

        let link = String::from_utf8(output.stdout)?;
        let url = link.trim().to_string();
        if url.is_empty() {
            return Err("yt-dlp returned an empty URL".into());
        }
        info!("Resolved direct URL (first 80 chars): {}", &url[..url.len().min(80)]);
        Ok(url)
    }

    /// Spawn `ffmpeg` to stream PCM into a buffered channel.
    /// This returns immediately; the streaming runs in a background task.
    pub async fn spawn_ffmpeg_stream(
        direct_url: &str,
        pcm_tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut child = Command::new("ffmpeg")
            // Flags for robust network stream reconnection
            .args(&["-reconnect", "1", "-reconnect_streamed", "1", "-reconnect_delay_max", "5"])
            .args(&["-i", direct_url])
            .args(&["-f", "s16le", "-ar", "48000", "-ac", "2"]) // Raw PCM output
            .arg("-")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let stdout = child.stdout.take().expect("Failed to open ffmpeg stdout");
        let mut reader = BufReader::with_capacity(64 * 1024, stdout); // 64KB read buffer

        // 20ms frame: 960 samples × 2 channels × 2 bytes = 3840 bytes
        let mut chunk = vec![0u8; 3840];

        tokio::spawn(async move {
            // Keep child alive for the duration of the stream
            let _child = child;
            loop {
                let mut bytes_read = 0;
                // Fill exactly one 20ms frame
                while bytes_read < 3840 {
                    match reader.read(&mut chunk[bytes_read..]).await {
                        Ok(0) => {
                            info!("ffmpeg PCM stream ended (EOF)");
                            return; // EOF — drain complete
                        }
                        Ok(n) => bytes_read += n,
                        Err(e) => {
                            error!("ffmpeg read error: {}", e);
                            return;
                        }
                    }
                }

                // Convert raw LE bytes to i16 samples
                let pcm_frame: Vec<i16> = chunk
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect();

                // Back-pressure: if the mixer is slow, we block here rather than drop frames
                if pcm_tx.send(pcm_frame).await.is_err() {
                    info!("PCM receiver dropped — stopping ffmpeg stream");
                    break;
                }
            }
        });

        Ok(())
    }

    /// High-level helper: resolve a URL or search query → spawn ffmpeg → return a ready `Track`.
    pub async fn create_track(query: &str) -> Result<crate::track::Track, Box<dyn std::error::Error + Send + Sync>> {
        let direct_url = Self::get_direct_url(query).await?;
        // Channel capacity of 256 = ~5 seconds of buffer. Gives FFmpeg time to start up
        // before the mixer needs the first frame, preventing a premature "track ended" signal.
        let (tx, rx) = tokio::sync::mpsc::channel(256);
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

    /// Convenience: call `create_track` directly with a search query (alias for clarity).
    pub async fn search_and_play(query: &str) -> Result<crate::track::Track, Box<dyn std::error::Error + Send + Sync>> {
        Self::create_track(query).await
    }
}
