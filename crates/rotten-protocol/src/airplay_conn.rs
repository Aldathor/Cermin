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
const MAX_RESPONSE_HEADERS: usize = 64 * 1024;
const MAX_RESPONSE_BODY: usize = 8 * 1024 * 1024;
const CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub struct AirPlayRtspConn {
    stream: TcpStream,
    cseq: u32,
    hap: Option<HapChannel>,
    response_buffer: Vec<u8>,
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
        let addr = format!(
            "{}:{}",
            rotten_core::format_host_for_url(&device.host),
            device.port
        );
        let stream = tokio::time::timeout(CONTROL_TIMEOUT, TcpStream::connect(&addr))
            .await
            .map_err(|_| RottenError::Protocol(format!("timeout connecting to {addr}")))?
            .map_err(|e| RottenError::Protocol(format!("connect {addr}: {e}")))?;
        Ok(Self {
            stream,
            cseq: 0,
            hap: None,
            response_buffer: Vec::new(),
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
    pub async fn exchange_parts(&mut self, header: &[u8], body: &[u8]) -> Result<(u16, Vec<u8>)> {
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
                    if frame_len > HAP_FRAME_LENGTH {
                        return Err(RottenError::Protocol("HAP frame exceeds 1024 bytes".into()));
                    }
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
        tokio::time::timeout(CONTROL_TIMEOUT, self.read_response_inner())
            .await
            .map_err(|_| RottenError::Protocol("RTSP response timeout (30s)".into()))?
    }

    async fn read_response_inner(&mut self) -> Result<(u16, HashMap<String, String>, Vec<u8>)> {
        // Keep incomplete replies on the connection so a cancelled read (including
        // try_read's timeout) can resume without losing bytes already received.
        let mut tmp = [0u8; 4096];
        let header_end = loop {
            if let Some(end) = self
                .response_buffer
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
            {
                if end + 4 > MAX_RESPONSE_HEADERS {
                    return Err(RottenError::Protocol(
                        "RTSP response headers too large".into(),
                    ));
                }
                break end + 4;
            }
            if self.response_buffer.len() >= MAX_RESPONSE_HEADERS {
                return Err(RottenError::Protocol(
                    "RTSP response headers too large".into(),
                ));
            }
            let n = self.read_plain(&mut tmp).await?;
            if n == 0 {
                return Err(RottenError::Protocol(
                    "RTSP connection closed before headers".into(),
                ));
            }
            self.response_buffer.extend_from_slice(&tmp[..n]);
        };
        let header_text = std::str::from_utf8(&self.response_buffer[..header_end])
            .map_err(|_| RottenError::Protocol("RTSP headers are not UTF-8".into()))?;
        let status = parse_status(header_text)?;
        let headers = parse_headers(header_text);
        let mut content_length = None;
        for line in header_text.lines().skip(1) {
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    let value = value.trim();
                    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                        return Err(RottenError::Protocol("invalid RTSP Content-Length".into()));
                    }
                    let length = value
                        .parse::<usize>()
                        .map_err(|_| RottenError::Protocol("invalid RTSP Content-Length".into()))?;
                    if content_length.replace(length).is_some() {
                        return Err(RottenError::Protocol(
                            "duplicate RTSP Content-Length".into(),
                        ));
                    }
                }
            }
        }
        let content_length = content_length.unwrap_or(0);
        if content_length > MAX_RESPONSE_BODY {
            return Err(RottenError::Protocol("RTSP response body too large".into()));
        }
        let response_end = header_end + content_length;
        while self.response_buffer.len() < response_end {
            let remaining = (response_end - self.response_buffer.len()).min(tmp.len());
            let n = self.read_plain(&mut tmp[..remaining]).await?;
            if n == 0 {
                return Err(RottenError::Protocol(format!(
                    "truncated RTSP response: expected {content_length} body bytes, received {}",
                    self.response_buffer.len() - header_end
                )));
            }
            self.response_buffer.extend_from_slice(&tmp[..n]);
        }
        let body = self.response_buffer[header_end..response_end].to_vec();
        self.response_buffer.drain(..response_end);
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn connection_pair() -> (AirPlayRtspConn, TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (peer, _) = listener.accept().await.unwrap();
        (
            AirPlayRtspConn {
                stream,
                cseq: 0,
                hap: None,
                response_buffer: Vec::new(),
            },
            peer,
        )
    }

    async fn connection_with_reply(reply: Vec<u8>) -> AirPlayRtspConn {
        let (conn, mut peer) = connection_pair().await;
        tokio::spawn(async move {
            peer.write_all(&reply).await.unwrap();
        });
        conn
    }

    fn encrypt_reply(reply: &[u8], key: &[u8; 32]) -> Vec<u8> {
        let mut wire = Vec::new();
        for (counter, chunk) in reply.chunks(HAP_FRAME_LENGTH).enumerate() {
            let length = (chunk.len() as u16).to_le_bytes();
            wire.extend_from_slice(&length);
            wire.extend_from_slice(&chacha8_seal(
                key,
                &(counter as u64).to_le_bytes(),
                chunk,
                &length,
            ));
        }
        wire
    }

    #[tokio::test]
    async fn resumes_partial_headers_and_body_after_timeout() {
        let reply = b"RTSP/1.0 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        for split in [10, reply.len() - 2] {
            let (mut conn, mut peer) = connection_pair().await;
            peer.write_all(&reply[..split]).await.unwrap();
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(40),
                    conn.read_rtsp_response(),
                )
                .await
                .is_err()
            );
            assert_eq!(conn.response_buffer, reply[..split]);
            peer.write_all(&reply[split..]).await.unwrap();
            let (status, _, body) = conn.read_rtsp_response().await.unwrap();
            assert_eq!(status, 200);
            assert_eq!(body, b"hello");
        }
    }

    #[tokio::test]
    async fn resumes_fragmented_encrypted_frames_and_preserves_next_response() {
        let body = vec![b'x'; 2500];
        let mut reply =
            format!("RTSP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        reply.extend_from_slice(&body);
        reply.extend_from_slice(b"RTSP/1.0 204 OK\r\n\r\n");
        let key = [7; 32];
        let wire = encrypt_reply(&reply, &key);
        // Split inside the length prefix, then inside a later encrypted frame
        // after plaintext from an earlier frame has already been consumed.
        for split in [1, 2 + HAP_FRAME_LENGTH + HAP_TAG_LENGTH + 8] {
            let (mut conn, mut peer) = connection_pair().await;
            conn.enable_hap_encryption([0; 32], key);
            peer.write_all(&wire[..split]).await.unwrap();
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(40),
                    conn.read_rtsp_response(),
                )
                .await
                .is_err()
            );
            let hap = conn.hap.as_ref().unwrap();
            assert_eq!(hap.raw.len(), if split == 1 { 1 } else { 8 });
            assert_eq!(hap.in_counter, if split == 1 { 0 } else { 1 });
            peer.write_all(&wire[split..]).await.unwrap();
            let (status, _, received) = conn.read_rtsp_response().await.unwrap();
            assert_eq!(status, 200);
            assert_eq!(received, body);
            let (status, _, received) = conn.read_rtsp_response().await.unwrap();
            assert_eq!(status, 204);
            assert!(received.is_empty());
        }
    }

    #[tokio::test]
    async fn rejects_corrupt_encrypted_response() {
        let key = [7; 32];
        let mut wire = encrypt_reply(b"RTSP/1.0 200 OK\r\n\r\n", &key);
        *wire.last_mut().unwrap() ^= 1;
        let mut conn = connection_with_reply(wire).await;
        conn.enable_hap_encryption([0; 32], key);
        assert!(conn.read_rtsp_response().await.is_err());
    }

    #[tokio::test]
    async fn preserves_coalesced_responses() {
        let mut conn = connection_with_reply(
            b"RTSP/1.0 200 OK\r\nContent-Length: 3\r\n\r\nabcRTSP/1.0 204 OK\r\n\r\n".to_vec(),
        )
        .await;
        let (status, _, body) = conn.read_rtsp_response().await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"abc");
        let (status, _, body) = conn.read_rtsp_response().await.unwrap();
        assert_eq!(status, 204);
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn rejects_truncated_body() {
        let mut conn =
            connection_with_reply(b"RTSP/1.0 200 OK\r\nContent-Length: 6\r\n\r\nabc".to_vec())
                .await;
        assert!(
            conn.read_rtsp_response()
                .await
                .unwrap_err()
                .to_string()
                .contains("truncated")
        );
    }

    #[tokio::test]
    async fn rejects_invalid_duplicate_and_oversized_lengths() {
        for headers in [
            "Content-Length: invalid",
            "Content-Length: -1",
            "Content-Length: +1",
            "Content-Length: 8388609",
            "Content-Length: 1\r\ncontent-length: 2",
        ] {
            let mut conn =
                connection_with_reply(format!("RTSP/1.0 200 OK\r\n{headers}\r\n\r\n").into_bytes())
                    .await;
            assert!(conn.read_rtsp_response().await.is_err(), "{headers}");
        }
    }

    #[tokio::test]
    async fn rejects_oversized_headers() {
        let mut reply = b"RTSP/1.0 200 OK\r\nX-Padding: ".to_vec();
        reply.resize(MAX_RESPONSE_HEADERS + 1, b'x');
        reply.extend_from_slice(b"\r\n\r\n");
        let mut conn = connection_with_reply(reply).await;
        assert!(
            conn.read_rtsp_response()
                .await
                .unwrap_err()
                .to_string()
                .contains("headers too large")
        );
    }

    #[tokio::test]
    async fn rejects_oversized_encrypted_frame() {
        let mut conn = connection_with_reply(1025u16.to_le_bytes().to_vec()).await;
        conn.enable_hap_encryption([0; 32], [0; 32]);
        assert!(
            conn.read_rtsp_response()
                .await
                .unwrap_err()
                .to_string()
                .contains("HAP frame exceeds")
        );
    }
}
