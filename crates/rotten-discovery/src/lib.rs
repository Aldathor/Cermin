mod browser;
mod cast;
mod resolve;

use std::time::Duration;

pub use browser::{discover_devices, discover_for};
pub use cast::discover_cast_for;
pub use resolve::resolve_device;

use rotten_core::device::{AirPlayDevice, CastDevice, ReceiverDevice};
use rotten_core::error::{Result, RottenError};
use tracing::warn;

/// Browse AirPlay and Google Cast receivers concurrently for up to `timeout`.
///
/// A failure in one protocol does not hide results from the other: the working
/// side is returned with a warning. When both protocols fail, both errors are
/// surfaced instead of guessing.
pub async fn discover_receivers_for(timeout: Duration) -> Result<Vec<ReceiverDevice>> {
    let (airplay, cast) = tokio::join!(discover_for(timeout), discover_cast_for(timeout));
    merge_discovery_results(airplay, cast)
}

fn merge_discovery_results(
    airplay: Result<Vec<AirPlayDevice>>,
    cast: Result<Vec<CastDevice>>,
) -> Result<Vec<ReceiverDevice>> {
    if let (Err(airplay_error), Err(cast_error)) = (&airplay, &cast) {
        return Err(RottenError::Discovery(format!(
            "AirPlay discovery failed: {airplay_error}; Google Cast discovery failed: {cast_error}"
        )));
    }

    let mut receivers = Vec::new();
    match airplay {
        Ok(devices) => receivers.extend(devices.into_iter().map(ReceiverDevice::AirPlay)),
        Err(error) => warn!(%error, "AirPlay discovery failed; returning Google Cast results"),
    }
    match cast {
        Ok(devices) => receivers.extend(devices.into_iter().map(ReceiverDevice::GoogleCast)),
        Err(error) => warn!(%error, "Google Cast discovery failed; returning AirPlay results"),
    }

    receivers.sort_by(|a, b| {
        a.name()
            .cmp(b.name())
            .then_with(|| a.device_id().cmp(b.device_id()))
    });
    Ok(receivers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rotten_core::device::DeviceFeatures;

    fn airplay(name: &str) -> AirPlayDevice {
        AirPlayDevice {
            name: name.to_string(),
            host: "192.168.1.10".to_string(),
            port: 7000,
            device_id: "airplay-id".to_string(),
            model: None,
            features: DeviceFeatures::default(),
            display_width: None,
            display_height: None,
            pi: None,
            pk: None,
        }
    }

    fn cast(name: &str, id: &str) -> CastDevice {
        CastDevice {
            name: name.to_string(),
            host: "192.168.1.20".to_string(),
            port: 8009,
            device_id: id.to_string(),
            model: None,
            capabilities: None,
        }
    }

    #[test]
    fn merges_both_protocols_in_name_order() {
        let merged = merge_discovery_results(
            Ok(vec![airplay("Zeta TV")]),
            Ok(vec![cast("Alpha Cast", "id-1")]),
        )
        .unwrap();
        let seen: Vec<(&str, &str)> = merged
            .iter()
            .map(|device| (device.name(), device.protocol_label()))
            .collect();
        assert_eq!(
            seen,
            vec![("Alpha Cast", "Google Cast"), ("Zeta TV", "AirPlay")]
        );
    }

    #[test]
    fn keeps_working_protocol_when_other_fails() {
        let merged = merge_discovery_results(
            Err(RottenError::Discovery("airplay down".into())),
            Ok(vec![cast("Alpha Cast", "id-1")]),
        )
        .unwrap();
        assert_eq!(merged.len(), 1);
        assert!(merged[0].is_cast());

        let merged = merge_discovery_results(
            Ok(vec![airplay("Zeta TV")]),
            Err(RottenError::Discovery("cast down".into())),
        )
        .unwrap();
        assert_eq!(merged.len(), 1);
        assert!(!merged[0].is_cast());
    }

    #[test]
    fn surfaces_both_errors_when_both_fail() {
        let error = merge_discovery_results(
            Err(RottenError::Discovery("airplay down".into())),
            Err(RottenError::Discovery("cast down".into())),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("airplay down"));
        assert!(message.contains("cast down"));
    }
}
