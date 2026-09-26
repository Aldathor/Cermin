//! Helpers for extend-like workflows using virtual monitors.

use crate::backend::DisplayInfo;

/// Heuristic: treat monitor names/descriptions containing these substrings as virtual.
pub fn is_virtual_display_name(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        "virtual",
        "indirect",
        "idd",
        "parsec",
        "spacedesk",
        "dummy",
        "vdd",
        "displaylink",
        "evdi",
        "sunshine",
        "moonlight",
    ]
    .iter()
    .any(|needle| n.contains(needle))
}

/// Filter displays to virtual monitors only.
pub fn filter_virtual_displays(displays: &[DisplayInfo]) -> Vec<&DisplayInfo> {
    displays.iter().filter(|d| d.is_virtual).collect()
}

/// Pick the display index for virtual-only capture mode.
pub fn select_virtual_display(
    displays: &[DisplayInfo],
    preferred_index: Option<u32>,
) -> Option<u32> {
    let virtuals: Vec<_> = filter_virtual_displays(displays);
    if virtuals.is_empty() {
        return None;
    }
    if let Some(idx) = preferred_index {
        if virtuals.iter().any(|d| d.index == idx) {
            return Some(idx);
        }
    }
    Some(virtuals[0].index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_virtual_names() {
        assert!(is_virtual_display_name("IDD HDR Virtual Display"));
        assert!(is_virtual_display_name("Parsec Virtual Display"));
        assert!(!is_virtual_display_name("Generic PnP Monitor"));
    }

    #[test]
    fn select_prefers_matching_index() {
        let displays = vec![
            DisplayInfo {
                index: 0,
                name: "primary".into(),
                width: 1920,
                height: 1080,
                is_virtual: false,
                adapter: None,
            },
            DisplayInfo {
                index: 1,
                name: "virtual".into(),
                width: 1280,
                height: 720,
                is_virtual: true,
                adapter: None,
            },
        ];
        assert_eq!(select_virtual_display(&displays, Some(1)), Some(1));
        assert_eq!(select_virtual_display(&displays, None), Some(1));
        assert_eq!(select_virtual_display(&displays, Some(0)), Some(1));
    }
}
