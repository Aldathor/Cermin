use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

pub const DEBUG_BUILD_ID: &str = "v0.1.0";

const LOG_FALLBACK: &str = "cermin-debug.log";

fn debug_logging_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("CERMIN_DEBUG_LOG")
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
    })
}

fn log_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(target_os = "windows")]
    {
        paths.push(std::env::temp_dir().join("cermin-debug.log"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join(LOG_FALLBACK));
        }
    }
    paths.push(PathBuf::from(LOG_FALLBACK));
    paths
}

/// Optional developer trace log. Disabled unless `CERMIN_DEBUG_LOG=1`.
pub fn agent_log(location: &str, message: &str, hypothesis_id: &str, data: serde_json::Value) {
    if !debug_logging_enabled() {
        return;
    }

    let line = serde_json::json!({
        "timestamp": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        "location": location,
        "message": message,
        "hypothesisId": hypothesis_id,
        "data": data,
    });
    let payload = format!("{line}\n");
    for path in log_paths() {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(payload.as_bytes());
            let _ = f.flush();
        }
    }
}

/// Format a host for use in an HTTP URL (bracket IPv6 literals).
pub fn format_host_for_url(host: &str) -> String {
    if host.starts_with('[') {
        return host.to_string();
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => v4.to_string(),
            std::net::IpAddr::V6(v6) => format!("[{v6}]"),
        };
    }
    host.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brackets_ipv6() {
        assert_eq!(format_host_for_url("fe80::1"), "[fe80::1]");
        assert_eq!(format_host_for_url("192.168.1.50"), "192.168.1.50");
        assert_eq!(format_host_for_url("Apple-TV.local"), "Apple-TV.local");
    }

    #[test]
    fn agent_log_disabled_by_default() {
        agent_log("test", "should be silent", "T", serde_json::json!({}));
    }
}
