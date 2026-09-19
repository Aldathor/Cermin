use rand::random;
use reqwest::Client;
use rotten_core::config::{DeviceCredentials, StreamConfig};
use rotten_core::debug_log::agent_log;
use rotten_core::device::AirPlayDevice;
use rotten_core::error::{Result, RottenError};
use rotten_crypto::{FairPlaySession, derive_session_keys};
use tracing::debug;

const AIRPLAY_USER_AGENT: &str = "AirPlay/320.20";
const MIRROR_STREAM_PORT: u16 = 7100;

/// Result of HTTP mirror setup.
pub struct MirrorSetupResult {
    pub stream_key: [u8; 32],
    pub event_port: Option<u16>,
    pub timing_port: Option<u16>,
}

pub async fn post_mirror_setup(
    client: &Client,
    device: &AirPlayDevice,
    creds: &DeviceCredentials,
    config: &StreamConfig,
    stream_port: u16,
    session_id: &str,
    fp: &FairPlaySession,
) -> Result<MirrorSetupResult> {
    let device_id_header = apple_device_id_header(creds);

    let reverse_status =
        post_reverse_ptth(client, device, creds, &device_id_header, session_id).await;
    // #region agent log
    agent_log(
        "http.rs:post_mirror_setup",
        "POST /reverse PTTH response",
        "M",
        serde_json::json!({
            "httpStatus": reverse_status,
            "deviceIdHeader": device_id_header,
        }),
    );
    // #endregion

    let stream_xml_status = get_mirror_stream_xml(client, device, &device_id_header).await;
    // #region agent log
    agent_log(
        "http.rs:post_mirror_setup",
        "GET /stream.xml on 7100",
        "M",
        serde_json::json!({ "httpStatus": stream_xml_status }),
    );
    // #endregion

    let session_id_int: i32 = random();
    let (aes_key, aes_iv) = FairPlaySession::random_aes_key_iv();
    let param1 = match fp.encrypt_aes_key(&aes_key) {
        Ok(blob) => {
            // #region agent log
            agent_log(
                "http.rs:post_mirror_setup",
                "FairPlay param1 encrypted",
                "N",
                serde_json::json!({ "param1Len": blob.len() }),
            );
            // #endregion
            Some(blob)
        }
        Err(e) => {
            // #region agent log
            agent_log(
                "http.rs:post_mirror_setup",
                "FairPlay param1 encrypt failed",
                "N",
                serde_json::json!({ "error": e.to_string() }),
            );
            // #endregion
            None
        }
    };

    post_mirror_stream(
        client,
        device,
        creds,
        &device_id_header,
        config,
        stream_port,
        session_id_int,
        param1.as_ref(),
        Some(&aes_iv),
    )
    .await?;

    let stream_key = if let Some(ref p1) = param1 {
        let mut key = [0u8; 32];
        key[..16].copy_from_slice(&aes_key);
        key[16..].copy_from_slice(&aes_iv);
        let _ = p1;
        key
    } else {
        let shared = creds.public_key.as_slice();
        let (_control_key, derived) =
            derive_session_keys(shared, b"AirPlay-Salt", b"AirPlay-Stream");
        derived
    };

    Ok(MirrorSetupResult {
        stream_key,
        event_port: Some(7010),
        timing_port: Some(7011),
    })
}

async fn post_reverse_ptth(
    client: &Client,
    device: &AirPlayDevice,
    creds: &DeviceCredentials,
    device_id_header: &str,
    session_id: &str,
) -> u16 {
    let url = format!("{}/reverse", device.base_url());
    debug!(url = %url, "POST /reverse PTTH");

    let resp = match client
        .post(&url)
        .header("User-Agent", AIRPLAY_USER_AGENT)
        .header("Connection", "Upgrade")
        .header("Upgrade", "PTTH/1.0")
        .header("X-Apple-Purpose", "event")
        .header("X-Apple-Session-ID", session_id)
        .header("X-Apple-Device-ID", device_id_header)
        .header("X-Apple-Client-Name", client_name(creds))
        .header("Content-Length", "0")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            debug!(error = %e, "reverse PTTH request failed");
            return 0;
        }
    };

    resp.status().as_u16()
}

async fn get_mirror_stream_xml(
    client: &Client,
    device: &AirPlayDevice,
    device_id_header: &str,
) -> u16 {
    let url = mirror_base_url(device, "/stream.xml");
    match client
        .get(&url)
        .header("User-Agent", AIRPLAY_USER_AGENT)
        .header("X-Apple-Device-ID", device_id_header)
        .send()
        .await
    {
        Ok(r) => r.status().as_u16(),
        Err(_) => 0,
    }
}

async fn post_mirror_stream(
    client: &Client,
    device: &AirPlayDevice,
    creds: &DeviceCredentials,
    device_id_header: &str,
    config: &StreamConfig,
    stream_port: u16,
    session_id: i32,
    param1: Option<&[u8; 72]>,
    param2: Option<&[u8; 16]>,
) -> Result<()> {
    let url = mirror_base_url(device, "/stream");
    let body = encode_mirror_stream_plist(creds, config, stream_port, session_id, param1, param2)?;

    debug!(url = %url, body_len = body.len(), hasParam1 = param1.is_some(), "POST /stream");

    let resp = client
        .post(&url)
        .header("User-Agent", AIRPLAY_USER_AGENT)
        .header("Connection", "keep-alive")
        .header("Content-Type", "application/x-apple-binary-plist")
        .header("X-Apple-Device-ID", device_id_header)
        .body(body)
        .send()
        .await
        .map_err(|e| RottenError::Protocol(format!("mirror stream: {e}")))?;

    let stream_status = resp.status().as_u16();
    // #region agent log
    agent_log(
        "http.rs:post_mirror_stream",
        "POST /stream response",
        "M",
        serde_json::json!({
            "httpStatus": stream_status,
            "mirrorPort": MIRROR_STREAM_PORT,
            "bodyLen": resp.content_length().unwrap_or(0),
            "deviceIdHeader": device_id_header,
            "hasParam1": param1.is_some(),
        }),
    );
    // #endregion

    if !resp.status().is_success() {
        return Err(RottenError::Protocol(format!(
            "mirror setup failed: {}",
            resp.status()
        )));
    }

    Ok(())
}

fn mirror_base_url(device: &AirPlayDevice, path: &str) -> String {
    format!(
        "http://{}:{}{}",
        rotten_core::debug_log::format_host_for_url(&device.host),
        MIRROR_STREAM_PORT,
        path
    )
}

fn encode_mirror_stream_plist(
    creds: &DeviceCredentials,
    config: &StreamConfig,
    stream_port: u16,
    session_id: i32,
    param1: Option<&[u8; 72]>,
    param2: Option<&[u8; 16]>,
) -> Result<Vec<u8>> {
    let device_id_int = mac_to_device_id_int(&creds.identifier);
    let mut dict = plist::Dictionary::new();
    dict.insert(
        "deviceID".into(),
        plist::Value::Integer(device_id_int.into()),
    );
    dict.insert("sessionID".into(), plist::Value::Integer(session_id.into()));
    dict.insert("version".into(), plist::Value::String("130.16".into()));
    dict.insert("latencyMs".into(), plist::Value::Integer(90.into()));
    dict.insert(
        "fpsInfo".into(),
        named_dict_array(&[
            "SubS", "B4En", "EnDp", "IdEn", "IdDp", "EQDp", "QueF", "Sent",
        ]),
    );
    dict.insert(
        "timestampInfo".into(),
        named_dict_array(&[
            "SubSu", "BePxT", "AfPxT", "BefEn", "EmEnc", "QueFr", "SndFr",
        ]),
    );
    dict.insert(
        "timestampSourcePort".into(),
        plist::Value::Integer(stream_port.into()),
    );
    dict.insert("width".into(), plist::Value::Integer(config.width.into()));
    dict.insert("height".into(), plist::Value::Integer(config.height.into()));
    dict.insert("fps".into(), plist::Value::Integer(config.fps.into()));

    if let Some(p1) = param1 {
        dict.insert("param1".into(), plist::Value::Data(p1.to_vec()));
    }
    if let Some(p2) = param2 {
        dict.insert("param2".into(), plist::Value::Data(p2.to_vec()));
    }

    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &plist::Value::Dictionary(dict))
        .map_err(|e| RottenError::Protocol(format!("mirror plist: {e}")))?;
    Ok(buf)
}

fn named_dict_array(names: &[&str]) -> plist::Value {
    plist::Value::Array(
        names
            .iter()
            .map(|name| {
                let mut d = plist::Dictionary::new();
                d.insert("name".into(), plist::Value::String((*name).into()));
                plist::Value::Dictionary(d)
            })
            .collect(),
    )
}

fn mac_to_device_id_int(device_id: &str) -> i64 {
    let hex: String = device_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    if hex.is_empty() {
        return 0;
    }
    let trimmed = if hex.len() > 12 { &hex[..12] } else { &hex };
    u64::from_str_radix(trimmed, 16).unwrap_or(0) as i64
}

fn apple_device_id_header(creds: &DeviceCredentials) -> String {
    let hex: String = creds
        .identifier
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    if hex.len() >= 12 {
        return format!("0x{}", &hex[..12]);
    }
    let device_hex: String = creds
        .device_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    if device_hex.len() >= 12 {
        return format!("0x{}", &device_hex[..12]);
    }
    format!("0x{hex}")
}

fn client_name(creds: &DeviceCredentials) -> String {
    if creds.device_id.is_empty() {
        "Cermin".into()
    } else {
        creds.device_id.clone()
    }
}

pub async fn send_feedback(client: &Client, device: &AirPlayDevice) -> Result<()> {
    let url = format!("{}/feedback", device.base_url());
    let _ = client.post(&url).send().await;
    Ok(())
}

/// Keep-alive POST /feedback every 2s (doubletake-style).
pub fn spawn_feedback_loop(device: AirPlayDevice) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = match Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        loop {
            let _ = send_feedback(&client, &device).await;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    })
}
