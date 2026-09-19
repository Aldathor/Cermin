use rand::RngCore;
use rotten_core::config::DeviceCredentials;
use rotten_core::debug_log::agent_log;
use rotten_core::device::AirPlayDevice;
use rotten_core::error::{Result, RottenError};
use rotten_crypto::{
    TLV_ENCRYPTED_DATA, TLV_IDENTIFIER, TLV_PUBLIC_KEY, TLV_SIGNATURE, TLV_STATE, chacha8_open,
    chacha8_seal, ed25519_sign, ed25519_verify, hkdf_sha512, pair_verify_step1, pair_verify_step2,
    tlv_decode, tlv_encode,
};
use tracing::debug;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::airplay_conn::AirPlayRtspConn;

/// HAP control-channel keys derived after pair-verify.
#[derive(Debug, Clone, Copy)]
pub struct HapKeys {
    pub out_key: [u8; 32],
    pub in_key: [u8; 32],
}

/// X25519 ECDH shared secret from pair-verify (needed for FairPlay + data-stream keys).
pub struct PairVerifyOutcome {
    pub shared_secret: [u8; 32],
    pub hap_keys: Option<HapKeys>,
}

/// Legacy raw pair-verify on a persistent RTSP connection (plaintext, required before fp-setup).
pub async fn pair_verify_conn(
    conn: &mut AirPlayRtspConn,
    device: &AirPlayDevice,
    creds: &DeviceCredentials,
) -> Result<PairVerifyOutcome> {
    let client_pk = key32(&creds.public_key, "client public key")?;
    let client_sk = key32(&creds.private_key, "client private key")?;
    let server_pk = resolve_server_public_key(creds, device)?;

    let (body1, eph_secret, eph_pk) = pair_verify_step1(&client_pk);

    debug!(host = %device.host, "pair-verify step 1");

    let (status1, bytes1) = conn.post_pair_verify("/pair-verify", &body1).await?;

    // #region agent log
    agent_log(
        "pair_verify.rs:pair_verify_conn",
        "pair-verify step1 response",
        "L",
        serde_json::json!({
            "httpStatus": status1,
            "bodyLen": bytes1.len(),
            "hasStoredServerPk": creds.server_public_key.len() == 32,
            "hasInfoPk": device.pk.is_some(),
            "sameSocket": true,
        }),
    );
    // #endregion

    if status1 != 200 {
        return Err(RottenError::Protocol(format!(
            "pair-verify step1 HTTP {status1}"
        )));
    }

    let (body2, shared_secret) =
        pair_verify_step2(&eph_secret, &eph_pk, &client_sk, &bytes1, &server_pk)
            .map_err(|e| RottenError::Protocol(e.to_string()))?;

    debug!(host = %device.host, "pair-verify step 2");

    let (status2, _) = conn.post_pair_verify("/pair-verify", &body2).await?;

    // #region agent log
    agent_log(
        "pair_verify.rs:pair_verify_conn",
        "pair-verify step2 response",
        "L",
        serde_json::json!({
            "httpStatus": status2,
            "sameSocket": true,
        }),
    );
    // #endregion

    if status2 != 200 {
        return Err(RottenError::Protocol(format!(
            "pair-verify step2 HTTP {status2}"
        )));
    }

    Ok(PairVerifyOutcome {
        shared_secret,
        hap_keys: None,
    })
}

/// HAP pair-verify (AirPlay 2): TLV8 M1–M3, X25519 + HKDF-SHA512 +
/// ChaCha20-Poly1305, yielding the encrypted control-channel keys.
pub async fn hap_pair_verify_conn(
    conn: &mut AirPlayRtspConn,
    device: &AirPlayDevice,
    creds: &DeviceCredentials,
) -> Result<PairVerifyOutcome> {
    let client_id = creds.identifier.as_bytes().to_vec();
    let client_sk = key32(&creds.private_key, "client private key")?;
    let accessory_pk = key32(&creds.server_public_key, "accessory public key")?;

    let mut eph = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut eph);
    let eph_secret = StaticSecret::from(eph);
    let eph_pub = PublicKey::from(&eph_secret).to_bytes();

    let m1 = tlv_encode(&[(TLV_STATE, &[0x01][..]), (TLV_PUBLIC_KEY, &eph_pub)]);
    debug!(host = %device.host, "HAP pair-verify M1");
    let (status1, body1) = conn.post_hap_http("/pair-verify", &m1).await?;
    if status1 != 200 {
        return Err(RottenError::Protocol(format!(
            "HAP pair-verify M1 HTTP {status1}"
        )));
    }

    let m2 = tlv_decode(&body1);
    let server_eph = m2
        .get(&TLV_PUBLIC_KEY)
        .cloned()
        .ok_or_else(|| RottenError::Protocol("HAP M2 missing public key".into()))?;
    let encrypted = m2
        .get(&TLV_ENCRYPTED_DATA)
        .cloned()
        .ok_or_else(|| RottenError::Protocol("HAP M2 missing encrypted data".into()))?;

    let server_eph: [u8; 32] = server_eph
        .as_slice()
        .try_into()
        .map_err(|_| RottenError::Protocol("HAP M2 bad public key length".into()))?;
    let shared = eph_secret.diffie_hellman(&PublicKey::from(server_eph));
    let shared_secret: [u8; 32] = *shared.as_bytes();

    let session_key_vec = hkdf_sha512(
        b"Pair-Verify-Encrypt-Salt",
        b"Pair-Verify-Encrypt-Info",
        &shared_secret,
        32,
    );
    let session_key: [u8; 32] = session_key_vec
        .try_into()
        .map_err(|_| RottenError::Protocol("bad pair-verify session key".into()))?;

    let decrypted = chacha8_open(&session_key, b"PV-Msg02", &encrypted, &[])?;
    let sub = tlv_decode(&decrypted);

    let accessory_id = sub.get(&TLV_IDENTIFIER).cloned().unwrap_or_default();
    if !creds.accessory_id.is_empty() && accessory_id != creds.accessory_id {
        return Err(RottenError::Protocol(
            "HAP pair-verify accessory identifier mismatch".into(),
        ));
    }

    if let Some(signature) = sub.get(&TLV_SIGNATURE) {
        let mut signed = Vec::with_capacity(32 + accessory_id.len() + 32);
        signed.extend_from_slice(&server_eph);
        signed.extend_from_slice(&accessory_id);
        signed.extend_from_slice(&eph_pub);
        if let Err(e) = ed25519_verify(&accessory_pk, &signed, signature) {
            tracing::warn!(error = %e, "accessory pair-verify signature check failed");
        }
    }

    let mut device_info = Vec::with_capacity(32 + client_id.len() + 32);
    device_info.extend_from_slice(&eph_pub);
    device_info.extend_from_slice(&client_id);
    device_info.extend_from_slice(&server_eph);
    let client_signature = ed25519_sign(&client_sk, &device_info);

    let sub3 = tlv_encode(&[
        (TLV_IDENTIFIER, &client_id),
        (TLV_SIGNATURE, &client_signature),
    ]);
    let encrypted3 = chacha8_seal(&session_key, b"PV-Msg03", &sub3, &[]);
    let m3 = tlv_encode(&[
        (TLV_STATE, &[0x03][..]),
        (TLV_ENCRYPTED_DATA, &encrypted3),
    ]);

    debug!(host = %device.host, "HAP pair-verify M3");
    let (status2, _) = conn.post_hap_http("/pair-verify", &m3).await?;
    if status2 != 200 {
        return Err(RottenError::Protocol(format!(
            "HAP pair-verify M3 HTTP {status2}"
        )));
    }

    let out_key_vec = hkdf_sha512(
        b"Control-Salt",
        b"Control-Write-Encryption-Key",
        &shared_secret,
        32,
    );
    let in_key_vec = hkdf_sha512(
        b"Control-Salt",
        b"Control-Read-Encryption-Key",
        &shared_secret,
        32,
    );
    let out_key: [u8; 32] = out_key_vec
        .try_into()
        .map_err(|_| RottenError::Protocol("bad control write key".into()))?;
    let in_key: [u8; 32] = in_key_vec
        .try_into()
        .map_err(|_| RottenError::Protocol("bad control read key".into()))?;

    Ok(PairVerifyOutcome {
        shared_secret,
        hap_keys: Some(HapKeys { out_key, in_key }),
    })
}

fn key32(bytes: &[u8], label: &str) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| RottenError::Protocol(format!("invalid {label} length")))
}

fn resolve_server_public_key(
    creds: &DeviceCredentials,
    device: &AirPlayDevice,
) -> Result<[u8; 32]> {
    if creds.server_public_key.len() == 32 {
        return creds
            .server_public_key
            .as_slice()
            .try_into()
            .map_err(|_| RottenError::Protocol("invalid stored server public key".into()));
    }
    device
        .pk
        .as_deref()
        .and_then(decode_info_pk)
        .ok_or_else(|| {
            RottenError::Protocol(
                "missing Apple TV public key for pair-verify — re-pair with `rottingapple pair --force`"
                    .into(),
            )
        })
}

fn decode_info_pk(pk: &str) -> Option<[u8; 32]> {
    use base64::Engine;
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(pk) {
        if bytes.len() == 32 {
            return bytes.try_into().ok();
        }
    }
    if pk.len() == 64 {
        if let Ok(bytes) = hex::decode(pk) {
            if bytes.len() == 32 {
                return bytes.try_into().ok();
            }
        }
    }
    None
}
