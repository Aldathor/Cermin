//! HAP (HomeKit Accessory Protocol) building blocks used by AirPlay 2 pairing.
//!
//! Implements the crypto primitives shared by `/pair-setup` and `/pair-verify`:
//! HKDF-SHA512, the 8-byte-nonce ChaCha20-Poly1305 variant (used for the
//! PS-Msg05/06 and PV-Msg02/03 TLVs) and TLV8 encoding.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use sha2::Sha512;

use rotten_core::error::{Result, RottenError};

/// Expand `ikm` with HKDF-SHA512.
pub fn hkdf_sha512(salt: &[u8], info: &[u8], ikm: &[u8], len: usize) -> Vec<u8> {
    let hk = Hkdf::<Sha512>::new(Some(salt), ikm);
    let mut out = vec![0u8; len];
    hk.expand(info, &mut out).expect("hkdf expand");
    out
}

/// Build the 12-byte IETF nonce from an up-to-8-byte HAP label
/// (4 zero bytes followed by the 8-byte little-endian value).
fn chacha8_nonce(nonce: &[u8]) -> [u8; 12] {
    let mut n = [0u8; 12];
    let len = nonce.len().min(8);
    n[12 - len..].copy_from_slice(&nonce[nonce.len() - len..]);
    n
}

/// Seal with the original ChaCha20-Poly1305 8-byte nonce variant (tag appended).
pub fn chacha8_seal(key: &[u8; 32], nonce: &[u8], plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let n = chacha8_nonce(nonce);
    cipher
        .encrypt(
            Nonce::from_slice(&n),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("chacha seal")
}

/// Open a sealed message produced with the 8-byte nonce variant.
pub fn chacha8_open(
    key: &[u8; 32],
    nonce: &[u8],
    ciphertext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let n = chacha8_nonce(nonce);
    cipher
        .decrypt(
            Nonce::from_slice(&n),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| RottenError::Crypto("chacha20-poly1305 open failed".into()))
}

/// Sign with an Ed25519 private key.
pub fn ed25519_sign(sk: &[u8; 32], msg: &[u8]) -> [u8; 64] {
    SigningKey::from_bytes(sk).sign(msg).to_bytes()
}

/// Verify an Ed25519 signature.
pub fn ed25519_verify(pk: &[u8; 32], msg: &[u8], sig: &[u8]) -> Result<()> {
    let sig_arr: [u8; 64] = sig
        .try_into()
        .map_err(|_| RottenError::Crypto("invalid ed25519 signature length".into()))?;
    let vk = VerifyingKey::from_bytes(pk).map_err(|e| RottenError::Crypto(e.to_string()))?;
    vk.verify_strict(msg, &Signature::from_bytes(&sig_arr))
        .map_err(|e| RottenError::Crypto(format!("ed25519 verify: {e}")))
}

/// Derive an Ed25519 public key from a private key.
pub fn ed25519_public_from_private(sk: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(sk).verifying_key().to_bytes()
}

// HAP TLV8 tags.
pub const TLV_METHOD: u8 = 0x00;
pub const TLV_IDENTIFIER: u8 = 0x01;
pub const TLV_SALT: u8 = 0x02;
pub const TLV_PUBLIC_KEY: u8 = 0x03;
pub const TLV_PROOF: u8 = 0x04;
pub const TLV_ENCRYPTED_DATA: u8 = 0x05;
pub const TLV_STATE: u8 = 0x06;
pub const TLV_ERROR: u8 = 0x07;
pub const TLV_SIGNATURE: u8 = 0x0a;

/// Encode TLV8 entries (values up to 255 bytes each; longer values are split).
pub fn tlv_encode(entries: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (tag, value) in entries {
        if value.is_empty() {
            out.push(*tag);
            out.push(0);
            continue;
        }
        for chunk in value.chunks(255) {
            out.push(*tag);
            out.push(chunk.len() as u8);
            out.extend_from_slice(chunk);
        }
    }
    out
}

/// Decode TLV8 bytes, concatenating values split across multiple chunks.
pub fn tlv_decode(data: &[u8]) -> std::collections::HashMap<u8, Vec<u8>> {
    let mut map: std::collections::HashMap<u8, Vec<u8>> = std::collections::HashMap::new();
    let mut i = 0usize;
    while i + 1 < data.len() {
        let tag = data[i];
        let len = data[i + 1] as usize;
        i += 2;
        if i + len > data.len() {
            break;
        }
        map.entry(tag).or_default().extend_from_slice(&data[i..i + len]);
        i += len;
    }
    map
}
