//! Minimal mirror audio RTP (ChaCha + ALAC silence) — Apple TV expects audio after frame 1.

use std::time::Duration;

use plist::Value;
use rotten_core::debug_log::agent_log;
use rotten_core::device::DeviceFeatures;
use rotten_core::error::{Result, RottenError};
use rotten_crypto::chacha64_seal;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio::time::{Duration as TokioDuration, MissedTickBehavior, interval};

use alac_encoder::{AlacEncoder, FormatDescription};

use crate::ntp::ntp_boot_with_epoch;

const AUDIO_SPF: u16 = 352;
/// Legacy default kept for callers; prefer [`playout_latency_samples`].
pub const AUDIO_LATENCY_SAMPLES: u32 = 44;
const TARGET_LATENCY_MS: u64 = 1;
const AUDIO_CHACHA_NONCE_SIZE: usize = 8;
/// owntone/cliairplay PTP anchor constants (44.1 kHz frames).
const PTP_ANCHOR_FRAME_1_OFFSET: u32 = 11_035;
const PTP_ANCHOR_BUFFER_FRAMES: u32 = 77_175;

/// Playout latency in 44.1 kHz samples (doubletake `samplesFor44k1(TargetLatency())`).
/// `CERMIN_AUDIO_LATENCY_MS` overrides the receiver floor for tuning
/// (higher = more buffering = more stable audio, more A/V lag).
pub fn playout_latency_samples(features: &DeviceFeatures) -> u32 {
    let floor_ms = if features.raw == 0 {
        // `/info` or mDNS features missing: mirror targets Apple TV; use low latency.
        0
    } else {
        features.playout_latency_floor_ms()
    };
    let override_ms = std::env::var("CERMIN_AUDIO_LATENCY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    let target_ms = override_ms.unwrap_or_else(|| TARGET_LATENCY_MS.max(floor_ms));
    samples_for_44k1(Duration::from_millis(target_ms))
}

fn samples_for_44k1(d: Duration) -> u32 {
    let samples = (d.as_secs_f64() * 44_100.0).round() as i64;
    samples.clamp(1, i64::from(u32::MAX)) as u32
}

/// Parsed audio stream ports from RTSP SETUP response (stream type 96).
pub fn plist_audio_ports(body: &[u8]) -> Option<(u16, u16)> {
    let value: Value = plist::from_bytes(body).ok()?;
    let dict = value.as_dictionary()?;
    let streams = dict.get("streams")?.as_array()?;
    for stream in streams {
        let sd = stream.as_dictionary()?;
        if plist_int(sd.get("type")?) != 96 {
            continue;
        }
        let (data, control) = plist_stream_ports(sd);
        if data > 0 && control > 0 {
            return Some((data, control));
        }
    }
    None
}

/// Match doubletake `plistStreamPorts`: legacy dataPort/controlPort or streamConnections RTP/RTCP keys.
fn plist_stream_ports(stream: &plist::Dictionary) -> (u16, u16) {
    let mut data_port =
        plist_int(stream.get("dataPort").unwrap_or(&Value::Integer(0.into()))) as u16;
    let mut control_port = plist_int(
        stream
            .get("controlPort")
            .unwrap_or(&Value::Integer(0.into())),
    ) as u16;

    if let Some(sc) = stream
        .get("streamConnections")
        .and_then(|v| v.as_dictionary())
    {
        if let Some(rtp) = sc
            .get("streamConnectionTypeRTP")
            .and_then(|v| v.as_dictionary())
        {
            if let Some(port) = rtp.get("streamConnectionKeyPort") {
                let p = plist_int(port);
                if p > 0 {
                    data_port = p as u16;
                }
            }
        }
        if let Some(rtcp) = sc
            .get("streamConnectionTypeRTCP")
            .and_then(|v| v.as_dictionary())
        {
            if let Some(port) = rtcp.get("streamConnectionKeyPort") {
                let p = plist_int(port);
                if p > 0 {
                    control_port = p as u16;
                }
            }
        }
    }

    (data_port, control_port)
}

fn plist_int(value: &Value) -> i64 {
    match value {
        Value::Integer(i) => i.as_signed().unwrap_or(0),
        Value::Real(f) => *f as i64,
        _ => 0,
    }
}

/// Timing flavour for the audio control-channel sync packets.
#[derive(Debug, Clone, Copy)]
pub enum AudioTiming {
    /// Legacy NTP form (0x90d4/0x80d4, 20 bytes).
    Ntp,
    /// AP2 PTP anchor form (0x90d7/0x80d7, 28 bytes) for receivers slaved to
    /// the session's PTP clock.
    Ptp { clock_id: u64 },
}

/// UDP sockets + keys needed to stream the mirror audio after the first video
/// frame. When `pcm_rx` is set the loop sends captured 44.1 kHz stereo S16
/// interleaved PCM; otherwise it streams silence (or the test tone).
pub struct MirrorAudioSetup {
    pub host: String,
    pub chacha_key: [u8; 32],
    pub remote_data_port: u16,
    pub remote_control_port: u16,
    pub ctrl_socket: UdpSocket,
    pub data_socket: UdpSocket,
    pub latency_samples: u32,
    pub timing: AudioTiming,
    pub pcm_rx: Option<tokio::sync::mpsc::Receiver<Vec<u8>>>,
}

/// Spawn silence audio RTP after the first video frame broadcast fires.
pub fn spawn_mirror_audio_silence(
    setup: MirrorAudioSetup,
    mut first_frame: tokio::sync::broadcast::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if first_frame.recv().await.is_err() {
            return;
        }
        if let Err(e) = run_audio_silence_loop(setup).await {
            agent_log(
                "audio_rtp.rs:run_audio_silence_loop",
                "mirror audio stream error",
                "H61",
                serde_json::json!({ "error": e.to_string() }),
            );
        }
    })
}

async fn run_audio_silence_loop(mut setup: MirrorAudioSetup) -> Result<()> {
    let data_addr = format!("{}:{}", setup.host, setup.remote_data_port);
    let ctrl_addr = format!("{}:{}", setup.host, setup.remote_control_port);

    let chacha_key = setup.chacha_key;
    let ssrc: u32 = 0;
    let tone = std::env::var("CERMIN_AUDIO_TONE").is_ok();
    let mut tone_phase: f64 = 0.0;
    let mut alac = AlacFrames::new();
    const FRAME_BYTES: usize = AUDIO_SPF as usize * 2 * 2; // stereo S16
    const PCM_BUFFER_MAX: usize = 44_100 * 4; // 1 s of stereo S16
    let alac_frame = alac.encode(&vec![0u8; FRAME_BYTES]);
    let mut pcm_rx = setup.pcm_rx.take();
    let mut pcm_buf: Vec<u8> = Vec::new();

    // #region agent log
    agent_log(
        "audio_rtp.rs:run_audio_silence_loop",
        "mirror audio silence starting",
        "H61",
        serde_json::json!({
            "remoteDataPort": setup.remote_data_port,
            "remoteControlPort": setup.remote_control_port,
            "alacBytes": alac_frame.len(),
            "dataAddr": data_addr,
        }),
    );
    // #endregion

    // PTP audio anchors are expressed on the receiver's clock; wait for the
    // PTP slave lock (or give up after 3 s) before freezing the anchor line.
    if matches!(setup.timing, AudioTiming::Ptp { .. }) {
        let deadline = tokio::time::Instant::now() + TokioDuration::from_secs(3);
        while rotten_core::ntp::session_offset_ns() == 0
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(TokioDuration::from_millis(20)).await;
        }
    }

    let latency_samples = setup.latency_samples;
    let mut ptp_anchor = PtpAnchor::default();
    // PTP anchor packets are what let the receiver schedule audio playback.
    // CERMIN_NO_AUDIO_SYNC=1 disables them for debugging.
    let send_syncs = std::env::var("CERMIN_NO_AUDIO_SYNC").is_err();
    for i in 0..1 {
        if !send_syncs {
            break;
        }
        let rtp_now = match setup.timing {
            AudioTiming::Ntp => 0,
            AudioTiming::Ptp { .. } => latency_samples,
        };
        send_sync(&setup, &ctrl_addr, rtp_now, i == 0, &mut ptp_anchor).await?;
        if i == 0 {
            // #region agent log
            agent_log(
                "audio_rtp.rs:run_audio_silence_loop",
                "audio sync burst at rtp=0",
                "H71",
                serde_json::json!({
                    "syncPackets": 1,
                    "latencySamples": latency_samples,
                }),
            );
            // #endregion
        }
    }

    let mut seq: u16 = 1;
    let mut rtp_time: u32 = latency_samples;
    let frame_samples = AUDIO_SPF as u32;

    let mut ticker = interval(TokioDuration::from_millis(
        (u64::from(frame_samples) * 1000 / 44100).max(1),
    ));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut sync_fast = interval(TokioDuration::from_millis(500));
    sync_fast.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut packets_sent: u64 = 0;
    let mut audio_nonce: u64 = 0;
    // Windows timer granularity is ~15 ms, so a plain 7 ms ticker starves the
    // receiver (~80 pkt/s instead of 125). Pace against elapsed time and send
    // the deficit as a catch-up burst every tick.
    let stream_start = tokio::time::Instant::now();

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Some(rx) = pcm_rx.as_mut() {
                    while let Ok(chunk) = rx.try_recv() {
                        pcm_buf.extend_from_slice(&chunk);
                    }
                    if pcm_buf.len() > PCM_BUFFER_MAX {
                        let excess = pcm_buf.len() - PCM_BUFFER_MAX;
                        pcm_buf.drain(..excess);
                    }
                }
                let elapsed_ns = stream_start.elapsed().as_nanos() as u64;
                let target = elapsed_ns * 44_100 / (u64::from(frame_samples) * 1_000_000_000)
                    + 1;
                while packets_sent < target {
                    let frame = if tone {
                        alac.encode(&tone_pcm(AUDIO_SPF, &mut tone_phase))
                    } else if pcm_rx.is_some() {
                        let mut pcm = vec![0u8; FRAME_BYTES];
                        let take = pcm_buf.len().min(FRAME_BYTES);
                        pcm[..take].copy_from_slice(&pcm_buf[..take]);
                        pcm_buf.drain(..take);
                        alac.encode(&pcm)
                    } else {
                        alac_frame.clone()
                    };
                    send_audio_packet(
                        &setup.data_socket,
                        &data_addr,
                        &chacha_key,
                        &frame,
                        rtp_time,
                        seq,
                        ssrc,
                        audio_nonce,
                    ).await?;
                    seq = seq.wrapping_add(1);
                    rtp_time = rtp_time.wrapping_add(frame_samples);
                    audio_nonce = audio_nonce.wrapping_add(1);
                    packets_sent += 1;
                }
                if packets_sent == 1 || packets_sent % 200 == 0 {
                    agent_log(
                        "audio_rtp.rs:run_audio_silence_loop",
                        "audio RTP packet sent",
                        "H61",
                        serde_json::json!({
                            "packetsSent": packets_sent,
                            "seq": seq,
                            "rtpTime": rtp_time,
                        }),
                    );
                }
            }
            _ = sync_fast.tick(), if send_syncs => {
                let sync_rtp = sync_rtp_for(rtp_time, latency_samples);
                // #region agent log
                if sync_rtp <= AUDIO_SPF as u32 {
                    agent_log(
                        "audio_rtp.rs:run_audio_silence_loop",
                        "audio periodic sync",
                        "H62",
                        serde_json::json!({
                            "currentRtp": rtp_time,
                            "syncRtp": sync_rtp,
                            "nextRtp": rtp_time,
                        }),
                    );
                }
                // #endregion
                let _ = send_sync(&setup, &ctrl_addr, rtp_time, false, &mut ptp_anchor).await;
            }
        }
    }
}

/// Anchor state for the PTP audio timeline (frozen at the first sync).
#[derive(Default)]
struct PtpAnchor {
    wall0_ns: Option<u64>,
    pos0: u32,
}

async fn send_sync(
    setup: &MirrorAudioSetup,
    ctrl_addr: &str,
    rtp_now: u32,
    is_first: bool,
    anchor: &mut PtpAnchor,
) -> Result<()> {
    match setup.timing {
        AudioTiming::Ntp => {
            send_sync_packet(
                &setup.ctrl_socket,
                ctrl_addr,
                ntp_boot_with_epoch(),
                rtp_now,
                setup.latency_samples,
                is_first,
            )
            .await
        }
        AudioTiming::Ptp { clock_id } => {
            send_sync_packet_ptp(
                &setup.ctrl_socket,
                ctrl_addr,
                clock_id,
                rtp_now,
                setup.latency_samples,
                is_first,
                anchor,
            )
            .await
        }
    }
}

/// AP2 PTP anchor packet (owntone `sync_packet_ptp` / cliairplay
/// `ap2_send_sync_packet_ptp`): frozen anchor line on the PTP session clock.
#[allow(clippy::too_many_arguments)]
async fn send_sync_packet_ptp(
    socket: &UdpSocket,
    remote: &str,
    clock_id: u64,
    rtp_now: u32,
    latency_samples: u32,
    is_first: bool,
    anchor: &mut PtpAnchor,
) -> Result<()> {
    let wall_ns = rotten_core::ntp::session_now_ns();
    if anchor.wall0_ns.is_none() {
        anchor.wall0_ns = Some(wall_ns);
        anchor.pos0 = rtp_now;
    }
    let wall0 = anchor.wall0_ns.unwrap_or(wall_ns);
    // Frozen anchor line (owntone/cliairplay): the sample currently rendering
    // is pos0 + (wall - wall0)*rate - lead. The lead gives the receiver time to
    // buffer; without it every packet is already too late and gets dropped.
    let lead_frames = i64::from(latency_samples);
    let elapsed_ns = wall_ns as i64 - wall0 as i64;
    let play_pos =
        (i64::from(anchor.pos0) + elapsed_ns * 44_100 / 1_000_000_000 - lead_frames).max(0) as u32;
    let frame_1 = play_pos.wrapping_add(PTP_ANCHOR_FRAME_1_OFFSET);
    let frame_2 = frame_1.wrapping_add(PTP_ANCHOR_BUFFER_FRAMES);

    // The anchor must name the timeline's master clock: the receiver's own PTP
    // identity when it is the elected master, otherwise ours.
    let peer_clock = crate::ptp::peer_clock_id();
    let anchor_clock_id = if peer_clock != 0 { peer_clock } else { clock_id };

    let mut packet = [0u8; 28];
    packet[0] = if is_first { 0x90 } else { 0x80 };
    packet[1] = 0xd7;
    packet[2..4].copy_from_slice(&6u16.to_be_bytes());
    packet[4..8].copy_from_slice(&frame_1.to_be_bytes());
    packet[8..16].copy_from_slice(&wall_ns.to_be_bytes());
    packet[16..20].copy_from_slice(&frame_2.to_be_bytes());
    packet[20..28].copy_from_slice(&anchor_clock_id.to_be_bytes());

    if is_first {
        // #region agent log
        agent_log(
            "audio_rtp.rs:send_sync_packet_ptp",
            "audio PTP anchor packet",
            "H-PTP-A",
            serde_json::json!({
                "wallNs": wall_ns,
                "playPos": play_pos,
                "frame1": frame_1,
                "frame2": frame_2,
                "clockId": format!("{clock_id:016x}"),
                "latencySamples": latency_samples,
            }),
        );
        // #endregion
    }

    socket
        .send_to(&packet, remote)
        .await
        .map_err(|e| RottenError::Protocol(format!("audio PTP anchor send: {e}")))?;
    Ok(())
}

async fn send_audio_packet(
    socket: &UdpSocket,
    remote: &str,
    chacha_key: &[u8; 32],
    payload: &[u8],
    rtp_time: u32,
    seq: u16,
    ssrc: u32,
    nonce: u64,
) -> Result<()> {
    let mut header = [0u8; 12];
    header[0] = 0x80;
    header[1] = 0x60;
    header[2..4].copy_from_slice(&seq.to_be_bytes());
    header[4..8].copy_from_slice(&rtp_time.to_be_bytes());
    header[8..12].copy_from_slice(&ssrc.to_be_bytes());

    // Apple senders use a monotonic 64-bit counter as the ChaCha nonce; the
    // receiver reads the 8-byte nonce from the packet trailer verbatim.
    let nonce_suffix = nonce.to_le_bytes();
    let aad = &header[4..12];

    let sealed = chacha64_seal(chacha_key, &nonce_suffix, payload, aad);

    let mut packet = Vec::with_capacity(12 + sealed.len() + AUDIO_CHACHA_NONCE_SIZE);
    packet.extend_from_slice(&header);
    packet.extend_from_slice(&sealed);
    packet.extend_from_slice(&nonce_suffix);

    if seq == 1 {
        // #region agent log
        agent_log(
            "audio_rtp.rs:send_audio_packet",
            "first audio packet chacha20-poly1305",
            "H69",
            serde_json::json!({
                "cipher": "chacha20-poly1305",
                "packetLen": packet.len(),
                "sealedLen": sealed.len(),
                "payloadLen": payload.len(),
                "seq": seq,
                "nonce": nonce,
            }),
        );
        // #endregion
    }

    socket
        .send_to(&packet, remote)
        .await
        .map_err(|e| RottenError::Protocol(format!("audio RTP send: {e}")))?;
    Ok(())
}

fn sync_rtp_for(rtp_now: u32, latency_samples: u32) -> u32 {
    if rtp_now >= latency_samples {
        rtp_now - latency_samples
    } else {
        rtp_now
    }
}

async fn send_sync_packet(
    socket: &UdpSocket,
    remote: &str,
    ntp_time: u64,
    rtp_now: u32,
    latency_samples: u32,
    is_first: bool,
) -> Result<()> {
    let mut packet = [0u8; 20];
    packet[0] = if is_first { 0x90 } else { 0x80 };
    packet[1] = 0xd4;
    packet[2..4].copy_from_slice(&4u16.to_be_bytes());
    let sync_rtp = sync_rtp_for(rtp_now, latency_samples);
    packet[4..8].copy_from_slice(&sync_rtp.to_be_bytes());
    packet[8..16].copy_from_slice(&ntp_time.to_be_bytes());
    packet[16..20].copy_from_slice(&rtp_now.to_be_bytes());
    if is_first {
        // #region agent log
        agent_log(
            "audio_rtp.rs:send_sync_packet",
            "audio initial sync packet",
            "H71",
            serde_json::json!({
                "currentRtp": rtp_now,
                "syncRtp": sync_rtp,
                "nextRtp": rtp_now,
                "latencySamples": latency_samples,
            }),
        );
        // #endregion
    }

    socket
        .send_to(&packet, remote)
        .await
        .map_err(|e| RottenError::Protocol(format!("audio sync send: {e}")))?;
    Ok(())
}

/// ALAC packet encoder for 352-sample stereo S16 frames, using the
/// alac-encoder crate (Rust port of Apple's ALACEncoder).
struct AlacFrames {
    encoder: AlacEncoder,
    input_format: FormatDescription,
    scratch: Vec<u8>,
}

impl AlacFrames {
    fn new() -> Self {
        let output_format = FormatDescription::alac(44_100.0, u32::from(AUDIO_SPF), 2);
        let input_format = FormatDescription::pcm::<i16>(44_100.0, 2);
        let scratch = vec![0u8; output_format.max_packet_size()];
        Self {
            encoder: AlacEncoder::new(&output_format),
            input_format,
            scratch,
        }
    }

    fn encode(&mut self, pcm: &[u8]) -> Vec<u8> {
        let n = self.encoder.encode(&self.input_format, pcm, &mut self.scratch);
        self.scratch[..n].to_vec()
    }
}

/// 352 stereo samples of 440 Hz test tone as S16 PCM.
fn tone_pcm(spf: u16, phase: &mut f64) -> Vec<u8> {
    let mut pcm = vec![0u8; spf as usize * 2 * 2];
    for i in 0..spf as usize {
        let sample =
            ((440.0 * 2.0 * std::f64::consts::PI * (*phase / 44_100.0)).sin() * 8000.0) as i16;
        pcm[i * 4..i * 4 + 2].copy_from_slice(&sample.to_le_bytes());
        pcm[i * 4 + 2..i * 4 + 4].copy_from_slice(&sample.to_le_bytes());
        *phase += 1.0;
        if *phase >= 44_100.0 {
            *phase -= 44_100.0;
        }
    }
    pcm
}
