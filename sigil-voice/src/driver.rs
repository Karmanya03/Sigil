use futures_util::{SinkExt, StreamExt};
use sigil_discord::SigilSession;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex, Notify};
use tracing::{info, warn, error, debug};

use crate::gateway::{
    ProtocolData, SelectProtocol, SessionDescription,
    VoiceGatewayClient, VoicePacket,
};
use crate::udp::{receive_ip_discovery, send_ip_discovery};

/// Opus silence frame — 3 bytes that decode to 20 ms of silence.
const OPUS_SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];

/// Maximum consecutive DAVE encrypt failures before logging a warning batch.
const DAVE_FAIL_LOG_INTERVAL: u64 = 500;

/// Maximum seconds to wait for DAVE key exchange before fallback.
const DAVE_TIMEOUT_SECS: u64 = 10;

/// Maximum seconds to wait for SessionDescription (OP 4).
const SESSION_DESC_TIMEOUT_SECS: u64 = 10;

/// Maximum consecutive missed heartbeat ACKs before considering the connection zombied.
const MAX_MISSED_HEARTBEATS: u8 = 3;

/// How often to send a keepalive silence frame when idle (every 5 seconds = 250 ticks).
const IDLE_KEEPALIVE_TICKS: u64 = 250;

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
    pub receiver_tx: Option<mpsc::Sender<(u64, Vec<i16>)>>,
    pub decoders: Arc<Mutex<std::collections::HashMap<u64, crate::audio::AudioDecoder>>>,
    pub dave_ready: Arc<AtomicBool>,
    pub dave_notify: Arc<Notify>,
    pub ws_alive: Arc<AtomicBool>,
    pub frames_sent: Arc<AtomicU64>,
    pub shutdown: Arc<AtomicBool>,
}

impl CoreDriver {
    pub async fn connect(
        endpoint: &str,
        server_id: &str,
        user_id: &str,
        session_id: &str,
        token: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        info!("⚡ Connecting [endpoint={}, server={}, user={}]", endpoint, server_id, user_id);

        // ── 1. SigilSession ───────────────────────────────────────────────
        let user_id_u64: u64 = user_id
            .parse()
            .map_err(|_| format!("Invalid user_id: {}", user_id))?;

        let sigil = Arc::new(Mutex::new(
            SigilSession::new(user_id_u64)
                .map_err(|e| format!("SigilSession::new: {:?}", e))?,
        ));
        let dave_ready = Arc::new(AtomicBool::new(false));
        let dave_notify = Arc::new(Notify::new());
        let ws_alive = Arc::new(AtomicBool::new(true));
        let shutdown = Arc::new(AtomicBool::new(false));
        info!("✅ 1/7 SigilSession created");

        // ── 2. WS connect + handshake ─────────────────────────────────────
        let mut gateway = VoiceGatewayClient::connect(endpoint).await?;
        info!("✅ 2a/7 WS connected");

        let (ready, heartbeat_interval) = gateway
            .handshake(server_id, user_id, session_id, token)
            .await?;
        info!("✅ 2b/7 WS handshake [SSRC={}, {}:{}]",
            ready.ssrc, ready.ip, ready.port);

        // ── 3. UDP bind ───────────────────────────────────────────────────
        let udp = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
        info!("✅ 3/7 UDP bound {:?}", udp.local_addr());

        // ── 4. IP Discovery ───────────────────────────────────────────────
        let target_addr = format!("{}:{}", ready.ip, ready.port);
        send_ip_discovery(&udp, &ready.ip, ready.port, ready.ssrc).await?;
        let (external_ip, external_port) = receive_ip_discovery(&udp).await?;
        info!("✅ 4/7 IP discovery [{}:{}]", external_ip, external_port);

        // ── 5. Select Protocol ────────────────────────────────────────────
        gateway.send_packet(1, SelectProtocol {
            protocol: "udp".to_string(),
            data: ProtocolData {
                address: external_ip,
                port: external_port,
                mode: "aead_aes256_gcm_rtpsize".to_string(),
            },
        }).await?;
        info!("✅ 5/7 SelectProtocol sent");

        // ── 6. Wait for SessionDescription (OP 4) ─────────────────────────
        let session_desc = tokio::time::timeout(
            std::time::Duration::from_secs(SESSION_DESC_TIMEOUT_SECS),
            async {
                loop {
                    match gateway.recv_packet().await?.ok_or("WS closed")? {
                        crate::gateway::WsMessage::Text(p) if p.op == 4 => {
                            let desc: SessionDescription = serde_json::from_value(
                                p.d.ok_or("OP4 missing d")?
                            )?;
                            return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(desc);
                        }
                        crate::gateway::WsMessage::Text(p) => {
                            debug!("Skipping OP {} while waiting for OP 4", p.op);
                        }
                        crate::gateway::WsMessage::Binary(b) => {
                            debug!("Skipping binary ({} bytes) while waiting for OP 4", b.len());
                        }
                    }
                }
            }
        ).await
        .map_err(|_| "Timed out waiting for SessionDescription")?
        .map_err(|e| format!("SessionDescription error: {:?}", e))?;

        if session_desc.secret_key.len() != 32 {
            return Err(format!(
                "Wrong secret_key length: {} (expected 32 for aead_aes256_gcm_rtpsize)",
                session_desc.secret_key.len()
            ).into());
        }
        info!("✅ 6/7 SessionDescription [mode={}, key=32 bytes]", session_desc.mode);

        let mode = Some(session_desc.mode);
        let secret_key = Some(session_desc.secret_key);

        // ── 7. Background WS task (heartbeats + DAVE) ─────────────────────
        let (mut ws_tx, mut ws_rx) = gateway.ws.split();
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<crate::gateway::WsMessage>(100);
        let ssrc_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let ssrc_map_clone = ssrc_map.clone();
        let sigil_clone = sigil.clone();
        let dave_ready_clone = dave_ready.clone();
        let dave_notify_clone = dave_notify.clone();
        let ws_alive_clone = ws_alive.clone();
        let shutdown_clone = shutdown.clone();
        let my_ssrc = ready.ssrc;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                std::time::Duration::from_millis(heartbeat_interval as u64)
            );
            interval.tick().await;
            let mut seq_ack: Option<u64> = None;
            let mut binary_seq: u16 = 0;
            let mut hb_nonce: u64 = 0;
            let mut hb_acked = true;
            let mut missed_acks: u8 = 0;
            let mut encryption_ready_sent = false;
            info!("🔄 WS background task started (hb={}ms)", heartbeat_interval);

            macro_rules! send_bin {
                ($op:expr, $data:expr) => {{
                    let mut buf = vec![0u8; 3];
                    buf[0..2].copy_from_slice(&binary_seq.to_be_bytes());
                    binary_seq = binary_seq.wrapping_add(1);
                    buf[2] = $op;
                    buf.extend_from_slice($data);
                    if ws_tx.send(
                        tokio_tungstenite::tungstenite::Message::Binary(buf.into())
                    ).await.is_err() {
                        warn!("Binary send failed — WS dropped");
                        break;
                    }
                }};
            }

            macro_rules! send_encryption_ready {
                        () => {
                            if !encryption_ready_sent {
                                // OP 12 EncryptionReady — MUST be binary DAVE frame
                                // Wire format: [seq_hi][seq_lo][12][payload...]
                                // Payload: little-endian audio_ssrc (4 bytes)
                                let mut er_buf: Vec<u8> = Vec::new();
                                er_buf.extend_from_slice(&(my_ssrc as u32).to_le_bytes());
                                send_bin!(12, &er_buf);
                                encryption_ready_sent = true;
                                info!("DAVE: → OP 12 EncryptionReady (binary, ssrc={})", my_ssrc);
                            }
                        }}


            macro_rules! mark_dave_ready {
                () => {{
                    if !dave_ready_clone.load(Ordering::Relaxed) {
                        dave_ready_clone.store(true, Ordering::Release);
                        dave_notify_clone.notify_waiters();
                    }
                }};
            }

            loop {
                if shutdown_clone.load(Ordering::Relaxed) {
                    info!("WS task: shutdown requested");
                    break;
                }

                tokio::select! {
                    // ── Heartbeat ────────────────────────────────────────
                    _ = interval.tick() => {
                        if !hb_acked {
                            missed_acks += 1;
                            warn!("⚠️ Heartbeat not ACKed (missed {}×)", missed_acks);
                            if missed_acks >= MAX_MISSED_HEARTBEATS {
                                error!("💀 {} consecutive heartbeats missed — connection is dead, closing", MAX_MISSED_HEARTBEATS);
                                break;
                            }
                        } else {
                            missed_acks = 0;
                        }

                        hb_nonce = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;
                        let mut hb_d = serde_json::json!({"t": hb_nonce});
                        if let Some(sa) = seq_ack {
                            hb_d["seq_ack"] = serde_json::json!(sa);
                        }
                        let hb = VoicePacket {
                            op: 3,
                            d: Some(hb_d),
                            s: None, t: None, seq_ack: None,
                        };
                        if let Ok(txt) = serde_json::to_string(&hb) {
                            if ws_tx.send(
                                tokio_tungstenite::tungstenite::Message::Text(txt.into())
                            ).await.is_err() {
                                warn!("Heartbeat send failed — WS dropped");
                                break;
                            }
                        }
                        hb_acked = false;
                    }

                    // ── Outbound command from driver ──────────────────────
                    cmd = cmd_rx.recv() => {
                        let Some(cmd) = cmd else {
                            info!("Command channel closed — WS task exiting");
                            break;
                        };
                        let msg = match cmd {
                            crate::gateway::WsMessage::Text(p) => {
                                match serde_json::to_string(&p) {
                                    Ok(t) => tokio_tungstenite::tungstenite::Message::Text(t.into()),
                                    Err(_) => continue,
                                }
                            }
                            crate::gateway::WsMessage::Binary(b) =>
                                tokio_tungstenite::tungstenite::Message::Binary(b.into()),
                        };
                        if ws_tx.send(msg).await.is_err() {
                            warn!("Command send failed — WS dropped");
                            break;
                        }
                    }

                    // ── Inbound WS message ────────────────────────────────
                    msg = ws_rx.next() => {
                        let Some(Ok(msg)) = msg else {
                            warn!("WS read ended — background task exiting");
                            break;
                        };

                        match msg {
                            // ══════════════════════════════════════════════
                            // TEXT (JSON opcodes)
                            // ══════════════════════════════════════════════
                            tokio_tungstenite::tungstenite::Message::Text(text) => {
                                let Ok(pkt) = serde_json::from_str::<VoicePacket>(&text) else {
                                    continue;
                                };
                                if let Some(seq) = pkt.s {
                                    seq_ack = Some(seq);
                                }

                                match pkt.op {
                                    6 => {
                                        hb_acked = true;
                                        if let Some(d) = &pkt.d {
                                            let ack_nonce = d.get("t")
                                                .and_then(|v| v.as_u64())
                                                .or_else(|| d.as_u64());
                                            if let Some(n) = ack_nonce {
                                                if n != hb_nonce {
                                                    warn!("Heartbeat ACK nonce mismatch: sent={}, got={}", hb_nonce, n);
                                                }
                                            }
                                        }
                                        debug!("Heartbeat ACK");
                                    }

                                    5 => {
                                        if let Some(d) = pkt.d {
                                            if let Ok(spk) = serde_json::from_value::<
                                                crate::gateway::Speaking>(d)
                                            {
                                                if let Some(uid_str) = spk.user_id {
                                                    if let Ok(uid) = uid_str.parse::<u64>() {
                                                        ssrc_map_clone.lock().await
                                                            .insert(spk.ssrc, uid);
                                                        debug!("SSRC {} → UserId {}", spk.ssrc, uid);
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // ── OP 21: PrepareTransition (JSON) ───
                                    21 => {
                                        if let Some(d) = pkt.d {
                                            use sigil_discord::gateway::opcodes::PrepareTransition;
                                            match serde_json::from_value::<PrepareTransition>(d) {
                                                Ok(pt) => {
                                                    info!("DAVE: ← OP 21 PrepareTransition (tid={}, proto={})",
                                                        pt.transition_id, pt.protocol_version);
                                                    // OP 23 ReadyForTransition — MUST be binary DAVE frame
                                                    // Wire format: [seq_hi][seq_lo][23][tid_lo][tid_hi] (2-byte LE transition_id)
                                                    send_bin!(23, &(pt.transition_id as u16).to_le_bytes());
                                                    info!("DAVE: → OP 23 ReadyForTransition (binary, tid={})", pt.transition_id);
                                                }
                                                Err(e) => {
                                                    error!("DAVE: Failed to parse PrepareTransition: {:?}", e);
                                                }
                                            }
                                        }
                                    }

                                    // ── OP 22: ExecuteTransition (JSON) ───
                                    22 => {
                                        if let Some(d) = pkt.d {
                                            use sigil_discord::gateway::opcodes::ExecuteTransition;
                                            match serde_json::from_value::<ExecuteTransition>(d) {
                                                Ok(et) => {
                                                    info!("DAVE: ← OP 22 ExecuteTransition (tid={})", et.transition_id);
                                                    encryption_ready_sent = false;
                                                    info!("DAVE: Reset encryption_ready_sent flag for new epoch");
                                                }
                                                Err(e) => {
                                                    error!("DAVE: Failed to parse ExecuteTransition: {:?}", e);
                                                }
                                            }
                                        }
                                    }

                                    // ── OP 24: PrepareEpoch (JSON) ────────
                                    // Only send KeyPackage on epoch == 1.
                                    24 => {
                                        if let Some(d) = pkt.d {
                                            use sigil_discord::gateway::opcodes::PrepareEpoch;
                                            match serde_json::from_value::<PrepareEpoch>(d) {
                                                Ok(pe) => {
                                                    info!("DAVE: ← OP 24 PrepareEpoch (epoch={}, proto={})",
                                                        pe.epoch, pe.protocol_version);
                                                    if pe.epoch == 1 {
                                                        let s = sigil_clone.lock().await;
                                                        match s.generate_key_package() {
                                                            Ok(kp) => {
                                                                info!("DAVE: Generated KeyPackage ({} bytes) for epoch reset", kp.len());
                                                                send_bin!(26, &kp);
                                                                info!("DAVE: → OP 26 KeyPackage sent (epoch=1 reset)");
                                                            }
                                                            Err(e) => error!("DAVE: KeyPackage generation failed: {:?}", e),
                                                        }
                                                    } else {
                                                        info!("DAVE: Skipping KeyPackage for epoch={} (only required on epoch=1)", pe.epoch);
                                                    }
                                                }
                                                Err(e) => {
                                                    error!("DAVE: Failed to parse PrepareEpoch: {:?}", e);
                                                }
                                            }
                                        }
                                    }

                                    9 => { info!("Voice WS resumed"); }
                                    13 => { debug!("Client connected (OP 13)"); }
                                    18 => { debug!("Client disconnected (OP 18)"); }
                                    _ => debug!("Text OP {}", pkt.op),
                                }
                            }

                            // ══════════════════════════════════════════════
                            // BINARY (DAVE opcodes 25, 27, 29, 30)
                            // ══════════════════════════════════════════════
                            tokio_tungstenite::tungstenite::Message::Binary(bin) => {
                                if bin.len() < 3 { continue; }
                                let opcode = bin[2];
                                info!("DAVE: ← Binary OP {} ({} bytes)", opcode, bin.len());

                                let mut s = sigil_clone.lock().await;
                                let event = match s.handle_gateway_event(opcode, &bin) {
                                    Ok(e) => e,
                                    Err(e) => {
                                        error!("DAVE: OP {} parse error: {:?}", opcode, e);
                                        continue;
                                    }
                                };

                                use sigil_discord::gateway::handler::DaveEvent;
                                match event {
                                    // ── OP 25: MlsExternalSender ─────────
                                    DaveEvent::MlsExternalSender(ext) => {
                                        info!("DAVE: Processing MlsExternalSender (credential {} bytes)", ext.credential.len());
                                        match s.set_external_sender(&ext.credential) {
                                            Ok(()) => {
                                                info!("DAVE: ✅ MLS group created with external sender");
                                                info!("DAVE: Group established (sole member): {}", s.is_established());
                                            }
                                            Err(e) => {
                                                error!("DAVE: ❌ set_external_sender failed: {:?}", e);
                                                continue;
                                            }
                                        }
                                    }

                                    // ── OP 27: MlsProposals ──────────────
                                    DaveEvent::MlsProposals { operation_type: _operation_type, transition_id, proposals } => {
                                        info!("DAVE: Processing MlsProposals ({} proposals, tid={})", proposals.len(), transition_id);
                                        if proposals.is_empty() {
                                            info!("DAVE: Empty proposal data, acknowledging");
                                            continue;
                                        }

                                        match s.process_proposals(&proposals) {
                                            Ok(_needs_commit) => {
                                                info!("DAVE: ✅ Proposals processed successfully");
                                                let has_pending = s.has_pending_proposals();
                                                info!("DAVE: Has pending proposals: {}", has_pending);

                                                if has_pending {
                                                    info!("DAVE: Committing proposals...");
                                                    match s.commit_and_welcome() {
                                                        Ok((commit, welcome)) => {
                                                            let mut payload = (transition_id as u16)
                                                                .to_le_bytes()
                                                                .to_vec();
                                                            payload.extend_from_slice(&commit);
                                                            let has_welcome = welcome.is_some();
                                                            if let Some(w) = welcome {
                                                                payload.extend_from_slice(&w);
                                                            }
                                                            info!("DAVE: Generated commit ({} bytes, welcome={}, tid={})", payload.len(), has_welcome, transition_id);
                                                            send_bin!(28, &payload);
                                                            info!("DAVE: → OP 28 CommitWelcome sent (tid={})", transition_id);

                                                            let member_ids = s.group_member_ids();
                                                            info!("DAVE: Exporting keys for {} group members (post-commit)", member_ids.len());
                                                            match s.export_sender_keys(&member_ids) {
                                                                Ok(keys) if keys.contains_key(&s.user_id) => {
                                                                    info!("DAVE: ✅ All member keys exported (post-commit, {} keys)", keys.len());
                                                                    mark_dave_ready!();
                                                                }
                                                                Ok(keys) => {
                                                                    error!("DAVE: ❌ Missing own key after commit (exported {} keys)", keys.len());
                                                                }
                                                                Err(e) => error!("DAVE: ❌ Export failed after commit: {:?}", e),
                                                            }
                                                        }
                                                        Err(e) => {
                                                            error!("DAVE: ❌ commit_and_welcome failed: {:?}", e);
                                                        }
                                                    }
                                                } else {
                                                    // No processable proposals: send self-update commit
                                                    info!("DAVE: No processable proposals — sending self-update commit with tid={} to unblock epoch", transition_id);
                                                    match s.commit_and_welcome() {
                                                        Ok((commit, welcome)) => {
                                                            let mut payload = (transition_id as u16)
                                                                .to_le_bytes()
                                                                .to_vec();
                                                            payload.extend_from_slice(&commit);
                                                            if let Some(w) = welcome {
                                                                payload.extend_from_slice(&w);
                                                            }
                                                            info!("DAVE: Sending OP 28 self-update ({} bytes, tid={})", payload.len(), transition_id);
                                                            send_bin!(28, &payload);
                                                            info!("DAVE: → OP 28 self-update CommitWelcome sent (tid={})", transition_id);

                                                            // Send OP 23 ReadyForTransition after self-update commit
                                                            send_bin!(23, &(transition_id as u16).to_le_bytes());
                                                            info!("DAVE: → OP 23 ReadyForTransition (binary, self-update tid={})", transition_id);

                                                            let member_ids = s.group_member_ids();
                                                            match s.export_sender_keys(&member_ids) {
                                                                Ok(keys) if keys.contains_key(&s.user_id) => {
                                                                    info!("DAVE: ✅ Keys exported after self-update ({} keys)", keys.len());
                                                                    mark_dave_ready!();
                                                                    send_encryption_ready!();
                                                                }
                                                                Ok(keys) => {
                                                                    info!("DAVE: Keys exported ({} keys), waiting for OP 29", keys.len());
                                                                }
                                                                Err(e) => {
                                                                    warn!("DAVE: Key export after self-update failed: {:?}", e);
                                                                }
                                                            }
                                                        }
                                                        Err(e) => {
                                                            warn!("DAVE: Self-update commit failed: {:?} — acknowledging anyway", e);
                                                        }
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                error!("DAVE: ❌ process_proposals failed: {:?}", e);
                                            }
                                        }
                                    }

                                    // ── OP 30: MlsWelcome ───────────────
                                    DaveEvent::MlsWelcome(w) => {
                                        info!("DAVE: Processing MlsWelcome (tid={}, {} bytes)", w.transition_id, w.welcome_bytes.len());
                                        match s.join_group(&w.welcome_bytes) {
                                            Ok(()) => {
                                                info!("DAVE: ✅ Joined group via Welcome");
                                                info!("DAVE: Group established: {}", s.is_established());
                                            }
                                            Err(e) => {
                                                error!("DAVE: ❌ join_group failed: {:?}", e);
                                                continue;
                                            }
                                        }

                                        let member_ids = s.group_member_ids();
                                        info!("DAVE: Exporting keys for {} group members (Welcome)", member_ids.len());
                                        match s.export_sender_keys(&member_ids) {
                                            Ok(keys) if keys.contains_key(&s.user_id) => {
                                                info!("DAVE: ✅ All member keys exported (Welcome, {} keys)", keys.len());
                                                mark_dave_ready!();
                                            }
                                            Ok(keys) => {
                                                error!("DAVE: ❌ Missing own key after welcome (exported {} keys)", keys.len());
                                            }
                                            Err(e) => error!("DAVE: ❌ Export failed after welcome: {:?}", e),
                                        }

                                        send_bin!(23, &(w.transition_id as u16).to_le_bytes());
                                        info!("DAVE: → OP 23 ReadyForTransition (binary, welcome tid={})", w.transition_id);

                                        send_encryption_ready!();
                                    }

                                    // ── OP 29: MlsAnnounceCommitTransition
                                    DaveEvent::MlsAnnounceCommitTransition(c) => {
                                        info!("DAVE: Processing MlsAnnounceCommitTransition (tid={}, {} bytes)",
                                            c.transition_id, c.commit_bytes.len());
                                        match s.process_commit(&c.commit_bytes) {
                                            Ok(epoch) => info!("DAVE: ✅ Commit processed successfully, new epoch={}", epoch),
                                            Err(e) => {
                                                error!("DAVE: ❌ process_commit failed: {:?}", e);
                                            }
                                        }

                                        send_bin!(23, &(c.transition_id as u16).to_le_bytes());
                                        info!("DAVE: → OP 23 ReadyForTransition (binary, commit tid={})", c.transition_id);

                                        let member_ids = s.group_member_ids();
                                        info!("DAVE: Refreshing keys for {} members after epoch advance", member_ids.len());
                                        if let Ok(keys) = s.export_sender_keys(&member_ids) {
                                            if keys.contains_key(&s.user_id) {
                                                info!("DAVE: ✅ All member keys refreshed (epoch advance, {} keys)", keys.len());
                                                mark_dave_ready!();
                                            } else {
                                                error!("DAVE: ❌ Missing own key after epoch advance (exported {} keys)", keys.len());
                                            }
                                        }

                                        send_encryption_ready!();
                                    }
                                }
                            }

                            tokio_tungstenite::tungstenite::Message::Ping(data) => {
                                let _ = ws_tx.send(
                                    tokio_tungstenite::tungstenite::Message::Pong(data)
                                ).await;
                            }

                            tokio_tungstenite::tungstenite::Message::Close(frame) => {
                                if let Some(f) = frame {
                                    warn!("Voice WS closed: code={}, reason={}", f.code, f.reason);
                                } else {
                                    warn!("Voice WS closed (no close frame)");
                                }
                                break;
                            }

                            _ => {}
                        }
                    }
                }
            }

            ws_alive_clone.store(false, Ordering::Release);
            warn!("WS background task ended — ws_alive set to false");
        });

        info!("✅ 7/7 CoreDriver ready");
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
            dave_ready,
            dave_notify,
            ws_alive,
            frames_sent: Arc::new(AtomicU64::new(0)),
            shutdown,
        })
    }

    pub async fn add_track(&self, track: crate::track::Track) -> crate::track::TrackHandle {
        let handle = crate::track::TrackHandle::new(track);
        self.tracks.lock().await.push(handle.clone());
        info!("🎵 Track added");
        handle
    }

    pub async fn stop(&self) {
        let mut tracks = self.tracks.lock().await;
        info!("⏹️ Stopping {} track(s)", tracks.len());
        tracks.clear();
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.ws_alive.store(false, Ordering::Release);
    }

    pub fn is_ws_alive(&self) -> bool {
        self.ws_alive.load(Ordering::Acquire)
    }

    /// Audio engine loop — 20ms tick.
    ///
    /// When tracks are active: mix PCM → Opus encode → DAVE encrypt → transport encrypt → UDP send
    /// When idle (no tracks): send a silence frame every 5 seconds to keep the session alive
    pub async fn start_mixing(
        &self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let secret_key = self.secret_key.clone()
            .ok_or("No secret key")?;

        let mut encoder = crate::audio::AudioEncoder::new()
            .map_err(|e| format!("AudioEncoder::new: {:?}", e))?;
        info!("🎙️ AudioEncoder created");

        // ── Wait for DAVE readiness ───────────────────────────────────────
        info!("🔒 Waiting for DAVE key...");
        let dave_established = if self.dave_ready.load(Ordering::Acquire) {
            true
        } else {
            tokio::time::timeout(
                std::time::Duration::from_secs(DAVE_TIMEOUT_SECS),
                async {
                    loop {
                        if self.dave_ready.load(Ordering::Acquire) {
                            return;
                        }
                        if !self.ws_alive.load(Ordering::Acquire) {
                            return;
                        }
                        tokio::select! {
                            _ = self.dave_notify.notified() => {
                                if self.dave_ready.load(Ordering::Acquire) {
                                    return;
                                }
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
                        }
                    }
                }
            ).await.is_ok()
        };

        // FIX: Check WS liveness UNCONDITIONALLY after DAVE wait
        if !self.ws_alive.load(Ordering::Acquire) {
            error!("🛑 WS died during DAVE negotiation — aborting mixing loop");
            return Err("WS connection lost before mixing could start".into());
        }

        if dave_established {
            info!("✅ DAVE ready — audio will be E2EE encrypted");
        } else {
            let has_key = {
                let s = self.sigil.lock().await;
                s.has_own_key()
            };
            if has_key {
                info!("✅ DAVE key exists (late arrival) — proceeding with E2EE");
            } else {
                warn!("⚠️ DAVE not ready after {}s — sending raw Opus (no E2EE)", DAVE_TIMEOUT_SECS);
            }
        }

        // ── OP 5 Speaking — MUST be sent before first RTP packet ─────────
        let speaking = crate::gateway::Speaking {
            speaking: 1, delay: 0, ssrc: self.ssrc, user_id: None,
        };
        if let Ok(d) = serde_json::to_value(&speaking) {
            let _ = self.ws_tx_channel.send(crate::gateway::WsMessage::Text(VoicePacket {
                op: 5, d: Some(d), s: None, t: None, seq_ack: None,
            })).await;
            info!("🎙️ Sent OP 5 Speaking (SSRC={})", self.ssrc);
        }

        info!("🎙️ Mixing loop started → {}", self.target_addr);

        let mut seq: u16 = 0;
        let mut timestamp: u32 = 0;
        let mut nonce_counter: u32 = 0;
        let mut opus_buf = [0u8; 4000];
        let mut frames_sent: u64 = 0;
        let mut dave_failures: u64 = 0;
        let mut was_active = false;
        let mut silence_sent: u8 = 0;
        let mut idle_tick_count: u64 = 0;

        let mut mixed = vec![0i32; 1920];

        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(20));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            if !self.ws_alive.load(Ordering::Acquire) {
                warn!("🛑 WS connection lost — stopping mixing loop (sent {} frames)", frames_sent);
                break;
            }

            if self.shutdown.load(Ordering::Relaxed) {
                info!("🛑 Shutdown requested — stopping mixing loop");
                break;
            }

            // ── 1. Mix PCM from all active tracks ─────────────────────────
            for s in mixed.iter_mut() { *s = 0; }
            let mut active = false;

            let mut tracks = self.tracks.lock().await;
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
                            active = true;
                            let vol = t.volume;
                            for (i, &s) in frame.iter().enumerate() {
                                if i < mixed.len() {
                                    mixed[i] += (s as f32 * vol) as i32;
                                }
                            }
                        }
                        None => {
                            t.state.store(
                                crate::track::PlayState::Stopped as u8,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            if let Some(tx) = &t.event_tx {
                                let _ = tx.send(crate::track::TrackEvent::End);
                            }
                        }
                    }
                } else {
                    active = true;
                }
            }

            // ═══════════════════════════════════════════════════════════════
            // IDLE KEEPALIVE: No tracks active → send silence every 5s
            // ═══════════════════════════════════════════════════════════════
            if !active && !was_active {
                idle_tick_count += 1;

                if idle_tick_count % IDLE_KEEPALIVE_TICKS == 1 {
                    let rtp_hdr = crate::udp::build_rtp_header(seq, timestamp, self.ssrc);
                    let payload = {
                        let mut s = self.sigil.lock().await;
                        if s.has_own_key() {
                            match s.encrypt_own_frame(&OPUS_SILENCE, sigil_discord::crypto::codec::Codec::Opus) {
                                Ok(ct) => ct,
                                Err(_) => OPUS_SILENCE.to_vec(),
                            }
                        } else {
                            OPUS_SILENCE.to_vec()
                        }
                    };
                    if let Ok(pkt) = crate::udp::transport_encrypt_rtpsize(
                        &secret_key, &rtp_hdr, &payload, nonce_counter,
                    ) {
                        let _ = self.udp.send_to(&pkt, &self.target_addr).await;
                    }
                    seq = seq.wrapping_add(1);
                    timestamp = timestamp.wrapping_add(960);
                    nonce_counter = nonce_counter.wrapping_add(1);
                    if idle_tick_count <= 1 {
                        info!("💤 Idle keepalive started — sending silence every 5s");
                    }
                }

                continue;
            }

            // ── Transition: active → idle: send 5 silence frames ──────────
            if !active && was_active && silence_sent < 5 {
                let rtp_hdr = crate::udp::build_rtp_header(seq, timestamp, self.ssrc);
                let payload = {
                    let mut s = self.sigil.lock().await;
                    if s.has_own_key() {
                        match s.encrypt_own_frame(&OPUS_SILENCE, sigil_discord::crypto::codec::Codec::Opus) {
                            Ok(ct) => ct,
                            Err(_) => OPUS_SILENCE.to_vec(),
                        }
                    } else {
                        OPUS_SILENCE.to_vec()
                    }
                };
                if let Ok(pkt) = crate::udp::transport_encrypt_rtpsize(
                    &secret_key, &rtp_hdr, &payload, nonce_counter,
                ) {
                    let _ = self.udp.send_to(&pkt, &self.target_addr).await;
                }
                seq = seq.wrapping_add(1);
                timestamp = timestamp.wrapping_add(960);
                nonce_counter = nonce_counter.wrapping_add(1);
                silence_sent += 1;

                if silence_sent == 5 {
                    info!("🔇 Sent 5 silence frames (audio stopped cleanly)");
                    let stop_speaking = crate::gateway::Speaking {
                        speaking: 0, delay: 0, ssrc: self.ssrc, user_id: None,
                    };
                    if let Ok(d) = serde_json::to_value(&stop_speaking) {
                        let _ = self.ws_tx_channel.send(crate::gateway::WsMessage::Text(VoicePacket {
                            op: 5, d: Some(d), s: None, t: None, seq_ack: None,
                        })).await;
                    }
                    was_active = false;
                    idle_tick_count = 0;
                }

                continue;
            }

            if !active {
                was_active = false;
                idle_tick_count = 0;
                continue;
            }

            // ── Transition: idle → active: reset and re-announce Speaking ─
            if active && !was_active {
                silence_sent = 0;
                idle_tick_count = 0;
                let resume_speaking = crate::gateway::Speaking {
                    speaking: 1, delay: 0, ssrc: self.ssrc, user_id: None,
                };
                if let Ok(d) = serde_json::to_value(&resume_speaking) {
                    let _ = self.ws_tx_channel.send(crate::gateway::WsMessage::Text(VoicePacket {
                        op: 5, d: Some(d), s: None, t: None, seq_ack: None,
                    })).await;
                    info!("🎙️ Re-sent OP 5 Speaking (resumed)");
                }
            }
            was_active = active;

            let pcm: Vec<i16> = mixed.iter()
                .map(|&s| s.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
                .collect();

            if frames_sent % 500 == 0 {
                let avg: i64 = pcm.iter().map(|&s| s.abs() as i64).sum::<i64>()
                    / pcm.len().max(1) as i64;
                info!("🎙️ Amplitude avg={} frames_sent={}", avg, frames_sent);
            }

            // ── 2. Opus encode ─────────────────────────────────────────────
            let opus_len = match encoder.encode_pcm(&pcm, &mut opus_buf) {
                Ok(n) => n,
                Err(e) => { warn!("Opus encode error: {:?}", e); continue; }
            };
            let opus = &opus_buf[..opus_len];

            // ── 3. DAVE encrypt (or raw fallback) ──────────────────────────
            let audio_payload: Vec<u8> = {
                let mut s = self.sigil.lock().await;
                if s.has_own_key() {
                    match s.encrypt_own_frame(opus, sigil_discord::crypto::codec::Codec::Opus) {
                        Ok(ct) => {
                            dave_failures = 0;
                            ct
                        }
                        Err(e) => {
                            dave_failures += 1;
                            if dave_failures == 1 || dave_failures % DAVE_FAIL_LOG_INTERVAL == 0 {
                                warn!("DAVE encrypt failed ({}×): {:?}", dave_failures, e);
                            }
                            opus.to_vec()
                        }
                    }
                } else {
                    opus.to_vec()
                }
            };

            // ── 4. Transport encrypt (AES-256-GCM rtpsize) ────────────────
            let rtp_hdr = crate::udp::build_rtp_header(seq, timestamp, self.ssrc);
            let udp_pkt = match crate::udp::transport_encrypt_rtpsize(
                &secret_key, &rtp_hdr, &audio_payload, nonce_counter,
            ) {
                Ok(p) => p,
                Err(e) => { warn!("Transport encrypt failed: {:?}", e); continue; }
            };

            // ── 5. UDP send ────────────────────────────────────────────────
            if let Err(e) = self.udp.send_to(&udp_pkt, &self.target_addr).await {
                warn!("UDP send failed: {:?}", e);
                continue;
            }

            frames_sent += 1;
            self.frames_sent.store(frames_sent, Ordering::Relaxed);
            match frames_sent {
                1 => info!("🔊 First frame sent!"),
                50 => info!("🔊 1 second of audio sent"),
                2500 => info!("🔊 50 seconds of audio sent"),
                _ => {}
            }

            seq = seq.wrapping_add(1);
            timestamp = timestamp.wrapping_add(960);
            nonce_counter = nonce_counter.wrapping_add(1);
        }

        info!("🎙️ Mixing loop exited (total frames: {})", frames_sent);
        Ok(())
    }

    /// Background UDP receiver for incoming audio.
    pub async fn start_receiver(
        &self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let udp = self.udp.clone();
        let sigil = self.sigil.clone();
        let ssrc_map = self.ssrc_map.clone();
        let decoders = self.decoders.clone();
        let ws_alive = self.ws_alive.clone();
        let shutdown = self.shutdown.clone();
        let receiver_tx = self.receiver_tx.clone()
            .ok_or("No receiver channel configured")?;
        let secret_key = self.secret_key.clone()
            .ok_or("No secret key")?;

        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut pcm_out = [0i16; 1920];

            loop {
                if !ws_alive.load(Ordering::Acquire) || shutdown.load(Ordering::Relaxed) {
                    info!("Receiver loop: WS dead or shutdown — exiting");
                    break;
                }

                let (n, _) = match udp.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(e) => { error!("UDP recv error: {:?}", e); break; }
                };

                if n < 12 + 16 + 4 { continue; }

                let pkt = &buf[..n];
                let ssrc = u32::from_be_bytes([pkt[8], pkt[9], pkt[10], pkt[11]]);

                let decrypted = match crate::udp::transport_decrypt_rtpsize(&secret_key, pkt) {
                    Ok(d) => d,
                    Err(_) => continue,
                };

                let uid = {
                    *ssrc_map.lock().await.get(&ssrc).unwrap_or(&0)
                };
                if uid == 0 { continue; }

                let dave_plain = {
                    let s = sigil.lock().await;
                    match s.decrypt_from_sender(uid, &decrypted) {
                        Ok(d) => d,
                        Err(_) => continue,
                    }
                };

                let mut decs = decoders.lock().await;
                let dec = decs.entry(uid).or_insert_with(|| {
                    crate::audio::AudioDecoder::new().expect("AudioDecoder::new")
                });
                if let Ok(n) = dec.decode_opus(&dave_plain, &mut pcm_out) {
                    let _ = receiver_tx.send((uid, pcm_out[..n].to_vec())).await;
                }
            }
        });

        Ok(())
    }
}
