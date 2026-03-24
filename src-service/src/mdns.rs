use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::HashMap;

pub const SERVICE_PORT: u16 = 7788;
const SERVICE_TYPE: &str = "_notetreetlm._tcp.local.";

pub fn start_mdns_broadcast(
    spki_pin: &str,
) -> Result<ServiceDaemon, Box<dyn std::error::Error>> {
    let mdns = ServiceDaemon::new()?;

    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "notetreetlm-host".to_string());

    // instance name：讓使用者看得懂
    let instance_name = format!("noteTreeLM on {}", hostname);

    let mut properties: HashMap<String, String> = HashMap::new();
    properties.insert("spki_pin".to_string(), spki_pin.to_string());
    properties.insert(
        "version".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    );

    // mdns-sd 0.18: host_name must end with ".local."
    let host_name = if hostname.ends_with(".local") {
        format!("{}.", hostname)
    } else {
        format!("{}.local.", hostname)
    };

    let service = ServiceInfo::new(
        SERVICE_TYPE,
        &instance_name,
        &host_name,
        (),           // IP 由 mdns-sd 自動偵測
        SERVICE_PORT,
        properties,
    )?;

    mdns.register(service)?;
    tracing::info!(
        "mDNS broadcast started: {} on port {}",
        instance_name,
        SERVICE_PORT
    );

    Ok(mdns)
}
