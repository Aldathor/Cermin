use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tracing::{debug, info};

use crate::encoder::{ENCODER_BUILD_ID, EncodedFrame, LazyEncoder};
use crate::mirror_packet::{
    bias_from_audio_latency_samples, build_avcc_config, build_codec_header, build_heartbeat_header,
    build_video_header, nal_type_summary, nals_to_avcc, ntp_time_from_elapsed_ns,
    partition_access_unit,
};
use crate::pacing::FramePacer;
use rotten_core::config::HwAccel;
use rotten_core::debug_log::agent_log;
use rotten_core::error::{Result, RottenError};
use rotten_crypto::{MirrorAesCtr, MirrorVideoCrypto, StreamCipher};

/// Streaming statistics.
#[derive(Debug, Default)]
pub struct StreamStats {
    pub frames_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub dropped_frames: AtomicU64,
}

/// Sends encrypted H.264 frames to Apple TV over TCP.
pub struct MirrorStreamer {
    host: String,
    port: u16,
    video_crypto: MirrorVideoCrypto,
    stats: Arc<StreamStats>,
}

impl MirrorStreamer {
    pub fn new(host: String, port: u16, video_crypto: MirrorVideoCrypto) -> Self {
        Self {
            host,
            port,
            video_crypto,
            stats: Arc::new(StreamStats::default()),
        }
    }

    pub fn stats(&self) -> Arc<StreamStats> {
        self.stats.clone()
    }

    /// Stream frames on an already-connected data TCP socket (opened during mirror setup).
    pub async fn stream_from_channel_on(
        &self,
        mut stream: TcpStream,
        width: u32,
        height: u32,
        codec_header_width: u32,
        codec_header_height: u32,
        presentation_width: u32,
        presentation_height: u32,
        timestamp_bias_samples: u32,
        fps: u32,
        bitrate_kbps: u32,
        hw_accel: HwAccel,
        mut frame_rx: LatestFrameReceiver,
        mut first_frame_tx: Option<tokio::sync::oneshot::Sender<()>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        info!("streaming on pre-connected Apple TV data socket");
        self.run_stream_loop(
            &mut stream,
            width,
            height,
            codec_header_width,
            codec_header_height,
            presentation_width,
            presentation_height,
            timestamp_bias_samples,
            fps,
            bitrate_kbps,
            hw_accel,
            &mut frame_rx,
            &mut first_frame_tx,
            stop,
        )
        .await
    }

    /// Stream frames received on the channel until sender is dropped.
    pub async fn stream_from_channel(
        &self,
        width: u32,
        height: u32,
        codec_header_width: u32,
        codec_header_height: u32,
        presentation_width: u32,
        presentation_height: u32,
        timestamp_bias_samples: u32,
        fps: u32,
        bitrate_kbps: u32,
        hw_accel: HwAccel,
        mut frame_rx: LatestFrameReceiver,
        mut first_frame_tx: Option<tokio::sync::oneshot::Sender<()>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        let addr = format!("{}:{}", self.host, self.port);
        let mut stream = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| RottenError::Video(format!("timeout connecting to {addr}")))?
        .map_err(|e| RottenError::Video(format!("connect {addr}: {e}")))?;

        info!(%addr, "connected to Apple TV data port");
        self.run_stream_loop(
            &mut stream,
            width,
            height,
            codec_header_width,
            codec_header_height,
            presentation_width,
            presentation_height,
            timestamp_bias_samples,
            fps,
            bitrate_kbps,
            hw_accel,
            &mut frame_rx,
            &mut first_frame_tx,
            stop,
        )
        .await
    }

    async fn run_stream_loop(
        &self,
        stream: &mut TcpStream,
        width: u32,
        height: u32,
        codec_header_width: u32,
        codec_header_height: u32,
        presentation_width: u32,
        presentation_height: u32,
        timestamp_bias_samples: u32,
        fps: u32,
        bitrate_kbps: u32,
        hw_accel: HwAccel,
        frame_rx: &mut LatestFrameReceiver,
        first_frame_tx: &mut Option<tokio::sync::oneshot::Sender<()>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        // #region agent log
        agent_log(
            "stream.rs:run_stream_loop",
            "stream loop entered",
            "H11",
            serde_json::json!({
                "width": width,
                "height": height,
                "codecHeaderW": codec_header_width,
                "codecHeaderH": codec_header_height,
                "presentationW": presentation_width,
                "presentationH": presentation_height,
                "timestampBiasSamples": timestamp_bias_samples,
                "fps": fps,
                "bitrateKbps": bitrate_kbps,
                "buildId": ENCODER_BUILD_ID,
            }),
        );
        // #endregion

        let encoder = Arc::new(Mutex::new(LazyEncoder::new(
            width,
            height,
            bitrate_kbps,
            fps,
            hw_accel,
        )));
        let mut cipher = MirrorCipher::from_crypto(self.video_crypto.clone());
        // #region agent log
        agent_log(
            "stream.rs:run_stream_loop",
            "mirror cipher initialized",
            "H25",
            serde_json::json!({
                "cipher": self.video_crypto.mode_name(),
            }),
        );
        // #endregion
        let mut pacer = FramePacer::new(fps);
        let mut pts_us: u64 = 0;
        let frame_interval_us = 1_000_000 / fps.max(1) as u64;
        let mut codec_sent = false;
        let mut stream_width = width;
        let mut stream_height = height;
        let mut first_frame_sent = false;
        let heartbeat_start = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let mut heartbeat =
            tokio::time::interval_at(heartbeat_start, std::time::Duration::from_secs(1));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let timestamp_bias = bias_from_audio_latency_samples(timestamp_bias_samples);
        let mut au_index: u64 = 0;
        let mut stream_watchdog = tokio::time::interval(std::time::Duration::from_secs(2));
        stream_watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut stop_tick = tokio::time::interval(std::time::Duration::from_millis(250));
        stop_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            if stop.load(Ordering::Relaxed) {
                info!("stop requested; ending mirror stream");
                break;
            }
            tokio::select! {
                frame = frame_rx.recv() => {
                    match frame {
                        Some((rgba, w, h, capture_elapsed_ns)) => {
                            pacer.wait().await;
                            let enc = encoder.clone();
                            let pts = pts_us;
                            let encoded = tokio::task::spawn_blocking(move || {
                                let mut enc = enc.lock().expect("encoder mutex");
                                enc.encode(&rgba, w, h, pts)
                            })
                            .await
                            .map_err(|e| RottenError::Video(format!("encode task join: {e}")))??;
                            pts_us += frame_interval_us;
                            if let Some(enc) = encoded {
                                if enc.coded_width != stream_width || enc.coded_height != stream_height {
                                    // #region agent log
                                    if au_index == 0 {
                                        agent_log(
                                            "stream.rs:run_stream_loop",
                                            "coded size changed — resetting codec frame",
                                            "H112",
                                            serde_json::json!({
                                                "fromW": stream_width,
                                                "fromH": stream_height,
                                                "toW": enc.coded_width,
                                                "toH": enc.coded_height,
                                            }),
                                        );
                                    }
                                    // #endregion
                                    codec_sent = false;
                                    stream_width = enc.coded_width;
                                    stream_height = enc.coded_height;
                                }
                                au_index += 1;
                                self.send_access_unit(
                                    stream,
                                    &mut cipher,
                                    enc,
                                    &mut codec_sent,
                                    au_index,
                                    stream_width,
                                    stream_height,
                                    presentation_width,
                                    presentation_height,
                                    timestamp_bias,
                                    capture_elapsed_ns,
                                    first_frame_tx,
                                )
                                    .await?;
                                first_frame_sent = true;
                            }
                        }
                        None => break,
                    }
                }
                _ = stop_tick.tick() => {
                    if stop.load(Ordering::Relaxed) {
                        info!("stop requested; ending mirror stream");
                        break;
                    }
                }
                _ = stream_watchdog.tick() => {
                    let sent = self.stats.frames_sent.load(Ordering::Relaxed);
                    let dropped = self.stats.dropped_frames.load(Ordering::Relaxed);
                    let queue_dropped = frame_rx.dropped_frames();
                    if queue_dropped > dropped {
                        self.stats
                            .dropped_frames
                            .store(queue_dropped, Ordering::Relaxed);
                    }
                    // #region agent log
                    agent_log(
                        "stream.rs:run_stream_loop",
                        "stream loop alive",
                        "H22",
                        serde_json::json!({
                            "framesSent": sent,
                            "droppedFrames": queue_dropped,
                        }),
                    );
                    // #endregion
                }
                _ = heartbeat.tick(), if first_frame_sent => {
                    let header = build_heartbeat_header();
                    // #region agent log
                    agent_log(
                        "stream.rs:heartbeat",
                        "data channel heartbeat sent",
                        "H5",
                        serde_json::json!({ "headerType": header[4] }),
                    );
                    // #endregion
                    if let Err(e) = stream.write_all(&header).await {
                        // #region agent log
                        agent_log(
                            "stream.rs:heartbeat",
                            "heartbeat write failed",
                            "H5",
                            serde_json::json!({ "error": e.to_string() }),
                        );
                        // #endregion
                        return Err(RottenError::Video(format!("heartbeat write: {e}")));
                    }
                }
            }
        }

        info!(
            frames = self.stats.frames_sent.load(Ordering::Relaxed),
            bytes = self.stats.bytes_sent.load(Ordering::Relaxed),
            "stream ended"
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_access_unit(
        &self,
        stream: &mut TcpStream,
        cipher: &mut MirrorCipher,
        frame: EncodedFrame,
        codec_sent: &mut bool,
        au_index: u64,
        content_width: u32,
        content_height: u32,
        display_width: u32,
        display_height: u32,
        timestamp_bias: std::time::Duration,
        capture_elapsed_ns: u64,
        first_frame_tx: &mut Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<()> {
        let ts = ntp_time_from_elapsed_ns(capture_elapsed_ns, timestamp_bias);
        let nal_types = nal_type_summary(&frame.data);
        let (sps, pps, vcl_nals) = partition_access_unit(&frame.data);

        // #region agent log
        if au_index <= 3 {
            agent_log(
                "stream.rs:send_access_unit",
                "access unit NAL layout",
                "H19",
                serde_json::json!({
                    "auIndex": au_index,
                    "nalTypes": nal_types,
                    "h264Bytes": frame.data.len(),
                    "keyframe": frame.is_keyframe,
                    "codedW": frame.coded_width,
                    "codedH": frame.coded_height,
                }),
            );
        }
        // #endregion

        if frame.is_keyframe && !*codec_sent {
            if let (Some(sps), Some(pps)) = (sps.as_ref(), pps.as_ref()) {
                let avcc = build_avcc_config(sps, pps);
                let header = build_codec_header(
                    content_width,
                    content_height,
                    display_width,
                    display_height,
                    avcc.len(),
                    ts,
                );
                self.write_packet(stream, &header, &avcc, "codec").await?;
                *codec_sent = true;
                // #region agent log
                agent_log(
                    "stream.rs:send_access_unit",
                    "codec frame sent (avcC)",
                    "H72",
                    serde_json::json!({
                        "avccBytes": avcc.len(),
                        "contentW": content_width,
                        "contentH": content_height,
                        "displayW": display_width,
                        "displayH": display_height,
                        "ntpTs": ts,
                        "ntpTsSec": ts >> 32,
                        "timestampBiasMs": timestamp_bias.as_millis(),
                        "codedW": frame.coded_width,
                        "codedH": frame.coded_height,
                    }),
                );
                agent_log(
                    "stream.rs:send_access_unit",
                    "SPS/PPS details for codec frame",
                    "H26",
                    serde_json::json!({
                        "spsPrefix": hex::encode(&sps[..sps.len().min(12)]),
                        "ppsPrefix": hex::encode(&pps[..pps.len().min(12)]),
                        "avccPrefix": hex::encode(&avcc[..avcc.len().min(16)]),
                        "codedW": frame.coded_width,
                        "codedH": frame.coded_height,
                    }),
                );
                // #endregion
            }
        }

        let vcl = if vcl_nals.is_empty() {
            let (_, _, vcl_only) = partition_access_unit(&frame.data);
            vcl_only
        } else {
            vcl_nals
        };
        if vcl.is_empty() {
            return Ok(());
        }

        let avcc_payload = nals_to_avcc(&vcl);
        let vcl_len = avcc_payload.len();
        let (header, encrypted) = match cipher {
            MirrorCipher::None => {
                let header = build_video_header(vcl_len, frame.is_keyframe, ts);
                (header, avcc_payload)
            }
            MirrorCipher::Aes(c) => {
                let encrypted = c.encrypt_frame(&avcc_payload);
                let header = build_video_header(vcl_len, frame.is_keyframe, ts);
                (header, encrypted)
            }
            MirrorCipher::ChaCha(c) => {
                let encrypted_len = vcl_len + 16;
                let header = build_video_header(encrypted_len, frame.is_keyframe, ts);
                let encrypted = c.encrypt_mirror_vcl(&avcc_payload, &header)?;
                // #region agent log
                if au_index == 1 {
                    let tag_start = encrypted.len().saturating_sub(16);
                    agent_log(
                        "stream.rs:send_access_unit",
                        "chacha first frame crypto details",
                        "H98",
                        serde_json::json!({
                            "plainPrefix": hex::encode(&avcc_payload[..avcc_payload.len().min(16)]),
                            "cipherPrefix": hex::encode(&encrypted[..encrypted.len().min(16)]),
                            "tagPrefix": hex::encode(&encrypted[tag_start..]),
                            "headerPrefix": hex::encode(&header[..16]),
                            "nonceCounter": 0u64,
                        }),
                    );
                }
                // #endregion
                (header, encrypted)
            }
        };

        // #region agent log
        if au_index <= 2 {
            agent_log(
                "stream.rs:send_access_unit",
                "VCL packet prepared",
                "H36",
                serde_json::json!({
                    "auIndex": au_index,
                    "cipher": cipher.mode_name(),
                    "vclBytes": vcl_len,
                    "payloadLenHeader": u32::from_le_bytes(header[0..4].try_into().unwrap()),
                    "encryptedBytes": encrypted.len(),
                    "keyframe": frame.is_keyframe,
                    "header5": header[5],
                }),
            );
        }
        // #endregion

        self.write_packet(stream, &header, &encrypted, "vcl")
            .await?;

        if au_index == 1 {
            if let Some(tx) = first_frame_tx.take() {
                let _ = tx.send(());
            }
        }

        // #region agent log
        if au_index <= 2 {
            agent_log(
                "stream.rs:send_access_unit",
                "VCL packet write completed",
                "H21",
                serde_json::json!({
                    "auIndex": au_index,
                    "vclBytes": vcl_len,
                    "encryptedBytes": encrypted.len(),
                    "payloadLenHeader": u32::from_le_bytes(header[0..4].try_into().unwrap()),
                    "cipher": cipher.mode_name(),
                    "keyframe": frame.is_keyframe,
                    "header5": header[5],
                }),
            );
        }
        // #endregion

        self.stats.frames_sent.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_sent
            .fetch_add((header.len() + encrypted.len()) as u64, Ordering::Relaxed);

        // #region agent log
        let sent = self.stats.frames_sent.load(Ordering::Relaxed);
        if sent == 1 || sent == 2 || sent % 30 == 0 {
            agent_log(
                "stream.rs:send_access_unit",
                "encrypted VCL frame sent",
                "H4",
                serde_json::json!({
                    "framesSent": sent,
                    "auIndex": au_index,
                    "vclBytes": vcl_len,
                    "encryptedBytes": encrypted.len(),
                    "keyframe": frame.is_keyframe,
                }),
            );
        }
        // #endregion

        debug!(
            pts = frame.pts_us,
            keyframe = frame.is_keyframe,
            vcl_bytes = vcl_len,
            "sent mirror frame"
        );
        Ok(())
    }

    async fn write_packet(
        &self,
        stream: &mut TcpStream,
        header: &[u8; 128],
        payload: &[u8],
        kind: &str,
    ) -> Result<()> {
        let mut packet = Vec::with_capacity(128 + payload.len());
        packet.extend_from_slice(header);
        packet.extend_from_slice(payload);
        if let Err(e) = stream.write_all(&packet).await {
            // #region agent log
            agent_log(
                "stream.rs:write_packet",
                "mirror packet write failed",
                "H66",
                serde_json::json!({
                    "kind": kind,
                    "error": e.to_string(),
                    "headerType": header[4],
                    "payloadLen": payload.len(),
                    "totalLen": packet.len(),
                }),
            );
            // #endregion
            return Err(RottenError::Video(format!("write {kind} packet: {e}")));
        }
        Ok(())
    }
}

pub type FrameItem = (Vec<u8>, u32, u32, u64);

struct LatestFrameInner {
    slot: Mutex<Option<FrameItem>>,
    notify: Notify,
}

/// Sender for a single-slot frame queue that drops stale frames when the encoder falls behind.
pub struct LatestFrameSender {
    inner: Arc<LatestFrameInner>,
    dropped: Arc<AtomicU64>,
}

/// Receiver for [`LatestFrameSender`].
pub struct LatestFrameReceiver {
    inner: Arc<LatestFrameInner>,
    dropped: Arc<AtomicU64>,
}

impl LatestFrameSender {
    pub fn send(&self, frame: FrameItem) {
        let mut slot = self.inner.slot.lock().expect("frame slot mutex");
        if slot.is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        *slot = Some(frame);
        drop(slot);
        self.inner.notify.notify_one();
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl LatestFrameReceiver {
    pub async fn recv(&mut self) -> Option<FrameItem> {
        loop {
            if let Some(frame) = self.inner.slot.lock().expect("frame slot mutex").take() {
                return Some(frame);
            }
            if Arc::strong_count(&self.inner) == 1 {
                return None;
            }
            self.inner.notify.notified().await;
        }
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

pub fn frame_channel() -> (LatestFrameSender, LatestFrameReceiver) {
    let dropped = Arc::new(AtomicU64::new(0));
    let inner = Arc::new(LatestFrameInner {
        slot: Mutex::new(None),
        notify: Notify::new(),
    });
    (
        LatestFrameSender {
            inner: inner.clone(),
            dropped: dropped.clone(),
        },
        LatestFrameReceiver { inner, dropped },
    )
}

enum MirrorCipher {
    None,
    Aes(MirrorAesCtr),
    ChaCha(StreamCipher),
}

impl MirrorCipher {
    fn from_crypto(crypto: MirrorVideoCrypto) -> Self {
        match crypto {
            MirrorVideoCrypto::None => Self::None,
            MirrorVideoCrypto::AesCtr { key, iv } => Self::Aes(MirrorAesCtr::new(&key, &iv)),
            MirrorVideoCrypto::ChaCha { key } => Self::ChaCha(StreamCipher::new(&key)),
        }
    }

    fn mode_name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Aes(_) => "aes-ctr",
            Self::ChaCha(_) => "chacha",
        }
    }
}
