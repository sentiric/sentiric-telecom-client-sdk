// sentiric-telecom-client-sdk/src/utils.rs

use std::net::SocketAddr;

/// SDP içinden c= ve m= satırlarını okuyarak gerçek RTP hedefini bulur.
pub fn extract_rtp_target(sdp_body: &[u8], default_ip: &str) -> Option<SocketAddr> {
    let sdp_str = String::from_utf8_lossy(sdp_body);
    let mut ip = default_ip.to_string();
    let mut port = 0u16;

    for line in sdp_str.lines() {
        if line.starts_with("c=IN IP4 ") {
            let parsed_ip = line[9..].trim();
            // 0.0.0.0 gelirse default IP'yi kullanmaya devam et
            if parsed_ip != "0.0.0.0" {
                ip = parsed_ip.to_string();
            }
        } else if line.starts_with("m=audio ") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() > 1 {
                port = parts[1].parse().unwrap_or(0);
            }
        }
    }

    if port > 0 {
        if let Ok(addr) = format!("{}:{}", ip, port).parse() {
            return Some(addr);
        }
    }
    None
}