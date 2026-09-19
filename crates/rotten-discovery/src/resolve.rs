use std::net::ToSocketAddrs;
use std::time::Duration;

use plist::Value;
use rotten_core::debug_log::{agent_log, format_host_for_url};
use rotten_core::device::{AirPlayDevice, DeviceFeatures};
use rotten_core::error::{Result, RottenError};

/// Resolve a manual target (hostname or IP, optional port) into an `AirPlayDevice`.
pub async fn resolve_device(target: &str, port: u16) -> Result<AirPlayDevice> {
    let (host, port) = parse_target(target, port)?;

    let addr = format!("{host}:{port}");

    // #region agent log
    agent_log(
        "resolve.rs:resolve_device",
        "dns lookup starting",
        "H17",
        serde_json::json!({ "addr": &addr, "host": &host }),
    );
    // #endregion

    let socket_addrs: Vec<std::net::SocketAddr> = match tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking({
            let addr = addr.clone();
            move || -> std::io::Result<Vec<_>> { addr.to_socket_addrs().map(|i| i.collect()) }
        }),
    )
    .await
    {
        Ok(Ok(Ok(addrs))) => addrs,
        Ok(Ok(Err(e))) if host.ends_with(".local") => Vec::new(),
        Ok(Ok(Err(e))) => {
            return Err(RottenError::DeviceNotFound(format!(
                "cannot resolve {addr}: {e}"
            )));
        }
        Ok(Err(e)) => {
            return Err(RottenError::DeviceNotFound(format!(
                "DNS task failed for {addr}: {e}"
            )));
        }
        Err(_) if host.ends_with(".local") => Vec::new(),
        Err(_) => {
            return Err(RottenError::DeviceNotFound(format!(
                "DNS timeout resolving {addr}"
            )));
        }
    };

    let resolved_ip = socket_addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| socket_addrs.first())
        .map(|a| a.ip());

    let resolved_ip = match resolved_ip {
        Some(ip) => ip,
        None => resolve_via_mdns(&host, port).await.ok_or_else(|| {
            RottenError::DeviceNotFound(format!(
                "cannot resolve {host}:{port} (DNS and mDNS failed; try the IP from `cermin discover`)"
            ))
        })?,
    };

    let resolved_host = format_host_for_url(&resolved_ip.to_string());

    // #region agent log
    agent_log(
        "resolve.rs:resolve_device",
        "resolved target",
        "A",
        serde_json::json!({
            "input": target,
            "inputHost": host,
            "resolvedHost": resolved_host,
            "resolvedIp": resolved_ip.to_string(),
            "isIpv6": resolved_ip.is_ipv6(),
            "addrCount": socket_addrs.len(),
            "viaMdns": socket_addrs.is_empty(),
        }),
    );
    // #endregion

    let (info, info_status) = fetch_device_info(&resolved_host, port).await;
    let mut features = plist_features(&info);
    let (display_w, display_h) = plist_display_size(&info);

    if features.raw == 0 {
        if let Some(mdns_features) = enrich_features_from_mdns(&resolved_host, port).await {
            features = mdns_features;
        }
    }

    let device = AirPlayDevice {
        name: plist_string(&info, "name").unwrap_or_else(|| host.clone()),
        host: resolved_host,
        port,
        device_id: plist_string(&info, "deviceID")
            .or_else(|| plist_string(&info, "deviceid"))
            .unwrap_or_else(|| host.clone()),
        model: plist_string(&info, "model"),
        features,
        display_width: display_w,
        display_height: display_h,
        pi: plist_string(&info, "pi"),
        pk: info
            .as_ref()
            .and_then(|dict| dict.get("pk"))
            .and_then(plist_value_to_pk_string),
    };

    // #region agent log
    agent_log(
        "resolve.rs:resolve_device",
        "device info fetched",
        "H81",
        serde_json::json!({
            "infoHttpStatus": info_status,
            "featuresRaw": format!("0x{:x}", device.features.raw),
            "fairplaySap": device.features.supports_fairplay_sap(),
            "displayW": device.display_width,
            "displayH": device.display_height,
            "model": device.model,
        }),
    );
    // #endregion

    Ok(device)
}

fn plist_string(dict: &Option<plist::Dictionary>, key: &str) -> Option<String> {
    dict.as_ref()
        .and_then(|d| d.get(key))
        .and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        })
}

fn plist_uint_value(value: &Value) -> Option<u64> {
    match value {
        Value::Integer(i) => i
            .as_unsigned()
            .or_else(|| i.as_signed().map(|v| v.max(0) as u64)),
        Value::Real(f) if *f >= 0.0 => Some(*f as u64),
        _ => None,
    }
}

fn plist_features(dict: &Option<plist::Dictionary>) -> DeviceFeatures {
    let Some(dict) = dict else {
        return DeviceFeatures::default();
    };
    if let Some(v) = dict.get("features") {
        match v {
            Value::String(s) => return DeviceFeatures::from_hex(s),
            Value::Integer(_) | Value::Real(_) => {
                if let Some(raw) = plist_uint_value(v) {
                    return DeviceFeatures { raw };
                }
            }
            _ => {}
        }
    }
    DeviceFeatures::default()
}

fn plist_display_size(dict: &Option<plist::Dictionary>) -> (Option<u32>, Option<u32>) {
    let Some(dict) = dict else {
        return (None, None);
    };
    let Some(displays) = dict.get("displays").and_then(|v| v.as_array()) else {
        return (None, None);
    };
    let Some(first) = displays.first().and_then(|v| v.as_dictionary()) else {
        return (None, None);
    };
    let w = first
        .get("widthPixels")
        .or_else(|| first.get("width"))
        .and_then(plist_uint_value)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v > 0);
    let h = first
        .get("heightPixels")
        .or_else(|| first.get("height"))
        .and_then(plist_uint_value)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v > 0);
    (w, h)
}

async fn enrich_features_from_mdns(host: &str, port: u16) -> Option<DeviceFeatures> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};

    let daemon = ServiceDaemon::new().ok()?;
    let receiver = daemon.browse("_airplay._tcp.local.").ok()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let host_lower = host.to_ascii_lowercase();

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let event = tokio::time::timeout(remaining, receiver.recv_async())
            .await
            .ok()?
            .ok()?;
        if let ServiceEvent::ServiceResolved(info) = event {
            let info_host = info
                .get_hostname()
                .trim_end_matches('.')
                .to_ascii_lowercase();
            let matches = info_host == host_lower
                || info
                    .get_addresses()
                    .iter()
                    .any(|ip| ip.to_string().eq_ignore_ascii_case(host));
            if matches && info.get_port() == port {
                if let Some(f) = info.get_properties().get("features") {
                    let features = DeviceFeatures::from_hex(f.val_str());
                    if features.raw != 0 {
                        return Some(features);
                    }
                }
            }
        }
    }
    None
}

fn plist_value_to_pk_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Data(d) if d.len() == 32 => Some(hex::encode(d)),
        _ => None,
    }
}

fn parse_target(target: &str, default_port: u16) -> Result<(String, u16)> {
    if let Some((host, port_str)) = target.rsplit_once(':') {
        if let Ok(port) = port_str.parse::<u16>() {
            return Ok((host.to_string(), port));
        }
    }
    Ok((target.to_string(), default_port))
}

/// Resolve `.local` hostnames via mDNS when system DNS fails (common on WSL).
async fn resolve_via_mdns(host: &str, port: u16) -> Option<std::net::IpAddr> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};

    let daemon = ServiceDaemon::new().ok()?;
    let receiver = daemon.browse("_airplay._tcp.local.").ok()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let host_lower = host.trim_end_matches('.').to_ascii_lowercase();

    let mut resolved = None;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let event = tokio::time::timeout(remaining, receiver.recv_async())
            .await
            .ok()?
            .ok()?;
        if let ServiceEvent::ServiceResolved(info) = event {
            if info.get_port() != port {
                continue;
            }
            let info_host = info
                .get_hostname()
                .trim_end_matches('.')
                .to_ascii_lowercase();
            let matches = info_host == host_lower
                || info
                    .get_fullname()
                    .to_ascii_lowercase()
                    .contains(&host_lower.trim_end_matches(".local"));
            if !matches {
                continue;
            }
            resolved = info
                .get_addresses()
                .iter()
                .find(|ip| ip.is_ipv4())
                .copied()
                .or_else(|| info.get_addresses().iter().next().copied());
            if resolved.is_some() {
                break;
            }
        }
    }

    let _ = daemon.shutdown();

    // #region agent log
    agent_log(
        "resolve.rs:resolve_via_mdns",
        "mDNS hostname resolution",
        "H120",
        serde_json::json!({
            "host": host,
            "port": port,
            "resolvedIp": resolved.map(|ip| ip.to_string()),
            "found": resolved.is_some(),
        }),
    );
    // #endregion

    resolved
}

async fn fetch_device_info(host: &str, port: u16) -> (Option<plist::Dictionary>, u16) {
    let url = format!("http://{}:{}/info", format_host_for_url(host), port);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(_) => return (None, 0),
    };

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return (None, 0),
    };
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        return (None, status);
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => return (None, status),
    };
    let value: Value = match plist::from_bytes(&bytes) {
        Ok(v) => v,
        Err(_) => return (None, status),
    };
    match value {
        Value::Dictionary(dict) => (Some(dict), status),
        _ => (None, status),
    }
}
