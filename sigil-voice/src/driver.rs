use sigil_discord::SigilSession;
use tokio::net::UdpSocket;
use tracing::info;

use crate::gateway::{ProtocolData, SelectProtocol, SessionDescription, VoiceGatewayClient};
use crate::udp::{receive_ip_discovery, send_ip_discovery};

pub struct CoreDriver {
    pub gateway: VoiceGatewayClient,
    pub udp: UdpSocket,
    pub sigil: SigilSession,
    pub heartbeat_interval: f64,
    pub mode: Option<String>,
    pub secret_key: Option<Vec<u8>>,
}

impl CoreDriver {
    /// Connects to Discord Voice, performs the WS handshake, completes UDP Hole Punching,
    /// and establishes the final transport session keys.
    pub async fn connect(
        endpoint: &str,
        server_id: &str,
        user_id: &str,
        session_id: &str,
        token: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // 1. Initialize SigilSession for DAVE End-to-End Encryption
        let sigil = SigilSession::new(user_id.parse()?)?;

        // 2. Connect to WS and Handshake
        let mut gateway = VoiceGatewayClient::connect(endpoint).await?;
        let (ready, heartbeat_interval) = gateway
            .handshake(server_id, user_id, session_id, token)
            .await?;

        // 3. Bind local UDP socket
        let udp = UdpSocket::bind("0.0.0.0:0").await?;

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
            if packet.op == 4 {
                let session_desc: SessionDescription = serde_json::from_value(packet.d.unwrap())?;
                info!(
                    "Received SessionDescription from Voice Gateway. Mode: {}",
                    session_desc.mode
                );
                mode = Some(session_desc.mode);
                secret_key = Some(session_desc.secret_key);
                break;
            }
        }

        Ok(Self {
            gateway,
            udp,
            sigil,
            heartbeat_interval,
            mode,
            secret_key,
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
        let mut encoder = crate::audio::AudioEncoder::new()?;
        let mut seq = 0u16;
        let mut timestamp = 0u32;
        let secret_key = self.secret_key.clone().ok_or("No secret key negotiated")?;
        let target = format!("{}:{}", target_ip, target_port);

        // 960 samples per channel (stereo) = 1920 i16s per 20ms frame
        let mut opus_buf = [0u8; 4000];

        while let Some(pcm_frame) = pcm_rx.recv().await {
            // 1. Encode PCM to Opus
            let opus_len = encoder.encode_pcm(&pcm_frame, &mut opus_buf)?;
            let opus_data = &opus_buf[..opus_len];

            // 2. Encrypt Opus via DAVE (SigilSession)
            let dave_ciphertext = self
                .sigil
                .encrypt_own_frame(opus_data, sigil_discord::crypto::codec::Codec::Opus)?;

            // 3. Build RTP Header
            let rtp_header = crate::udp::build_rtp_header(seq, timestamp, ssrc);

            // 4. Transport Encrypt via RTPSIZE AES-256-GCM
            let udp_payload = crate::udp::transport_encrypt_rtpsize(&secret_key, &rtp_header, &dave_ciphertext)
                .map_err(|_| "AES-GCM transport encryption failed")?;

            // 5. Send to Discord Voice Server
            self.udp.send_to(&udp_payload, &target).await?;

            seq = seq.wrapping_add(1);
            timestamp = timestamp.wrapping_add(960);
        }

        Ok(())
    }
}
