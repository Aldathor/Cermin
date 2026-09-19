//! HAP (AirPlay 2) pair-setup over `/pair-setup`.
//!
//! SRP-6a (3072-bit, SHA-512) + ChaCha20-Poly1305 M5/M6 exchange, producing
//! Ed25519 long-term credentials used later by HAP pair-verify.
//!
//! Runs on a single persistent TCP connection, matching the reference client:
//! the accessory keeps pairing state per connection.

use rand::RngCore;
use rotten_core::config::DeviceCredentials;
use rotten_core::device::AirPlayDevice;
use rotten_core::error::{Result, RottenError};
use rotten_crypto::{
    Ed25519KeyPair, SrpClient, TLV_ENCRYPTED_DATA, TLV_ERROR, TLV_IDENTIFIER, TLV_METHOD,
    TLV_PROOF, TLV_PUBLIC_KEY, TLV_SALT, TLV_SIGNATURE, TLV_STATE, chacha8_open, chacha8_seal,
    ed25519_sign, generate_ed25519_keypair, hkdf_sha512, tlv_decode, tlv_encode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info};

const PAIR_SETUP: &str = "/pair-setup";
const PAIR_PIN_START: &str = "/pair-pin-start";
const AIRPLAY_USER_AGENT: &str = "AirPlay/320.20";
const USERNAME: &str = "Pair-Setup";

struct HttpConn {
    stream: TcpStream,
    host: String,
    buf: Vec<u8>,
}

impl HttpConn {
    async fn connect(host: &str, port: u16) -> Result<Self> {
        let stream = TcpStream::connect((host, port))
            .await
            .map_err(|e| RottenError::Pairing(format!("connect {host}:{port}: {e}")))?;
        eprintln!(
            "[hap] HTTP connected to {:?} from {:?}",
            stream.peer_addr(),
            stream.local_addr()
        );
        Ok(Self {
            stream,
            host: format!("{host}:{port}"),
            buf: Vec::with_capacity(8192),
        })
    }

    async fn post(&mut self, path: &str, body: &[u8]) -> Result<(u16, Vec<u8>)> {
        let header = format!(
            "POST {path} HTTP/1.1\r\n\
             Host: {}\r\n\
             User-Agent: {AIRPLAY_USER_AGENT}\r\n\
             Connection: keep-alive\r\n\
             X-Apple-HKP: 3\r\n\
             Content-Type: application/octet-stream\r\n\
             Content-Length: {}\r\n\r\n",
            self.host,
            body.len()
        );
        eprintln!("[hap] request: {header:?}");
        self.stream
            .write_all(header.as_bytes())
            .await
            .map_err(|e| RottenError::Pairing(format!("write {path}: {e}")))?;
        self.stream
            .write_all(body)
            .await
            .map_err(|e| RottenError::Pairing(format!("write body {path}: {e}")))?;

        self.read_response().await
    }

    async fn read_response(&mut self) -> Result<(u16, Vec<u8>)> {
        let mut tmp = [0u8; 8192];
        let header_end = loop {
            if let Some(pos) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            let n = self
                .stream
                .read(&mut tmp)
                .await
                .map_err(|e| RottenError::Pairing(format!("read: {e}")))?;
            if n == 0 {
                return Err(RottenError::Pairing("connection closed".into()));
            }
            self.buf.extend_from_slice(&tmp[..n]);
        };

        let header_text = String::from_utf8_lossy(&self.buf[..header_end]).to_string();
        let status = header_text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| RottenError::Pairing("bad HTTP status line".into()))?;
        let content_length = header_text
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);

        while self.buf.len() < header_end + content_length {
            let n = self
                .stream
                .read(&mut tmp)
                .await
                .map_err(|e| RottenError::Pairing(format!("read body: {e}")))?;
            if n == 0 {
                break;
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }

        let end = (header_end + content_length).min(self.buf.len());
        let body = self.buf[header_end..end].to_vec();
        self.buf.drain(..end);
        Ok((status, body))
    }
}

/// In-progress pairing state between M2 (TV shows PIN) and M3.
pub struct PairingSession {
    conn: HttpConn,
    device_id: String,
    pairing_id: Vec<u8>,
    salt: Vec<u8>,
    server_pub: Vec<u8>,
    srp: SrpClient,
    keypair: Ed25519KeyPair,
}

/// Send M1 and parse M2. The TV displays the PIN after this returns.
pub async fn start_pairing(device: &AirPlayDevice) -> Result<PairingSession> {
    let mut conn = HttpConn::connect(&device.host, device.port).await?;

    let mut pin_start_status = 0u16;
    for attempt in 1..=4 {
        let (status, _) = conn.post(PAIR_PIN_START, &[]).await?;
        pin_start_status = status;
        eprintln!("[hap] pair-pin-start status={status} (attempt {attempt})");
        if (200..300).contains(&status) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    if !(200..300).contains(&pin_start_status) {
        return Err(RottenError::Pairing(format!(
            "pair-pin-start HTTP {pin_start_status}"
        )));
    }

    let mut pairing_id_raw = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut pairing_id_raw);
    let pairing_id = hex::encode(pairing_id_raw).into_bytes();
    let keypair = generate_ed25519_keypair();
    let srp = SrpClient::new();

    let m1 = tlv_encode(&[(TLV_METHOD, &[0x00]), (TLV_STATE, &[0x01])]);
    debug!("pair-setup M1");
    let (status, body) = conn.post(PAIR_SETUP, &m1).await?;
    eprintln!("[hap] M1 status={status} len={}", body.len());
    if !(200..300).contains(&status) {
        eprintln!("[hap] M1 body: {}", hex::encode(&body));
        return Err(RottenError::Pairing(format!("M2 HTTP {status}")));
    }

    let m2 = tlv_decode(&body);
    if let Some(err) = m2.get(&TLV_ERROR) {
        let code = err.first().copied().unwrap_or(0xff);
        return Err(RottenError::Pairing(format!(
            "M2 pairing error TLV: {code}"
        )));
    }

    let salt = m2
        .get(&TLV_SALT)
        .ok_or_else(|| RottenError::Pairing("M2 missing salt".into()))?
        .clone();
    let server_pub = m2
        .get(&TLV_PUBLIC_KEY)
        .ok_or_else(|| RottenError::Pairing("M2 missing server public key".into()))?
        .clone();

    eprintln!(
        "[hap] M2 ok: salt={}B pubkey={}B",
        salt.len(),
        server_pub.len()
    );

    Ok(PairingSession {
        conn,
        device_id: device_identifier(device),
        pairing_id,
        salt,
        server_pub,
        srp,
        keypair,
    })
}

/// Complete pairing with the PIN shown on the TV (M3 through M6).
pub async fn finish_pairing(session: PairingSession, pin: &str) -> Result<DeviceCredentials> {
    let PairingSession {
        mut conn,
        device_id,
        pairing_id,
        salt,
        server_pub,
        mut srp,
        keypair,
    } = session;

    let (proof, expected_server_proof) = srp.process_challenge(&salt, &server_pub, USERNAME, pin);

    let m3 = tlv_encode(&[
        (TLV_STATE, &[0x03][..]),
        (TLV_PUBLIC_KEY, &srp.client_public()),
        (TLV_PROOF, &proof),
    ]);
    debug!("pair-setup M3");
    let (status3, body3) = conn.post(PAIR_SETUP, &m3).await?;
    let m4 = tlv_decode(&body3);
    eprintln!(
        "[hap] M4 status={} len={} tlv={}",
        status3,
        body3.len(),
        hex::encode(&body3)
    );

    let server_proof = m4
        .get(&TLV_PROOF)
        .ok_or_else(|| RottenError::Pairing("M4 missing server proof".into()))?;
    if server_proof != &expected_server_proof {
        return Err(RottenError::Pairing(
            "SRP server proof mismatch (wrong PIN?)".into(),
        ));
    }
    eprintln!("[hap] M4 server proof OK");

    let session_key = srp
        .session_key()
        .ok_or_else(|| RottenError::Pairing("missing SRP session key".into()))?;

    let sign_key = hkdf_sha512(
        b"Pair-Setup-Controller-Sign-Salt",
        b"Pair-Setup-Controller-Sign-Info",
        &session_key,
        32,
    );
    let enc_key_vec = hkdf_sha512(
        b"Pair-Setup-Encrypt-Salt",
        b"Pair-Setup-Encrypt-Info",
        &session_key,
        32,
    );
    let enc_key: [u8; 32] = enc_key_vec
        .try_into()
        .map_err(|_| RottenError::Pairing("bad encrypt key length".into()))?;

    let mut signed = Vec::with_capacity(32 + pairing_id.len() + 32);
    signed.extend_from_slice(&sign_key);
    signed.extend_from_slice(&pairing_id);
    signed.extend_from_slice(&keypair.public_key);
    let signature = ed25519_sign(&keypair.private_key, &signed);

    let sub_tlv = tlv_encode(&[
        (TLV_IDENTIFIER, &pairing_id),
        (TLV_PUBLIC_KEY, &keypair.public_key),
        (TLV_SIGNATURE, &signature),
    ]);
    let encrypted = chacha8_seal(&enc_key, b"PS-Msg05", &sub_tlv, &[]);

    let m5 = tlv_encode(&[(TLV_STATE, &[0x05][..]), (TLV_ENCRYPTED_DATA, &encrypted)]);
    debug!("pair-setup M5");
    let (status5, body5) = conn.post(PAIR_SETUP, &m5).await?;
    eprintln!("[hap] M6 status={} len={}", status5, body5.len());
    if !(200..300).contains(&status5) {
        eprintln!("[hap] M6 body: {}", hex::encode(&body5));
        return Err(RottenError::Pairing(format!("M5 failed: HTTP {status5}")));
    }

    let m6 = tlv_decode(&body5);
    if let Some(err) = m6.get(&TLV_ERROR) {
        let code = err.first().copied().unwrap_or(0xff);
        return Err(RottenError::Pairing(format!(
            "M6 pairing error TLV: {code}"
        )));
    }

    let encrypted_data = m6
        .get(&TLV_ENCRYPTED_DATA)
        .ok_or_else(|| RottenError::Pairing("M6 missing encrypted data".into()))?;
    let decrypted = chacha8_open(&enc_key, b"PS-Msg06", encrypted_data, &[])?;
    let sub = tlv_decode(&decrypted);

    let accessory_id = sub.get(&TLV_IDENTIFIER).cloned().unwrap_or_default();
    let accessory_public_key = sub
        .get(&TLV_PUBLIC_KEY)
        .cloned()
        .ok_or_else(|| RottenError::Pairing("M6 missing accessory public key".into()))?;

    eprintln!(
        "[hap] M6 ok: accessory_id={}B ltpk={}B",
        accessory_id.len(),
        accessory_public_key.len()
    );

    info!(device_id = %device_id, "HAP pairing complete");

    Ok(DeviceCredentials {
        device_id,
        identifier: String::from_utf8_lossy(&pairing_id).to_string(),
        public_key: keypair.public_key.to_vec(),
        private_key: keypair.private_key.to_vec(),
        server_public_key: accessory_public_key,
        hap: true,
        accessory_id,
    })
}

fn device_identifier(device: &AirPlayDevice) -> String {
    if device.device_id.is_empty() {
        device.host.clone()
    } else {
        device.device_id.clone()
    }
}

/// One-shot pairing when the PIN is already known (scripts, tests).
pub async fn pair_device(device: &AirPlayDevice, pin: &str) -> Result<DeviceCredentials> {
    let session = start_pairing(device).await?;
    finish_pairing(session, pin).await
}
