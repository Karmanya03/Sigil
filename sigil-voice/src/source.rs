use std::process::Stdio;
use tokio::process::Command;
use tokio::io::{AsyncReadExt, AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{info, warn, error, debug};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct YtDlpSource;

// ─── Cookie helpers ──────────────────────────────────────────────────────────

fn get_cookies_for_ffmpeg() -> Option<String> {
    // Priority 1: Raw cookie string env var
    if let Ok(c) = std::env::var("YT_COOKIES") {
        if !c.is_empty() && !std::path::Path::new(&c).exists() {
            // It's a raw cookie string, not a file path
            return Some(c);
        }
    }
    // Priority 2: Cookie file path env var
    if let Ok(path) = std::env::var("YOUTUBE_COOKIES_FILE") {
        if std::path::Path::new(&path).exists() {
            if let Some(c) = parse_cookies_file(&path) {
                return Some(c);
            }
        }
    }
    // Priority 3: Default cookies.txt
    if std::path::Path::new("cookies.txt").exists() {
        return parse_cookies_file("cookies.txt");
    }
    None
}

/// Resolve the cookies.txt file path for yt-dlp --cookies flag.
/// Returns the path if a Netscape-format cookie file exists.
fn get_cookies_file_path() -> Option<String> {
    // Priority 1: YT_COOKIES env — only if it's a file path (not raw cookies)
    if let Ok(val) = std::env::var("YT_COOKIES") {
        if std::path::Path::new(&val).exists() {
            return Some(val);
        }
    }
    // Priority 2: YOUTUBE_COOKIES_FILE env
    if let Ok(path) = std::env::var("YOUTUBE_COOKIES_FILE") {
        if std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }
    // Priority 3: cookies.txt in cwd
    if std::path::Path::new("cookies.txt").exists() {
        return Some("cookies.txt".to_string());
    }
    None
}

fn parse_cookies_file(path: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let pairs: Vec<String> = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let parts: Vec<&str> = l.split('\t').collect();
            if parts.len() >= 7 && !parts[5].is_empty() {
                Some(format!("{}={}", parts[5], parts[6]))
            } else {
                None
            }
        })
        .collect();
    if pairs.is_empty() {
        return None;
    }
    info!("Loaded {} cookies from {}", pairs.len(), path);
    Some(pairs.join("; "))
}

// ─── YtDlpSource ─────────────────────────────────────────────────────────────

impl YtDlpSource {
    /// Resolve a URL or search query to a direct audio URL via yt-dlp.
    ///
    /// Uses `-J` (JSON dump) for single-pass resolution. Handles both direct
    /// URLs (where `url` is at the root) and search queries (where yt-dlp
    /// wraps the result in `entries[0]`).
    pub async fn get_direct_url_and_headers(
        query: &str,
    ) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
        let resolved = if query.starts_with("http://") || query.starts_with("https://") {
            query.to_string()
        } else {
            format!("ytsearch1:{}", query)
        };

        let mut cmd = Command::new("yt-dlp");
        cmd.args(&[
            "-f", "bestaudio[ext=webm]/bestaudio[ext=m4a]/bestaudio",
            "--no-playlist",
            "-J",
            "--no-warnings",
        ]);

        // Use the resolved cookie file path (not raw cookie string)
        if let Some(cookie_path) = get_cookies_file_path() {
            cmd.arg("--cookies").arg(&cookie_path);
        }
        cmd.arg(&resolved);

        let output = cmd.output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("yt-dlp failed: {}", stderr.trim()).into());
        }

        let json_str = String::from_utf8(output.stdout)?;
        let parsed: serde_json::Value = serde_json::from_str(&json_str)?;

        // ─── FIX: handle both direct URLs and search results ─────────
        // Direct URL → `parsed["url"]` exists at root.
        // Search query → yt-dlp returns `{ "_type": "playlist", "entries": [{ "url": ... }] }`
        // so we must dig into entries[0].
        let entry = if parsed.get("url").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()) {
            // Direct URL — metadata is at root
            &parsed
        } else if let Some(first) = parsed.get("entries")
            .and_then(|e| e.as_array())
            .and_then(|arr| arr.first())
        {
            // Search result — metadata is inside entries[0]
            first
        } else {
            return Err("yt-dlp returned no playable results".into());
        };

        let url = entry.get("url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("yt-dlp resolved entry has no stream URL")?
            .to_string();

        // Extract HTTP headers from the same entry (or fall back to root)
        let mut header_str = String::new();
        let headers_obj = entry.get("http_headers")
            .or_else(|| parsed.get("http_headers"))
            .and_then(|v| v.as_object());
        if let Some(hdrs) = headers_obj {
            for (k, v) in hdrs {
                if let Some(vs) = v.as_str() {
                    header_str.push_str(&format!("{}: {}\r\n", k, vs));
                }
            }
        }

        info!("Resolved URL ({}...)", &url[..url.len().min(60)]);
        Ok((url, header_str))
    }

    /// Spawn ffmpeg, pipe raw s16le 48 kHz stereo PCM into `pcm_tx`.
    ///
    /// The caller owns the `Receiver` end. When it drops, the ffmpeg process
    /// is killed automatically (child is moved into the spawned task).
    pub async fn spawn_ffmpeg_stream(
        direct_url: &str,
        headers: &str,
        pcm_tx: mpsc::Sender<Vec<i16>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(&[
            "-reconnect", "1",
            "-reconnect_streamed", "1",
            "-reconnect_delay_max", "5",
            "-nostdin",
            "-loglevel", "error",
            "-hide_banner",
        ]);

        // Combine yt-dlp headers + cookie header for ffmpeg
        let mut full_headers = headers.to_string();
        if let Some(cookie) = get_cookies_for_ffmpeg() {
            if !full_headers.is_empty() && !full_headers.ends_with("\r\n") {
                full_headers.push_str("\r\n");
            }
            full_headers.push_str(&format!("Cookie: {}\r\n", cookie));
        }
        if !full_headers.is_empty() {
            cmd.args(&["-headers", &full_headers]);
        }

        cmd.args(&["-i", direct_url]);

        // Single -af chain: resample → format → output.
        // Two separate -af flags would cause the second to silently override the first!
        cmd.args(&[
            "-af", "aresample=48000,aformat=sample_fmts=s16:channel_layouts=stereo",
            "-f",  "s16le",
            "-ar", "48000",
            "-ac", "2",
        ]);
        cmd.arg("-")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            error!("Failed to spawn ffmpeg: {}", e);
            e
        })?;

        let stdout = child.stdout.take().expect("ffmpeg stdout");
        let stderr = child.stderr.take().expect("ffmpeg stderr");

        let got_audio = Arc::new(AtomicBool::new(false));
        let got_audio_stderr = got_audio.clone();

        // ─── stderr logger ────────────────────────────────────────────
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while let Ok(n) = reader.read_line(&mut line).await {
                if n == 0 { break; }
                let l = line.trim();
                if l.is_empty() { line.clear(); continue; }

                let is_shutdown_noise = l.contains("Error muxing")
                    || l.contains("Error submitting a packet")
                    || l.contains("Error writing trailer")
                    || l.contains("Error closing file")
                    || l.contains("Task finished with error code: -22")
                    || l.contains("Conversion failed");

                if is_shutdown_noise {
                    debug!("FFmpeg pipe-close noise (normal on stop): {}", l);
                } else if !got_audio_stderr.load(Ordering::Relaxed) {
                    info!("FFmpeg init: {}", l);
                } else if l.to_lowercase().contains("error")
                    || l.to_lowercase().contains("failed")
                {
                    error!("FFmpeg error: {}", l);
                } else {
                    debug!("FFmpeg: {}", l);
                }
                line.clear();
            }
        });

        // ─── PCM reader ───────────────────────────────────────────────
        // 128 KiB buffer ≈ 340 ms of pre-buffered audio at 48 kHz stereo s16le.
        let mut reader = BufReader::with_capacity(128 * 1024, stdout);

        tokio::spawn(async move {
            let _child = child; // Guard — drops child (kills ffmpeg) when task ends

            // 3840 bytes = 960 samples × 2 channels × 2 bytes = one 20 ms frame
            const FRAME_BYTES: usize = 3840;
            let mut chunk = vec![0u8; FRAME_BYTES];
            let mut frames_produced: u64 = 0;

            loop {
                // Fill exactly one 20 ms frame
                let mut cursor = 0usize;
                loop {
                    match reader.read(&mut chunk[cursor..]).await {
                        Ok(0) => {
                            if cursor > 0 {
                                warn!("FFmpeg EOF mid-frame ({}/{} bytes), discarding", cursor, FRAME_BYTES);
                            }
                            info!(
                                "FFmpeg PCM stream ended after {} frames (~{}s)",
                                frames_produced,
                                frames_produced / 50
                            );
                            return;
                        }
                        Ok(n) => {
                            cursor += n;
                            if cursor >= FRAME_BYTES { break; }
                        }
                        Err(e) => {
                            error!("FFmpeg read error: {}", e);
                            return;
                        }
                    }
                }

                if frames_produced == 0 {
                    got_audio.store(true, Ordering::Relaxed);
                    info!("🎞️ FFmpeg producing PCM frames");
                }

                // Convert raw bytes → i16 PCM samples
                let pcm: Vec<i16> = chunk
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect();

                frames_produced += 1;
                if frames_produced % 500 == 0 {
                    debug!("FFmpeg heartbeat: {} frames (~{}s)", frames_produced, frames_produced / 50);
                }

                if pcm_tx.send(pcm).await.is_err() {
                    info!("PCM channel closed — ffmpeg task stopping");
                    break;
                }
            }
        });

        Ok(())
    }

    /// One-shot: resolve → spawn ffmpeg → return a ready-to-play Track.
    pub async fn create_track(
        query: &str,
    ) -> Result<crate::track::Track, Box<dyn std::error::Error + Send + Sync>> {
        let (direct_url, headers) = Self::get_direct_url_and_headers(query).await?;
        // Channel depth 512 ≈ 10 s of pre-buffered PCM — absorbs MLS handshake delay.
        let (tx, rx) = mpsc::channel(512);
        Self::spawn_ffmpeg_stream(&direct_url, &headers, tx).await?;

        use crate::track::{Track, PlayState, ChannelSource};
        use std::sync::atomic::AtomicU8;
        Ok(Track {
            source:   Box::new(ChannelSource { receiver: rx }),
            state:    Arc::new(AtomicU8::new(PlayState::Playing as u8)),
            volume:   1.0,
            loops:    1,
            event_tx: None,
        })
    }

    /// Alias for `create_track` — kept for backward compat.
    pub async fn search_and_play(
        query: &str,
    ) -> Result<crate::track::Track, Box<dyn std::error::Error + Send + Sync>> {
        Self::create_track(query).await
    }
}
