// Dosya: sentiric-telecom-client-sdk/src/auth.rs

use std::collections::HashMap;

pub fn generate_auth_header(
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
    www_authenticate: &str,
) -> Option<String> {
    let header_clean = www_authenticate.trim();
    if !header_clean.to_lowercase().starts_with("digest ") {
        return None;
    }

    let mut map = HashMap::new();
    let parts = header_clean[7..].split(',');
    for part in parts {
        if let Some((k, v)) = part.split_once('=') {
            map.insert(k.trim(), v.trim().trim_matches('"'));
        }
    }

    let realm = map.get("realm")?;
    let nonce = map.get("nonce")?;

    // HA1 = MD5(username:realm:password)
    let ha1_str = format!("{}:{}:{}", username, realm, password);
    let ha1 = format!("{:x}", md5::compute(ha1_str.as_bytes()));

    // HA2 = MD5(method:digestURI)
    let ha2_str = format!("{}:{}", method, uri);
    let ha2 = format!("{:x}", md5::compute(ha2_str.as_bytes()));

    // Response = MD5(HA1:nonce:HA2)
    let resp_str = format!("{}:{}:{}", ha1, nonce, ha2);
    let response = format!("{:x}", md5::compute(resp_str.as_bytes()));

    Some(format!(
        "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
        username, realm, nonce, uri, response
    ))
}
