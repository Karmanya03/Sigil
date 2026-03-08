use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoicePacket {
    pub op: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub d: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identify {
    pub server_id: String,
    pub user_id: String,
    pub session_id: String,
    pub token: String,
    pub max_dave_protocol_version: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectProtocol {
    pub protocol: String,
    pub data: ProtocolData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolData {
    pub address: String,
    pub port: u16,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ready {
    pub ssrc: u32,
    pub ip: String,
    pub port: u16,
    pub modes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDescription {
    pub mode: String,
    pub secret_key: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub heartbeat_interval: f64,
}

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tracing::{info, warn};

pub struct VoiceGatewayClient {
    pub ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl VoiceGatewayClient {
    /// Connects to the Discord Voice WebSocket Endpoint (v8)
    pub async fn connect(endpoint: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("wss://{}/?v=8", endpoint.trim_end_matches(":80"));
        info!("Connecting to Voice Gateway: {}", url);

        let (ws, _) = connect_async(&url).await?;
        Ok(Self { ws })
    }

    /// Read the next parsed JSON VoicePacket from the WebSocket
    pub async fn recv_packet(
        &mut self,
    ) -> Result<Option<VoicePacket>, Box<dyn std::error::Error + Send + Sync>> {
        while let Some(msg) = self.ws.next().await {
            let msg = msg?;
            match msg {
                Message::Text(text) => {
                    let packet: VoicePacket = serde_json::from_str(&text)?;
                    return Ok(Some(packet));
                }
                Message::Close(_) => {
                    warn!("Voice WS closed");
                    return Ok(None);
                }
                _ => {} // Ignore binary/ping/pong for now
            }
        }
        Ok(None)
    }

    /// Serialize and send a VoicePacket to the WebSocket
    pub async fn send_packet(
        &mut self,
        op: u8,
        data: impl Serialize,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let packet = VoicePacket {
            op,
            d: Some(serde_json::to_value(data)?),
            s: None,
            t: None,
        };
        let text = serde_json::to_string(&packet)?;
        self.ws.send(Message::Text(text.into())).await?;
        Ok(())
    }

    /// Performs the initial handshake (Hello -> Identify -> Ready).
    /// Returns the initial Ready packet and the heartbeat interval.
    pub async fn handshake(
        &mut self,
        server_id: &str,
        user_id: &str,
        session_id: &str,
        token: &str,
    ) -> Result<(Ready, f64), Box<dyn std::error::Error + Send + Sync>> {
        // 1. Wait for Hello (OP 8)
        let hello_packet = self
            .recv_packet()
            .await?
            .ok_or("Connection closed before Hello")?;
        if hello_packet.op != 8 {
            return Err(format!("Expected Hello (8), got {}", hello_packet.op).into());
        }
        let hello: Hello = serde_json::from_value(hello_packet.d.unwrap())?;
        info!(
            "Received Hello, heartbeat interval: {}ms",
            hello.heartbeat_interval
        );

        // 2. Send Identify (OP 0)
        let identify = Identify {
            server_id: server_id.to_string(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            token: token.to_string(),
            max_dave_protocol_version: Some(1), // We support DAVE v1.1
        };
        self.send_packet(0, identify).await?;
        info!("Sent Identify");

        // 3. Wait for Ready (OP 2)
        let ready_packet = self
            .recv_packet()
            .await?
            .ok_or("Connection closed before Ready")?;
        if ready_packet.op != 2 {
            return Err(format!("Expected Ready (2), got {}", ready_packet.op).into());
        }
        let ready: Ready = serde_json::from_value(ready_packet.d.unwrap())?;
        info!(
            "Received Ready: IP={} Port={} SSRC={}",
            ready.ip, ready.port, ready.ssrc
        );

        Ok((ready, hello.heartbeat_interval))
    }
}
