//! AirPlay session probe: pairs with stored credentials, then sends candidate
//! requests over the HAP-encrypted channel and reports which ones the receiver
//! answers. Diagnostic tool for receivers that ignore SETUP.

use anyhow::Result;
use plist::{Dictionary, Value};
use rotten_core::config::resolve_credentials_path;
use rotten_discovery::resolve_device;
use rotten_pairing::PairingManager;
use rotten_protocol::airplay_conn::AirPlayRtspConn;
use rotten_protocol::hap_pair_verify_conn;

const HOST: &str = "192.168.137.247";
const PORT: u16 = 47439;
const REPLY_TIMEOUT_SECS: u64 = 8;

enum Request {
    Single(Vec<u8>),
    Split(Vec<u8>, Vec<u8>),
    Sequence(Vec<Vec<u8>>),
}

fn build(
    method: &str,
    path: &str,
    protocol: &str,
    cseq: u32,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let mut out =
        format!("{method} {path} {protocol}\r\nCSeq: {cseq}\r\nUser-Agent: AirPlay/320.20\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        out.push_str("Content-Type: application/x-apple-binary-plist\r\n");
    }
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn plist_bytes(dict: Dictionary) -> Vec<u8> {
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &Value::Dictionary(dict)).expect("plist encode");
    buf
}

fn audio_stream(stream_id: i64) -> Dictionary {
    let mut s = Dictionary::new();
    s.insert("type".into(), 96.into());
    s.insert("streamConnectionID".into(), stream_id.into());
    s.insert("ct".into(), 2.into());
    s.insert("spf".into(), 352.into());
    s.insert("sr".into(), 44100.into());
    s.insert("audioFormat".into(), 0x40000i64.into());
    s.insert("controlPort".into(), 49156i64.into());
    s.insert("audioMode".into(), "default".into());
    s.insert("usingScreen".into(), true.into());
    s.insert("latencyMin".into(), 22050i64.into());
    s.insert("latencyMax".into(), 22050i64.into());
    s.insert("redundantAudio".into(), 0.into());
    s.insert("disableRetransmits".into(), true.into());
    s.insert("isMedia".into(), true.into());
    s.insert("supportsDynamicStreamID".into(), false.into());
    s.insert("shk".into(), Value::Data(vec![0x42; 32]));
    s
}

fn video_stream(stream_id: i64, with_keys: bool) -> Dictionary {
    let mut s = Dictionary::new();
    s.insert("type".into(), 110.into());
    s.insert("streamConnectionID".into(), stream_id.into());
    let mut ti = Vec::new();
    for name in ["SubSu", "BePxT", "AfPxT", "BefEn", "EmEnc"] {
        let mut e = Dictionary::new();
        e.insert("name".into(), name.into());
        ti.push(Value::Dictionary(e));
    }
    s.insert("timestampInfo".into(), Value::Array(ti));
    if with_keys {
        s.insert("shk".into(), Value::Data(vec![0x11; 16]));
        s.insert("shiv".into(), Value::Data(vec![0x22; 16]));
    }
    s
}

fn root_plist(
    device_id: i64,
    session: &str,
    streams: Vec<Dictionary>,
    mirroring: bool,
    timing_protocol: &str,
) -> Vec<u8> {
    let mut d = Dictionary::new();
    d.insert("deviceID".into(), device_id.into());
    d.insert("macAddress".into(), device_id.into());
    d.insert("sessionUUID".into(), session.into());
    d.insert("sourceVersion".into(), "280.33".into());
    if mirroring {
        d.insert("isScreenMirroringSession".into(), true.into());
    }
    d.insert("timingProtocol".into(), timing_protocol.into());
    d.insert("timingPort".into(), 49155i64.into());
    d.insert("osBuildVersion".into(), "13F69".into());
    d.insert("model".into(), "Linux".into());
    d.insert("name".into(), "Linux".into());
    d.insert(
        "streams".into(),
        Value::Array(streams.into_iter().map(Value::Dictionary).collect()),
    );
    plist_bytes(d)
}

fn fmt_plist(body: &[u8]) -> String {
    if body.len() < 6 || &body[..6] != b"bplist" {
        return format!("<non-plist {} bytes>", body.len());
    }
    let Ok(value) = plist::from_bytes::<Value>(body) else {
        return format!("<unparseable plist {} bytes>", body.len());
    };
    fn fmt(v: &Value) -> String {
        match v {
            Value::Dictionary(d) => {
                let items: Vec<String> =
                    d.iter().map(|(k, v)| format!("{k}: {}", fmt(v))).collect();
                format!("{{{}}}", items.join(", "))
            }
            Value::Array(a) => {
                let items: Vec<String> = a.iter().map(fmt).collect();
                format!("[{}]", items.join(", "))
            }
            Value::Data(d) => {
                let hex: String = d.iter().take(16).map(|b| format!("{b:02x}")).collect();
                format!("<{}B {hex}>", d.len())
            }
            other => format!("{other:?}"),
        }
    }
    fmt(&value)
}

async fn run_probe(
    device: &rotten_core::device::AirPlayDevice,
    creds: &rotten_core::config::DeviceCredentials,
    name: &str,
    request: Request,
) {
    let total = match &request {
        Request::Single(bytes) => bytes.len(),
        Request::Split(header, body) => header.len() + body.len(),
        Request::Sequence(reqs) => reqs.iter().map(Vec::len).sum(),
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(REPLY_TIMEOUT_SECS * 3),
        async {
            let mut conn = AirPlayRtspConn::connect(device).await?;
            let pv = hap_pair_verify_conn(&mut conn, device, creds).await?;
            if let Some(keys) = pv.hap_keys {
                conn.enable_hap_encryption(keys.out_key, keys.in_key);
            }
            match request {
                Request::Single(bytes) => conn.exchange_full(&bytes).await,
                Request::Split(header, body) => {
                    let (status, body) = conn.exchange_parts(&header, &body).await?;
                    Ok((status, std::collections::HashMap::new(), body))
                }
                Request::Sequence(reqs) => {
                    for (i, req) in reqs.iter().enumerate() {
                        println!("    seq[{i}]: sending {}B", req.len());
                        conn.send(req).await?;
                        match conn.try_read(4).await? {
                            Some((status, body)) => {
                                let prefix: String =
                                    body.iter().take(24).map(|b| format!("{b:02x}")).collect();
                                println!(
                                    "    seq[{i}]: -> HTTP {status}, body {}B [{}]\n      {}",
                                    body.len(),
                                    prefix,
                                    fmt_plist(&body)
                                );
                            }
                            None => println!("    seq[{i}]: -> no response in 4s"),
                        }
                    }
                    Ok((0u16, std::collections::HashMap::new(), Vec::new()))
                }
            }
        },
    )
    .await;

    match result {
        Ok(Ok((0, _, _))) => println!("=== {name} ({total}B request)\n    -> sequence done"),
        Ok(Ok((status, headers, body))) => {
            let prefix: String = body.iter().take(32).map(|b| format!("{b:02x}")).collect();
            let interesting: Vec<String> = headers
                .iter()
                .filter(|(k, _)| {
                    k.starts_with("x-") || k.as_str() == "server" || k.as_str() == "content-type"
                })
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            println!(
                "=== {name} ({}B request)\n    -> HTTP {status}, body {} bytes [{}], headers: {}\n    {}",
                total,
                body.len(),
                prefix,
                interesting.join(", "),
                fmt_plist(&body)
            );
        }
        Ok(Err(e)) => println!("=== {name} ({total}B request)\n    -> ERROR {e}"),
        Err(_) => println!(
            "=== {name} ({total}B request)\n    -> TIMEOUT ({REPLY_TIMEOUT_SECS}s, no response)"
        ),
    }
}

async fn resolve_target() -> Result<(String, u16)> {
    let host = std::env::var("PROBE_HOST").unwrap_or_default();
    let port = std::env::var("PROBE_PORT").unwrap_or_default();
    if !host.is_empty() {
        let port = if port.is_empty() {
            PORT
        } else {
            port.parse().unwrap_or(PORT)
        };
        return Ok((host, port));
    }
    println!("PROBE_HOST not set; browsing mDNS for AirPlay receivers...");
    match rotten_discovery::discover_for(std::time::Duration::from_secs(6)).await {
        Ok(devices) => {
            if let Some(d) = devices
                .iter()
                .find(|d| d.model.as_deref() == Some("LSP7") || d.name.contains("Samsung"))
                .or_else(|| devices.first())
            {
                println!("discovered {} at {}:{}", d.name, d.host, d.port);
                return Ok((d.host.clone(), d.port));
            }
            println!("no AirPlay receivers discovered; falling back to {HOST}:{PORT}");
        }
        Err(e) => println!("discovery failed ({e}); falling back to {HOST}:{PORT}"),
    }
    Ok((HOST.to_string(), PORT))
}

#[tokio::main]
async fn main() -> Result<()> {
    let (host, port) = resolve_target().await?;
    let device = resolve_device(&host, port).await?;
    let mut pairing = PairingManager::load(resolve_credentials_path(None))?;
    let creds = pairing.pair(&device, None, false).await?;

    if let Ok(path) = std::env::var("PROBE_REPLAY_HEX") {
        let hex_str = std::fs::read_to_string(&path)?;
        let bytes = hex::decode(hex_str.trim())?;
        println!("replaying {} bytes from {path}", bytes.len());
        if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let header = bytes[..pos + 4].to_vec();
            let body = bytes[pos + 4..].to_vec();
            run_probe(
                &device,
                &creds,
                "replay-split",
                Request::Split(header, body),
            )
            .await;
        } else {
            run_probe(&device, &creds, "replay-single", Request::Single(bytes)).await;
        }
        return Ok(());
    }

    let dacp = [
        ("DACP-ID", "0011223344556677"),
        ("Active-Remote", "12345678"),
    ];
    let hex: String = creds
        .identifier
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    let real_dev = i64::from_str_radix(&hex[..hex.len().min(12)], 16).unwrap_or(0);
    let uuid = "e83f1f2e-1234-4abc-9def-0123456789ab";
    let audio_id: i64 = 1_111_111_111;
    let video_id: i64 = 2_222_222_222;
    let audio_uri = format!("rtsp://{host}:{port}/{audio_id}");
    let video_uri = format!("rtsp://{host}:{port}/{video_id}");

    let audio_ptp = build(
        "SETUP",
        &audio_uri,
        "RTSP/1.0",
        3,
        &dacp,
        &root_plist(real_dev, uuid, vec![audio_stream(audio_id)], false, "PTP"),
    );
    let video_ptp = build(
        "SETUP",
        &video_uri,
        "RTSP/1.0",
        4,
        &dacp,
        &root_plist(
            real_dev,
            uuid,
            vec![video_stream(video_id, true)],
            true,
            "PTP",
        ),
    );
    let record_req = build("RECORD", &audio_uri, "RTSP/1.0", 5, &dacp, &[]);
    let combined_ptp = build(
        "SETUP",
        &audio_uri,
        "RTSP/1.0",
        3,
        &dacp,
        &root_plist(
            real_dev,
            uuid,
            vec![audio_stream(audio_id), video_stream(video_id, true)],
            true,
            "PTP",
        ),
    );

    let mut candidates: Vec<(String, Request)> = Vec::new();
    candidates.push(("audio SETUP PTP".into(), Request::Single(audio_ptp.clone())));
    candidates.push((
        "seq audio PTP + video PTP + RECORD".into(),
        Request::Sequence(vec![
            audio_ptp.clone(),
            video_ptp.clone(),
            record_req.clone(),
        ]),
    ));
    candidates.push((
        "combined audio+video PTP".into(),
        Request::Single(combined_ptp),
    ));

    for (name, request) in candidates {
        run_probe(&device, &creds, &name, request).await;
    }

    Ok(())
}
