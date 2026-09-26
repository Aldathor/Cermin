//! Experimental Google Cast control and live H.264/HLS delivery.
//!
//! This is Default Media Receiver playback, not the low-latency Cast mirroring
//! protocol. Control TLS does not yet authenticate receiver identity; media is
//! served over HTTP on the selected LAN interface. Use only on trusted networks.

pub mod control;
pub mod hls;
pub mod http;

pub use control::CastClient;
