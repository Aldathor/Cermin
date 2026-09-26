//! Cast-only user presets: video quality and HLS startup latency.
//!
//! Both choices belong to the experimental Google Cast path only and are
//! independent of each other; AirPlay sessions keep their existing fixed
//! configuration. A session snapshots the selection when it starts: the
//! encoder size, the HLS muxer/store profile, the IDR cadence and the initial
//! buffer are locked for the whole run, so the live stream can never change
//! size or seek mid-session.

use clap::ValueEnum;
use rotten_cast::hls::HlsProfile;

/// Cast video quality preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum CastQuality {
    /// Balanced: up to 720p at 4 Mbps (the established default).
    #[default]
    Balanced,
    /// High: up to 1080p at 8 Mbps; needs encoder and network headroom.
    High,
}

impl CastQuality {
    /// Video bitrate handed to the encoder for this preset.
    pub const fn bitrate_kbps(self) -> u32 {
        match self {
            Self::Balanced => 4000,
            Self::High => 8000,
        }
    }

    /// Advertised HLS bandwidth hint; keeps headroom above the video bitrate
    /// for the transport stream and audio.
    pub const fn bandwidth_bps(self) -> u64 {
        match self {
            Self::Balanced => 8_000_000,
            Self::High => 16_000_000,
        }
    }

    /// Synthetic `--test` source size: the real encode size for this preset,
    /// never a smaller stand-in.
    pub const fn synthetic_dims(self) -> (u32, u32) {
        match self {
            Self::Balanced => (1280, 720),
            Self::High => (1920, 1080),
        }
    }

    /// Stable machine name reported by the pipeline summary.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::High => "high",
        }
    }

    /// User-facing label for the CLI and GUI selectors.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Balanced => "Balanced — up to 720p, 4 Mbps",
            Self::High => "High — up to 1080p, 8 Mbps",
        }
    }
}

/// Cast HLS startup-latency preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum CastLatency {
    /// Stable: about 8 seconds of initial buffer (the established default).
    #[default]
    Stable,
    /// Responsive: about 4 seconds of initial buffer; may rebuffer more often.
    Responsive,
}

impl CastLatency {
    /// The HLS profile that drives the muxer, store and IDR cadence together.
    pub const fn hls_profile(self) -> HlsProfile {
        match self {
            Self::Stable => HlsProfile::Stable,
            Self::Responsive => HlsProfile::Responsive,
        }
    }

    /// Initial media buffer advertised before the receiver is asked to play;
    /// the user-facing startup copy and timeout text use this same value.
    pub const fn initial_buffer_secs(self) -> u32 {
        match self {
            Self::Stable => 8,
            Self::Responsive => 4,
        }
    }

    /// Stable machine name reported by the pipeline summary.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Responsive => "responsive",
        }
    }

    /// User-facing label for the CLI and GUI selectors.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stable => "Stable",
            Self::Responsive => "Lower delay (experimental)",
        }
    }
}

/// The two Cast-only choices, snapshotted when a session starts so a later
/// change in the GUI can never alter the running stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CastSettings {
    /// Video quality preset.
    pub quality: CastQuality,
    /// HLS startup-latency preset.
    pub latency: CastLatency,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_balanced_and_stable() {
        assert_eq!(CastQuality::default(), CastQuality::Balanced);
        assert_eq!(CastLatency::default(), CastLatency::Stable);
        assert_eq!(CastSettings::default().quality, CastQuality::Balanced);
        assert_eq!(CastSettings::default().latency, CastLatency::Stable);
    }

    #[test]
    fn quality_presets_expose_documented_targets_and_labels() {
        assert_eq!(CastQuality::Balanced.bitrate_kbps(), 4000);
        assert_eq!(CastQuality::High.bitrate_kbps(), 8000);
        assert_eq!(CastQuality::Balanced.bandwidth_bps(), 8_000_000);
        assert_eq!(CastQuality::High.bandwidth_bps(), 16_000_000);
        assert_eq!(CastQuality::Balanced.synthetic_dims(), (1280, 720));
        assert_eq!(CastQuality::High.synthetic_dims(), (1920, 1080));
        assert_eq!(CastQuality::Balanced.name(), "balanced");
        assert_eq!(CastQuality::High.name(), "high");
        assert!(CastQuality::Balanced.label().contains("720p"));
        assert!(CastQuality::Balanced.label().contains("4 Mbps"));
        assert!(CastQuality::High.label().contains("1080p"));
        assert!(CastQuality::High.label().contains("8 Mbps"));
    }

    #[test]
    fn latency_presets_expose_documented_buffers_and_labels() {
        assert_eq!(CastLatency::Stable.initial_buffer_secs(), 8);
        assert_eq!(CastLatency::Responsive.initial_buffer_secs(), 4);
        assert_eq!(CastLatency::Stable.name(), "stable");
        assert_eq!(CastLatency::Responsive.name(), "responsive");
        assert_eq!(CastLatency::Stable.label(), "Stable");
        assert!(CastLatency::Responsive.label().contains("experimental"));
    }

    /// The app-side buffer copy must match the HLS profile the muxer and store
    /// actually use: 8 s for Stable, 4 s for Responsive, with the matching
    /// segment cadence.
    #[test]
    fn latency_profiles_match_the_advertised_buffers() {
        let stable = CastLatency::Stable.hls_profile();
        assert_eq!(stable.target_duration_secs(), 2);
        assert_eq!(stable.segment_duration_us(), 1_000_000);
        assert_eq!(
            stable.ready_duration_us(),
            u64::from(CastLatency::Stable.initial_buffer_secs()) * 1_000_000
        );

        let responsive = CastLatency::Responsive.hls_profile();
        assert_eq!(responsive.target_duration_secs(), 1);
        assert_eq!(responsive.segment_duration_us(), 500_000);
        assert_eq!(
            responsive.ready_duration_us(),
            u64::from(CastLatency::Responsive.initial_buffer_secs()) * 1_000_000
        );
    }
}
