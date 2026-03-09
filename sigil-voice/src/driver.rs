use futures_util::{SinkExt, StreamExt};
use sigil_discord::SigilSession;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn, error, debug};

use crate::gateway::{ProtocolData, SelectProtocol, SessionDescription, VoiceGatewayClient, VoicePacket};
use crate::udp::{receive_ip_discovery, send_ip_discovery};

pub struct CoreDriver {
    pub udp: Arc<UdpSocket>,
    pub sigil: Arc<Mutex<SigilSession>>,
    pub mode: Option<String>,
    pub secret_key: Option<Vec<u8>>,
    pub ws_tx_channel: mpsc::Sender<crate::gateway::WsMessage>,
    pub tracks: Arc<Mutex<Vec<crate::track::TrackHandle>>>,
    pub ssrc: u32,
    pub target_addr: String,
    pub ssrc_map: Arc<Mutex<std::collections::HashMap<u32, u64>>>,
    pub receiver_tx: Option<mpsc::UnboundedSender<(u64, Vec<i16>)>>,
    pub decoders: Arc<Mutex<std::collections::HashMap<u64, crate::audio::AudioDecoder>>>,
}

impl CoreDriver {
    /// Full voice connection lifecycle:
    /// 1. SigilSession init → 2. WS Handshake → 3. UDP Bind → 4. IP Discovery
    /// → 5. Select Protocol → 6. Session Description → 7. Background WS task
    pub async fn connect(
        endpoint: &str,
        server_id: &str,
        user_id: &str,
        session_id: &str,
        token: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        info!("⚡ CoreDriver::connect starting [endpoint={}, server={}, user={}]", endpoint, server_id, user_id);

        // 1. SigilSession for DAVE E2EE
        let user_id_u64: u64 = user_id.parse().map_err(|e| {
            error!("Failed to parse user_id '{}': {:?}", user_id, e);
            format!("Invalid user_id: {}", user_id)
        })?;
        let sigil = Arc::new(Mutex::new(SigilSession::new(user_id_u64).map_err(|e| {
            error!("SigilSession::new failed: {:?}", e);
            e
        })?));
        info!("✅ Step 1/7: SigilSession created");

        // 2. Voice Gateway WS connect + handshake
        let mut gateway = VoiceGatewayClient::connect(endpoint).await.map_err(|e| {
            error!("Voice WS connect failed for endpoint '{}': {:?}", endpoint, e);
            e
        })?;
        info!("✅ Step 2a/7: WS connected");

        let (ready, heartbeat_interval) = gateway
            .handshake(server_id, user_id, session_id, token)
            .await
            .map_err(|e| {
                error!("Voice WS handshake failed: {:?}", e);
                e
            })?;
        info!("✅ Step 2b/7: WS handshake done [SSRC={}, IP={}, Port={}]", ready.ssrc, ready.ip, ready.port);

        // 3. Bind local UDP
        let udp = Arc::new(UdpSocket::bind("0.0.0.0:0").await.map_err(|e| {
            error!("UDP bind failed: {:?}", e);
            e
        })?);
        info!("✅ Step 3/7: UDP socket bound to {:?}", udp.local_addr());

        // 4. IP Discovery
        let target_addr = format!("{}:{}", ready.ip, ready.port);
        info!("Starting IP discovery towards {}", target_addr);
        send_ip_discovery(&udp, &ready.ip, ready.port, ready.ssrc).await.map_err(|e| {
            error!("IP discovery send failed: {:?}", e);
            e
        })?;
        let (external_ip, external_port) = receive_ip_discovery(&udp).await.map_err(|e| {
            error!("IP discovery receive failed: {:?}", e);
            e
        })?;
        info!("✅ Step 4/7: IP Discovery done [external={}:{}]", external_ip, external_port);

        // 5. Select Protocol
        let select_protocol = SelectProtocol {
            protocol: "udp".to_string(),
            data: ProtocolData {
                address: external_ip,
                port: external_port,
                mode: "aead_aes256_gcm_rtpsize".to_string(),
            },
        };
        gateway.send_packet(1, select_protocol).await.map_err(|e| {
            error!("SelectProtocol send failed: {:?}", e);
            e
        })?;
        info!("✅ Step 5/7: SelectProtocol sent");

        // 6. Wait for SessionDescription (OP 4) with timeout
        let mode;
        let secret_key;
        let session_desc_timeout = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            async {
                loop {
                    let packet = gateway
                        .recv_packet()
                        .await?
                        .ok_or("Connection closed before SessionDescription")?;
                    match packet {
                        crate::gateway::WsMessage::Text(p) => {
                            debug!("Voice WS received OP {} (waiting for OP 4)", p.op);
                            if p.op == 4 {
                                let desc: SessionDescription = serde_json::from_value(
                                    p.d.ok_or("SessionDescription missing 'd' field")?
                                )?;
                                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(desc);
                            }
                            // Skip other text opcodes while waiting
                        }
                        crate::gateway::WsMessage::Binary(bin) => {
                            debug!("Voice WS skipping binary len={} while waiting for OP 4", bin.len());
                        }
                    }
                }
            }
        ).await;

        let session_desc = match session_desc_timeout {
            Ok(Ok(desc)) => desc,
            Ok(Err(e)) => {
                error!("SessionDescription receive failed: {:?}", e);
                return Err(e);
            }
            Err(_) => {
                error!("Timed out waiting for SessionDescription (10s)");
                return Err("Timed out waiting for SessionDescription".into());
            }
        };

        info!("✅ Step 6/7: SessionDescription received [mode={}, key_len={}]",
            session_desc.mode, session_desc.secret_key.len());
        mode = Some(session_desc.mode);
        secret_key = Some(session_desc.secret_key);

        // 7. Spawn background WS task for heartbeats + DAVE opcodes
        let (mut ws_tx, mut ws_rx) = gateway.ws.split();
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<crate::gateway::WsMessage>(100);
        let ssrc_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let ssrc_map_clone = ssrc_map.clone();
        let sigil_clone = sigil.clone();
        let my_ssrc = ready.ssrc; // Correctly capture handshake SSRC

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(heartbeat_interval as u64));
            interval.tick().await; // skip first immediate tick
            let mut seq_ack: Option<u64> = None;
            let mut binary_seq = 0u16;
            info!("🔄 Voice WS background task started (heartbeat={}ms)", heartbeat_interval);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let nonce = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;
                        let hb = VoicePacket {
                            op: 3,
                            d: Some(serde_json::json!(nonce)),
                            s: None, t: None, seq_ack,
                        };
                        if let Ok(text) = serde_json::to_string(&hb) {
                            if ws_tx.send(tokio_tungstenite::tungstenite::Message::Text(text.into())).await.is_err() {
                                warn!("WS heartbeat send failed — connection dropped");
                                break;
                            }
                        }
                    }
                    cmd_opt = cmd_rx.recv() => {
                        let Some(cmd) = cmd_opt else {
                            info!("Command channel closed — WS background task exiting");
                            break;
                        };
                        let msg = match cmd {
                            crate::gateway::WsMessage::Text(p) => {
                                if let Ok(text) = serde_json::to_string(&p) {
                                    tokio_tungstenite::tungstenite::Message::Text(text.into())
                                } else {
                                    continue;
                                }
                            }
                            crate::gateway::WsMessage::Binary(bin) => {
                                tokio_tungstenite::tungstenite::Message::Binary(bin.into())
                            }
                        };
                        if ws_tx.send(msg).await.is_err() {
                            warn!("WS command send failed — connection dropped");
                            break;
                        }
                    }
                    msg_opt = ws_rx.next() => {
                        let Some(Ok(msg)) = msg_opt else {
                            warn!("Voice WS read ended — background task exiting");
                            break;
                        };
                        match msg {
                            tokio_tungstenite::tungstenite::Message::Text(text) => {
                                if let Ok(packet) = serde_json::from_str::<VoicePacket>(&text) {
                                    if let Some(seq) = packet.s {
                                        seq_ack = Some(seq);
                                    }
                                    match packet.op {
                                        6 => debug!("Heartbeat ACK"),
                                        5 => {
                                            if let Some(d) = packet.d {
                                                if let Ok(spk) = serde_json::from_value::<crate::gateway::Speaking>(d) {
                                                    if let Some(uid_str) = spk.user_id {
                                                        if let Ok(uid) = uid_str.parse::<u64>() {
                                                            let mut map = ssrc_map_clone.lock().await;
                                                            map.insert(spk.ssrc, uid);
                                                            debug!("Mapped SSRC {} → UserId {}", spk.ssrc, uid);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        _ => debug!("Voice WS text OP {}", packet.op),
                                    }
                                }
                            }
                            tokio_tungstenite::tungstenite::Message::Binary(bin) => {
                                if bin.len() > 2 {
                                    let opcode = bin[2];
                                    debug!("Voice WS binary OP {} (len={})", opcode, bin.len());
                                    let mut s = sigil_clone.lock().await;
                                    match s.handle_gateway_event(opcode, &bin) {
                                        Ok(event) => {
                                            use sigil_discord::gateway::handler::DaveEvent;
                                            match event {
                                                DaveEvent::PrepareTransition(p) => {
                                                    let ready = sigil_discord::gateway::opcodes::ReadyForTransition {
                                                        transition_id: p.transition_id,
                                                    };
                                                    let packet = VoicePacket {
                                                        op: 23,
                                                        d: Some(serde_json::to_value(ready).unwrap_or_default()),
                                                        s: None, t: None, seq_ack: None,
                                                    };
                                                    if let Ok(text) = serde_json::to_string(&packet) {
                                                        let _ = ws_tx.send(tokio_tungstenite::tungstenite::Message::Text(text.into())).await;
                                                        info!("DAVE: Sent OP 23 ReadyForTransition");
                                                    }
                                                }
                                                DaveEvent::ExecuteTransition(e) => {
                                                    info!("DAVE: ExecuteTransition (ID: {})", e.transition_id);
                                                }
                                                DaveEvent::MlsExternalSender(ext) => {
                                                    // First create the MLS group using the external sender credential
                                                    match s.set_external_sender(&ext.credential) {
                                                        Ok(()) => {
                                                            info!("DAVE: ✅ Group created via External Sender");
                                                        }
                                                        Err(e) => {
                                                            error!("DAVE: ❌ set_external_sender FAILED: {:?}", e);
                                                            continue;
                                                        }
                                                    };
                                                    
                                                    // Now export our own sender key so we can encrypt audio
                                                    let uid = s.user_id;
                                                    match s.export_sender_keys(&[uid]) {
                                                        Ok(keys) => {
                                                            if keys.contains_key(&uid) {
                                                                info!("DAVE: ✅ Exported own sender key via External Sender (key: {:02x}...)", 
                                                                    keys.get(&uid).unwrap()[0]);
                                                            } else {
                                                                error!("DAVE: ❌ export_sender_keys returned OK but missing own key!");
                                                            }
                                                        }
                                                        Err(e) => {
                                                            error!("DAVE: ❌ export_sender_keys FAILED: {:?}", e);
                                                        }
                                                    }
                                                    info!("DAVE: Processed OP 25 External Sender");
                                                }
                                                DaveEvent::MlsProposals(prop) => {
                                                    // Process incoming proposals
                                                    if let Err(e) = s.process_proposals(&[prop.data.clone()]) {
                                                        error!("DAVE: ❌ process_proposals FAILED: {:?}", e);
                                                        continue;
                                                    }
                                                    
                                                    // Create commit to resolve proposals
                                                    match s.commit_and_welcome() {
                                                        Ok((commit_bytes, opt_welcome)) => {
                                                            // Export our own sender key so we can encrypt audio
                                                            let uid = s.user_id;
                                                            match s.export_sender_keys(&[uid]) {
                                                                Ok(keys) => {
                                                                    if keys.contains_key(&uid) {
                                                                        info!("DAVE: ✅ Exported own sender key via Proposals (key: {:02x}...)", 
                                                                            keys.get(&uid).unwrap()[0]);
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    error!("DAVE: ❌ export_sender_keys FAILED: {:?}", e);
                                                                }
                                                            }
                                                            
                                                            let mut payload = vec![0u8; 3];
                                                            payload[0..2].copy_from_slice(&binary_seq.to_be_bytes());
                                                            binary_seq = binary_seq.wrapping_add(1);
                                                            payload[2] = 28;
                                                            payload.extend_from_slice(&commit_bytes);
                                                            if let Some(w) = opt_welcome {
                                                                payload.extend_from_slice(&w);
                                                            }
                                                            let _ = ws_tx.send(tokio_tungstenite::tungstenite::Message::Binary(payload.into())).await;
                                                            info!("DAVE: Sent OP 28 MlsCommitWelcome (Keys Exported)");
                                                        }
                                                        Err(e) => {
                                                            error!("DAVE: ❌ commit_and_welcome FAILED: {:?}", e);
                                                        }
                                                    }
                                                }
                                                DaveEvent::MlsWelcome(w) => {
                                                    match s.join_group(&w.welcome_bytes) {
                                                        Ok(()) => info!("DAVE: ✅ Group joined via Welcome"),
                                                        Err(e) => {
                                                            error!("DAVE: ❌ join_group FAILED: {:?}", e);
                                                            continue;
                                                        }
                                                    }
                                                    
                                                    // Export our own sender key so we can encrypt audio
                                                    let uid = s.user_id;
                                                    match s.export_sender_keys(&[uid]) {
                                                        Ok(keys) => {
                                                            if keys.contains_key(&uid) {
                                                                info!("DAVE: ✅ Exported own sender key via Welcome (key: {:02x}...)", 
                                                                    keys.get(&uid).unwrap()[0]);
                                                            } else {
                                                                error!("DAVE: ❌ export_sender_keys returned OK but missing own key!");
                                                            }
                                                        }
                                                        Err(e) => {
                                                            error!("DAVE: ❌ export_sender_keys FAILED: {:?}", e);
                                                        }
                                                    }
                                                    info!("DAVE: ✅ Group joined via Welcome! (Keys Exported)");
                                                }
                                                DaveEvent::MlsAnnounceCommitTransition(c) => {
                                                    // Process the commit and advance the epoch
                                                    match s.process_commit(&c.commit_bytes) {
                                                        Ok(_) => info!("DAVE: ✅ Processed commit, epoch advanced"),
                                                        Err(e) => {
                                                            error!("DAVE: ❌ process_commit FAILED: {:?}", e);
                                                            continue;
                                                        }
                                                    }
                                                    
                                                    // Export our own sender key for the new epoch
                                                    let uid = s.user_id;
                                                    match s.export_sender_keys(&[uid]) {
                                                        Ok(keys) => {
                                                            if keys.contains_key(&uid) {
                                                                info!("DAVE: ✅ Exported own sender key via Commit (key: {:02x}...)", 
                                                                    keys.get(&uid).unwrap()[0]);
                                                            } else {
                                                                error!("DAVE: ❌ export_sender_keys returned OK but missing own key!");
                                                            }
                                                        }
                                                        Err(e) => {
                                                            error!("DAVE: ❌ export_sender_keys FAILED: {:?}", e);
                                                        }
                                                    }
                                                    info!("DAVE: Processed OP 29 Commit (Keys Exported)");
                                                }
                                                DaveEvent::PrepareEpoch(p) => {
                                                    info!("DAVE: PrepareEpoch {}", p.epoch);
                                                    
                                                    // Export sender keys for ALL epochs, not just epoch 1
                                                    // This ensures we have our own key cached for encryption
                                                    let uid = s.user_id;
                                                    match s.export_sender_keys(&[uid]) {
                                                        Ok(keys) => {
                                                            if let Some(key) = keys.get(&uid) {
                                                                info!("DAVE: ✅ Exported own sender key for epoch {} (key: {:02x}...)", 
                                                                    p.epoch, key[0]);
                                                            } else {
                                                                warn!("DAVE: export_sender_keys returned OK but no key for own user_id!");
                                                            }
                                                        }
                                                        Err(e) => {
                                                            error!("DAVE: ❌ export_sender_keys FAILED at PrepareEpoch {}: {:?}", p.epoch, e);
                                                        }
                                                    }
                                                    
                                                    // Send key package at epoch 1 (initial group creation)
                                                    if p.epoch == 1 {
                                                        if let Ok(kp) = s.generate_key_package() {
                                                            let mut payload = vec![0u8; 3];
                                                            payload[0..2].copy_from_slice(&binary_seq.to_be_bytes());
                                                            binary_seq = binary_seq.wrapping_add(1);
                                                            payload[2] = 26;
                                                            payload.extend_from_slice(&kp);
                                                            let _ = ws_tx.send(tokio_tungstenite::tungstenite::Message::Binary(payload.into())).await;
                                                            info!("DAVE: Sent OP 26 MlsKeyPackage");
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            debug!("DAVE event OP {} parse error: {:?}", opcode, e);
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            warn!("Voice WS background task ended");
        });

        info!("✅ Step 7/7: Background WS task spawned — CoreDriver fully ready");

        Ok(Self {
            udp,
            sigil,
            mode,
            secret_key,
            ws_tx_channel: cmd_tx,
            tracks: Arc::new(Mutex::new(Vec::new())),
            ssrc: ready.ssrc,
            target_addr,
            ssrc_map,
            receiver_tx: None,
            decoders: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// Add a track to the driver. It will be mixed into the outbound audio stream.
    pub async fn add_track(&self, track: crate::track::Track) -> crate::track::TrackHandle {
        let handle = crate::track::TrackHandle::new(track);
        let mut tracks = self.tracks.lock().await;
        tracks.push(handle.clone());
        info!("🎵 Track added (total active: {})", tracks.len());
        handle
    }

    /// Stop all playback and clear the track list.
    pub async fn stop(&self) {
        let mut tracks = self.tracks.lock().await;
        info!("⏹️ Stopping {} tracks", tracks.len());
        tracks.clear();
    }


    /// The primary audio engine loop.
    /// Runs every 20ms: poll tracks → mix PCM → Opus encode → DAVE encrypt → UDP send.
    /// The loop NEVER exits on per-frame errors — it logs and skips to keep the connection alive.
    pub async fn start_mixing(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let secret_key = match self.secret_key.clone() {
            Some(k) => k,
            None => {
                error!("start_mixing: No secret_key — cannot start mixing loop");
                return Err("No secret key".into());
            }
        };

        // Create the Opus encoder
        let mut encoder = match crate::audio::AudioEncoder::new() {
            Ok(e) => {
                info!("🎙️ AudioEncoder created successfully");
                e
            }
            Err(e) => {
                error!("start_mixing: Failed to create AudioEncoder: {:?}", e);
                return Err(e);
            }
        };

        // Send OP 5 Speaking — non-fatal if it fails
        let speaking = crate::gateway::Speaking { speaking: 1, delay: 0, ssrc: self.ssrc, user_id: None };
        if let Ok(d) = serde_json::to_value(speaking) {
            let _ = self.ws_tx_channel.send(crate::gateway::WsMessage::Text(VoicePacket {
                op: 5, d: Some(d), s: None, t: None, seq_ack: None,
            })).await;
        }
        info!("🎙️ Mixing loop started for SSRC={}, target={}", self.ssrc, self.target_addr);

        let mut seq = 0u16;
        let mut timestamp = 0u32;
        let mut nonce_counter = 0u32;
        let mut opus_buf = [0u8; 4000];
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(20));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut frames_sent: u64 = 0;
        let mut dave_skip_count: u64 = 0;

        loop {
            ticker.tick().await;

            // --- 1. Poll all tracks and mix PCM ---
            let mut mixed_pcm = vec![0i16; 1920];
            let mut active_any = false;
            {
                let mut tracks = self.tracks.lock().await;

                // Drop finished tracks
                tracks.retain(|h| {
                    use crate::track::PlayState;
                    matches!(h.get_state_atomic(), PlayState::Playing | PlayState::Paused)
                });

                for handle in tracks.iter() {
                    if handle.get_state_atomic() != crate::track::PlayState::Playing {
                        continue;
                    }
                    if let Ok(mut t) = handle.inner().try_lock() {
                        match t.source.read_frame() {
                            Some(frame) => {
                                active_any = true;
                                for (i, &s) in frame.iter().enumerate() {
                                    if i >= mixed_pcm.len() { break; }
                                    let scaled = (s as f32 * t.volume) as i32;
                                    mixed_pcm[i] = (mixed_pcm[i] as i32 + scaled)
                                        .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                                }
                            }
                            None => {
                                t.state.store(crate::track::PlayState::Stopped as u8,
                                    std::sync::atomic::Ordering::SeqCst);
                                if let Some(tx) = &t.event_tx {
                                    let _ = tx.send(crate::track::TrackEvent::End);
                                }
                                info!("Track source exhausted (sender dropped)");
                            }
                        }
                    } else {
                        active_any = true;
                    }
                }
            }

            if !active_any {
                continue;
            }

            // Log amplitude every 10 seconds
            if frames_sent % 500 == 0 {
                let sum: i64 = mixed_pcm.iter().map(|&s| s.abs() as i64).sum();
                let avg = sum / mixed_pcm.len() as i64;
                info!("🎙️ Mixed PCM Avg Amplitude: {} (frames_sent={})", avg, frames_sent);
            }

            // --- 2. Encode to Opus ---
            let opus_len = match encoder.encode_pcm(&mixed_pcm, &mut opus_buf) {
                Ok(n) => n,
                Err(e) => {
                    warn!("Opus encode error (skipping frame): {:?}", e);
                    continue;
                }
            };
            let opus_data = &opus_buf[..opus_len];

            // --- 3. DAVE encrypt (may fail if MLS group not ready yet) ---
            let dave_ciphertext = {
                let mut sigil_guard = self.sigil.lock().await;
                match sigil_guard.encrypt_own_frame(opus_data, sigil_discord::crypto::codec::Codec::Opus) {
                    Ok(ct) => {
                        if dave_skip_count > 0 {
                            info!("🔊 DAVE encryption active! (skipped {} frames while MLS was pending)", dave_skip_count);
                            dave_skip_count = 0;
                        }
                        if frames_sent == 0 {
                            info!("🔊 First DAVE-encrypted audio frame produced! Sending via UDP...");
                        }
                        ct
                    }
                    Err(e) => {
                        dave_skip_count += 1;
                        if dave_skip_count == 1 || dave_skip_count % 250 == 0 {
                            info!("🔒 DAVE encrypt failed/pending: {:?} (dropped {} frames so far — established: {})", 
                                e, dave_skip_count, sigil_guard.is_established());
                        }
                        continue;
                    }
                }
            };

            // --- 4. Transport encrypt (AES-256-GCM) ---
            let rtp_header = crate::udp::build_rtp_header(seq, timestamp, self.ssrc);
            let udp_payload = match crate::udp::transport_encrypt_rtpsize(
                &secret_key, &rtp_header, &dave_ciphertext, nonce_counter
            ) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Transport encrypt failed (skipping frame): {:?}", e);
                    continue;
                }
            };

            // --- 5. Send via UDP ---
            if let Err(e) = self.udp.send_to(&udp_payload, &self.target_addr).await {
                warn!("UDP send error (skipping frame): {:?}", e);
                continue;
            }

            frames_sent += 1;
            if frames_sent == 1 {
                info!("🔊 First audio frame sent via UDP!");
            } else if frames_sent == 50 {
                info!("🔊 50 audio frames sent (1 second of audio)");
            } else if frames_sent % 2500 == 0 {
                info!("🔊 {} audio frames sent (~{} seconds)", frames_sent, frames_sent / 50);
            }

            seq = seq.wrapping_add(1);
            timestamp = timestamp.wrapping_add(960);
            nonce_counter = nonce_counter.wrapping_add(1);
        }
    }

    /// Start the background UDP receiver task to handle incoming audio.
    pub async fn start_receiver(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let udp = self.udp.clone();
        let sigil = self.sigil.clone();
        let ssrc_map = self.ssrc_map.clone();
        let decoders = self.decoders.clone();
        let receiver_tx = self.receiver_tx.clone().ok_or("No receiver channel configured")?;
        let secret_key = self.secret_key.clone().ok_or("No secret key negotiated")?;

        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut pcm_out = [0i16; 1920];

            loop {
                match udp.recv_from(&mut buf).await {
                    Ok((n, _addr)) => {
                        let packet = &buf[..n];
                        if n < 12 { continue; }

                        let ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);

                        let decrypted_rtp = match crate::udp::transport_decrypt_rtpsize(&secret_key, packet) {
                            Ok(d) => d,
                            Err(_) => continue,
                        };

                        let mut user_id = 0u64;
                        {
                            let map = ssrc_map.lock().await;
                            if let Some(&uid) = map.get(&ssrc) {
                                user_id = uid;
                            }
                        }

                        if user_id == 0 { continue; }

                        let dave_decrypted = {
                            let s = sigil.lock().await;
                            match s.decrypt_from_sender(user_id, &decrypted_rtp) {
                                Ok(d) => d,
                                Err(_) => continue,
                            }
                        };

                        let mut decs = decoders.lock().await;
                        let decoder = decs.entry(user_id).or_insert_with(|| {
                            crate::audio::AudioDecoder::new().expect("Failed to create decoder")
                        });

                        if let Ok(samples) = decoder.decode_opus(&dave_decrypted, &mut pcm_out) {
                            let _ = receiver_tx.send((user_id, pcm_out[..samples].to_vec()));
                        }
                    }
                    Err(e) => {
                        error!("UDP Receiver error: {:?}", e);
                        break;
                    }
                }
            }
        });

        Ok(())
    }
}
