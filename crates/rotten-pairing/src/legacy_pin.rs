use aes_gcm::aead::consts::U16;
use aes_gcm::{
    AesGcm, Nonce,
    aead::{Aead, KeyInit, Payload},
    aes::Aes128,
};
use plist::{Dictionary, Value};
use rand::RngCore;
use reqwest::Client;
use rotten_core::config::DeviceCredentials;
use rotten_core::debug_log::agent_log;
use rotten_core::device::AirPlayDevice;
use rotten_core::error::{Result, RottenError};
use rotten_crypto::{Ed25519KeyPair, LegacySrpClient, generate_ed25519_keypair};
use sha2::{Digest, Sha512};
use tracing::debug;

const PAIR_PIN_START: &str = "pair-pin-start";
const PAIR_SETUP_PIN: &str = "pair-setup-pin";
const AIRPLAY_USER_AGENT: &str = "AirPlay/320.20";

/// Apple pair-setup-pin uses a 16-byte GCM IV (not the default 12-byte nonce).
type AppleAes128Gcm = AesGcm<Aes128, U16>;

pub struct LegacyPairingSession {
    client: Client,
    base_url: String,
    device_id: String,
    client_id: String,
    keypair: Ed25519KeyPair,
}

fn pairing_client() -> Result<Client> {
    Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(1)
        .build()
        .map_err(|e| RottenError::Pairing(e.to_string()))
}

/// Show PIN on Apple TV (`pair-pin-start` only). SRP steps run in `finish_pairing`.
pub async fn start_pairing(device: &AirPlayDevice) -> Result<LegacyPairingSession> {
    let client = pairing_client()?;
    let base_url = device.base_url();
    let mut client_id_bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut client_id_bytes);
    let client_id = hex::encode(client_id_bytes);
    let keypair = generate_ed25519_keypair();

    // #region agent log
    agent_log(
        "legacy_pin.rs:start_pairing",
        "legacy pairing starting",
        "F",
        serde_json::json!({
            "baseUrl": base_url,
            "clientId": client_id,
            "protocol": "pair-setup-pin",
            "flow": "pin-start-only",
        }),
    );
    // #endregion

    let pin_start_url = format!("{base_url}/{PAIR_PIN_START}");
    let pin_start_status = post_empty(&client, &pin_start_url).await?;
    // #region agent log
    agent_log(
        "legacy_pin.rs:start_pairing",
        "pair-pin-start response",
        "E",
        serde_json::json!({ "httpStatus": pin_start_status.as_u16() }),
    );
    // #endregion

    if !pin_start_status.is_success() {
        return Err(RottenError::Pairing(format!(
            "pair-pin-start HTTP {}",
            pin_start_status.as_u16()
        )));
    }

    debug!(device_id = %device.device_id, "legacy pairing ready for PIN");

    Ok(LegacyPairingSession {
        client,
        base_url,
        device_id: device.device_id.clone(),
        client_id,
        keypair,
    })
}

/// Steps 1–3 of pair-setup-pin on one keep-alive connection (matches AirPlayAuth).
pub async fn finish_pairing(session: LegacyPairingSession, pin: &str) -> Result<DeviceCredentials> {
    let LegacyPairingSession {
        client,
        base_url,
        device_id,
        client_id,
        keypair,
    } = session;

    // #region agent log
    agent_log(
        "legacy_pin.rs:finish_pairing",
        "using shared pairing HTTP client",
        "H121",
        serde_json::json!({
            "clientId": client_id,
            "sharedClient": true,
        }),
    );
    // #endregion

    let setup_url = format!("{base_url}/{PAIR_SETUP_PIN}");

    let mut step1 = Dictionary::new();
    step1.insert("method".into(), Value::String("pin".into()));
    step1.insert("user".into(), Value::String(client_id.clone()));

    let (step1_status, step1_resp, step1_raw_len) =
        post_bplist(&client, &setup_url, step1, "step1").await?;

    let salt = plist_bytes(&step1_resp, "salt")
        .ok_or_else(|| RottenError::Pairing("pair-setup-pin step1 missing salt".into()))?;
    let server_pk = plist_bytes(&step1_resp, "pk")
        .ok_or_else(|| RottenError::Pairing("pair-setup-pin step1 missing pk".into()))?;

    // #region agent log
    agent_log(
        "legacy_pin.rs:finish_pairing",
        "pair-setup-pin step1 response",
        "J",
        serde_json::json!({
            "httpStatus": step1_status.as_u16(),
            "bodyLen": step1_raw_len,
            "saltLen": salt.len(),
            "serverPkLen": server_pk.len(),
            "sameSocket": true,
            "sharedClient": true,
        }),
    );
    // #endregion

    if !step1_status.is_success() {
        return Err(RottenError::Pairing(format!(
            "pair-setup-pin step1 HTTP {}",
            step1_status.as_u16()
        )));
    }

    let mut srp = LegacySrpClient::new(&salt, &server_pk, &client_id);
    let (proof, expected_server_proof) = srp
        .authenticate(pin)
        .map_err(|e| RottenError::Pairing(format!("SRP: {e}")))?;

    let client_pk = srp.client_public();
    let session_key_len = srp.session_key_hash().map(|k| k.len()).unwrap_or(0);
    let proof_hex: String = proof.iter().map(|b| format!("{b:02x}")).collect();

    // #region agent log
    agent_log(
        "legacy_pin.rs:finish_pairing",
        "pair-setup-pin step2 request",
        "I",
        serde_json::json!({
            "clientPkLen": client_pk.len(),
            "proofLen": proof.len(),
            "proofHex": proof_hex,
            "sessionKeyLen": session_key_len,
            "srpVariant": "airplay-auth-40byte-k",
            "sameSocket": true,
            "sharedClient": true,
        }),
    );
    // #endregion

    let mut step2 = Dictionary::new();
    step2.insert("pk".into(), Value::Data(client_pk));
    step2.insert("proof".into(), Value::Data(proof));

    let (step2_status, step2_resp, step2_raw_len) =
        post_bplist(&client, &setup_url, step2, "step2").await?;

    if !step2_status.is_success() {
        return Err(RottenError::Pairing(format!(
            "pair-setup-pin step2 HTTP {} (wrong PIN or pairing not started on TV?)",
            step2_status.as_u16()
        )));
    }

    let server_proof = plist_bytes(&step2_resp, "proof")
        .ok_or_else(|| RottenError::Pairing("pair-setup-pin step2 missing proof".into()))?;

    // #region agent log
    agent_log(
        "legacy_pin.rs:finish_pairing",
        "pair-setup-pin step2 response",
        "G",
        serde_json::json!({
            "httpStatus": step2_status.as_u16(),
            "bodyLen": step2_raw_len,
            "proofMatch": server_proof == expected_server_proof,
            "respKeys": step2_resp.keys().collect::<Vec<_>>(),
        }),
    );
    // #endregion

    if server_proof != expected_server_proof {
        return Err(RottenError::Pairing(
            "SRP server proof mismatch (wrong PIN?)".into(),
        ));
    }

    let session_key = srp
        .session_key_hash()
        .ok_or_else(|| RottenError::Pairing("missing SRP session key".into()))?;

    let (aes_key, mut aes_iv) = derive_aes_key_iv(session_key);
    aes_iv[15] = aes_iv[15].wrapping_add(1);

    // #region agent log
    agent_log(
        "legacy_pin.rs:finish_pairing",
        "pair-setup-pin step3 encrypt",
        "K",
        serde_json::json!({
            "sessionKeyLen": session_key.len(),
            "ivLen": aes_iv.len(),
            "gcmIvLen": 16,
        }),
    );
    // #endregion

    let cipher = AppleAes128Gcm::new_from_slice(&aes_key)
        .map_err(|e| RottenError::Pairing(format!("AES init: {e}")))?;
    let encrypted = cipher
        .encrypt(
            Nonce::<U16>::from_slice(&aes_iv),
            Payload {
                msg: &keypair.public_key,
                aad: b"",
            },
        )
        .map_err(|e| RottenError::Pairing(format!("AES encrypt: {e}")))?;

    let epk = encrypted[..32].to_vec();
    let auth_tag = encrypted[32..].to_vec();

    let mut step3 = Dictionary::new();
    step3.insert("epk".into(), Value::Data(epk));
    step3.insert("authTag".into(), Value::Data(auth_tag));

    let (step3_status, step3_resp, step3_raw_len) =
        post_bplist(&client, &setup_url, step3, "step3").await?;

    // #region agent log
    agent_log(
        "legacy_pin.rs:finish_pairing",
        "pair-setup-pin step3 response",
        "F",
        serde_json::json!({
            "httpStatus": step3_status.as_u16(),
            "bodyLen": step3_raw_len,
            "respKeys": step3_resp.keys().collect::<Vec<_>>(),
        }),
    );
    // #endregion

    if !step3_status.is_success() {
        return Err(RottenError::Pairing(format!(
            "pair-setup-pin step3 HTTP {}",
            step3_status.as_u16()
        )));
    }

    let server_epk = plist_bytes(&step3_resp, "epk")
        .ok_or_else(|| RottenError::Pairing("pair-setup-pin step3 missing epk".into()))?;
    let server_auth_tag = plist_bytes(&step3_resp, "authTag")
        .ok_or_else(|| RottenError::Pairing("pair-setup-pin step3 missing authTag".into()))?;

    aes_iv[15] = aes_iv[15].wrapping_add(1);
    let mut server_ciphertext = server_epk;
    server_ciphertext.extend_from_slice(&server_auth_tag);
    let server_public_key = cipher
        .decrypt(
            Nonce::<U16>::from_slice(&aes_iv),
            Payload {
                msg: &server_ciphertext,
                aad: b"",
            },
        )
        .map_err(|e| RottenError::Pairing(format!("server epk decrypt: {e}")))?;

    if server_public_key.len() != 32 {
        return Err(RottenError::Pairing(format!(
            "server public key has unexpected length {}",
            server_public_key.len()
        )));
    }

    // #region agent log
    agent_log(
        "legacy_pin.rs:finish_pairing",
        "server epk decrypted",
        "K",
        serde_json::json!({
            "serverPkLen": server_public_key.len(),
        }),
    );
    // #endregion

    Ok(DeviceCredentials {
        device_id,
        identifier: client_id,
        public_key: keypair.public_key.to_vec(),
        private_key: keypair.private_key.to_vec(),
        server_public_key,
        hap: false,
        accessory_id: Vec::new(),
    })
}

async fn post_empty(client: &Client, url: &str) -> Result<reqwest::StatusCode> {
    let resp = client
        .post(url)
        .header("User-Agent", AIRPLAY_USER_AGENT)
        .header("Connection", "keep-alive")
        .send()
        .await
        .map_err(|e| RottenError::Pairing(format!("POST {url}: {e}")))?;
    Ok(resp.status())
}

async fn post_bplist(
    client: &Client,
    url: &str,
    dict: Dictionary,
    step: &str,
) -> Result<(reqwest::StatusCode, Dictionary, usize)> {
    let mut body = Vec::new();
    plist::to_writer_binary(&mut body, &Value::Dictionary(dict))
        .map_err(|e| RottenError::Pairing(format!("plist encode: {e}")))?;

    let resp = client
        .post(url)
        .header("User-Agent", AIRPLAY_USER_AGENT)
        .header("Connection", "keep-alive")
        .header("Content-Type", "application/x-apple-binary-plist")
        .body(body)
        .send()
        .await
        .map_err(|e| RottenError::Pairing(format!("POST {url}: {e}")))?;

    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| RottenError::Pairing(format!("response body: {e}")))?;

    let raw_len = bytes.len();
    let hex_prefix: String = bytes.iter().take(64).map(|b| format!("{b:02x}")).collect();

    // #region agent log
    agent_log(
        "legacy_pin.rs:post_bplist",
        "raw HTTP response",
        "G",
        serde_json::json!({
            "step": step,
            "httpStatus": status.as_u16(),
            "bodyLen": raw_len,
            "bodyHexPrefix": hex_prefix,
        }),
    );
    // #endregion

    if raw_len == 0 {
        return Err(RottenError::Pairing(format!(
            "pair-setup-pin {step} empty response (HTTP {})",
            status.as_u16()
        )));
    }

    let plist_bytes = extract_plist_body(&bytes);

    let value: Value = plist::from_bytes(plist_bytes).map_err(|e| {
        RottenError::Pairing(format!(
            "pair-setup-pin {step} plist decode: {e} (HTTP {}, bodyLen {})",
            status.as_u16(),
            raw_len
        ))
    })?;
    let dict = match value {
        Value::Dictionary(d) => d,
        _ => {
            return Err(RottenError::Pairing(format!(
                "pair-setup-pin {step} expected plist dictionary"
            )));
        }
    };
    Ok((status, dict, raw_len))
}

/// Apple TV may return raw binary plist or RTSP-framed plist.
fn extract_plist_body(bytes: &[u8]) -> &[u8] {
    if bytes.starts_with(b"bplist") {
        return bytes;
    }
    if let Some(idx) = bytes.windows(6).position(|w| w == b"bplist") {
        return &bytes[idx..];
    }
    bytes
}

fn plist_bytes(dict: &Dictionary, key: &str) -> Option<Vec<u8>> {
    dict.get(key).and_then(|v| match v {
        Value::Data(d) => Some(d.clone()),
        _ => None,
    })
}

fn derive_aes_key_iv(session_key: &[u8]) -> ([u8; 16], [u8; 16]) {
    let mut key_hasher = Sha512::new();
    key_hasher.update(b"Pair-Setup-AES-Key");
    key_hasher.update(session_key);
    let key_digest = key_hasher.finalize();

    let mut iv_hasher = Sha512::new();
    iv_hasher.update(b"Pair-Setup-AES-IV");
    iv_hasher.update(session_key);
    let iv_digest = iv_hasher.finalize();

    let mut aes_key = [0u8; 16];
    let mut aes_iv = [0u8; 16];
    aes_key.copy_from_slice(&key_digest[..16]);
    aes_iv.copy_from_slice(&iv_digest[..16]);
    (aes_key, aes_iv)
}
