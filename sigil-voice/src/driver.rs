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
    pub ws_tx_channel: mpsc::Sender<VoicePacket>,
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
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<VoicePacket>(100);
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
                        if let Ok(text) = serde_json::to_string(&cmd) {
                            if ws_tx.send(tokio_tungstenite::tungstenite::Message::Text(text.into())).await.is_err() {
                                break;
                            }
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
                                            DaveEvent::MlsWelcome(w) => {
                                                let _ = s.join_group(&w.welcome_bytes);
                                            }
                                            DaveEvent::MlsAnnounceCommitTransition(c) => {
                                                let _ = s.process_commit(&c.commit_bytes);
                                            }
                                            DaveEvent::PrepareEpoch(p) => {
                                                if p.epoch == 1 {
                                                    // Send KeyPackage (OP 26)
                                                    if let Ok(kp) = s.generate_key_package() {
                                                        let mut payload = vec![0u8; 3];
                                                        payload[2] = 26; // OP 26 MlsKeyPackage
                                                        payload.extend_from_slice(&kp);
                                                        let _ = ws_tx.send(tokio_tungstenite::tungstenite::Message::Binary(payload.into())).await;
                                                    }
                                                }
                                            }
                                            _ => {
                                                tracing::debug!("Parsed DAVE opcode: {:?}", opcode);
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
        })
    }

    /// Continuously reads 20ms PCM audio frames from the channel,
    /// processes them via the Audio Pipeline (Opus -> DAVE -> RTP -> Transport),
    /// and dispatches via UDP to the Voice server.
    pub async fn play_pcm_stream(
        &mut self,
        mut pcm_rx: tokio::sync::mpsc::Receiver<Vec<i16>>,
        ssrc: u32,
        target_ip: &str,
        target_port: u16,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Send OP 5 Speaking Before Audio Payload Dispatch
        let speaking = crate::gateway::Speaking { speaking: 1, delay: 0, ssrc };
        self.ws_tx_channel.send(VoicePacket {
            op: 5,
            d: Some(serde_json::to_value(speaking)?),
            s: None, t: None, seq_ack: None,
        }).await?;

        let mut encoder = crate::audio::AudioEncoder::new()?;
        let mut seq = 0u16;
        let mut timestamp = 0u32;
        let mut nonce_counter = 0u32;
        let secret_key = self.secret_key.clone().ok_or("No secret key negotiated")?;
        let target = format!("{}:{}", target_ip, target_port);

        // 960 samples per channel (stereo) = 1920 i16s per 20ms frame
        let mut opus_buf = [0u8; 4000];

        while let Some(pcm_frame) = pcm_rx.recv().await {
            // 1. Encode PCM to Opus
            let opus_len = encoder.encode_pcm(&pcm_frame, &mut opus_buf)?;
            let opus_data = &opus_buf[..opus_len];

            // 2. Encrypt Opus via DAVE (SigilSession)
            let dave_ciphertext = {
                let mut sigil = self.sigil.lock().await;
                sigil.encrypt_own_frame(opus_data, sigil_discord::crypto::codec::Codec::Opus)?
            };

            // 3. Build RTP Header
            let rtp_header = crate::udp::build_rtp_header(seq, timestamp, ssrc);

            // 4. Transport Encrypt via RTPSIZE AES-256-GCM
            let udp_payload = crate::udp::transport_encrypt_rtpsize(&secret_key, &rtp_header, &dave_ciphertext, nonce_counter)
                .map_err(|_| "AES-GCM transport encryption failed")?;

            // 5. Send to Discord Voice Server
            self.udp.send_to(&udp_payload, &target).await?;

            seq = seq.wrapping_add(1);
            timestamp = timestamp.wrapping_add(960);
            nonce_counter = nonce_counter.wrapping_add(1);
        }

        // 6. Send 5 frames of Silence (0xF8 0xFF 0xFE) to gracefully terminate playback
        let silence_opus = [0xF8, 0xFF, 0xFE];
        for _ in 0..5 {
            let dave_ciphertext = {
                let mut sigil = self.sigil.lock().await;
                sigil.encrypt_own_frame(&silence_opus, sigil_discord::crypto::codec::Codec::Opus)?
            };
            let rtp_header = crate::udp::build_rtp_header(seq, timestamp, ssrc);
            let udp_payload = crate::udp::transport_encrypt_rtpsize(&secret_key, &rtp_header, &dave_ciphertext, nonce_counter)
                .map_err(|_| "AES-GCM transport encryption failed")?;
            self.udp.send_to(&udp_payload, &target).await?;

            seq = seq.wrapping_add(1);
            timestamp = timestamp.wrapping_add(960);
            nonce_counter = nonce_counter.wrapping_add(1);
        }

        Ok(())
    }
}
