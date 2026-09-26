use serde::{Deserialize, Serialize};

use crate::error::{Result, RottenError};

/// Parsed feature flags from AirPlay TXT record `features` field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceFeatures {
    pub raw: u64,
}

impl DeviceFeatures {
    /// Parse AirPlay `features` TXT (`0xLOW,0xHIGH` or single hex value).
    pub fn from_hex(hex: &str) -> Self {
        let s = hex.trim();
        if let Some((lo, hi)) = s.split_once(',') {
            let lo = parse_hex_u64(lo);
            let hi = parse_hex_u64(hi);
            return Self {
                raw: (hi << 32) | lo,
            };
        }
        Self {
            raw: parse_hex_u64(s),
        }
    }

    pub fn supports_screen_mirroring(&self) -> bool {
        const FEATURE_SCREEN: u64 = 1 << 8;
        self.raw & FEATURE_SCREEN != 0 || self.raw & 0x80 != 0
    }

    /// FairPlay SAP 2.5 — modern Apple receivers buffer at very low latency.
    pub fn supports_fairplay_sap(&self) -> bool {
        const FEATURE_FP_SAP_25: u64 = 1 << 14;
        self.raw & FEATURE_FP_SAP_25 != 0
    }

    /// PTP clock support (feature bit 41). Samsung TVs/projectors are PTP-only.
    pub fn supports_ptp_clock(&self) -> bool {
        const FEATURE_PTP: u64 = 1 << 41;
        self.raw & FEATURE_PTP != 0
    }

    /// NTP clock support (feature bit 45). Apple TVs advertise both PTP and NTP.
    pub fn supports_ntp_clock(&self) -> bool {
        const FEATURE_NTP: u64 = 1 << 45;
        self.raw & FEATURE_NTP != 0
    }

    /// Timing protocol for SETUP plists. PTP-only receivers (Samsung) stall
    /// silently on NTP; NTP-only receivers reject/ignore PTP.
    pub fn timing_protocol(&self) -> &'static str {
        if self.supports_ptp_clock() && !self.supports_ntp_clock() {
            "PTP"
        } else {
            "NTP"
        }
    }

    /// Minimum playout lead for receivers without robust jitter buffers.
    pub fn playout_latency_floor_ms(&self) -> u64 {
        if self.supports_fairplay_sap() { 0 } else { 500 }
    }
}

fn parse_hex_u64(s: &str) -> u64 {
    let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    u64::from_str_radix(s, 16).unwrap_or(0)
}

/// A discovered or manually specified AirPlay receiver (Apple TV).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AirPlayDevice {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub device_id: String,
    pub model: Option<String>,
    pub features: DeviceFeatures,
    /// Primary display width from `/info` `displays[0]` (presentation size in codec header).
    pub display_width: Option<u32>,
    /// Primary display height from `/info` `displays[0]`.
    pub display_height: Option<u32>,
    pub pi: Option<String>,
    pub pk: Option<String>,
}

impl AirPlayDevice {
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    pub fn base_url(&self) -> String {
        format!(
            "http://{}:{}",
            crate::debug_log::format_host_for_url(&self.host),
            self.port
        )
    }
}

/// A discovered or manually entered Google Cast receiver.
///
/// This is the control endpoint for the experimental Default Media Receiver
/// video path. It is not an AirPlay device and has no HAP pairing identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CastDevice {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub device_id: String,
    pub model: Option<String>,
    /// Raw `ca` capability bitmask from the `_googlecast._tcp` TXT record.
    pub capabilities: Option<u64>,
}

impl CastDevice {
    /// Build a receiver from a manually entered hostname or IP.
    ///
    /// `host` may be an ASCII DNS hostname (labels of letters/digits/`-`, with
    /// an optional final dot) or an IPv4/IPv6 literal (IPv6 may be bracketed).
    /// The port is always explicit: an embedded `host:port` is rejected so the
    /// CLI `--port` flag stays authoritative. Unusable addresses and invalid
    /// numeric names are rejected here instead of failing at connect time. No
    /// DNS or HTTP work happens here.
    pub fn manual(host: &str, port: u16) -> Result<Self> {
        let host = host.trim();
        if host.is_empty() {
            return Err(RottenError::Discovery("Google Cast target is empty".into()));
        }
        if port == 0 {
            return Err(RottenError::Discovery(
                "Google Cast target port must not be 0".into(),
            ));
        }
        if host.chars().any(char::is_whitespace) {
            return Err(RottenError::Discovery(format!(
                "Google Cast target {host:?} contains whitespace"
            )));
        }
        if host.contains("://")
            || host.contains('/')
            || host.contains('\\')
            || host.contains('?')
            || host.contains('#')
        {
            return Err(RottenError::Discovery(format!(
                "Google Cast target {host:?} is not a bare hostname or IP"
            )));
        }

        let host = normalize_manual_host(host)?;
        let name = host.clone();
        // A manual target has no TXT `id`; the host doubles as its identity.
        let device_id = host.clone();
        Ok(Self {
            name,
            host,
            port,
            device_id,
            model: None,
            capabilities: None,
        })
    }
}

fn normalize_manual_host(host: &str) -> Result<String> {
    if let Some(rest) = host.strip_prefix('[') {
        let inner = rest.strip_suffix(']').ok_or_else(|| {
            RottenError::Discovery(format!(
                "Google Cast target {host:?} has an unclosed IPv6 bracket"
            ))
        })?;
        let ip: std::net::Ipv6Addr = inner.parse().map_err(|_| {
            RottenError::Discovery(format!(
                "Google Cast target {host:?} is not a valid IPv6 address"
            ))
        })?;
        reject_unusable_ipv6(&ip, host)?;
        return Ok(ip.to_string());
    }
    if host.contains(':') {
        // Colons are valid only inside IPv6 literals. Everything else is an
        // embedded host:port, which the separate --port option must express.
        if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
            reject_unusable_ipv6(&ip, host)?;
            return Ok(ip.to_string());
        }
        return Err(RottenError::Discovery(format!(
            "Google Cast target {host:?} embeds a port; use the --port option"
        )));
    }

    let numeric_dotted = host.contains('.')
        && host
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.');
    if numeric_dotted {
        // A numeric dotted name that is not a valid IPv4 literal is invalid;
        // it must not silently become a DNS hostname.
        let ip: std::net::Ipv4Addr = host.parse().map_err(|_| {
            RottenError::Discovery(format!(
                "Google Cast target {host:?} is not a valid IPv4 address or hostname"
            ))
        })?;
        if ip.is_unspecified() {
            return Err(RottenError::Discovery(format!(
                "Google Cast target {host:?} does not name a receiver"
            )));
        }
        return Ok(ip.to_string());
    }

    validate_hostname(host)?;
    Ok(host.to_string())
}

/// Reject addresses that cannot reach a LAN receiver.
fn reject_unusable_ipv6(ip: &std::net::Ipv6Addr, input: &str) -> Result<()> {
    if ip.is_unspecified() || ip.is_multicast() {
        return Err(RottenError::Discovery(format!(
            "Google Cast target {input:?} does not name a receiver"
        )));
    }
    if (ip.segments()[0] & 0xffc0) == 0xfe80 {
        return Err(RottenError::Discovery(format!(
            "Google Cast target {input:?} is a link-local IPv6 address without a scope id; use the receiver's IPv4 address"
        )));
    }
    Ok(())
}

/// Validate an ASCII DNS hostname: labels of letters/digits/`-` joined by
/// dots, with an optional final dot.
fn validate_hostname(host: &str) -> Result<()> {
    let trimmed = host.strip_suffix('.').unwrap_or(host);
    let invalid = || {
        RottenError::Discovery(format!(
            "Google Cast target {host:?} is not a valid IP address or hostname"
        ))
    };
    if trimmed.is_empty() || trimmed.len() > 253 {
        return Err(invalid());
    }
    for label in trimmed.split('.') {
        let valid = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        if !valid {
            return Err(invalid());
        }
    }
    Ok(())
}

/// A receiver from either supported protocol.
///
/// Protocol-specific identities stay inside their respective device types:
/// AirPlay uses HAP device IDs/credentials, Google Cast uses TXT `id`s.
#[derive(Debug, Clone)]
pub enum ReceiverDevice {
    AirPlay(AirPlayDevice),
    GoogleCast(CastDevice),
}

impl ReceiverDevice {
    pub fn name(&self) -> &str {
        match self {
            Self::AirPlay(device) => &device.name,
            Self::GoogleCast(device) => &device.name,
        }
    }

    pub fn host(&self) -> &str {
        match self {
            Self::AirPlay(device) => &device.host,
            Self::GoogleCast(device) => &device.host,
        }
    }

    pub fn port(&self) -> u16 {
        match self {
            Self::AirPlay(device) => device.port,
            Self::GoogleCast(device) => device.port,
        }
    }

    pub fn device_id(&self) -> &str {
        match self {
            Self::AirPlay(device) => &device.device_id,
            Self::GoogleCast(device) => &device.device_id,
        }
    }

    pub fn model(&self) -> Option<&str> {
        match self {
            Self::AirPlay(device) => device.model.as_deref(),
            Self::GoogleCast(device) => device.model.as_deref(),
        }
    }

    pub fn protocol_label(&self) -> &'static str {
        match self {
            Self::AirPlay(_) => "AirPlay",
            Self::GoogleCast(_) => "Google Cast",
        }
    }

    pub fn is_cast(&self) -> bool {
        matches!(self, Self::GoogleCast(_))
    }
}

#[cfg(test)]
mod tests {
    use super::{AirPlayDevice, CastDevice, DeviceFeatures, ReceiverDevice};

    #[test]
    fn parses_comma_separated_features() {
        let f = DeviceFeatures::from_hex("0x527feec,0x0");
        assert_ne!(f.raw, 0);
    }

    #[test]
    fn samsung_lsp7_is_ptp_only() {
        let f = DeviceFeatures::from_hex("0x38bcb46007f8ad0");
        assert!(!f.supports_ntp_clock());
        assert!(f.supports_ptp_clock());
        assert_eq!(f.timing_protocol(), "PTP");
    }

    #[test]
    fn apple_tv_prefers_ntp() {
        let f = DeviceFeatures::from_hex("0x527feec,0x0");
        assert_eq!(f.timing_protocol(), "NTP");
    }

    #[test]
    fn manual_cast_target_trims_and_accepts_ipv6_literals() {
        let device = CastDevice::manual("  192.168.1.20  ", 8009).unwrap();
        assert_eq!(device.host, "192.168.1.20");
        assert_eq!(device.name, "192.168.1.20");
        assert_eq!(device.device_id, "192.168.1.20");
        assert_eq!(device.port, 8009);

        let device = CastDevice::manual("[2001:db8::5]", 8009).unwrap();
        assert_eq!(device.host, "2001:db8::5");

        // Bare IPv6 literal: parsed before any host:port interpretation.
        let device = CastDevice::manual("2001:db8::5", 8009).unwrap();
        assert_eq!(device.host, "2001:db8::5");
    }

    #[test]
    fn manual_cast_target_accepts_dns_names_and_ips() {
        assert_eq!(
            CastDevice::manual("tv.local", 8009).unwrap().host,
            "tv.local"
        );
        assert_eq!(
            CastDevice::manual("living-room.local.", 8009).unwrap().host,
            "living-room.local."
        );
        assert_eq!(CastDevice::manual("TV123", 8009).unwrap().host, "TV123");
        assert!(
            CastDevice::manual("192.168.001.020", 8009).is_err(),
            "non-canonical IPv4 must not become a hostname"
        );
        assert_eq!(
            CastDevice::manual("2001:db8::7", 8009).unwrap().host,
            "2001:db8::7"
        );
    }

    #[test]
    fn manual_cast_target_rejects_invalid_hosts_and_unusable_ipv6() {
        for bad in [
            "",
            "   ",
            "http://192.168.1.20",
            "192.168.1.20/path",
            "192.168.1.20:8009",
            "host name",
            "[not-ipv6]",
            "[2001:db8::5]:8009",
            // Characters and shapes that are not IP or DNS hostname labels.
            "bad]",
            "a@b",
            "a\0b",
            "under_score",
            "-leading",
            "trailing-",
            "a..b",
            ".leadingdot",
            "999.1.1.1",
            "1.2.3",
            "0.0.0.0",
            "[::]",
            "::",
            "ff02::1",
            "fe80::1",
            "[fe80::1]",
        ] {
            assert!(
                CastDevice::manual(bad, 8009).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        assert!(CastDevice::manual("192.168.1.20", 0).is_err());
        let long_label = "a".repeat(64);
        assert!(CastDevice::manual(&long_label, 8009).is_err());
        let long_name = vec!["abcdefghij"; 30].join(".");
        assert!(CastDevice::manual(&long_name, 8009).is_err());
    }

    #[test]
    fn link_local_ipv6_error_advises_ipv4() {
        let error = CastDevice::manual("fe80::1", 8009).unwrap_err().to_string();
        assert!(error.contains("IPv4"), "unexpected error: {error}");
    }

    #[test]
    fn receiver_device_keeps_protocol_specific_identity() {
        let airplay = ReceiverDevice::AirPlay(AirPlayDevice {
            name: "Apple TV".into(),
            host: "192.168.1.10".into(),
            port: 7000,
            device_id: "aa:bb:cc".into(),
            model: Some("AppleTV6,2".into()),
            features: DeviceFeatures::default(),
            display_width: None,
            display_height: None,
            pi: None,
            pk: None,
        });
        let cast = ReceiverDevice::GoogleCast(CastDevice {
            name: "Kitchen".into(),
            host: "192.168.1.20".into(),
            port: 8009,
            device_id: "cast-id".into(),
            model: Some("Chromecast".into()),
            capabilities: Some(5),
        });

        assert_eq!(airplay.protocol_label(), "AirPlay");
        assert_eq!(cast.protocol_label(), "Google Cast");
        assert!(!airplay.is_cast());
        assert!(cast.is_cast());
        assert_eq!(airplay.device_id(), "aa:bb:cc");
        assert_eq!(cast.device_id(), "cast-id");
        assert_eq!(airplay.model(), Some("AppleTV6,2"));
        assert_eq!(cast.model(), Some("Chromecast"));
        assert_eq!(cast.host(), "192.168.1.20");
        assert_eq!(cast.port(), 8009);
    }
}
