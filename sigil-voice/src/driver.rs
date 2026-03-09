use futures_util::{SinkExt, StreamExt};
use sigil_discord::SigilSession;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tracing::info;

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
    /// Connects to Discord Voice, performs the WS handshake, completes UDP Hole Punching,
    /// and establishes the final transport session keys while spinning up a WS background task.
    pub async fn connect(
        endpoint: &str,
        server_id: &str,
        user_id: &str,
        session_id: &str,
        token: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // 1. Initialize SigilSession for DAVE End-to-End Encryption
        let sigil = Arc::new(Mutex::new(SigilSession::new(user_id.parse()?)?));

        // 2. Connect to WS and Handshake
        let mut gateway = VoiceGatewayClient::connect(endpoint).await?;
        let (ready, heartbeat_interval) = gateway
            .handshake(server_id, user_id, session_id, token)
            .await?;

        // 3. Bind local UDP socket
        let udp = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);

        // 4. Perform IP Discovery
        info!("Starting IP discovery towards {}:{}", ready.ip, ready.port);
        send_ip_discovery(&udp, &ready.ip, ready.port, ready.ssrc).await?;
        let (external_ip, external_port) = receive_ip_discovery(&udp).await?;

        // 5. Select Protocol based on UDP discovery
        let select_protocol = SelectProtocol {
            protocol: "udp".to_string(),
            data: ProtocolData {
                address: external_ip,
                port: external_port,
                mode: "aead_aes256_gcm_rtpsize".to_string(), // Discord's preferred UDP encryption
            },
        };
        gateway.send_packet(1, select_protocol).await?;
        info!("Sent SelectProtocol");

        // 6. Wait for SessionDescription (OP 4)
        let mode;
        let secret_key;
        loop {
            let packet = gateway
                .recv_packet()
                .await?
                .ok_or("Connection closed before SessionDescription")?;
            if let crate::gateway::WsMessage::Text(p) = packet {
                if p.op == 4 {
                    let session_desc: SessionDescription = serde_json::from_value(p.d.unwrap())?;
                    info!(
                        "Received SessionDescription from Voice Gateway. Mode: {}",
                        session_desc.mode
                    );
                    mode = Some(session_desc.mode);
                    secret_key = Some(session_desc.secret_key);
                    break;
                }
            }
        }

        // 7. Spawn Background Task for WS (Heartbeats, DAVE Opcodes)
        let (mut ws_tx, mut ws_rx) = gateway.ws.split();
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<crate::gateway::WsMessage>(100);
        let ssrc_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let ssrc_map_clone = ssrc_map.clone();
        let sigil_clone = sigil.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(heartbeat_interval as u64));
            interval.tick().await; // skip first immediate tick
            let mut seq_ack: Option<u64> = None;

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
                                break;
                            }
                        }
                    }
                    cmd_opt = cmd_rx.recv() => {
                        let Some(cmd) = cmd_opt else { break; };
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
                            break;
                        }
                    }
                    msg_opt = ws_rx.next() => {
                        let Some(Ok(msg)) = msg_opt else { break; };
                        match msg {
                            tokio_tungstenite::tungstenite::Message::Text(text) => {
                                if let Ok(packet) = serde_json::from_str::<VoicePacket>(&text) {
                                    if let Some(seq) = packet.s {
                                        seq_ack = Some(seq);
                                    }
                                    if packet.op == 6 {
                                        tracing::debug!("Received Heartbeat ACK");
                                    } else if packet.op == 5 {
                                        if let Some(d) = packet.d {
                                            if let Ok(spk) = serde_json::from_value::<crate::gateway::Speaking>(d) {
                                                if let Some(uid_str) = spk.user_id {
                                                    if let Ok(uid) = uid_str.parse::<u64>() {
                                                        let mut map = ssrc_map_clone.lock().await;
                                                        map.insert(spk.ssrc, uid);
                                                        tracing::debug!("Mapped SSRC {} to UserId {}", spk.ssrc, uid);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            tokio_tungstenite::tungstenite::Message::Binary(bin) => {
                                if bin.len() > 2 {
                                    let opcode = bin[2];
                                    let mut s = sigil_clone.lock().await;
                                    if let Ok(event) = s.handle_gateway_event(opcode, &bin) {
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
                                                    tracing::info!("Sent OP 23 ReadyForTransition for {:?}", p.transition_id);
                                                }
                                            }
                                            DaveEvent::ExecuteTransition(e) => {
                                                tracing::info!("Executing Dave Transition {:?}", e);
                                            }
                                            DaveEvent::MlsExternalSender(ext) => {
                                                // ext.credential contains the full blended binary payload from OP 25
                                                let _ = s.set_external_sender(&ext.credential);
                                                tracing::info!("Processed DAVE OP 25 External Sender");
                                            }
                                            DaveEvent::MlsProposals(prop) => {
                                                let _ = s.process_proposals(&[prop.data.clone()]);
                                                if let Ok((commit_bytes, opt_welcome)) = s.commit_and_welcome() {
                                                    let mut payload = vec![0u8; 3];
                                                    payload[2] = 28; // OP 28 MlsCommitWelcome
                                                    payload.extend_from_slice(&commit_bytes);
                                                    if let Some(w) = opt_welcome {
                                                        payload.extend_from_slice(&w);
                                                    }
                                                    let _ = ws_tx.send(tokio_tungstenite::tungstenite::Message::Binary(payload.into())).await;
                                                }
                                            }
                                            DaveEvent::MlsWelcome(w) => {
                                                let _ = s.join_group(&w.welcome_bytes);
                                                tracing::info!("DAVE Group successfully joined!");
                                            }
                                            DaveEvent::MlsAnnounceCommitTransition(c) => {
                                                let _ = s.process_commit(&c.commit_bytes);
                                                tracing::info!("DAVE Processed OP 29 Announce Commit");
                                            }
                                            DaveEvent::PrepareEpoch(p) => {
                                                if p.epoch == 1 {
                                                    if let Ok(kp) = s.generate_key_package() {
                                                        let mut payload = vec![0u8; 3];
                                                        payload[2] = 26; // OP 26 MlsKeyPackage
                                                        payload.extend_from_slice(&kp);
                                                        let _ = ws_tx.send(tokio_tungstenite::tungstenite::Message::Binary(payload.into())).await;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        Ok(Self {
            udp,
            sigil,
            mode,
            secret_key,
            ws_tx_channel: cmd_tx,
            tracks: Arc::new(Mutex::new(Vec::new())),
            ssrc: ready.ssrc,
            target_addr: ready.ip + ":" + &ready.port.to_string(),
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
        handle
    }

    /// Stop all playback and clear the track list.
    pub async fn stop(&self) {
        let mut tracks = self.tracks.lock().await;
        tracks.clear();
    }


    /// The primary audio engine loop.
    /// Runs every 20ms: poll tracks → mix PCM → Opus encode → DAVE encrypt → UDP send.
    /// The loop NEVER exits on per-frame errors — it logs and skips to keep the connection alive.
    pub async fn start_mixing(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let secret_key = match self.secret_key.clone() {
            Some(k) => k,
            None => {
                tracing::error!("start_mixing: No secret_key — cannot start mixing loop");
                return Err("No secret key".into());
            }
        };

        // Create the Opus encoder
        let mut encoder = match crate::audio::AudioEncoder::new() {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("start_mixing: Failed to create AudioEncoder: {:?}", e);
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
        tracing::info!("🎙️ Mixing loop started for SSRC={}", self.ssrc);

        let mut seq = 0u16;
        let mut timestamp = 0u32;
        let mut nonce_counter = 0u32;
        let mut opus_buf = [0u8; 4000];
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(20));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

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
                                // Sender dropped — stream truly finished
                                t.state.store(crate::track::PlayState::Stopped as u8,
                                    std::sync::atomic::Ordering::SeqCst);
                                if let Some(tx) = &t.event_tx {
                                    let _ = tx.send(crate::track::TrackEvent::End);
                                }
                                tracing::info!("Track finished (source exhausted)");
                            }
                        }
                    } else {
                        active_any = true; // locked by user, keep tick alive
                    }
                }
            }

            if !active_any {
                continue; // nothing to encode this tick
            }

            // --- 2. Encode to Opus ---
            let opus_len = match encoder.encode_pcm(&mixed_pcm, &mut opus_buf) {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!("Opus encode error (skipping frame): {:?}", e);
                    continue;
                }
            };
            let opus_data = &opus_buf[..opus_len];

            // --- 3. DAVE encrypt (may fail if MLS group not ready yet) ---
            let dave_ciphertext = {
                let mut sigil_guard = self.sigil.lock().await;
                match sigil_guard.encrypt_own_frame(opus_data, sigil_discord::crypto::codec::Codec::Opus) {
                    Ok(ct) => ct,
                    Err(e) => {
                        tracing::debug!("DAVE encrypt not ready yet (MLS pending?): {:?}", e);
                        continue; // Skip this frame — DAVE key exchange still in progress
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
                    tracing::warn!("Transport encrypt failed (skipping frame): {:?}", e);
                    continue;
                }
            };

            // --- 5. Send via UDP ---
            if let Err(e) = self.udp.send_to(&udp_payload, &self.target_addr).await {
                tracing::warn!("UDP send error (skipping frame): {:?}", e);
                continue;
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
                        if n < 12 { continue; } // Too short for RTP

                        // 1. Extract RTP Header
                        let ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);
                        
                        // 2. Transport Decrypt
                        let decrypted_rtp = match crate::udp::transport_decrypt_rtpsize(&secret_key, packet) {
                            Ok(d) => d,
                            Err(_) => continue,
                        };

                        // 3. Sigil DAVE Decrypt (requires UserId)
                        let mut user_id = 0u64;
                        {
                            let map = ssrc_map.lock().await;
                            if let Some(&uid) = map.get(&ssrc) {
                                user_id = uid;
                            }
                        }

                        if user_id == 0 { continue; } // Unknown sender

                        let dave_decrypted = {
                            let s = sigil.lock().await;
                            match s.decrypt_from_sender(user_id, &decrypted_rtp) {
                                Ok(d) => d,
                                Err(_) => continue,
                            }
                        };

                        // 4. Decode Opus to PCM
                        let mut decs = decoders.lock().await;
                        let decoder = decs.entry(user_id).or_insert_with(|| {
                            crate::audio::AudioDecoder::new().expect("Failed to create decoder")
                        });

                        if let Ok(samples) = decoder.decode_opus(&dave_decrypted, &mut pcm_out) {
                            let _ = receiver_tx.send((user_id, pcm_out[..samples].to_vec()));
                        }
                    }
                    Err(e) => {
                        tracing::error!("UDP Receiver error: {:?}", e);
                        break;
                    }
                }
            }
        });

        Ok(())
    }
}
