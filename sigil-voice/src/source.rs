use std::process::Stdio;
use tokio::process::Command;
use tokio::io::{AsyncReadExt, AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{info, warn, error, debug};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct YtDlpSource;

// ── Constants ────────────────────────────────────────────────────────────────

/// 960 stereo samples × 2 bytes/sample × 2 channels = one 20 ms Opus frame
const FRAME_BYTES: usize = 3840;

/// Pre-buffer depth: 128 KiB ≈ 340 ms of s16le stereo 48 kHz
const READER_BUF: usize = 128 * 1024;

/// mpsc channel depth: 512 frames ≈ 10.2 s of PCM — absorbs MLS handshake delay
const CHANNEL_DEPTH: usize = 512;

// ── Cookie helpers ───────────────────────────────────────────────────────────

/// Return a filesystem path suitable for `yt-dlp --cookies <path>`.
/// Checks `YOUTUBE_COOKIES_FILE` env first, then `YT_COOKIES` (if it points to
/// a file), then falls back to `./cookies.txt`.
fn get_cookies_file_path() -> Option<String> {
    // Explicit file path env
    if let Ok(path) = std::env::var("YOUTUBE_COOKIES_FILE") {
        if std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }
    // YT_COOKIES might be a path (legacy)
    if let Ok(val) = std::env::var("YT_COOKIES") {
        if !val.is_empty() && std::path::Path::new(&val).exists() {
            return Some(val);
        }
    }
    // Default
    if std::path::Path::new("cookies.txt").exists() {
        return Some("cookies.txt".to_string());
    }
    None
}

/// Return a `Cookie: …` value for ffmpeg's `-headers`. Parses Netscape-format
/// cookie files into `name=value; …` pairs.
fn get_cookies_for_ffmpeg() -> Option<String> {
    // If YT_COOKIES is a raw cookie string (not a file), use it directly
    if let Ok(c) = std::env::var("YT_COOKIES") {
        if !c.is_empty() && !std::path::Path::new(&c).exists() {
            return Some(c);
        }
    }
    // Otherwise parse from a file
    let path = get_cookies_file_path()?;
    parse_cookies_file(&path)
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

// ── YtDlpSource ──────────────────────────────────────────────────────────────

impl YtDlpSource {
    /// Resolve a URL **or** search query to a direct audio stream URL via
    /// `yt-dlp -J`. For `ytsearch1:` queries the real URL lives inside
    /// `entries[0]` — not at the root level.
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
            // Accept any audio format — ffmpeg transcodes to s16le regardless
            "-f", "bestaudio/best",
            "--no-playlist",
            "-J",
            "--no-warnings",
            "--no-check-certificates",
            "--extractor-args", "youtube:player_client=ios,web",
        ]);

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

        // ── FIX: for `ytsearch1:` results the URL is inside entries[0] ───
        let entry = if parsed["url"].as_str().filter(|s| !s.is_empty()).is_some() {
            &parsed                        // direct URL / single video
        } else if let Some(first) = parsed["entries"].get(0) {
            first                          // search result → unwrap entries
        } else {
            return Err("yt-dlp returned no URL and no entries".into());
        };

        let url = entry["url"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("yt-dlp: resolved entry has empty URL")?
            .to_string();

        // Headers may live in the entry or at root level
        let mut header_str = String::new();
        let hdrs = entry
            .get("http_headers")
            .and_then(|v| v.as_object())
            .or_else(|| parsed.get("http_headers").and_then(|v| v.as_object()));
        if let Some(h) = hdrs {
            for (k, v) in h {
                if let Some(vs) = v.as_str() {
                    header_str.push_str(&format!("{}: {}\r\n", k, vs));
                }
            }
        }

        info!("Resolved URL ({}...)", &url[..url.len().min(80)]);
        Ok((url, header_str))
    }

    /// Spawn `ffmpeg`, pipe raw s16le 48 kHz stereo PCM into `pcm_tx`.
    pub async fn spawn_ffmpeg_stream(
        direct_url: &str,
        headers: &str,
        pcm_tx: mpsc::Sender<Vec<i16>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut cmd = Command::new("ffmpeg");

        // ── Input options ─────────────────────────────────────────────────
        cmd.args(&[
            "-reconnect",           "1",
            "-reconnect_streamed",  "1",
            "-reconnect_delay_max", "5",
            "-nostdin",
            "-loglevel", "error",
            "-hide_banner",
        ]);

        // Combine yt-dlp headers + cookies into one `-headers` value
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

        // ── Output options ────────────────────────────────────────────────
        // Single -af chain: resample → format (avoids the silent-override bug
        // where a second -af flag discards the first).
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
        let got_audio_clone = got_audio.clone();

        // ── stderr logger ─────────────────────────────────────────────────
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
                    debug!("FFmpeg pipe-close noise: {}", l);
                } else if !got_audio_clone.load(Ordering::Relaxed) {
                    info!("FFmpeg init: {}", l);
                } else if l.to_lowercase().contains("error")
                       || l.to_lowercase().contains("failed") {
                    error!("FFmpeg error: {}", l);
                } else {
                    debug!("FFmpeg: {}", l);
                }
                line.clear();
            }
        });

        // ── PCM reader ────────────────────────────────────────────────────
        let mut reader = BufReader::with_capacity(READER_BUF, stdout);

        tokio::spawn(async move {
            let _child = child; // child is killed when this task exits

            let mut chunk = vec![0u8; FRAME_BYTES];
            let mut frames_produced: u64 = 0;

            loop {
                // Fill exactly one 20 ms frame
                let mut cursor = 0;
                loop {
                    match reader.read(&mut chunk[cursor..]).await {
                        Ok(0) => {
                            if cursor > 0 {
                                warn!("FFmpeg EOF mid-frame ({cursor} bytes), discarding");
                            }
                            info!(
                                "FFmpeg PCM stream ended after {} frames (~{}s)",
                                frames_produced, frames_produced / 50
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

                let pcm: Vec<i16> = chunk
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect();

                frames_produced += 1;
                if frames_produced % 500 == 0 {
                    debug!("FFmpeg: {} frames (~{}s)", frames_produced, frames_produced / 50);
                }

                if pcm_tx.send(pcm).await.is_err() {
                    info!("PCM channel closed — ffmpeg task stopping");
                    break;
                }
            }
        });

        Ok(())
    }

    /// Convenience: resolve + spawn ffmpeg → return a Track ready for playback.
    pub async fn create_track(
        query: &str,
    ) -> Result<crate::track::Track, Box<dyn std::error::Error + Send + Sync>> {
        let (direct_url, headers) = Self::get_direct_url_and_headers(query).await?;
        let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
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

    pub async fn search_and_play(
        query: &str,
    ) -> Result<crate::track::Track, Box<dyn std::error::Error + Send + Sync>> {
        Self::create_track(query).await
    }
}
