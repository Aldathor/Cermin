pub mod airplay_conn;
mod audio_rtp;
mod fp_setup;
pub mod http;
mod mirror;
mod mirror_rtsp;
mod ntp;
mod pair_verify;
mod ptp;
mod rtsp;

pub use audio_rtp::{
    AUDIO_LATENCY_SAMPLES, MirrorAudioSetup, playout_latency_samples, plist_audio_ports,
    spawn_mirror_audio_silence,
};
pub use mirror::{MirrorConnection, MirrorHandle};
pub use mirror_rtsp::{encode_audio_setup_plist_chacha, set_tv_volume_percent, tv_volume_body};
pub use ntp::{ntp_boot_relative, ntp_boot_with_epoch};
pub use pair_verify::{PairVerifyOutcome, hap_pair_verify_conn, pair_verify_conn};
pub use ptp::{PtpMaster, PtpPeer, clock_id_from_identifier};
pub use rtsp::RtspSession;
