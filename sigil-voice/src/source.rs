use std::process::Stdio;
use tokio::process::Command;
use tokio::io::{AsyncReadExt, AsyncBufReadExt, BufReader};
use tracing::{info, error};
use std::sync::Arc;

/// A utility to spawn `yt-dlp` to resolve a URL or search query into a direct playable audio
/// stream, then pipe it into `ffmpeg` to produce raw 48kHz stereo 16-bit PCM.
pub struct YtDlpSource;

/// Reads cookies from the cookies.txt file and formats them for FFmpeg
/// Supports three methods (in order of priority):
/// 1. YOUTUBE_COOKIES env var - raw cookie string (name1=value1; name2=value2)
/// 2. YOUTUBE_COOKIES_FILE env var - path to cookies.txt file
/// 3. cookies.txt in current working directory (default fallback)
fn get_cookies_for_ffmpeg() -> Option<String> {
    // Priority 1: Check for raw cookie string in environment variable
    if let Ok(cookies) = std::env::var("YOUTUBE_COOKIES") {
        if !cookies.is_empty() {
            info!("Using cookies from YOUTUBE_COOKIES env var");
            return Some(cookies);
        }
    }
    
    // Priority 2: Check for cookie file path in environment variable
    if let Ok(cookies_file) = std::env::var("YOUTUBE_COOKIES_FILE") {
        if std::path::Path::new(&cookies_file).exists() {
            if let Some(cookies) = parse_cookies_file(&cookies_file) {
                return Some(cookies);
            }
        }
    }
    
    // Priority 3: Default cookies.txt file
    let cookies_file = "cookies.txt";
    if std::path::Path::new(cookies_file).exists() {
        if let Some(cookies) = parse_cookies_file(cookies_file) {
            return Some(cookies);
        }
    }
    
    None
}

/// Parse a cookies.txt file (Netscape format) and return cookie header string
fn parse_cookies_file(cookies_file: &str) -> Option<String> {
    let content = std::fs::read_to_string(cookies_file).ok()?;
    
    // Parse cookies.txt (Netscape format) and extract cookie name=value pairs
    let mut cookies_vec: Vec<String> = Vec::new();
    
    for line in content.lines() {
        let line = line.trim();
        // Skip comments and empty lines
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        
        // Format: domain	flag	path	expire	name	value
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 7 {
            let name = parts[5];
            let value = parts[6];
            if !name.is_empty() && !value.is_empty() {
                cookies_vec.push(format!("{}={}", name, value));
            }
        }
    }
    
    if cookies_vec.is_empty() {
        return None;
    }
    
    // Join cookies with "; " (standard cookie separator)
    let cookie_header = cookies_vec.join("; ");
    info!("Loaded {} cookies from {}", cookies_vec.len(), cookies_file);
    Some(cookie_header)
}

impl YtDlpSource {
    pub async fn get_direct_url_and_headers(query: &str) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
        let resolved_query = if query.starts_with("http://") || query.starts_with("https://") {
            query.to_string()
        } else {
            format!("ytsearch1:{}", query)
        };

        let cookies_file = std::env::var("YOUTUBE_COOKIES").unwrap_or_else(|_| "cookies.txt".to_string());
        
        let mut cmd = Command::new("yt-dlp");
        cmd.args(&[
            "-f", "bestaudio", 
            "--no-playlist", 
            "-J", 
        ]);
        
        if std::path::Path::new(&cookies_file).exists() {
            cmd.arg("--cookies").arg(&cookies_file);
        }
        
        cmd.arg(&resolved_query);

        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("yt-dlp failed: {}", stderr);
            return Err(format!("yt-dlp failed: {}", stderr.trim()).into());
        }

        let json_str = String::from_utf8(output.stdout)?;
        let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
        
        let url = parsed.get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
            
        if url.is_empty() {
            return Err("yt-dlp returned an empty URL in the JSON".into());
        }
        
        let mut header_str = String::new();
        if let Some(headers) = parsed.get("http_headers").and_then(|v| v.as_object()) {
            for (k, v) in headers {
                if let Some(v_str) = v.as_str() {
                    header_str.push_str(&format!("{}: {}\r\n", k, v_str));
                }
            }
        }

        info!("Resolved direct URL (first 80 chars): {}", &url[..url.len().min(80)]);
        Ok((url, header_str))
    }

    pub async fn spawn_ffmpeg_stream(
        direct_url: &str,
        headers: &str,
        pcm_tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(&["-reconnect", "1", "-reconnect_streamed", "1", "-reconnect_delay_max", "5"]);
        
        // Build headers string, including cookies if available
        let mut full_headers = headers.to_string();
        
        // Add cookies from cookies.txt to the headers
        if let Some(cookie_header) = get_cookies_for_ffmpeg() {
            if !full_headers.is_empty() && !full_headers.ends_with("\r\n") {
                full_headers.push_str("\r\n");
            }
            full_headers.push_str(&format!("Cookie: {}\r\n", cookie_header));
            info!("Added cookies to FFmpeg request");
        }
        
        if !full_headers.is_empty() {
            cmd.args(&["-headers", &full_headers]);
        }
        
        cmd.args(&["-i", direct_url])
            .args(&["-f", "s16le", "-ar", "48000", "-ac", "2"])
            .arg("-")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
            
        let mut child = cmd.spawn()
            .map_err(|e| {
                error!("Failed to spawn ffmpeg: {}", e);
                e
            })?;

        let stdout = child.stdout.take().expect("Failed to open ffmpeg stdout");
        let stderr = child.stderr.take().expect("Failed to open ffmpeg stderr");

        // Atomic to signal when we have audio data
        let got_audio = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let got_audio_clone = got_audio.clone();

        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while let Ok(n) = reader.read_line(&mut line).await {
                if n == 0 { break; }
                let l = line.trim();
                if l.is_empty() { continue; }

                // Log everything until we get frames, so we can see 403 or connection errors
                if !got_audio_clone.load(std::sync::atomic::Ordering::SeqCst) {
                    info!("FFmpeg Init: {}", l);
                } else if l.to_lowercase().contains("error") || l.to_lowercase().contains("failed") {
                    error!("FFmpeg Error: {}", l);
                }
                line.clear();
            }
        });

        let mut reader = BufReader::with_capacity(64 * 1024, stdout);

        tokio::spawn(async move {
            let _child = child; // Guard

            let mut chunk = vec![0u8; 3840];
            let mut frames_produced: u64 = 0;

            loop {
                let mut bytes_read = 0;
                while bytes_read < 3840 {
                    match reader.read(&mut chunk[bytes_read..]).await {
                        Ok(0) => {
                            info!("ffmpeg PCM stream ended (EOF) after {} frames", frames_produced);
                            return;
                        }
                        Ok(n) => bytes_read += n,
                        Err(e) => {
                            error!("ffmpeg read error: {}", e);
                            return;
                        }
                    }
                }

                if frames_produced == 0 {
                    got_audio.store(true, std::sync::atomic::Ordering::SeqCst);
                    info!("🎞️ FFmpeg started producing PCM frames");
                }

                let pcm_frame: Vec<i16> = chunk
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect();

                frames_produced += 1;
                if frames_produced % 250 == 0 {
                    info!("🎞️ Track Heartbeat: Produced {} frames (~{}s)", frames_produced, frames_produced / 50);
                }

                if pcm_tx.send(pcm_frame).await.is_err() {
                    info!("PCM receiver dropped — stopping stream");
                    break;
                }
            }
        });

        Ok(())
    }

    pub async fn create_track(query: &str) -> Result<crate::track::Track, Box<dyn std::error::Error + Send + Sync>> {
        let (direct_url, headers) = Self::get_direct_url_and_headers(query).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        Self::spawn_ffmpeg_stream(&direct_url, &headers, tx).await?;

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

    pub async fn search_and_play(query: &str) -> Result<crate::track::Track, Box<dyn std::error::Error + Send + Sync>> {
        Self::create_track(query).await
    }
}

