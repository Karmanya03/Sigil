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
        "--no-check-certificates",
    ]);

    if let Some(cookie_path) = get_cookies_file_path() {
        cmd.arg("--cookies").arg(&cookie_path);
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
        let mut audio_only: Vec<&serde_json::Value> = formats
            .iter()
            .filter(|f| {
                let vcodec = f.get("vcodec").and_then(|v| v.as_str()).unwrap_or("");
                let acodec = f.get("acodec").and_then(|v| v.as_str()).unwrap_or("none");
                let has_url = f.get("url").and_then(|u| u.as_str())
                    .map(|u| !u.is_empty() && u.starts_with("http"))
                    .unwrap_or(false);
                (vcodec == "none" || vcodec.is_empty()) && acodec != "none" && has_url
            })
            .collect();

        audio_only.sort_by(|a, b| {
            let abr_a = a.get("abr").and_then(|v| v.as_f64())
                .or_else(|| a.get("tbr").and_then(|v| v.as_f64()))
                .unwrap_or(0.0);
            let abr_b = b.get("abr").and_then(|v| v.as_f64())
                .or_else(|| b.get("tbr").and_then(|v| v.as_f64()))
                .unwrap_or(0.0);
            abr_b.partial_cmp(&abr_a).unwrap_or(std::cmp::Ordering::Equal)
        });

        if let Some(best) = audio_only.first() {
            let url = best.get("url").unwrap().as_str().unwrap().to_string();
            let headers = best.get("http_headers")
                .or_else(|| entry.get("http_headers"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            return Some((url, headers));
        }

        let mut with_audio: Vec<&serde_json::Value> = formats
            .iter()
            .filter(|f| {
                let acodec = f.get("acodec").and_then(|v| v.as_str()).unwrap_or("none");
                let has_url = f.get("url").and_then(|u| u.as_str())
                    .map(|u| !u.is_empty() && u.starts_with("http"))
                    .unwrap_or(false);
                acodec != "none" && has_url
            })
            .collect();

        with_audio.sort_by(|a, b| {
            let abr_a = a.get("abr").and_then(|v| v.as_f64())
                .or_else(|| a.get("tbr").and_then(|v| v.as_f64()))
                .unwrap_or(0.0);
            let abr_b = b.get("abr").and_then(|v| v.as_f64())
                .or_else(|| b.get("tbr").and_then(|v| v.as_f64()))
                .unwrap_or(0.0);
            abr_b.partial_cmp(&abr_a).unwrap_or(std::cmp::Ordering::Equal)
        });

        if let Some(best) = with_audio.first() {
            let url = best.get("url").unwrap().as_str().unwrap().to_string();
            let headers = best.get("http_headers")
                .or_else(|| entry.get("http_headers"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            return Some((url, headers));
        }
    }

    if let Some(url) = entry.get("url").and_then(|u| u.as_str()) {
        if !url.is_empty() && url.starts_with("http") {
            let headers = entry.get("http_headers")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
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
            "--no-check-certificates",
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