// sentiric-telecom-client-sdk/src/stun.rs

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::timeout;

const STUN_BINDING_REQUEST: u16 = 0x0001;
const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

pub struct StunClient;

impl StunClient {
    /// Mevcut bir soketi kullanarak Public IP/Port keşfi yapar.
    pub async fn discover_public_addr(socket: &UdpSocket, stun_server: &str) -> Option<SocketAddr> {
        let mut request = Vec::with_capacity(20);

        // 1. STUN Header (20 bytes)
        request.extend_from_slice(&STUN_BINDING_REQUEST.to_be_bytes()); // Type
        request.extend_from_slice(&0u16.to_be_bytes()); // Length (No attributes)
        request.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes()); // Magic Cookie

        // 12 byte Transaction ID (Rastgele)
        let transaction_id: [u8; 12] = rand::random();
        request.extend_from_slice(&transaction_id);

        // 2. İsteği Gönder
        if socket.send_to(&request, stun_server).await.is_err() {
            return None;
        }

        // 3. Yanıtı Bekle (Timeout: 500ms - Hızlı olmalı)
        let mut buf = [0u8; 512];
        let Ok(Ok((len, _))) =
            timeout(Duration::from_millis(500), socket.recv_from(&mut buf)).await
        else {
            return None;
        };

        Self::parse_xor_address(&buf[..len])
    }

    fn parse_xor_address(data: &[u8]) -> Option<SocketAddr> {
        if data.len() < 20 {
            return None;
        }

        // Header validasyonu (Cevap tipi 0x0101 olmalı)
        let msg_type = u16::from_be_bytes([data[0], data[1]]);
        if msg_type != 0x0101 {
            return None;
        }

        let mut pos = 20;
        while pos + 4 <= data.len() {
            let attr_type = u16::from_be_bytes([data[pos], data[pos + 1]]);
            let attr_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
            pos += 4;

            if attr_type == ATTR_XOR_MAPPED_ADDRESS && pos + attr_len <= data.len() {
                // XOR Mapped Address formatı: [Reserved(1), Family(1), XOR-Port(2), XOR-IP(4/16)]
                let family = data[pos + 1];
                let x_port = u16::from_be_bytes([data[pos + 2], data[pos + 3]]);

                // Portu çöz (XOR with high 16 bits of Magic Cookie)
                let port = x_port ^ (STUN_MAGIC_COOKIE >> 16) as u16;

                if family == 0x01 {
                    // IPv4
                    let mut ip_bytes = [0u8; 4];
                    for i in 0..4 {
                        // IP'yi çöz (XOR with Magic Cookie)
                        ip_bytes[i] = data[pos + 4 + i] ^ (STUN_MAGIC_COOKIE.to_be_bytes()[i]);
                    }
                    return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip_bytes)), port));
                }
            }
            pos += attr_len;
        }
        None
    }
}
