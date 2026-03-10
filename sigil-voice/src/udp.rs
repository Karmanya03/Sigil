use tokio::net::UdpSocket;
use tracing::debug;

/// Sends the 74-byte IP Discovery packet to Discord's UDP socket.
/// Format:
/// - 2 bytes: Type (0x00 0x01)
/// - 2 bytes: Length (70)
/// - 4 bytes: SSRC
/// - 66 bytes: 0-padding
pub async fn send_ip_discovery(
    socket: &UdpSocket,
    server_ip: &str,
    server_port: u16,
    ssrc: u32,
) -> Result<(), std::io::Error> {
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
/// Discord sends back our public IP and the UDP port we are routing from.
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

/// Standard RTP Header used by Discord:
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
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Key, Nonce,
};

/// Encrypts an RTP block using `aead_aes256_gcm_rtpsize` mode.
///
/// This is the **transport-level** encryption between bot and Discord's SFU.
/// Separate from DAVE E2EE frame encryption.
///
/// - Nonce: A 32-bit incrementing integer, **big-endian** padded to 96 bits.
///   Discord expects BE for transport nonces (unlike DAVE frame nonces which are LE).
/// - AAD: The 12-byte RTP header.
/// - Output: `[12-byte RTP Header] + [Encrypted Payload + 16-byte MAC Tag] + [4-byte Nonce BE]`
pub fn transport_encrypt_rtpsize(
    key: &[u8],
    rtp_header: &[u8; 12],
    payload: &[u8],
    nonce_counter: u32,
) -> Result<Vec<u8>, aes_gcm::Error> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));

    // Transport nonce: 4 bytes big-endian at the end of 12-byte nonce
    let mut nonce_bytes = [0u8; 12];
    nonce_bytes[8..12].copy_from_slice(&nonce_counter.to_be_bytes());
    let nonce = Nonce::from_slice(&nonce_bytes);

    let aead_payload = Payload {
        msg: payload,
        aad: rtp_header,
    };

    let ciphertext = cipher.encrypt(nonce, aead_payload)?;

    let mut out = Vec::with_capacity(12 + ciphertext.len() + 4);
    out.extend_from_slice(rtp_header);
    out.extend_from_slice(&ciphertext);
    out.extend_from_slice(&nonce_counter.to_be_bytes()); // Append nonce suffix for receiver
    Ok(out)
}

/// Decrypts an RTP block using `aead_aes256_gcm_rtpsize` mode.
///
/// Expects: `[12-byte RTP Header] + [Encrypted Payload + 16-byte MAC Tag] + [4-byte Nonce BE]`
pub fn transport_decrypt_rtpsize(key: &[u8], packet: &[u8]) -> Result<Vec<u8>, aes_gcm::Error> {
    // Minimum: 12 (header) + 16 (tag) + 4 (nonce) = 32 bytes
    if packet.len() < 12 + 16 + 4 {
        return Err(aes_gcm::Error);
    }

    let rtp_header = &packet[0..12];
    let nonce_bytes_suffix = &packet[packet.len() - 4..];
    let encrypted_payload = &packet[12..packet.len() - 4];

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));

    let mut nonce_bytes = [0u8; 12];
    nonce_bytes[8..12].copy_from_slice(nonce_bytes_suffix);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let aead_payload = Payload {
        msg: encrypted_payload,
        aad: rtp_header,
    };

    cipher.decrypt(nonce, aead_payload)
}
