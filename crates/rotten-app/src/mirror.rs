use std::sync::Arc;

use rotten_core::config::MirrorConfig;
use rotten_core::debug_log::{DEBUG_BUILD_ID, agent_log};
use rotten_core::device::AirPlayDevice;
use rotten_core::error::Result;
use rotten_pairing::PairingManager;
use rotten_protocol::{MirrorConnection, playout_latency_samples};
use rotten_video::{
    MirrorStreamer, SyntheticSource, auto_bitrate_kbps, downscale_rgba, fit_stream_dims,
    frame_channel,
};
use tracing::info;

use crate::audio::AudioMirror;

pub async fn run_mirror(
    device: AirPlayDevice,
    config: MirrorConfig,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    on_first_frame: Option<Box<dyn FnOnce() + Send + 'static>>,
) -> Result<()> {
    let mut tasks = tokio::task::JoinSet::new();
    // #region agent log
    agent_log(
        "mirror.rs:run_mirror",
        "mirror session starting",
        "H0",
        serde_json::json!({
            "buildId": DEBUG_BUILD_ID,
            "testMode": config.test_mode,
            "host": device.host,
        }),
    );
    // #endregion

    let mut pairing = PairingManager::load(config.credentials_path.clone())?;

    let Some(creds) = until_stopped(
        pairing.pair(&device, config.pin.as_deref(), config.force_pair),
        stop.clone(),
    )
    .await?
    else {
        return Ok(());
    };

    let mut stream_config = config.stream.clone();
    let Some(mut handle) = until_stopped(
        MirrorConnection::connect(device.clone(), &creds, &config),
        stop.clone(),
    )
    .await?
    else {
        return Ok(());
    };

    let video_crypto = handle.video_crypto.clone();
    let data_port = handle.data_port();
    let control_uri = handle.control_uri().to_string();
    let session_uuid = handle.session().session_id.clone().unwrap_or_default();
    let rtsp_conn = handle.take_rtsp_conn().ok_or_else(|| {
        rotten_core::error::RottenError::Protocol("missing RTSP connection".into())
    })?;
    let data_stream = handle
        .take_data_stream()
        .ok_or_else(|| rotten_core::error::RottenError::Protocol("missing data stream".into()))?;
    let mut audio_setup = handle.audio.take();
    let streamer = MirrorStreamer::new(device.host.clone(), data_port, video_crypto);
    let rtsp_conn = Arc::new(tokio::sync::Mutex::new(rtsp_conn));
    let (first_frame_broadcast, _) = tokio::sync::broadcast::channel::<()>(3);
    if let Some(callback) = on_first_frame {
        let mut first_frame_rx = first_frame_broadcast.subscribe();
        tasks.spawn(async move {
            if first_frame_rx.recv().await.is_ok() {
                callback();
            }
        });
    }
    let mut rtsp_first_frame = first_frame_broadcast.subscribe();
    let mut heartbeat_first_frame = first_frame_broadcast.subscribe();
    let audio_latency_samples = audio_setup
        .as_ref()
        .map(|a| a.latency_samples)
        .unwrap_or_else(|| playout_latency_samples(&device.features));

    // Finish fallible capture setup before starting or muting local audio. Once
    // audio starts, every normal/error return must pass through its awaited stop.
    let capture = if !config.test_mode {
        let capture = rotten_capture::create_capture_backend(
            config.display_index,
            config.virtual_display_only,
        )?;
        let displays = capture.displays()?;
        if let Some(capture_display) = displays.first() {
            if stream_config.width == 0 || stream_config.height == 0 {
                stream_config.width = capture_display.width;
                stream_config.height = capture_display.height;
            } else {
                // Explicit --width/--height: scale the capture down to it (never up).
                let (rw, rh) = fit_stream_dims(stream_config.width, stream_config.height);
                let (cw, ch) = fit_stream_dims(capture_display.width, capture_display.height);
                stream_config.width = rw.min(cw).max(16);
                stream_config.height = rh.min(ch).max(16);
            }
            if config.virtual_display_only {
                info!(
                    name = %capture_display.name,
                    width = capture_display.width,
                    height = capture_display.height,
                    "virtual display capture active"
                );
            }
        }
        info!(backend = capture.backend_name(), "screen capture ready");
        Some(capture)
    } else {
        None
    };

    if config.test_mode && (stream_config.width == 0 || stream_config.height == 0) {
        stream_config.width = 1920;
        stream_config.height = 1080;
    }

    let bitrate = if stream_config.bitrate_kbps == 0 {
        auto_bitrate_kbps(stream_config.width, stream_config.height, stream_config.fps)
    } else {
        stream_config.bitrate_kbps
    };

    info!(
        encoder = ?config.hw_accel,
        width = stream_config.width,
        height = stream_config.height,
        fps = stream_config.fps,
        bitrate_kbps = bitrate,
        test_mode = config.test_mode,
        "starting mirror stream"
    );

    // Optional system-audio capture: feed captured PCM into the audio stream.
    let audio_handle = if config.audio {
        match audio_setup.as_mut() {
            Some(setup) => {
                let Some((handle, rx)) =
                    until_stopped(AudioMirror::start(&device), stop.clone()).await?
                else {
                    return Ok(());
                };
                setup.pcm_rx = Some(rx);
                if std::env::var("CERMIN_KEEP_LOCAL_AUDIO").is_err() {
                    handle.set_local_mute(true);
                    info!(
                        "local output muted during mirroring (set CERMIN_KEEP_LOCAL_AUDIO=1 to keep it)"
                    );
                }
                Some(handle)
            }
            None => None,
        }
    } else {
        None
    };

    let audio_task = audio_setup.map(|audio| {
        rotten_core::task::ScopedTask::new(rotten_protocol::spawn_mirror_audio_silence(
            audio,
            first_frame_broadcast.subscribe(),
        ))
    });
    let (first_frame_tx, first_frame_rx) = tokio::sync::oneshot::channel();
    let first_frame_notify = first_frame_broadcast.clone();
    tasks.spawn(async move {
        if first_frame_rx.await.is_ok() {
            let _ = first_frame_notify.send(());
        }
    });
    let rtsp_feedback = rtsp_conn.clone();
    tasks.spawn(async move {
        if rtsp_first_frame.recv().await.is_err() {
            return;
        }
        loop {
            let mut conn = rtsp_feedback.lock().await;
            match conn.rtsp_post_feedback().await {
                Ok((status, body)) => {
                    // #region agent log
                    agent_log(
                        "mirror.rs:rtsp_feedback",
                        "RTSP POST /feedback sent",
                        "H97",
                        serde_json::json!({
                            "httpStatus": status,
                            "bodyLen": body.len(),
                            "bodyByte0": body.first().copied().unwrap_or(0),
                            "feedback": feedback_plist_summary(&body),
                        }),
                    );
                    // #endregion
                }
                Err(e) => {
                    // #region agent log
                    agent_log(
                        "mirror.rs:rtsp_feedback",
                        "RTSP POST /feedback failed",
                        "H35",
                        serde_json::json!({ "error": e.to_string() }),
                    );
                    // #endregion
                    break;
                }
            }
            drop(conn);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });

    let rtsp_heartbeat = rtsp_conn.clone();
    let heartbeat_uri = control_uri.clone();
    let heartbeat_session = session_uuid.clone();
    tasks.spawn(async move {
        if heartbeat_first_frame.recv().await.is_err() {
            return;
        }
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(15));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let mut conn = rtsp_heartbeat.lock().await;
            match conn
                .rtsp_get_parameter(&heartbeat_uri, &heartbeat_session)
                .await
            {
                Ok((status, body)) => {
                    // #region agent log
                    agent_log(
                        "mirror.rs:rtsp_heartbeat",
                        "RTSP GET_PARAMETER sent",
                        "H118",
                        serde_json::json!({
                            "httpStatus": status,
                            "bodyLen": body.len(),
                        }),
                    );
                    // #endregion
                    if status == 400 {
                        break;
                    }
                }
                Err(e) => {
                    // #region agent log
                    agent_log(
                        "mirror.rs:rtsp_heartbeat",
                        "RTSP GET_PARAMETER failed",
                        "H118",
                        serde_json::json!({ "error": e.to_string() }),
                    );
                    // #endregion
                    break;
                }
            }
        }
    });

    let (frame_tx, frame_rx) = frame_channel();

    // Receivers often reject the volume SET_PARAMETER while setting up and
    // accept it once frames are flowing; retry until the TV takes the level.
    let volume_conn = rtsp_conn.clone();
    let volume_uri = control_uri.clone();
    let volume_session = session_uuid.clone();
    let mut volume_first_frame = first_frame_broadcast.subscribe();
    tasks.spawn(async move {
        if volume_first_frame.recv().await.is_err() {
            return;
        }
        let body = rotten_protocol::tv_volume_body();
        for attempt in 1..=5u32 {
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            let mut conn = volume_conn.lock().await;
            match conn
                .rtsp_set_parameter(&volume_uri, &volume_session, &body)
                .await
            {
                Ok((status, _)) => {
                    // #region agent log
                    agent_log(
                        "mirror.rs:tv_volume",
                        "TV volume SET_PARAMETER",
                        "H73",
                        serde_json::json!({
                            "httpStatus": status,
                            "attempt": attempt,
                            "body": String::from_utf8_lossy(&body),
                        }),
                    );
                    // #endregion
                    if (200..300).contains(&status) {
                        break;
                    }
                }
                Err(e) => {
                    // #region agent log
                    agent_log(
                        "mirror.rs:tv_volume",
                        "TV volume SET_PARAMETER failed",
                        "H73",
                        serde_json::json!({ "error": e.to_string(), "attempt": attempt }),
                    );
                    // #endregion
                    break;
                }
            }
        }
    });

    if let Some(capture) = capture {
        let fps = stream_config.fps;
        let target_width = stream_config.width;
        let target_height = stream_config.height;
        let capture = std::sync::Arc::new(std::sync::Mutex::new(capture));
        let capture_worker = capture.clone();
        let producer_stop = stop.clone();
        tasks.spawn(async move {
            let mut produced: u64 = 0;
            let mut last_watchdog = std::time::Instant::now();
            // Deadline-based pacing: sleep only the remaining time in the frame
            // budget (a fixed sleep on top of capture/conversion costs capped the
            // stream at ~16 fps on a 30 fps target).
            let frame_budget = std::time::Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
            let mut next_frame = tokio::time::Instant::now();
            loop {
                if producer_stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                if last_watchdog.elapsed() >= std::time::Duration::from_secs(2) {
                    // #region agent log
                    agent_log(
                        "mirror.rs:producer",
                        "capture producer alive",
                        "H22",
                        serde_json::json!({ "produced": produced }),
                    );
                    // #endregion
                    last_watchdog = std::time::Instant::now();
                }

                let cap = capture_worker.clone();
                let grabbed = tokio::task::spawn_blocking(move || {
                    let mut cap = cap.lock().expect("capture mutex");
                    cap.grab_frame().map(|frame| {
                        let captured_ns = rotten_core::ntp::session_elapsed().as_nanos() as u64;
                        (frame, captured_ns)
                    })
                })
                .await;

                match grabbed {
                    Ok(Ok((frame, captured_ns))) => {
                        produced += 1;
                        let (cw, ch) = fit_stream_dims(target_width, target_height);
                        let rgba = if cw != frame.width || ch != frame.height {
                            if produced == 1 {
                                // #region agent log
                                agent_log(
                                    "mirror.rs:producer",
                                    "downscaling capture before encode queue",
                                    "H23",
                                    serde_json::json!({
                                        "fromW": frame.width,
                                        "fromH": frame.height,
                                        "toW": cw,
                                        "toH": ch,
                                    }),
                                );
                                // #endregion
                            }
                            downscale_rgba(&frame.rgba, frame.width, frame.height, cw, ch)
                        } else {
                            frame.rgba
                        };
                        // #region agent log
                        if produced == 1 {
                            let sample_len = rgba.len().min(4096);
                            let mut r_sum = 0u64;
                            let mut g_sum = 0u64;
                            let mut b_sum = 0u64;
                            let mut samples = 0u64;
                            for chunk in rgba[..sample_len].chunks_exact(4) {
                                r_sum += chunk[0] as u64;
                                g_sum += chunk[1] as u64;
                                b_sum += chunk[2] as u64;
                                samples += 1;
                            }
                            agent_log(
                                "mirror.rs:producer",
                                "first capture frame luma sample",
                                "H37",
                                serde_json::json!({
                                    "produced": produced,
                                    "width": cw,
                                    "height": ch,
                                    "avgR": if samples > 0 { r_sum / samples } else { 0 },
                                    "avgG": if samples > 0 { g_sum / samples } else { 0 },
                                    "avgB": if samples > 0 { b_sum / samples } else { 0 },
                                    "cornerRgba": format!(
                                        "{:02x}{:02x}{:02x}{:02x}",
                                        rgba[0], rgba[1], rgba[2], rgba[3]
                                    ),
                                }),
                            );
                        }
                        if produced == 1 || produced % 30 == 0 {
                            agent_log(
                                "mirror.rs:producer",
                                "capture frame queued for encoder",
                                "H2",
                                serde_json::json!({
                                    "produced": produced,
                                    "width": cw,
                                    "height": ch,
                                    "rgbaBytes": rgba.len(),
                                }),
                            );
                        }
                        // #endregion
                        frame_tx.send((rgba, cw, ch, captured_ns));
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "capture error");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "capture task join error");
                    }
                }
                next_frame += frame_budget;
                let now = tokio::time::Instant::now();
                if next_frame > now {
                    tokio::time::sleep(next_frame - now).await;
                } else if now.duration_since(next_frame) > frame_budget {
                    next_frame = now;
                }
            }
        });
    } else {
        let fps = stream_config.fps;
        let width = stream_config.width;
        let height = stream_config.height;
        let synthetic_stop = stop.clone();
        tasks.spawn(async move {
            let mut synthetic = SyntheticSource::new(width, height);
            let frame_budget = std::time::Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
            let mut next_frame = tokio::time::Instant::now();
            loop {
                if synthetic_stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                match synthetic.next_frame() {
                    Ok((rgba, w, h)) => {
                        frame_tx.send((rgba, w, h, 0));
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "synthetic frame error");
                        break;
                    }
                }
                next_frame += frame_budget;
                let now = tokio::time::Instant::now();
                if next_frame > now {
                    tokio::time::sleep(next_frame - now).await;
                } else if now.duration_since(next_frame) > frame_budget {
                    next_frame = now;
                }
            }
        });
    }

    let (stream_w, stream_h) = fit_stream_dims(stream_config.width, stream_config.height);
    // Codec-header offsets 16/40 = coded picture size; 56/60 = visible presentation size.
    // Apple TV /info often omits display size — use capture dimensions (e.g. 1080), not coded pad (1088).
    let capture_w = stream_config.width & !1;
    let capture_h = stream_config.height & !1;
    let (presentation_w, presentation_h, presentation_source) =
        match (device.display_width, device.display_height) {
            (Some(w), Some(h)) if w > 0 && h > 0 && w == capture_w && h == capture_h => {
                (w, h, "receiver-info")
            }
            _ => (capture_w, capture_h, "capture-size"),
        };

    // #region agent log
    agent_log(
        "mirror.rs:run_mirror",
        "starting video streamer",
        "H0",
        serde_json::json!({
            "buildId": DEBUG_BUILD_ID,
            "width": stream_config.width,
            "height": stream_config.height,
            "streamW": stream_w,
            "streamH": stream_h,
            "presentationW": presentation_w,
            "presentationH": presentation_h,
            "presentationSource": presentation_source,
            "bitrateKbps": bitrate,
        }),
    );
    // #endregion

    let stream_result = until_stopped(
        streamer.stream_from_channel_on(
            data_stream,
            stream_w,
            stream_h,
            stream_w,
            stream_h,
            presentation_w,
            presentation_h,
            audio_latency_samples,
            stream_config.fps,
            bitrate,
            config.hw_accel,
            frame_rx,
            Some(first_frame_tx),
            stop.clone(),
        ),
        stop,
    )
    .await
    .map(|_| ());

    tasks.shutdown().await;
    drop(audio_task);

    if let Some(audio) = audio_handle {
        if let Err(error) = audio.stop().await {
            if stream_result.is_ok() {
                return Err(error);
            }
            tracing::warn!(%error, "audio cleanup also failed");
        }
    }

    stream_result
}

// Poll outside setup and streaming futures: a pending connection, response, or
// TCP write must not prevent Disconnect from reaching session cleanup.
pub(crate) async fn until_stopped<T, E>(
    future: impl std::future::Future<Output = std::result::Result<T, E>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> std::result::Result<Option<T>, E> {
    tokio::pin!(future);
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(None);
        }
        tokio::select! {
            result = &mut future => return result.map(Some),
            _ = tick.tick() => {}
        }
    }
}

fn feedback_plist_summary(body: &[u8]) -> serde_json::Value {
    let Ok(value) = plist::from_bytes::<plist::Value>(body) else {
        return serde_json::json!({ "parsed": false });
    };
    let Some(dict) = value.as_dictionary() else {
        return serde_json::json!({ "parsed": true, "root": "non-dict" });
    };
    let keys: Vec<&str> = dict.keys().map(String::as_str).collect();
    let mut out = serde_json::json!({ "parsed": true, "keys": keys });
    for key in [
        "status",
        "statusFlags",
        "error",
        "mirroring",
        "video",
        "audio",
        "reason",
    ] {
        if let Some(v) = dict.get(key) {
            out[key] = plist_value_json(v);
        }
    }
    if let Some(streams) = dict.get("streams").and_then(|v| v.as_array()) {
        let entries: Vec<serde_json::Value> = streams
            .iter()
            .filter_map(|s| s.as_dictionary())
            .map(|sd| {
                serde_json::json!({
                    "type": sd.get("type").and_then(|v| v.as_signed_integer()),
                    "buffered": sd.get("buffered").and_then(plist::Value::as_boolean),
                    "playing": sd.get("playing").and_then(plist::Value::as_boolean),
                    "ready": sd.get("ready").and_then(plist::Value::as_boolean),
                    "state": sd.get("state").map(plist_value_json),
                })
            })
            .collect();
        out["streams"] = serde_json::json!(entries);
    }
    out
}

fn plist_value_json(value: &plist::Value) -> serde_json::Value {
    match value {
        plist::Value::String(s) => serde_json::Value::String(s.clone()),
        plist::Value::Boolean(b) => serde_json::Value::Bool(*b),
        plist::Value::Integer(i) => serde_json::json!(i.as_signed().unwrap_or(0)),
        plist::Value::Real(f) => serde_json::json!(*f),
        plist::Value::Data(d) => serde_json::json!({ "dataLen": d.len() }),
        _ => serde_json::json!(format!("{value:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn disconnect_cancels_blocked_stream_and_drops_resources() {
        let stop = Arc::new(AtomicBool::new(false));
        let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(until_stopped(
            async move {
                let _held = held_tx;
                ready_tx.send(()).unwrap();
                std::future::pending::<Result<()>>().await
            },
            stop.clone(),
        ));
        ready_rx.await.unwrap();
        stop.store(true, Ordering::Relaxed);
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result, None);
        assert!(held_rx.await.is_err());
    }

    #[tokio::test]
    async fn stream_error_is_preserved() {
        let error = until_stopped(
            async { Err::<(), _>(rotten_core::RottenError::Video("test failure".into())) },
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("test failure"));
    }

    #[tokio::test]
    async fn stopped_session_does_not_start_next_setup_stage() {
        let result = until_stopped(
            async {
                panic!("setup must not run after Disconnect");
                #[allow(unreachable_code)]
                Ok::<_, rotten_core::RottenError>(())
            },
            Arc::new(AtomicBool::new(true)),
        )
        .await
        .unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn completed_setup_returns_its_resource() {
        let resource = until_stopped(
            async { Ok::<_, rotten_core::RottenError>(String::from("session resource")) },
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        assert_eq!(resource.as_deref(), Some("session resource"));
    }
}
