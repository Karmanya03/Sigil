use tokio::net::UdpSocket;
use tracing::{debug, error};

/// Sends the 74-byte IP Discovery packet to Discord's UDP socket.
/// Format:
/// - 2 bytes: Type (0x00 0x01)
/// - 2 bytes: Length (70)
/// - 4 bytes: SSRC
/// - 66 bytes: 0-padding
pub async fn send_ip_discovery(socket: &UdpSocket, server_ip: &str, server_port: u16, ssrc: u32) -> Result<(), std::io::Error> {
    let mut packet = [0u8; 74];
    packet[0] = 0x00;
    packet[1] = 0x01; // Type = 1 (Request)
    packet[2] = 0x00;
    packet[3] = 70;   // Length = 70 bytes of data after header
    packet[4..8].copy_from_slice(&ssrc.to_be_bytes());

    let target = format!("{}:{}", server_ip, server_port);
    socket.send_to(&packet, &target).await?;
    Ok(())
}

/// Parses the 74-byte IP Discovery response block.
/// discord sends back our public IP and the UDP port we are routing from.
pub async fn receive_ip_discovery(socket: &UdpSocket) -> Result<(String, u16), std::io::Error> {
    let mut buf = [0u8; 74];
    socket.recv_from(&mut buf).await?;

    // The IP string starts at offset 8, and is null-terminated
    let mut ip_end = 8;
    while ip_end < 72 && buf[ip_end] != 0 {
        ip_end += 1;
    }

    let ip = String::from_utf8_lossy(&buf[8..ip_end]).into_owned();
    let port = u16::from_be_bytes([buf[72], buf[73]]);

    debug!("IP Discovery complete: IP={}, Port={}", ip, port);
    Ok((ip, port))
}

/// standard RTP Header used by Discord:
/// - Version 2, no padding, no extensions, no CSRC
/// - Payload type: 0x78 (120) for Opus
pub fn build_rtp_header(seq: u16, timestamp: u32, ssrc: u32) -> [u8; 12] {
    let mut header = [0u8; 12];
    header[0] = 0x80; // V=2
    header[1] = 0x78; // PT=120
    header[2..4].copy_from_slice(&seq.to_be_bytes());
    header[4..8].copy_from_slice(&timestamp.to_be_bytes());
    header[8..12].copy_from_slice(&ssrc.to_be_bytes());
    header
}

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce, Key,
};

/// Encrypts an RTP block using `aead_aes256_gcm_rtpsize` mode.
/// - Nonce: The 12-byte RTP header.
/// - Plaintext: The inner payload (which in DAVE is the DAVE ciphertext).
/// - Output: [12-byte RTP Header] + [Encrypted Payload + 16-byte MAC Tag]
pub fn transport_encrypt_rtpsize(key: &[u8], rtp_header: &[u8; 12], payload: &[u8]) -> Result<Vec<u8>, aes_gcm::Error> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(rtp_header);
    
    // In rtpsize mode, the header is NOT AAD, it's just the nonce.
    // The payload is encrypted outright.
    let ciphertext = cipher.encrypt(nonce, payload)?;

    let mut out = Vec::with_capacity(12 + ciphertext.len());
    out.extend_from_slice(rtp_header);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypts an RTP block using `aead_aes256_gcm_rtpsize` mode.
/// Returns the decrypted inner payload.
pub fn transport_decrypt_rtpsize(key: &[u8], packet: &[u8]) -> Result<Vec<u8>, aes_gcm::Error> {
    if packet.len() < 12 + 16 {
        return Err(aes_gcm::Error); // Too short to contain Header + Tag
    }

    let rtp_header = &packet[0..12];
    let encrypted_payload = &packet[12..];

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(rtp_header);

    cipher.decrypt(nonce, encrypted_payload)
}
