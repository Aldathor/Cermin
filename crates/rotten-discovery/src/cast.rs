//! Google Cast receiver discovery via mDNS (`_googlecast._tcp.local.`).
//!
//! Only the Default Media Receiver video path is supported. Receivers whose
//! `ca` TXT capability mask is present and has no video bit are skipped; a
//! missing or malformed `ca` is kept because audio-only cannot be assumed.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rotten_core::device::CastDevice;
use rotten_core::error::{Result, RottenError};
use tracing::{debug, info};

const GOOGLE_CAST_SERVICE: &str = "_googlecast._tcp.local.";

/// Cast `ca` capability bit for video output.
const CAP_VIDEO_OUT: u64 = 1 << 0;

/// Stops the mDNS daemon on every exit path, including future cancellation.
struct DaemonGuard(ServiceDaemon);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.shutdown();
    }
}

/// Browse the LAN for Google Cast receivers for up to `timeout`.
pub async fn discover_cast_for(timeout: Duration) -> Result<Vec<CastDevice>> {
    let daemon =
        ServiceDaemon::new().map_err(|e| RottenError::Discovery(format!("mDNS daemon: {e}")))?;
    // Install the guard before the fallible browse: a browse error must not
    // leave the daemon running. Owned for the whole function so dropping a
    // cancelled future still shuts it down; nothing is detached from the caller.
    let daemon = DaemonGuard(daemon);
    let receiver = daemon
        .0
        .browse(GOOGLE_CAST_SERVICE)
        .map_err(|e| RottenError::Discovery(format!("browse: {e}")))?;

    let mut registry = CastRegistry::default();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, receiver.recv_async()).await {
            Ok(Ok(event)) => match event {
                ServiceEvent::ServiceResolved(info) => {
                    let fullname = info.get_fullname().to_string();
                    match cast_device_from_service_info(&info) {
                        Some(device) => {
                            debug!(
                                name = %device.name,
                                host = %device.host,
                                port = device.port,
                                "resolved Google Cast device"
                            );
                            registry.upsert(fullname, device);
                        }
                        None => {
                            debug!(%fullname, "ignoring audio-only Google Cast service");
                            registry.remove_by_fullname(&fullname);
                        }
                    }
                }
                ServiceEvent::ServiceFound(_, fullname) => {
                    debug!(%fullname, "Google Cast service found, resolving...");
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    debug!(%fullname, "Google Cast service removed");
                    registry.remove_by_fullname(&fullname);
                }
                _ => {}
            },
            Ok(Err(e)) => {
                return Err(RottenError::Discovery(format!("recv: {e}")));
            }
            Err(_) => break,
        }
    }

    let list = registry.into_sorted();
    info!(count = list.len(), "Google Cast discovery complete");
    Ok(list)
}

/// Parse a resolved `_googlecast._tcp` service into a receiver.
///
/// Returns `None` only for a known audio-only device (a parseable `ca` without
/// the video bit). Unknown or malformed capability data is kept.
fn cast_device_from_service_info(info: &ServiceInfo) -> Option<CastDevice> {
    let fullname = info.get_fullname().to_string();
    let properties = info.get_properties();

    let capabilities = properties
        .get("ca")
        .and_then(|value| parse_capabilities(value.val_str()));
    if capabilities.is_some_and(|ca| ca & CAP_VIDEO_OUT == 0) {
        return None;
    }

    let name = properties
        .get("fn")
        .map(|value| value.val_str().trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| instance_name(&fullname));
    let model = properties
        .get("md")
        .map(|value| value.val_str().trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let device_id = properties
        .get("id")
        .map(|value| value.val_str().trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fullname.clone());

    Some(CastDevice {
        name,
        host: select_host(info),
        port: info.get_port(),
        device_id,
        model,
        capabilities,
    })
}

/// TXT `ca` is the receiver capability bitmask. Google advertises it as a
/// decimal string (for example `4101` or `5`); an explicit `0x` prefix means
/// hexadecimal. Anything else is unknown, not audio-only.
fn parse_capabilities(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).ok();
    }
    raw.parse::<u64>().ok()
}

fn instance_name(fullname: &str) -> String {
    let name = fullname.split('.').next().unwrap_or(fullname);
    if name.is_empty() {
        fullname.to_string()
    } else {
        name.to_string()
    }
}

/// Prefer IPv4, then a usable IPv6 address, then the advertised hostname.
fn select_host(info: &ServiceInfo) -> String {
    if let Some(ip) = info.get_addresses().iter().filter(|ip| ip.is_ipv4()).min() {
        return ip.to_string();
    }
    if let Some(ip) = info
        .get_addresses()
        .iter()
        .filter_map(|ip| match ip {
            IpAddr::V6(v6) if usable_ipv6(v6) => Some(*v6),
            _ => None,
        })
        .min()
    {
        return ip.to_string();
    }
    info.get_hostname().trim_end_matches('.').to_string()
}

/// Scopeless link-local and mapped addresses are not usable as a Cast host.
fn usable_ipv6(ip: &Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    if ip.to_ipv4_mapped().is_some() {
        return false;
    }
    // fe80::/10 requires a scope id that a bare mDNS address does not carry.
    if (ip.segments()[0] & 0xffc0) == 0xfe80 {
        return false;
    }
    true
}

struct Entry {
    device: CastDevice,
    fullname: String,
}

/// Collects resolved services, deduplicated by TXT `id` (falling back to the
/// service fullname) while removals continue to arrive keyed by fullname.
#[derive(Default)]
struct CastRegistry {
    entries: HashMap<String, Entry>,
    by_fullname: HashMap<String, String>,
}

impl CastRegistry {
    fn upsert(&mut self, fullname: String, device: CastDevice) {
        let key = device.device_id.clone();
        // A repeated resolution of the same fullname with a different id (for
        // example the TXT `id` appeared later) replaces the previous entry;
        // otherwise the old key would linger unreachable by removals.
        if let Some(previous_key) = self.by_fullname.get(&fullname).cloned()
            && previous_key != key
        {
            self.entries.remove(&previous_key);
        }
        if let Some(previous) = self.entries.get(&key)
            && previous.fullname != fullname
        {
            self.by_fullname.remove(&previous.fullname);
        }
        self.by_fullname.insert(fullname.clone(), key.clone());
        self.entries.insert(key, Entry { device, fullname });
    }

    fn remove_by_fullname(&mut self, fullname: &str) {
        if let Some(key) = self.by_fullname.remove(fullname) {
            // A newer resolution may already have replaced this fullname's entry.
            if self
                .entries
                .get(&key)
                .is_some_and(|entry| entry.fullname == fullname)
            {
                self.entries.remove(&key);
            }
        }
    }

    fn into_sorted(self) -> Vec<CastDevice> {
        let mut list: Vec<CastDevice> = self
            .entries
            .into_values()
            .map(|entry| entry.device)
            .collect();
        list.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.device_id.cmp(&b.device_id))
        });
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(
        name: &str,
        host: &str,
        ips: &str,
        port: u16,
        props: &[(&str, &str)],
    ) -> ServiceInfo {
        let properties: HashMap<String, String> = props
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        ServiceInfo::new(GOOGLE_CAST_SERVICE, name, host, ips, port, properties)
            .expect("fixture service info")
    }

    fn cast_device(id: &str, name: &str) -> CastDevice {
        CastDevice {
            name: name.to_string(),
            host: "192.168.1.99".to_string(),
            port: 8009,
            device_id: id.to_string(),
            model: None,
            capabilities: None,
        }
    }

    #[test]
    fn fixture_uses_friendly_name_and_txt_metadata() {
        let info = fixture(
            "Chromecast-A1",
            "chromecast.local.",
            "192.168.1.30",
            8009,
            &[
                ("fn", "Living Room"),
                ("md", "Chromecast Ultra"),
                ("id", "abcd1234"),
                ("ca", "5"),
            ],
        );
        let device = cast_device_from_service_info(&info).expect("device");
        assert_eq!(device.name, "Living Room");
        assert_eq!(device.host, "192.168.1.30");
        assert_eq!(device.port, 8009);
        assert_eq!(device.device_id, "abcd1234");
        assert_eq!(device.model.as_deref(), Some("Chromecast Ultra"));
        assert_eq!(device.capabilities, Some(5));
    }

    #[test]
    fn fixture_falls_back_to_instance_name_and_fullname_id() {
        let info = fixture("Bedroom TV", "bedroom.local.", "192.168.1.31", 8009, &[]);
        let device = cast_device_from_service_info(&info).expect("device");
        assert_eq!(device.name, "Bedroom TV");
        assert_eq!(device.device_id, info.get_fullname());
        // Absent `ca` must not be treated as audio-only.
        assert_eq!(device.capabilities, None);
    }

    #[test]
    fn audio_only_ca_is_excluded_but_malformed_is_kept() {
        let audio_only = fixture(
            "Audio",
            "audio.local.",
            "192.168.1.32",
            8009,
            &[("ca", "0")],
        );
        assert!(cast_device_from_service_info(&audio_only).is_none());

        let no_video_bit = fixture("Odd", "odd.local.", "192.168.1.33", 8009, &[("ca", "4")]);
        assert!(cast_device_from_service_info(&no_video_bit).is_none());

        let malformed = fixture(
            "Unknown",
            "unknown.local.",
            "192.168.1.34",
            8009,
            &[("ca", "zz")],
        );
        let device = cast_device_from_service_info(&malformed).expect("device");
        assert_eq!(device.capabilities, None);
    }

    #[test]
    fn host_selection_prefers_ipv4_then_global_ipv6_then_hostname() {
        let dual = fixture(
            "Dual",
            "dual.local.",
            "192.168.1.35,2001:db8::35",
            8009,
            &[],
        );
        assert_eq!(
            cast_device_from_service_info(&dual).unwrap().host,
            "192.168.1.35"
        );

        let v6 = fixture("V6", "v6.local.", "2001:db8::36", 8009, &[]);
        assert_eq!(
            cast_device_from_service_info(&v6).unwrap().host,
            "2001:db8::36"
        );

        // Scopeless link-local cannot be connected to; fall back to the hostname.
        let link_local = fixture("Link", "link.local.", "fe80::1", 8009, &[]);
        assert_eq!(
            cast_device_from_service_info(&link_local).unwrap().host,
            "link.local"
        );
    }

    #[test]
    fn ca_is_decimal_unless_explicitly_hex() {
        // Google advertises decimal bitmasks; 4101 must not be read as 0x4101.
        assert_eq!(parse_capabilities("4101"), Some(4101));
        assert_eq!(parse_capabilities("5"), Some(5));
        assert_eq!(parse_capabilities("0x5"), Some(5));
        assert_eq!(parse_capabilities("0X0"), Some(0));
        assert_eq!(parse_capabilities(" 4101 "), Some(4101));
        assert_eq!(parse_capabilities("abc"), None);
        assert_eq!(parse_capabilities(""), None);

        let decimal = fixture(
            "Decimal",
            "decimal.local.",
            "192.168.1.40",
            8009,
            &[("ca", "4101")],
        );
        let device =
            cast_device_from_service_info(&decimal).expect("decimal ca keeps video device");
        assert_eq!(device.capabilities, Some(4101));

        let hex = fixture("Hex", "hex.local.", "192.168.1.41", 8009, &[("ca", "0x5")]);
        assert_eq!(
            cast_device_from_service_info(&hex).unwrap().capabilities,
            Some(5)
        );
    }

    #[test]
    fn registry_replaces_old_entry_when_fullname_id_changes() {
        let mut registry = CastRegistry::default();
        registry.upsert(
            "Living Room._googlecast._tcp.local.".into(),
            cast_device("old-id", "Living Room"),
        );
        registry.upsert(
            "Living Room._googlecast._tcp.local.".into(),
            cast_device("new-id", "Living Room"),
        );
        assert_eq!(registry.entries.len(), 1);
        assert!(registry.entries.contains_key("new-id"));
        assert_eq!(
            registry
                .by_fullname
                .get("Living Room._googlecast._tcp.local.")
                .map(String::as_str),
            Some("new-id")
        );
        registry.remove_by_fullname("Living Room._googlecast._tcp.local.");
        assert!(registry.entries.is_empty());
        assert!(registry.by_fullname.is_empty());
    }

    #[test]
    fn registry_dedups_by_id_and_removes_by_fullname() {
        let mut registry = CastRegistry::default();
        registry.upsert(
            "Living Room._googlecast._tcp.local.".into(),
            cast_device("id-1", "Living Room"),
        );
        registry.upsert(
            "Living Room (2)._googlecast._tcp.local.".into(),
            cast_device("id-1", "Living Room"),
        );
        assert_eq!(registry.entries.len(), 1);

        // The stale fullname must not remove the newer resolution.
        registry.remove_by_fullname("Living Room._googlecast._tcp.local.");
        assert_eq!(registry.entries.len(), 1);
        registry.remove_by_fullname("Living Room (2)._googlecast._tcp.local.");
        assert!(registry.entries.is_empty());
        assert!(registry.by_fullname.is_empty());
    }

    #[test]
    fn registry_sorts_by_name_then_id() {
        let mut registry = CastRegistry::default();
        registry.upsert("b".into(), cast_device("z", "Alpha"));
        registry.upsert("a".into(), cast_device("a", "Alpha"));
        registry.upsert("c".into(), cast_device("m", "Beta"));
        let sorted: Vec<(String, String)> = registry
            .into_sorted()
            .into_iter()
            .map(|device| (device.name, device.device_id))
            .collect();
        assert_eq!(
            sorted,
            vec![
                ("Alpha".to_string(), "a".to_string()),
                ("Alpha".to_string(), "z".to_string()),
                ("Beta".to_string(), "m".to_string()),
            ]
        );
    }
}
