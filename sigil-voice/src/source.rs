use std::process::Stdio;
use tokio::process::Command;
use tokio::io::{AsyncReadExt, AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{info, warn, error, debug};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct YtDlpSource;

const FRAME_BYTES: usize = 3840;
const READER_BUF: usize = 128 * 1024;
const CHANNEL_DEPTH: usize = 512;

fn get_cookies_file_path() -> Option<String> {
    if let Ok(path) = std::env::var("YOUTUBE_COOKIES_FILE") {
        if std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }
    if let Ok(val) = std::env::var("YT_COOKIES") {
        if !val.is_empty() && std::path::Path::new(&val).exists() {
            return Some(val);
        }
    }
    if std::path::Path::new("cookies.txt").exists() {
        return Some("cookies.txt".to_string());
    }
    None
}

fn get_cookies_for_ffmpeg() -> Option<String> {
    if let Ok(c) = std::env::var("YT_COOKIES") {
        if !c.is_empty() && !std::path::Path::new(&c).exists() {
            return Some(c);
        }
    }
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

async fn get_available_formats(query: &str) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error + Send + Sync>> {
    let mut cmd = Command::new("yt-dlp");
    cmd.args(&[
        "--no-playlist",
        "-F",
        "--no-warnings",
        // Add comprehensive headers to bypass bot detection
        "--user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/121.0.0.0 Safari/537.36",
        "--referer", "https://www.youtube.com/",
        // Add additional headers that real browsers send
        "--add-header", "Accept-Language:en-US,en;q=0.9",
        "--add-header", "Accept:text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
        "--add-header", "Accept-Encoding:gzip, deflate, br",
        "--add-header", "Sec-Fetch-Dest:document",
        "--add-header", "Sec-Fetch-Mode:navigate",
        "--add-header", "Sec-Fetch-Site:none",
        "--add-header", "Sec-Fetch-User:?1",
        "--add-header", "Upgrade-Insecure-Requests:1",
        // Resilience and timeout settings
        "--extractor-retries", "3",
        "--socket-timeout", "30",
        // YouTube-specific optimizations - use android client for better reliability
        "--extractor-args", "youtube:player_client=android,web",
    ]);

    if let Some(cookie_path) = get_cookies_file_path() {
        cmd.arg("--cookies").arg(&cookie_path);
        info!("Using cookies from: {}", cookie_path);
    }
    cmd.arg(query);

    let output = cmd.output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("yt-dlp failed: {}", stderr.trim()).into());
    }

    let stdout = String::from_utf8(output.stdout)?;
    let mut formats = Vec::new();

    for line in stdout.lines().skip(2) {
        if line.trim().is_empty() || line.contains("format code") {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 5 {
            continue;
        }
        let format_code = parts[0];
        let format = serde_json::json!({
            "format_code": format_code,
            "format": line.trim().to_string()
        });
        formats.push(format);
    }

    Ok(formats)
}

fn pick_best_audio(
    parsed: &serde_json::Value,
) -> Option<(String, serde_json::Value)> {
    let entry = if parsed.get("formats").is_some() {
        parsed
    } else if let Some(first) = parsed.get("entries").and_then(|e| e.get(0)) {
        first
    } else {
        parsed
    };

    if let Some(formats) = entry.get("formats").and_then(|f| f.as_array()) {
        // Helper function to safely check if a format has a valid URL
        let has_valid_url = |f: &serde_json::Value| -> bool {
            f.get("url")
                .and_then(|u| u.as_str())
                .map(|u| !u.is_empty() && (u.starts_with("http://") || u.starts_with("https://")))
                .unwrap_or(false)
        };

        // Helper function to check if format is audio-only
        let is_audio_only = |f: &serde_json::Value| -> bool {
            let vcodec = f.get("vcodec").and_then(|v| v.as_str()).unwrap_or("");
            let acodec = f.get("acodec").and_then(|v| v.as_str()).unwrap_or("none");
            
            // Audio-only if: vcodec is "none" or missing/empty, AND acodec is not "none"
            let no_video = vcodec == "none" || vcodec.is_empty();
            let has_audio = acodec != "none" && !acodec.is_empty();
            
            no_video && has_audio
        };

        // Helper function to get bitrate for sorting
        let get_bitrate = |f: &serde_json::Value| -> f64 {
            // Try abr (audio bitrate) first, then tbr (total bitrate), then asr (audio sample rate as proxy)
            f.get("abr")
                .and_then(|v| v.as_f64())
                .or_else(|| f.get("tbr").and_then(|v| v.as_f64()))
                .or_else(|| f.get("asr").and_then(|v| v.as_f64()).map(|asr| asr / 1000.0))
                .unwrap_or(0.0)
        };

        // Helper function to prefer webm/m4a formats
        let format_preference_score = |f: &serde_json::Value| -> i32 {
            let ext = f.get("ext").and_then(|e| e.as_str()).unwrap_or("");
            let acodec = f.get("acodec").and_then(|a| a.as_str()).unwrap_or("");
            
            // Prefer webm (opus) and m4a (aac) for audio quality and compatibility
            if ext == "webm" || acodec.contains("opus") {
                return 3;
            }
            if ext == "m4a" || acodec.contains("aac") || acodec.contains("m4a") {
                return 2;
            }
            if !ext.is_empty() {
                return 1;
            }
            0
        };

        // First pass: Try to find audio-only formats (highest priority)
        let mut audio_only: Vec<&serde_json::Value> = formats
            .iter()
            .filter(|f| has_valid_url(f) && is_audio_only(f))
            .collect();

        if !audio_only.is_empty() {
            // Sort by format preference, then by bitrate
            audio_only.sort_by(|a, b| {
                let pref_cmp = format_preference_score(b).cmp(&format_preference_score(a));
                if pref_cmp != std::cmp::Ordering::Equal {
                    return pref_cmp;
                }
                let abr_a = get_bitrate(a);
                let abr_b = get_bitrate(b);
                abr_b.partial_cmp(&abr_a).unwrap_or(std::cmp::Ordering::Equal)
            });

            if let Some(best) = audio_only.first() {
                // Safe extraction with defensive checks
                if let Some(url_str) = best.get("url").and_then(|u| u.as_str()) {
                    let url = url_str.to_string();
                    let headers = best.get("http_headers")
                        .or_else(|| entry.get("http_headers"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    
                    debug!("Selected audio-only format: ext={}, acodec={}, bitrate={}", 
                        best.get("ext").and_then(|e| e.as_str()).unwrap_or("unknown"),
                        best.get("acodec").and_then(|a| a.as_str()).unwrap_or("unknown"),
                        get_bitrate(best));
                    
                    return Some((url, headers));
                }
            }
        }

        // Second pass: Try formats with audio (may include video)
        let mut with_audio: Vec<&serde_json::Value> = formats
            .iter()
            .filter(|f| {
                let acodec = f.get("acodec").and_then(|v| v.as_str()).unwrap_or("none");
                let has_audio = acodec != "none" && !acodec.is_empty();
                has_valid_url(f) && has_audio
            })
            .collect();

        if !with_audio.is_empty() {
            with_audio.sort_by(|a, b| {
                let pref_cmp = format_preference_score(b).cmp(&format_preference_score(a));
                if pref_cmp != std::cmp::Ordering::Equal {
                    return pref_cmp;
                }
                let abr_a = get_bitrate(a);
                let abr_b = get_bitrate(b);
                abr_b.partial_cmp(&abr_a).unwrap_or(std::cmp::Ordering::Equal)
            });

            if let Some(best) = with_audio.first() {
                if let Some(url_str) = best.get("url").and_then(|u| u.as_str()) {
                    let url = url_str.to_string();
                    let headers = best.get("http_headers")
                        .or_else(|| entry.get("http_headers"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    
                    warn!("Using format with video (no audio-only available): ext={}, acodec={}, vcodec={}", 
                        best.get("ext").and_then(|e| e.as_str()).unwrap_or("unknown"),
                        best.get("acodec").and_then(|a| a.as_str()).unwrap_or("unknown"),
                        best.get("vcodec").and_then(|v| v.as_str()).unwrap_or("unknown"));
                    
                    return Some((url, headers));
                }
            }
        }

        // Third pass: Try any format with a valid URL (last resort)
        for format in formats.iter() {
            if has_valid_url(format) {
                if let Some(url_str) = format.get("url").and_then(|u| u.as_str()) {
                    let url = url_str.to_string();
                    let headers = format.get("http_headers")
                        .or_else(|| entry.get("http_headers"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    
                    warn!("Using fallback format (no audio codec info): ext={}", 
                        format.get("ext").and_then(|e| e.as_str()).unwrap_or("unknown"));
                    
                    return Some((url, headers));
                }
            }
        }
    }

    // Final fallback: Check if entry itself has a direct URL
    if let Some(url) = entry.get("url").and_then(|u| u.as_str()) {
        if !url.is_empty() && (url.starts_with("http://") || url.starts_with("https://")) {
            let headers = entry.get("http_headers")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            
            debug!("Using direct URL from entry");
            return Some((url.to_string(), headers));
        }
    }

    None
}

impl YtDlpSource {
    pub async fn get_direct_url_and_headers(
        query: &str,
    ) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
        let resolved = if query.starts_with("http://") || query.starts_with("https://") {
            query.to_string()
        } else {
            format!("ytsearch1:{}", query)
        };

        // First get available formats
        let formats = get_available_formats(&resolved).await?;
        if formats.is_empty() {
            return Err("No formats available for this video".into());
        }

        // Show available formats for debugging
        info!("Available formats:");
        for format in &formats {
            if let Some(format_str) = format.get("format").and_then(|f| f.as_str()) {
                info!("{}", format_str);
            }
        }

        // Now get the JSON data with the selected format
        let mut cmd = Command::new("yt-dlp");
        cmd.args(&[
            "--no-playlist",
            "-J",
            "--no-warnings",
            // Add comprehensive headers to bypass bot detection
            "--user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/121.0.0.0 Safari/537.36",
            "--referer", "https://www.youtube.com/",
            // Add additional headers that real browsers send
            "--add-header", "Accept-Language:en-US,en;q=0.9",
            "--add-header", "Accept:text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
            "--add-header", "Accept-Encoding:gzip, deflate, br",
            "--add-header", "Sec-Fetch-Dest:document",
            "--add-header", "Sec-Fetch-Mode:navigate",
            "--add-header", "Sec-Fetch-Site:none",
            "--add-header", "Sec-Fetch-User:?1",
            "--add-header", "Upgrade-Insecure-Requests:1",
            // Resilience and timeout settings
            "--extractor-retries", "3",
            "--socket-timeout", "30",
            // YouTube-specific optimizations - use android client for better reliability
            "--extractor-args", "youtube:player_client=android,web",
            // Extract audio format explicitly
            "-f", "bestaudio[ext=webm]/bestaudio[ext=m4a]/bestaudio/best",
        ]);

        if let Some(cookie_path) = get_cookies_file_path() {
            cmd.arg("--cookies").arg(&cookie_path);
            info!("Using cookies from: {}", cookie_path);
        }
        cmd.arg(&resolved);

        let output = cmd.output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("yt-dlp failed: {}", stderr.trim()).into());
        }

        let json_str = String::from_utf8(output.stdout)?;
        let parsed: serde_json::Value = serde_json::from_str(&json_str)?;

        let (url, headers_val) = pick_best_audio(&parsed)
            .ok_or("yt-dlp: no suitable audio format found in JSON output")?;

        let mut header_str = String::new();
        if let Some(obj) = headers_val.as_object() {
            for (k, v) in obj {
                if let Some(vs) = v.as_str() {
                    header_str.push_str(&format!("{}: {}\r\n", k, vs));
                }
            }
        }

        info!("Resolved audio URL ({}...)", &url[..url.len().min(80)]);
        Ok((url, header_str))
    }

    pub async fn spawn_ffmpeg_stream(
        direct_url: &str,
        headers: &str,
        pcm_tx: mpsc::Sender<Vec<i16>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut cmd = Command::new("ffmpeg");

        cmd.args(&[
            "-reconnect",           "1",
            "-reconnect_streamed",  "1",
            "-reconnect_delay_max", "5",
            "-nostdin",
            "-loglevel", "error",
            "-hide_banner",
        ]);

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

        cmd.args(&[
            "-af", "aresample=48000,aformat=sample_fmts=s16:channel_layouts=stereo",
            "-vn",
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

        let mut reader = BufReader::with_capacity(READER_BUF, stdout);

        tokio::spawn(async move {
            let _child = child;
            let mut chunk = vec![0u8; FRAME_BYTES];
            let mut frames_produced: u64 = 0;

            loop {
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
                    info!("\u{1f39e}\u{fe0f} FFmpeg producing PCM frames");
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
                    info!("PCM channel closed \u{2014} ffmpeg task stopping");
                    break;
                }
            }
        });

        Ok(())
    }

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