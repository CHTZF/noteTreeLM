use rcgen::{generate_simple_self_signed, CertifiedKey};
use sha2::{Digest, Sha256};

pub struct TlsCert {
    pub cert_pem: String,
    pub key_pem: String,
    pub spki_pin: String, // sha256:<hex>
}

pub fn generate_tls_cert(hostnames: Vec<String>) -> Result<TlsCert, Box<dyn std::error::Error>> {
    let CertifiedKey { cert, key_pair } = generate_simple_self_signed(hostnames)?;

    // SPKI fingerprint：對 DER 格式的公鑰做 SHA-256
    let spki_der = key_pair.public_key_der();
    let hash = Sha256::digest(&spki_der);
    let spki_pin = format!("sha256:{}", hex::encode(hash));

    Ok(TlsCert {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
        spki_pin,
    })
}

/// 取得本機所有應該放入 SAN 的 hostname
pub fn collect_san_hostnames() -> Vec<String> {
    let mut names = vec!["localhost".to_string(), "127.0.0.1".to_string()];

    // 本機 mDNS hostname（e.g. macbookpro.local）
    if let Ok(hostname) = hostname::get() {
        let h = hostname.to_string_lossy().to_string();
        // 若不含 .local 後綴則加上
        if !h.ends_with(".local") {
            names.push(format!("{}.local", h));
        } else {
            names.push(h);
        }
    }

    // 本機 LAN IP
    if let Ok(ip) = local_ip_address::local_ip() {
        names.push(ip.to_string());
    }

    names
}
