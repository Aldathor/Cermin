mod chacha;
mod chacha64;
mod ed25519_pair;
mod fairplay;
mod fairplay_setup;
mod fpsap_helper;
mod hap;
mod hkdf_keys;
mod legacy_srp;
mod mirror_aes;
mod mirror_crypto;
mod pair_verify;
mod srp;

pub use chacha::StreamCipher;
pub use chacha64::seal as chacha64_seal;
pub use ed25519_pair::{Ed25519KeyPair, generate_ed25519_keypair};
pub use fairplay::{FairPlaySap, SapProvider, StubSapProvider};
pub use fairplay_setup::{FairPlaySession, MirrorFpKeys};
pub use hap::{
    TLV_ENCRYPTED_DATA, TLV_ERROR, TLV_IDENTIFIER, TLV_METHOD, TLV_PROOF, TLV_PUBLIC_KEY, TLV_SALT,
    TLV_SIGNATURE, TLV_STATE, chacha8_open, chacha8_seal, ed25519_public_from_private,
    ed25519_sign, ed25519_verify, hkdf_sha512, tlv_decode, tlv_encode,
};
pub use hkdf_keys::{derive_data_stream_chacha_key, derive_session_keys};
pub use legacy_srp::LegacySrpClient;
pub use mirror_aes::{MirrorAesCtr, derive_mirror_video_keys};
pub use mirror_crypto::MirrorVideoCrypto;
pub use pair_verify::{pair_verify_step1, pair_verify_step2};
pub use srp::{SrpClient, SrpCredentials};
