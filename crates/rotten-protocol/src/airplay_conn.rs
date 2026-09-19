//! Single persistent RTSP/1.0 connection to the AirPlay control port
//! (pair-verify + fp-setup + mirror negotiation), optionally HAP-encrypted.

use std::collections::HashMap;

use rotten_core::debug_log::agent_log;
use rotten_core::device::AirPlayDevice;
use rotten_core::error::{Result, RottenError};
use rotten_crypto::{chacha8_open, chacha8_seal};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const AIRPLAY_USER_AGENT: &str = "AirPlay/320.20";
const HAP_FRAME_LENGTH: usize = 1024;
const HAP_TAG_LENGTH: usize = 16;

pub struct AirPlayRtspConn {
    stream: TcpStream,
    cseq: u32,
    hap: Option<HapChannel>,
}

struct HapChannel {
    out_key: [u8; 32],
    in_key: [u8; 32],
    out_counter: u64,
    in_counter: u64,
    plain: Vec<u8>,
    raw: Vec<u8>,
}

impl AirPlayRtspConn {
    pub async fn connect(device: &AirPlayDevice) -> Result<Self> {
        let addr = format!("{}:{}", device.host, device.port);
        let stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| RottenError::Protocol(format!("connect {addr}: {e}")))?;
        Ok(Self {
            stream,
            cseq: 0,
            hap: None,
        })
    }

    /// Enable HAP control-channel encryption with the pair-verify derived keys.
    pub fn enable_hap_encryption(&mut self, out_key: [u8; 32], in_key: [u8; 32]) {
        self.hap = Some(HapChannel {
            out_key,
            in_key,
            out_counter: 0,
            in_counter: 0,
            plain: Vec::new(),
            raw: Vec::new(),
        });
    }

    /// Plaintext HTTP POST used for HAP pair-verify (before encryption is enabled).
    pub async fn post_hap_http(&mut self, path: &str, body: &[u8]) -> Result<(u16, Vec<u8>)> {
        self.cseq += 1;
        let seq = self.cseq;
        let header = format!(
            "POST {path} HTTP/1.1\r\n\
             CSeq: {seq}\r\n\
             User-Agent: {AIRPLAY_USER_AGENT}\r\n\
             X-Apple-HKP: 3\r\n\
             Content-Type: application/octet-stream\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        );
        self.write_plain(header.as_bytes()).await?;
        self.write_plain(body).await?;
        let (status, _, resp_body) = self.read_rtsp_response().await?;
        Ok((status, resp_body))
    }

    /// Pair-verify style POST (`X-Apple-ProtocolVersion: 1`).
    pub async fn post_pair_verify(&mut self, path: &str, body: &[u8]) -> Result<(u16, Vec<u8>)> {
        self.post(
            path,
            "application/octet-stream",
            body,
            &[("X-Apple-ProtocolVersion", "1")],
            "U",
        )
        .await
    }

    /// FairPlay fp-setup POST (`X-Apple-ET: 32` only — no DACP headers).
    pub async fn post_fp_setup(&mut self, body: &[u8]) -> Result<(u16, Vec<u8>)> {
        self.post(
            "/fp-setup",
            "application/octet-stream",
            body,
            &[("X-Apple-ET", "32")],
            "U",
        )
        .await
    }

    /// Send arbitrary bytes over the (optionally HAP-encrypted) channel and read one response.
    pub async fn exchange(&mut self, request: &[u8]) -> Result<(u16, Vec<u8>)> {
        self.write_plain(request).await?;
        let (status, _headers, body) = self.read_rtsp_response().await?;
        Ok((status, body))
    }

    /// Like `exchange`, but returns response headers too (diagnostics).
    pub async fn exchange_full(
        &mut self,
        request: &[u8],
    ) -> Result<(u16, HashMap<String, String>, Vec<u8>)> {
        self.write_plain(request).await?;
        self.read_rtsp_response().await
    }

    /// Two separate HAP frames for header and body (sender's historical framing).
    pub async fn exchange_parts(
        &mut self,
        header: &[u8],
        body: &[u8],
    ) -> Result<(u16, Vec<u8>)> {
        self.write_plain(header).await?;
        self.write_plain(body).await?;
        let (status, _headers, body) = self.read_rtsp_response().await?;
        Ok((status, body))
    }

    /// Write raw request bytes (diagnostics).
    pub async fn send(&mut self, request: &[u8]) -> Result<()> {
        self.write_plain(request).await
    }

    /// Read one response with a timeout; `Ok(None)` on timeout (diagnostics).
    pub async fn try_read(&mut self, secs: u64) -> Result<Option<(u16, Vec<u8>)>> {
        match tokio::time::timeout(
            std::time::Duration::from_secs(secs),
            self.read_rtsp_response(),
        )
        .await
        {
            Ok(Ok((status, _headers, body))) => Ok(Some((status, body))),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(None),
        }
    }

    /// RTSP SETUP with binary plist body (mirror negotiation).
    pub async fn rtsp_setup(
        &mut self,
        uri: &str,
        body: &[u8],
        dacp_id: &str,
        active_remote: u32,
    ) -> Result<(u16, Vec<u8>)> {
        let active_remote_str = active_remote.to_string();
        let (status, _, body) = self
            .rtsp_request(
                "SETUP",
                uri,
                "application/x-apple-binary-plist",
                body,
                &[
                    ("DACP-ID", dacp_id),
                    ("Active-Remote", active_remote_str.as_str()),
                ],
                "X",
            )
            .await?;
        Ok((status, body))
    }

    /// Local address of the RTSP socket (advertised to PTP peers).
    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.stream.local_addr().ok()
    }

    /// RTSP SETPEERS carrying the PTP peer list (binary plist array).
    pub async fn rtsp_set_peers(
        &mut self,
        uri: &str,
        session_uuid: &str,
        body: &[u8],
        dacp_id: &str,
        active_remote: u32,
    ) -> Result<(u16, Vec<u8>)> {
        let active_remote_str = active_remote.to_string();
        let (status, _, resp_body) = self
            .rtsp_request(
                "SETPEERS",
                uri,
                "application/x-apple-binary-plist",
                body,
                &[
                    ("Session", session_uuid),
                    ("DACP-ID", dacp_id),
                    ("Active-Remote", active_remote_str.as_str()),
                ],
                "H-PTP",
            )
            .await?;
        Ok((status, resp_body))
    }

    /// RTSP RECORD on the audio stream URI.
    pub async fn rtsp_record(
        &mut self,
        uri: &str,
        session_uuid: &str,
        dacp_id: &str,
        active_remote: u32,
    ) -> Result<(u16, Vec<u8>, Option<u32>)> {
        let active_remote_str = active_remote.to_string();
        let (status, headers, body) = self
            .rtsp_request(
                "RECORD",
                uri,
                "",
                &[],
                &[
                    ("Session", session_uuid),
                    ("DACP-ID", dacp_id),
                    ("Active-Remote", active_remote_str.as_str()),
                ],
                "X",
            )
            .await?;
        let audio_latency = headers
            .get("audio-latency")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&v| v > 0);
        Ok((status, body, audio_latency))
    }

    /// RTSP SET_PARAMETER (e.g. volume on audio URI after RECORD).
    pub async fn rtsp_set_parameter(
        &mut self,
        uri: &str,
        session_uuid: &str,
        body: &[u8],
    ) -> Result<(u16, Vec<u8>)> {
        let (status, _, resp_body) = self
            .rtsp_request(
                "SET_PARAMETER",
                uri,
                "text/parameters",
                body,
                &[("Session", session_uuid)],
                "X",
            )
            .await?;
        Ok((status, resp_body))
    }

    /// RTSP POST /feedback on the persistent mirror control connection (doubletake-style).
    pub async fn rtsp_post_feedback(&mut self) -> Result<(u16, Vec<u8>)> {
        self.post("/feedback", "application/octet-stream", &[], &[], "H35")
            .await
    }

    /// RTSP GET_PARAMETER keepalive (doubletake heartbeatLoop).
    pub async fn rtsp_get_parameter(
        &mut self,
        uri: &str,
        session_uuid: &str,
    ) -> Result<(u16, Vec<u8>)> {
        let (status, _, body) = self
            .rtsp_request(
                "GET_PARAMETER",
                uri,
                "",
                &[],
                &[("Session", session_uuid)],
                "H118",
            )
            .await?;
        Ok((status, body))
    }

    async fn rtsp_request(
        &mut self,
        method: &str,
        uri: &str,
        content_type: &str,
        body: &[u8],
        extra: &[(&str, &str)],
        hypothesis_id: &str,
    ) -> Result<(u16, HashMap<String, String>, Vec<u8>)> {
        self.cseq += 1;
        let seq = self.cseq;

        let mut header = format!(
            "{method} {uri} RTSP/1.0\r\n\
             CSeq: {seq}\r\n\
             User-Agent: {AIRPLAY_USER_AGENT}\r\n"
        );
        for (name, value) in extra {
            header.push_str(&format!("{name}: {value}\r\n"));
        }
        if !content_type.is_empty() && !body.is_empty() {
            header.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        if method == "RECORD" {
            header.push_str("Range: npt=0-\r\n");
            header.push_str("RTP-Info: seq=0;rtptime=0\r\n");
        }
        header.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));

        // #region agent log
        agent_log(
            "airplay_conn.rs:rtsp_request",
            "RTSP request",
            hypothesis_id,
            serde_json::json!({
                "method": method,
                "uri": uri,
                "cseq": seq,
                "bodyLen": body.len(),
            }),
        );
        // #endregion

        self.write_plain(header.as_bytes()).await?;
        if !body.is_empty() {
            self.write_plain(body).await?;
        }

        let (status, headers, resp_body) = self.read_rtsp_response().await?;

        // #region agent log
        agent_log(
            "airplay_conn.rs:rtsp_request",
            "RTSP response",
            hypothesis_id,
            serde_json::json!({
                "method": method,
                "uri": uri,
                "cseq": seq,
                "httpStatus": status,
                "bodyLen": resp_body.len(),
                "audioLatency": headers.get("audio-latency"),
            }),
        );
        // #endregion

        Ok((status, headers, resp_body))
    }

    async fn post(
        &mut self,
        path: &str,
        content_type: &str,
        body: &[u8],
        extra: &[(&str, &str)],
        hypothesis_id: &str,
    ) -> Result<(u16, Vec<u8>)> {
        self.cseq += 1;
        let seq = self.cseq;

        let mut header = format!(
            "POST {path} RTSP/1.0\r\n\
             CSeq: {seq}\r\n\
             User-Agent: {AIRPLAY_USER_AGENT}\r\n"
        );
        for (name, value) in extra {
            header.push_str(&format!("{name}: {value}\r\n"));
        }
        header.push_str(&format!(
            "Content-Type: {content_type}\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        ));

        // #region agent log
        agent_log(
            "airplay_conn.rs:post",
            "RTSP request",
            hypothesis_id,
            serde_json::json!({
                "path": path,
                "cseq": seq,
                "bodyLen": body.len(),
                "protocol": "RTSP/1.0",
                "extraHeaders": extra.iter().map(|(k,v)| format!("{k}:{v}")).collect::<Vec<_>>(),
            }),
        );
        // #endregion

        self.write_plain(header.as_bytes()).await?;
        self.write_plain(body).await?;

        let (status, _, resp_body) = self.read_rtsp_response().await?;

        // #region agent log
        agent_log(
            "airplay_conn.rs:post",
            "RTSP response",
            hypothesis_id,
            serde_json::json!({
                "path": path,
                "cseq": seq,
                "httpStatus": status,
                "bodyLen": resp_body.len(),
            }),
        );
        // #endregion

        Ok((status, resp_body))
    }

    /// Write plaintext (HAP-framed when encryption is enabled).
    async fn write_plain(&mut self, data: &[u8]) -> Result<()> {
        if self.hap.is_some() {
            // #region agent log
            agent_log(
                "airplay_conn.rs:write_plain",
                "hap plaintext frame",
                "WIRE",
                serde_json::json!({
                    "len": data.len(),
                    "hex": hex::encode(data),
                }),
            );
            // #endregion
            let mut framed = Vec::with_capacity(data.len() + 32);
            {
                let hap = self.hap.as_mut().expect("hap checked");
                for chunk in data.chunks(HAP_FRAME_LENGTH) {
                    let len = (chunk.len() as u16).to_le_bytes();
                    let nonce = hap.out_counter.to_le_bytes();
                    let sealed = chacha8_seal(&hap.out_key, &nonce, chunk, &len);
                    hap.out_counter += 1;
                    framed.extend_from_slice(&len);
                    framed.extend_from_slice(&sealed);
                }
            }
            self.stream
                .write_all(&framed)
                .await
                .map_err(|e| RottenError::Protocol(format!("RTSP write: {e}")))?;
        } else {
            self.stream
                .write_all(data)
                .await
                .map_err(|e| RottenError::Protocol(format!("RTSP write: {e}")))?;
        }
        Ok(())
    }

    /// Read plaintext (decrypting HAP frames when encryption is enabled).
    async fn read_plain(&mut self, out: &mut [u8]) -> Result<usize> {
        if self.hap.is_none() {
            return self
                .stream
                .read(out)
                .await
                .map_err(|e| RottenError::Protocol(format!("RTSP read: {e}")));
        }

        let mut tmp = [0u8; 8192];
        loop {
            {
                let hap = self.hap.as_mut().expect("hap checked");

                if !hap.plain.is_empty() {
                    let n = hap.plain.len().min(out.len());
                    out[..n].copy_from_slice(&hap.plain[..n]);
                    hap.plain.drain(..n);
                    return Ok(n);
                }

                loop {
                    if hap.raw.len() < 2 {
                        break;
                    }
                    let frame_len = u16::from_le_bytes([hap.raw[0], hap.raw[1]]) as usize;
                    let total = 2 + frame_len + HAP_TAG_LENGTH;
                    if hap.raw.len() < total {
                        break;
                    }
                    let aad = [hap.raw[0], hap.raw[1]];
                    let ciphertext = hap.raw[2..total].to_vec();
                    let nonce = hap.in_counter.to_le_bytes();
                    let plaintext = chacha8_open(&hap.in_key, &nonce, &ciphertext, &aad)?;
                    hap.in_counter += 1;
                    hap.raw.drain(..total);
                    hap.plain.extend_from_slice(&plaintext);
                }

                if !hap.plain.is_empty() {
                    continue;
                }
            }

            let (hap_raw_len, out_counter, in_counter) = {
                let hap = self.hap.as_ref().expect("hap checked");
                (hap.raw.len(), hap.out_counter, hap.in_counter)
            };

            let read = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.stream.read(&mut tmp),
            )
            .await
            .map_err(|_| {
                agent_log(
                    "airplay_conn.rs:read_plain",
                    "RTSP read timeout",
                    "RAW",
                    serde_json::json!({
                        "pendingRaw": hap_raw_len,
                        "outCounter": out_counter,
                        "inCounter": in_counter,
                    }),
                );
                RottenError::Protocol("RTSP read timeout (30s)".into())
            })?
            .map_err(|e| RottenError::Protocol(format!("RTSP read: {e}")))?;
            let n = read;
            if n == 0 {
                return Ok(0);
            }
            // #region agent log
            agent_log(
                "airplay_conn.rs:read_plain",
                "raw encrypted bytes read",
                "RAW",
                serde_json::json!({
                    "n": n,
                    "prefix": hex::encode(&tmp[..n.min(16)]),
                }),
            );
            // #endregion
            self.hap
                .as_mut()
                .expect("hap checked")
                .raw
                .extend_from_slice(&tmp[..n]);
        }
    }

    async fn read_rtsp_response(&mut self) -> Result<(u16, HashMap<String, String>, Vec<u8>)> {
        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 4096];
        loop {
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            let n = self.read_plain(&mut tmp).await?;
            if n == 0 {
                return Err(RottenError::Protocol("RTSP connection closed".into()));
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.len() > 64 * 1024 {
                return Err(RottenError::Protocol(
                    "RTSP response headers too large".into(),
                ));
            }
        }

        let header_end = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| RottenError::Protocol("RTSP malformed headers".into()))?
            + 4;
        let header_text = String::from_utf8_lossy(&buf[..header_end]);
        let status = parse_status(&header_text)?;
        let headers = parse_headers(&header_text);

        let mut body = buf[header_end..].to_vec();
        let content_length = headers
            .get("content-length")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);

        while body.len() < content_length {
            let n = self.read_plain(&mut tmp).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(content_length);

        Ok((status, headers, body))
    }
}

fn parse_status(headers: &str) -> Result<u16> {
    let first = headers
        .lines()
        .next()
        .ok_or_else(|| RottenError::Protocol("RTSP empty response".into()))?;
    let status = first
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| RottenError::Protocol(format!("RTSP bad status line: {first}")))?
        .parse()
        .map_err(|_| RottenError::Protocol(format!("RTSP bad status code: {first}")))?;
    Ok(status)
}

fn parse_headers(header_text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in header_text.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            map.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    map
}
