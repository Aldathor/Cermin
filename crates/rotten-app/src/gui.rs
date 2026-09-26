#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Cermin GUI: pick a TV, connect, mirror. The mirror session itself runs on a
//! dedicated thread with its own Tokio runtime; the UI only sends commands and
//! drains status events.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Frame, Layout, Margin, Pos2,
    Rect, RichText, ScrollArea, Sense, Shape, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use rotten_app::cast::{CastLatency, CastQuality, CastSettings};
use rotten_capture::{DisplayInfo, list_displays};
use rotten_core::config::{
    HwAccel, MirrorCipherMode, MirrorConfig, StreamConfig, resolve_credentials_path,
};
use rotten_core::device::{CastDevice, ReceiverDevice};
use rotten_discovery::discover_receivers_for;
use rotten_pairing::PairingManager;

// ---- palette (matches the mockup) -----------------------------------------

const BG: Color32 = Color32::from_rgb(0xF1, 0xF5, 0xFB);
const CARD: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
const BORDER: Color32 = Color32::from_rgb(0xE3, 0xE8, 0xF2);
const ACCENT: Color32 = Color32::from_rgb(0x1F, 0x6F, 0xE5);
const ACCENT_HOVER: Color32 = Color32::from_rgb(0x1A, 0x60, 0xC8);
const TEXT: Color32 = Color32::from_rgb(0x14, 0x1B, 0x2B);
const MUTED: Color32 = Color32::from_rgb(0x64, 0x6E, 0x80);
const FAINT: Color32 = Color32::from_rgb(0x94, 0x9C, 0xAB);
const DISABLED_BG: Color32 = Color32::from_rgb(0xE7, 0xEB, 0xF1);
const DISABLED_FG: Color32 = Color32::from_rgb(0xA0, 0xA8, 0xB4);
const SEL_BG: Color32 = Color32::from_rgb(0xE7, 0xF0, 0xFD);
const GREEN: Color32 = Color32::from_rgb(0x22, 0xB1, 0x5A);
const AMBER: Color32 = Color32::from_rgb(0xE0, 0x9B, 0x1E);
const RED: Color32 = Color32::from_rgb(0xDC, 0x35, 0x45);
const LOG_BG: Color32 = Color32::from_rgb(0xF8, 0xFA, 0xFC);

const CAST_WARNING: &str = "Experimental Google Cast: H.264 video with optional AAC system sound, several seconds of buffering. Your speakers are not muted. Trusted LAN only (unauthenticated control, unencrypted media).";
const CAST_PORT: u16 = 8009;

/// PLAYING status text for a Cast session. It must not claim video only when
/// system audio is enabled.
fn cast_playing_desc(audio: bool) -> &'static str {
    if audio {
        "Receiver reports PLAYING; video and AAC system audio may take a few seconds to appear."
    } else {
        "Receiver reports PLAYING; video may take a few seconds to appear (system audio is off)."
    }
}

// ---- commands / events ------------------------------------------------------

enum Cmd {
    Search,
    Displays,
    /// Connect to a receiver. The `bool` is the Cast system-audio choice and
    /// [`CastSettings`] the Cast-only quality/latency choice; the AirPlay path
    /// always mirrors with system audio and ignores both.
    Connect(
        Box<ReceiverDevice>,
        Option<String>,
        Option<u32>,
        bool,
        CastSettings,
    ),
    Stop,
}

enum Ev {
    Searching,
    Devices(Vec<ReceiverDevice>),
    Displays(Vec<DisplayInfo>),
    SessionStarting(String),
    Streaming,
    Stopped(String),
    Error(String),
}

fn timestamp() -> String {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second())
}

// ---- protocol-independent session planning ----------------------------------

#[derive(Debug, PartialEq, Eq)]
enum ConnectPlanError {
    PinRequired,
}

#[derive(Debug, PartialEq, Eq)]
enum SessionPlan {
    Cast,
    AirPlay { pin: Option<String> },
}

/// Decide how to start a session without doing any I/O.
///
/// Google Cast never consults AirPlay credentials or the PIN field; the caller
/// must dispatch before loading stored pairing data.
fn connect_plan(
    device: &ReceiverDevice,
    has_airplay_credentials: bool,
    entered_pin: &str,
) -> Result<SessionPlan, ConnectPlanError> {
    if device.is_cast() {
        return Ok(SessionPlan::Cast);
    }
    if has_airplay_credentials {
        return Ok(SessionPlan::AirPlay { pin: None });
    }
    let entered = entered_pin.trim();
    if entered.is_empty() {
        return Err(ConnectPlanError::PinRequired);
    }
    Ok(SessionPlan::AirPlay {
        pin: Some(entered.to_owned()),
    })
}

fn manual_cast_device(input: &str) -> Result<CastDevice, rotten_core::error::RottenError> {
    CastDevice::manual(input, CAST_PORT)
}

/// Cast receivers have no AirPlay code to enter; no selection shows the
/// default AirPlay view.
fn shows_airplay_pin(device: Option<&ReceiverDevice>) -> bool {
    device.is_none_or(|device| !device.is_cast())
}

/// TV volume control is part of the AirPlay RTSP session.
fn shows_tv_volume(device: Option<&ReceiverDevice>) -> bool {
    device.is_none_or(|device| !device.is_cast())
}

fn main() -> Result<(), eframe::Error> {
    let (cmd_tx, cmd_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    let worker = spawn_worker(cmd_rx, ev_tx);

    rotten_protocol::set_tv_volume_percent(35);

    let mut app = CerminApp::new(cmd_tx, ev_rx);
    let _ = app.cmd_tx.send(Cmd::Search);
    let _ = app.cmd_tx.send(Cmd::Displays);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 800.0])
            .with_min_inner_size([340.0, 420.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "Cermin",
        options,
        Box::new(|_cc| Ok(Box::new(app) as Box<dyn eframe::App>)),
    );
    // Closing the window drops the command sender. Wait for the worker to stop
    // the session and restore local audio before the GUI process exits.
    let _ = worker.join();
    result
}

fn spawn_worker(cmd_rx: Receiver<Cmd>, ev_tx: Sender<Ev>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ev_tx.send(Ev::Error(format!("runtime: {e}")));
                return;
            }
        };

        let mut session: Option<(std::thread::JoinHandle<Result<(), String>>, Arc<AtomicBool>)> =
            None;

        loop {
            if session
                .as_ref()
                .is_some_and(|(handle, _)| handle.is_finished())
            {
                let (handle, _) = session.take().expect("finished session");
                let reason = match handle.join() {
                    Ok(Ok(())) => "session ended".to_string(),
                    Ok(Err(error)) => error,
                    Err(_) => "session worker panicked".to_string(),
                };
                // Announce completion only after releasing session ownership,
                // so Connect can always accept a retry following Stopped.
                let _ = ev_tx.send(Ev::Stopped(reason));
            }

            let cmd = match cmd_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(cmd) => cmd,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            match cmd {
                Cmd::Search => {
                    let _ = ev_tx.send(Ev::Searching);
                    match rt.block_on(discover_receivers_for(Duration::from_secs(5))) {
                        Ok(devices) => {
                            let _ = ev_tx.send(Ev::Devices(devices));
                        }
                        Err(e) => {
                            let _ = ev_tx.send(Ev::Error(format!("discovery failed: {e}")));
                        }
                    }
                }
                Cmd::Displays => match list_displays() {
                    Ok(displays) => {
                        let _ = ev_tx.send(Ev::Displays(displays));
                    }
                    Err(e) => {
                        let _ = ev_tx.send(Ev::Error(format!("display enumeration failed: {e}")));
                    }
                },
                Cmd::Connect(device, pin, display, cast_audio, cast_settings) => {
                    if session.is_some() {
                        continue;
                    }
                    let stop = Arc::new(AtomicBool::new(false));
                    let stop_thread = stop.clone();
                    let ev_stream = ev_tx.clone();
                    let name = device.name().to_string();
                    // Publish this before launching the session: immediate
                    // failures and first-frame events must follow Starting.
                    let _ = ev_tx.send(Ev::SessionStarting(name));
                    let handle = std::thread::Builder::new()
                        .name("mirror-session".into())
                        .spawn(move || {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build();
                            let first_frame: Option<Box<dyn FnOnce() + Send>> =
                                Some(Box::new(move || {
                                    let _ = ev_stream.send(Ev::Streaming);
                                }));
                            match rt {
                                Ok(rt) => run_on_session_runtime(
                                    rt,
                                    run_session(
                                        *device,
                                        pin,
                                        display,
                                        cast_audio,
                                        cast_settings,
                                        stop_thread,
                                        first_frame,
                                    ),
                                ),
                                Err(e) => Err(format!("runtime: {e}")),
                            }
                        });
                    match handle {
                        Ok(handle) => session = Some((handle, stop)),
                        Err(error) => {
                            let _ = ev_tx.send(Ev::Stopped(format!(
                                "could not start session worker: {error}"
                            )));
                        }
                    }
                }
                Cmd::Stop => {
                    if let Some((_, stop)) = &session {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        if let Some((handle, stop)) = session {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
    })
}

fn run_on_session_runtime<T>(
    runtime: tokio::runtime::Runtime,
    session: impl std::future::Future<Output = T>,
) -> T {
    let result = runtime.block_on(session);
    // Session/audio cleanup is complete. A cancelled blocking capture/PIN read
    // must not hold window close or a subsequent connection open forever.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

#[cfg(test)]
mod session_tests {
    use super::*;

    #[test]
    fn completed_session_does_not_wait_forever_for_blocking_work() {
        let (release_tx, release_rx) = channel::<()>();
        let (started_tx, started_rx) = channel();
        let (done_tx, done_rx) = channel();
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = run_on_session_runtime(runtime, async {
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                tokio::task::spawn_blocking(move || {
                    ready_tx.send(()).unwrap();
                    // Simulate a capture driver or stdin read that ignores
                    // cancellation after the session has finished cleanup.
                    let _ = release_rx.recv();
                });
                ready_rx.await.unwrap();
                started_tx.send(()).unwrap();
                Err::<(), _>("expected session failure")
            });
            done_tx.send(result).unwrap();
        });

        let started = started_rx.recv_timeout(Duration::from_secs(5));
        let result = done_rx.recv_timeout(Duration::from_secs(5));
        // Release the simulated driver even if the shutdown regression returns,
        // so a failing test leaves no stuck worker behind.
        let _ = release_tx.send(());
        worker.join().unwrap();
        started.expect("blocking work did not start");
        assert_eq!(
            result.expect("completed session was held open by blocking work"),
            Err("expected session failure")
        );
    }
}

#[cfg(test)]
mod cast_ui_tests {
    use super::*;

    fn airplay_device() -> ReceiverDevice {
        ReceiverDevice::AirPlay(rotten_core::device::AirPlayDevice {
            name: "Apple TV".into(),
            host: "192.168.1.10".into(),
            port: 7000,
            device_id: "aa:bb:cc".into(),
            model: Some("AppleTV6,2".into()),
            features: Default::default(),
            display_width: None,
            display_height: None,
            pi: None,
            pk: None,
        })
    }

    fn cast_device() -> ReceiverDevice {
        ReceiverDevice::GoogleCast(CastDevice {
            name: "Cast TV".into(),
            host: "192.168.1.20".into(),
            port: CAST_PORT,
            device_id: "cast-id".into(),
            model: Some("Chromecast".into()),
            capabilities: Some(5),
        })
    }

    #[test]
    fn cast_connect_bypasses_airplay_pin() {
        assert_eq!(
            connect_plan(&cast_device(), false, "").unwrap(),
            SessionPlan::Cast
        );
        // A typed PIN and "missing credentials" are irrelevant for Cast.
        assert_eq!(
            connect_plan(&cast_device(), false, "1234").unwrap(),
            SessionPlan::Cast
        );
    }

    #[test]
    fn airplay_still_requires_a_pin_without_credentials() {
        assert_eq!(
            connect_plan(&airplay_device(), false, ""),
            Err(ConnectPlanError::PinRequired)
        );
        assert_eq!(
            connect_plan(&airplay_device(), false, "1234").unwrap(),
            SessionPlan::AirPlay {
                pin: Some("1234".into())
            }
        );
        assert_eq!(
            connect_plan(&airplay_device(), true, "").unwrap(),
            SessionPlan::AirPlay { pin: None }
        );
    }

    #[test]
    fn manual_target_creates_a_cast_device_on_8009() {
        let device = manual_cast_device(" 192.168.1.50 ").unwrap();
        assert_eq!(device.host, "192.168.1.50");
        assert_eq!(device.port, CAST_PORT);
        assert!(manual_cast_device("http://192.168.1.50").is_err());
        assert!(manual_cast_device("192.168.1.50:8009").is_err());
    }

    #[test]
    fn cast_selection_hides_protocol_specific_airplay_controls() {
        assert!(!shows_airplay_pin(Some(&cast_device())));
        assert!(shows_airplay_pin(Some(&airplay_device())));
        assert!(!shows_tv_volume(Some(&cast_device())));
        assert!(shows_tv_volume(Some(&airplay_device())));
        assert!(shows_airplay_pin(None));
        assert!(shows_tv_volume(None));
        assert!(CAST_WARNING.contains("AAC"));
        assert!(CAST_WARNING.contains("not muted"));
        assert!(CAST_WARNING.contains("unencrypted media"));
        assert!(CAST_WARNING.contains("Trusted LAN"));
    }

    #[test]
    fn cast_playing_text_never_claims_video_only_when_audio_is_on() {
        let with_audio = cast_playing_desc(true);
        assert!(with_audio.contains("audio"), "{with_audio}");
        assert!(
            !with_audio.to_lowercase().contains("video only"),
            "{with_audio}"
        );
        let video_only = cast_playing_desc(false);
        assert!(video_only.contains("system audio is off"), "{video_only}");
    }

    #[test]
    fn system_audio_defaults_to_platform_support() {
        let (cmd_tx, _cmd_rx) = channel();
        let (_ev_tx, ev_rx) = channel();
        let app = CerminApp::new(cmd_tx, ev_rx);
        assert_eq!(app.cast_audio, cfg!(target_os = "windows"));
    }

    #[test]
    fn cast_presets_default_to_balanced_and_stable() {
        let (cmd_tx, _cmd_rx) = channel();
        let (_ev_tx, ev_rx) = channel();
        let app = CerminApp::new(cmd_tx, ev_rx);
        assert_eq!(app.cast_quality, CastQuality::Balanced);
        assert_eq!(app.cast_latency, CastLatency::Stable);
        assert_eq!(app.active_cast_latency, CastLatency::Stable);
    }

    #[test]
    fn connect_passes_the_cast_audio_choice_and_airplay_stays_on() {
        let (cmd_tx, cmd_rx) = channel();
        let (_ev_tx, ev_rx) = channel();
        let mut app = CerminApp::new(cmd_tx, ev_rx);
        app.cast_audio = true;
        app.cast_quality = CastQuality::High;
        app.cast_latency = CastLatency::Responsive;
        app.finish_connect(cast_device(), None);
        match cmd_rx.try_recv() {
            Ok(Cmd::Connect(_, _, _, cast_audio, settings)) => {
                assert!(cast_audio);
                assert_eq!(settings.quality, CastQuality::High);
                assert_eq!(settings.latency, CastLatency::Responsive);
            }
            _ => panic!("expected a Cast connect command"),
        }
        assert!(app.active_cast_audio);
        assert_eq!(app.active_cast_latency, CastLatency::Responsive);

        // AirPlay always mirrors with system audio even with the Cast box off;
        // the Cast-only presets are ignored by the AirPlay path.
        app.cast_audio = false;
        app.connect_pending = false;
        app.finish_connect(airplay_device(), Some("1234".into()));
        match cmd_rx.try_recv() {
            Ok(Cmd::Connect(_, _, _, cast_audio, _settings)) => assert!(cast_audio),
            _ => panic!("expected an AirPlay connect command"),
        }
        assert!(!app.active_cast_audio);
        assert_eq!(
            app.active_cast_latency,
            CastLatency::default(),
            "AirPlay has no Cast latency"
        );
    }

    #[test]
    fn finish_connect_resets_pending_when_worker_channel_is_closed() {
        let (cmd_tx, cmd_rx) = channel();
        let (_ev_tx, ev_rx) = channel();
        drop(cmd_rx);
        let mut app = CerminApp::new(cmd_tx, ev_rx);
        app.finish_connect(cast_device(), None);
        assert!(!app.connect_pending);
        assert_eq!(app.status_title, "Error");
        assert!(app.status_desc.contains("session worker"));
        assert!(
            app.log
                .iter()
                .any(|line| line.contains("worker channel closed"))
        );
    }

    #[test]
    fn cast_session_starting_announces_the_initial_buffer() {
        let (cmd_tx, _cmd_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        let mut app = CerminApp::new(cmd_tx, ev_rx);
        app.finish_connect(cast_device(), None);
        ev_tx
            .send(Ev::SessionStarting("Cast TV".into()))
            .expect("send session starting");
        app.handle_events();
        assert_eq!(app.status_title, "Connecting");
        assert!(
            app.status_desc.contains("initial buffer about 8 seconds"),
            "{}",
            app.status_desc
        );
        assert!(
            app.log
                .iter()
                .any(|line| line.contains("initial buffer about 8 seconds")),
            "{:?}",
            app.log
        );

        // AirPlay keeps its existing startup copy.
        app.finish_connect(airplay_device(), Some("1234".into()));
        ev_tx
            .send(Ev::SessionStarting("Apple TV".into()))
            .expect("send session starting");
        app.handle_events();
        assert_eq!(app.status_desc, "Setting up the session with Apple TV...");
        assert!(
            app.log
                .iter()
                .any(|line| line.contains("Connecting to Apple TV...")),
            "{:?}",
            app.log
        );
    }

    /// The startup copy uses the latency snapshot taken at Connect, never the
    /// live selector: changing the combo box during startup must not rewrite
    /// the running session's buffer promise.
    #[test]
    fn cast_session_starting_uses_the_snapshot_latency() {
        let (cmd_tx, _cmd_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        let mut app = CerminApp::new(cmd_tx, ev_rx);
        app.cast_latency = CastLatency::Responsive;
        app.finish_connect(cast_device(), None);
        // The user changes the selector after Connect but before the worker
        // reports the session starting.
        app.cast_latency = CastLatency::Stable;
        ev_tx
            .send(Ev::SessionStarting("Cast TV".into()))
            .expect("send session starting");
        app.handle_events();
        assert!(
            app.status_desc.contains("initial buffer about 4 seconds"),
            "{}",
            app.status_desc
        );
        assert!(
            app.log
                .iter()
                .any(|line| line.contains("initial buffer about 4 seconds")),
            "{:?}",
            app.log
        );
    }

    #[test]
    fn repeated_search_starts_only_once() {
        let (cmd_tx, cmd_rx) = channel();
        let (_ev_tx, ev_rx) = channel();
        let mut app = CerminApp::new(cmd_tx, ev_rx);
        app.search();
        app.search();
        assert!(app.searching);
        assert!(matches!(cmd_rx.try_recv(), Ok(Cmd::Search)));
        assert!(cmd_rx.try_recv().is_err());
    }
}

async fn run_session(
    device: ReceiverDevice,
    pin: Option<String>,
    display_index: Option<u32>,
    cast_audio: bool,
    cast_settings: CastSettings,
    stop: Arc<AtomicBool>,
    on_first_frame: Option<Box<dyn FnOnce() + Send + 'static>>,
) -> Result<(), String> {
    match device {
        ReceiverDevice::GoogleCast(device) => {
            let config = rotten_app::cast::CastConfig {
                display_index,
                test_mode: false,
                http_port: 0,
                audio: cast_audio,
                quality: cast_settings.quality,
                latency: cast_settings.latency,
            };
            // `{:#}` keeps anyhow's context chain (connect/HLS/HTTP causes)
            // instead of collapsing to the outermost message.
            rotten_app::cast::run_cast(device, config, stop, on_first_frame)
                .await
                .map_err(|e| format!("{e:#}"))
        }
        ReceiverDevice::AirPlay(device) => {
            let config = MirrorConfig {
                stream: StreamConfig {
                    width: 0,
                    height: 0,
                    fps: 30,
                    bitrate_kbps: 0,
                },
                pin,
                force_pair: false,
                test_mode: false,
                audio: true,
                hw_accel: HwAccel::Auto,
                credentials_path: resolve_credentials_path(None),
                display_index,
                virtual_display_only: false,
                no_encrypt: false,
                cipher: MirrorCipherMode::ChaCha,
            };
            rotten_app::mirror::run_mirror(device, config, stop, on_first_frame)
                .await
                .map_err(|e| e.to_string())
        }
    }
}

// ---- icons (painted, no font dependency) ------------------------------------

#[derive(Clone, Copy)]
enum Icon {
    Monitor,
    Magnifier,
    Refresh,
    Speaker,
    Trash,
}

fn paint_icon(p: &egui::Painter, icon: Icon, rect: Rect, color: Color32) {
    let stroke = Stroke::new(1.7, color);
    match icon {
        Icon::Monitor => {
            let body = Rect::from_center_size(
                pos2(rect.center().x, rect.center().y - rect.height() * 0.10),
                vec2(rect.width() * 0.86, rect.height() * 0.60),
            );
            p.rect_stroke(body, CornerRadius::same(2), stroke, StrokeKind::Inside);
            let stand_y = body.bottom();
            p.line_segment(
                [
                    pos2(body.center().x, stand_y),
                    pos2(body.center().x, stand_y + rect.height() * 0.12),
                ],
                stroke,
            );
            p.line_segment(
                [
                    pos2(
                        body.center().x - rect.width() * 0.20,
                        stand_y + rect.height() * 0.12,
                    ),
                    pos2(
                        body.center().x + rect.width() * 0.20,
                        stand_y + rect.height() * 0.12,
                    ),
                ],
                stroke,
            );
        }
        Icon::Magnifier => {
            let c = pos2(
                rect.center().x - rect.width() * 0.08,
                rect.center().y - rect.height() * 0.10,
            );
            let r = rect.width() * 0.27;
            p.circle_stroke(c, r, stroke);
            p.line_segment(
                [
                    pos2(c.x + r * 0.72, c.y + r * 0.72),
                    pos2(
                        rect.right() - rect.width() * 0.16,
                        rect.bottom() - rect.height() * 0.16,
                    ),
                ],
                stroke,
            );
        }
        Icon::Refresh => {
            let c = rect.center();
            let r = rect.width() * 0.30;
            let start = 45f32.to_radians();
            let end = 330f32.to_radians();
            let mut points: Vec<Pos2> = Vec::new();
            for i in 0..=24 {
                let a = start + (end - start) * (i as f32 / 24.0);
                points.push(pos2(c.x + r * a.cos(), c.y + r * a.sin()));
            }
            p.add(Shape::line(points, stroke));
            let tip = pos2(c.x + r * start.cos(), c.y + r * start.sin());
            let s = rect.width() * 0.17;
            let tangent = vec2(-start.sin(), start.cos());
            let inward = vec2(-start.cos(), -start.sin());
            p.add(Shape::convex_polygon(
                vec![
                    tip + tangent * s * 0.9,
                    tip - tangent * s * 0.9,
                    tip + inward * s * 1.1,
                ],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Speaker => {
            let cone = Rect::from_center_size(
                pos2(rect.center().x - rect.width() * 0.24, rect.center().y),
                vec2(rect.width() * 0.20, rect.height() * 0.34),
            );
            p.rect_filled(cone, CornerRadius::same(1), color);
            p.add(Shape::convex_polygon(
                vec![
                    pos2(cone.right(), cone.top()),
                    pos2(
                        rect.center().x + rect.width() * 0.04,
                        rect.top() + rect.height() * 0.18,
                    ),
                    pos2(
                        rect.center().x + rect.width() * 0.04,
                        rect.bottom() - rect.height() * 0.18,
                    ),
                    pos2(cone.right(), cone.bottom()),
                ],
                color,
                Stroke::NONE,
            ));
            let c = pos2(rect.center().x, rect.center().y);
            for rr in [rect.width() * 0.18, rect.width() * 0.32] {
                let mut points: Vec<Pos2> = Vec::new();
                for k in 0..=12 {
                    let a = (-50.0 + 100.0 * (k as f32 / 12.0)).to_radians();
                    points.push(pos2(c.x + rr * a.cos(), c.y + rr * a.sin()));
                }
                p.add(Shape::line(points, Stroke::new(1.5, color)));
            }
        }
        Icon::Trash => {
            let w = rect.width() * 0.56;
            let h = rect.height() * 0.52;
            let body = Rect::from_center_size(
                pos2(rect.center().x, rect.center().y + rect.height() * 0.10),
                vec2(w, h),
            );
            p.rect_stroke(body, CornerRadius::same(2), stroke, StrokeKind::Inside);
            let lid_y = body.top() - rect.height() * 0.06;
            p.line_segment(
                [
                    pos2(rect.center().x - w * 0.75, lid_y),
                    pos2(rect.center().x + w * 0.75, lid_y),
                ],
                stroke,
            );
            p.line_segment(
                [
                    pos2(rect.center().x - w * 0.22, lid_y - rect.height() * 0.10),
                    pos2(rect.center().x + w * 0.22, lid_y - rect.height() * 0.10),
                ],
                stroke,
            );
            for dx in [-w * 0.20, w * 0.20] {
                p.line_segment(
                    [
                        pos2(rect.center().x + dx, body.top() + h * 0.18),
                        pos2(rect.center().x + dx, body.bottom() - h * 0.18),
                    ],
                    Stroke::new(1.3, color),
                );
            }
        }
    }
}

fn draw_logo(p: &egui::Painter, rect: Rect) {
    let blue = Color32::from_rgb(0x2E, 0x86, 0xF0);
    let arrow_blue = Color32::from_rgb(0x14, 0x4E, 0xA6);
    p.rect_filled(rect, CornerRadius::same(12), blue);
    let screen = Rect::from_center_size(
        pos2(
            rect.center().x + rect.width() * 0.06,
            rect.center().y - rect.height() * 0.07,
        ),
        vec2(rect.width() * 0.58, rect.height() * 0.42),
    );
    p.rect_filled(screen, CornerRadius::same(3), Color32::WHITE);
    let stand = Rect::from_center_size(
        pos2(screen.center().x, screen.bottom() + rect.height() * 0.10),
        vec2(rect.width() * 0.24, rect.height() * 0.08),
    );
    p.rect_filled(stand, CornerRadius::same(2), Color32::WHITE);
    // arrow entering the screen from the lower left (dark blue on the screen)
    let tip = pos2(
        screen.left() + screen.width() * 0.62,
        screen.center().y + screen.height() * 0.05,
    );
    let t = screen.height() * 0.62;
    p.add(Shape::convex_polygon(
        vec![
            tip,
            tip + vec2(-t * 1.15, -t * 0.30),
            tip + vec2(-t * 1.15, t * 0.40),
        ],
        arrow_blue,
        Stroke::NONE,
    ));
    p.rect_filled(
        Rect::from_min_size(
            pos2(tip.x - t * 2.1, tip.y - t * 0.02),
            vec2(t * 1.0, t * 0.14),
        ),
        CornerRadius::same(1),
        arrow_blue,
    );
}

// ---- reusable widgets -------------------------------------------------------

fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(12))
        .show(ui, add)
        .inner
}

fn action_button(
    ui: &mut Ui,
    label: &str,
    icon: Option<Icon>,
    primary: bool,
    enabled: bool,
    min_width: f32,
) -> egui::Response {
    let font = FontId::proportional(14.5);
    let color = if !enabled {
        DISABLED_FG
    } else if primary {
        Color32::WHITE
    } else {
        MUTED
    };
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), color);
    let icon_w = if icon.is_some() { 17.0 } else { 0.0 };
    let gap = if icon.is_some() { 8.0 } else { 0.0 };
    let content_w = galley.size().x + icon_w + gap;
    let height = 36.0;
    let width = min_width.max(content_w + 30.0);
    let (rect, response) = ui.allocate_exact_size(
        vec2(width, height),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );

    let hovered = enabled && response.hovered();
    let fill = if !enabled {
        DISABLED_BG
    } else if primary {
        if hovered { ACCENT_HOVER } else { ACCENT }
    } else if hovered {
        Color32::from_rgb(0xEC, 0xF0, 0xF7)
    } else {
        Color32::from_rgb(0xF3, 0xF5, 0xF9)
    };

    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(9), fill);
    if !primary {
        p.rect_stroke(
            rect,
            CornerRadius::same(9),
            Stroke::new(1.0, BORDER),
            StrokeKind::Inside,
        );
    }

    let mut x = rect.center().x - content_w / 2.0;
    if let Some(icon) = icon {
        paint_icon(
            p,
            icon,
            Rect::from_center_size(
                pos2(x + icon_w / 2.0, rect.center().y),
                vec2(icon_w, icon_w),
            ),
            color,
        );
        x += icon_w + gap;
    }
    p.text(
        pos2(x, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        font,
        color,
    );

    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response
}

fn icon_button(ui: &mut Ui, icon: Icon, size: f32, enabled: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::splat(size),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let hovered = enabled && response.hovered();
    let p = ui.painter();
    p.rect_filled(
        rect,
        CornerRadius::same(9),
        if hovered {
            Color32::from_rgb(0xEC, 0xF0, 0xF7)
        } else {
            Color32::from_rgb(0xF3, 0xF5, 0xF9)
        },
    );
    p.rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(1.0, BORDER),
        StrokeKind::Inside,
    );
    paint_icon(
        p,
        icon,
        rect.shrink(size * 0.24),
        if enabled { MUTED } else { DISABLED_FG },
    );
    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response
}

fn dot(ui: &mut Ui, color: Color32, diameter: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(diameter + 4.0), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), diameter / 2.0, color);
}

fn pill(ui: &mut Ui, label: &str, color: Color32, bg: Color32) {
    Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(100))
        .inner_margin(Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                dot(ui, color, 9.0);
                ui.label(RichText::new(label).size(12.5).color(color));
            });
        });
}

fn device_row(ui: &mut Ui, device: &ReceiverDevice, selected: bool) -> egui::Response {
    let narrow = ui.available_width() < 330.0;
    let protocol = device.protocol_label();
    let ir = Frame::new()
        .fill(if selected { SEL_BG } else { CARD })
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
                paint_icon(
                    ui.painter(),
                    Icon::Monitor,
                    rect.shrink(2.0),
                    if selected { ACCENT } else { MUTED },
                );
                ui.add_space(6.0);
                let subtitle = {
                    let mut line = format!("{protocol}  ·  {}:{}", device.host(), device.port());
                    if let (true, Some(model)) = (narrow, device.model()) {
                        line.push_str(&format!("  ·  {model}"));
                    }
                    line
                };
                if narrow {
                    ui.vertical(|ui| {
                        ui.add_space(1.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(device.name()).size(15.0).strong().color(TEXT),
                            )
                            .truncate(),
                        );
                        ui.add(
                            egui::Label::new(RichText::new(subtitle).size(12.5).color(MUTED))
                                .truncate(),
                        );
                    });
                } else {
                    let badge_width = 78.0;
                    let info_width = (ui.available_width() - badge_width - 8.0).max(90.0);
                    ui.allocate_ui_with_layout(
                        vec2(info_width, 36.0),
                        Layout::top_down(Align::LEFT),
                        |ui| {
                            ui.add_space(1.0);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(device.name()).size(15.0).strong().color(TEXT),
                                )
                                .truncate(),
                            );
                            ui.add(
                                egui::Label::new(RichText::new(subtitle).size(12.5).color(MUTED))
                                    .truncate(),
                            );
                        },
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let badge = device.model().unwrap_or(protocol);
                        ui.label(RichText::new(badge).size(12.5).color(FAINT));
                    });
                }
            });
        });

    let response = ir.response.interact(Sense::click());
    if response.hovered() && !selected {
        ui.painter().rect_filled(
            ir.response.rect,
            CornerRadius::same(9),
            Color32::from_rgba_unmultiplied(31, 111, 229, 10),
        );
    }
    if selected {
        let r = ir.response.rect;
        ui.painter().rect_filled(
            Rect::from_min_size(pos2(r.left(), r.top() + 7.0), vec2(3.0, r.height() - 14.0)),
            CornerRadius::same(2),
            ACCENT,
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response
}

// ---- app --------------------------------------------------------------------

struct CerminApp {
    cmd_tx: Sender<Cmd>,
    ev_rx: Receiver<Ev>,
    devices: Vec<ReceiverDevice>,
    selected: usize,
    displays: Vec<DisplayInfo>,
    selected_display: usize,
    log: Vec<String>,
    searching: bool,
    session_active: bool,
    connect_pending: bool,
    active_is_cast: bool,
    /// Cast system-audio choice for the next connection.
    cast_audio: bool,
    /// System-audio choice of the running Cast session (for status text).
    active_cast_audio: bool,
    /// Cast video-quality choice for the next connection.
    cast_quality: CastQuality,
    /// Cast latency choice for the next connection.
    cast_latency: CastLatency,
    /// Latency choice of the running Cast session; only for status text, so a
    /// selector change during a session can never rewrite the startup copy.
    active_cast_latency: CastLatency,
    streaming: bool,
    needs_pin: bool,
    focus_pin: bool,
    pin: String,
    manual_target: String,
    manual_error: Option<String>,
    volume: u32,
    status_title: String,
    status_desc: String,
    status_color: Color32,
    styled: bool,
}

impl CerminApp {
    fn new(cmd_tx: Sender<Cmd>, ev_rx: Receiver<Ev>) -> Self {
        let mut app = Self {
            cmd_tx,
            ev_rx,
            devices: Vec::new(),
            selected: 0,
            displays: Vec::new(),
            selected_display: 0,
            log: Vec::new(),
            searching: false,
            session_active: false,
            connect_pending: false,
            active_is_cast: false,
            cast_audio: cfg!(target_os = "windows"),
            active_cast_audio: false,
            cast_quality: CastQuality::default(),
            cast_latency: CastLatency::default(),
            active_cast_latency: CastLatency::default(),
            streaming: false,
            needs_pin: false,
            focus_pin: false,
            pin: String::new(),
            manual_target: String::new(),
            manual_error: None,
            volume: 35,
            status_title: "Ready".into(),
            status_desc: "Press Search to look for AirPlay and Google Cast devices.".into(),
            status_color: GREEN,
            styled: false,
        };
        app.push_log("Cermin started");
        app
    }

    fn push_log(&mut self, line: impl Into<String>) {
        self.log.push(format!("{}  {}", timestamp(), line.into()));
        if self.log.len() > 300 {
            self.log.remove(0);
        }
    }

    fn search(&mut self) {
        // Disable further starts immediately, not only when the worker
        // acknowledges with Ev::Searching, so double clicks cannot queue two
        // scans (the GUI `enabled` flags also check `searching`).
        if self.searching || self.session_active || self.connect_pending {
            return;
        }
        self.searching = true;
        self.status_title = "Searching".into();
        self.status_desc = "Looking for AirPlay and Google Cast devices on your network...".into();
        self.status_color = ACCENT;
        if self.cmd_tx.send(Cmd::Search).is_err() {
            self.searching = false;
            self.status_title = "Error".into();
            self.status_desc = "Could not reach the session worker.".into();
            self.status_color = RED;
            self.push_log("Search failed: session worker channel closed.");
        }
    }

    fn refresh_displays(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Displays);
    }

    fn disconnect(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Stop);
        self.status_title = "Stopping".into();
        self.status_desc = "Closing the session...".into();
        self.status_color = MUTED;
    }

    fn connect(&mut self) {
        if self.connect_pending {
            return;
        }
        let Some(device) = self.devices.get(self.selected).cloned() else {
            self.status_title = "No TV selected".into();
            self.status_desc = "Pick a TV from the list first.".into();
            self.status_color = AMBER;
            return;
        };

        // Dispatch before the AirPlay credential lookup: Google Cast has no
        // HAP pairing and must never read or store an AirPlay PIN.
        let has_credentials = match &device {
            ReceiverDevice::AirPlay(airplay) => {
                PairingManager::load(resolve_credentials_path(None))
                    .map(|m| m.has_credentials(&airplay.device_id))
                    .unwrap_or(false)
            }
            ReceiverDevice::GoogleCast(_) => false,
        };

        match connect_plan(&device, has_credentials, &self.pin) {
            Ok(SessionPlan::Cast) => self.finish_connect(device, None),
            Ok(SessionPlan::AirPlay { pin }) => self.finish_connect(device, pin),
            Err(ConnectPlanError::PinRequired) => {
                self.needs_pin = true;
                self.focus_pin = true;
                self.status_title = "AirPlay code needed".into();
                self.status_desc = "Enter the code shown on your TV.".into();
                self.status_color = AMBER;
            }
        }
    }

    fn finish_connect(&mut self, device: ReceiverDevice, pin: Option<String>) {
        self.needs_pin = false;
        self.connect_pending = true;
        self.active_is_cast = device.is_cast();
        // AirPlay always mirrors with system audio; the flag is Cast-only. The
        // checkbox is disabled (and false by default) off Windows, and a
        // programmatic `true` still reaches the backend's actionable error.
        let cast_audio = if device.is_cast() {
            self.cast_audio
        } else {
            true
        };
        self.active_cast_audio = device.is_cast() && self.cast_audio;
        // Snapshot the Cast-only choices now: changing a selector while the
        // session runs must not alter its encoder or HLS profile.
        let cast_settings = CastSettings {
            quality: self.cast_quality,
            latency: self.cast_latency,
        };
        self.active_cast_latency = if device.is_cast() {
            cast_settings.latency
        } else {
            CastLatency::default()
        };
        let display_index = self.displays.get(self.selected_display).map(|d| d.index);
        if self
            .cmd_tx
            .send(Cmd::Connect(
                Box::new(device),
                pin,
                display_index,
                cast_audio,
                cast_settings,
            ))
            .is_err()
        {
            self.connect_pending = false;
            self.status_title = "Error".into();
            self.status_desc = "Could not reach the session worker.".into();
            self.status_color = RED;
            self.push_log("Could not start the session: worker channel closed.");
        }
    }

    fn add_manual_cast(&mut self) {
        match manual_cast_device(&self.manual_target) {
            Ok(device) => {
                let (host, port) = (device.host.clone(), device.port);
                self.manual_error = None;
                self.manual_target.clear();
                self.devices.push(ReceiverDevice::GoogleCast(device));
                self.selected = self.devices.len() - 1;
                self.needs_pin = false;
                self.status_title = "Ready".into();
                self.status_desc = format!("Manual Google Cast target {host}:{port} added.");
                self.status_color = GREEN;
                self.push_log(format!("Added manual Google Cast target {host}:{port}"));
            }
            Err(error) => {
                self.manual_error = Some(error.to_string());
                self.status_title = "Invalid Google Cast target".into();
                self.status_desc = error.to_string();
                self.status_color = AMBER;
                self.push_log(format!("Manual Google Cast target rejected: {error}"));
            }
        }
    }

    fn selected_device(&self) -> Option<&ReceiverDevice> {
        self.devices.get(self.selected)
    }

    fn handle_events(&mut self) {
        loop {
            match self.ev_rx.try_recv() {
                Ok(Ev::Searching) => {
                    self.searching = true;
                    self.devices.clear();
                    self.selected = 0;
                    self.manual_error = None;
                    self.status_title = "Searching".into();
                    self.status_desc =
                        "Looking for AirPlay and Google Cast devices on your network...".into();
                    self.status_color = ACCENT;
                    self.push_log("Searching for devices on your network...");
                }
                Ok(Ev::Devices(devices)) => {
                    self.searching = false;
                    self.devices = devices;
                    self.selected = 0;
                    if self.devices.is_empty() {
                        self.status_title = "No TVs found".into();
                        self.status_desc =
                            "Check that the TV is on and on the same Wi-Fi, then search again."
                                .into();
                        self.status_color = AMBER;
                        self.push_log("Found 0 device(s).");
                    } else {
                        self.status_title = "Ready".into();
                        self.status_desc = format!(
                            "Found {} device(s). Select one and press Connect.",
                            self.devices.len()
                        );
                        self.status_color = GREEN;
                        self.push_log(format!(
                            "Found {} device(s). Select one and press Connect.",
                            self.devices.len()
                        ));
                    }
                }
                Ok(Ev::Displays(displays)) => {
                    self.displays = displays;
                    if self.selected_display >= self.displays.len() {
                        self.selected_display = 0;
                    }
                    match self.displays.get(self.selected_display) {
                        Some(display) => {
                            self.push_log(format!("Capture display: {}", display.label()));
                        }
                        None => {
                            self.push_log("No capture displays detected.");
                        }
                    }
                }
                Ok(Ev::SessionStarting(name)) => {
                    self.session_active = true;
                    self.connect_pending = false;
                    self.streaming = false;
                    self.status_title = "Connecting".into();
                    if self.active_is_cast {
                        // The Cast path buffers the snapshot latency preset's
                        // initial buffer before the receiver is asked to play.
                        let initial_buffer_secs = self.active_cast_latency.initial_buffer_secs();
                        self.status_desc = format!(
                            "Preparing live stream (initial buffer about {initial_buffer_secs} \
                             seconds)..."
                        );
                        self.push_log(format!(
                            "Connecting to {name}: Preparing live stream (initial buffer about \
                             {initial_buffer_secs} seconds)..."
                        ));
                    } else {
                        self.status_desc = format!("Setting up the session with {name}...");
                        self.push_log(format!("Connecting to {name}..."));
                    }
                    self.status_color = ACCENT;
                }
                Ok(Ev::Streaming) => {
                    self.streaming = true;
                    // Surface an automatic capture decision (e.g. the GDI
                    // fallback on hybrid GPUs) once, when the stream starts.
                    if let Some(notice) = rotten_capture::take_capture_notice() {
                        self.push_log(notice);
                    }
                    if self.active_is_cast {
                        // PLAYING only proves the receiver accepted the stream.
                        self.status_title = "Playing".into();
                        self.status_desc = cast_playing_desc(self.active_cast_audio).into();
                        self.push_log(if self.active_cast_audio {
                            "Receiver reports PLAYING (video + system audio)."
                        } else {
                            "Receiver reports PLAYING (video only)."
                        });
                    } else {
                        self.status_title = "Mirroring".into();
                        self.status_desc = "Your screen is on the TV.".into();
                        self.push_log("Streaming started.");
                    }
                    self.status_color = GREEN;
                }
                Ok(Ev::Stopped(reason)) => {
                    self.session_active = false;
                    self.connect_pending = false;
                    self.streaming = false;
                    self.status_title = "Stopped".into();
                    self.status_desc = reason.clone();
                    self.status_color = MUTED;
                    self.push_log(format!("Session stopped: {reason}"));
                }
                Ok(Ev::Error(e)) => {
                    self.searching = false;
                    self.connect_pending = false;
                    self.status_title = "Error".into();
                    self.status_desc = e.clone();
                    self.status_color = RED;
                    self.push_log(e);
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    // ---- sections -----------------------------------------------------------

    fn header(&mut self, ui: &mut Ui) {
        let width = ui.available_width();
        let show_tagline = width > 620.0;
        let compact = width < 520.0;
        ui.horizontal(|ui| {
            let logo = if compact { 38.0 } else { 46.0 };
            let (rect, _) = ui.allocate_exact_size(vec2(logo, logo), Sense::hover());
            draw_logo(ui.painter(), rect);
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Cermin")
                        .size(if compact { 24.0 } else { 28.0 })
                        .strong()
                        .color(TEXT),
                );
                ui.label(
                    RichText::new("AirPlay and Google Cast — no cables")
                        .size(if compact { 12.0 } else { 13.5 })
                        .color(MUTED),
                );
            });
            if show_tagline {
                ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.add_space(7.0);
                        ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                            ui.label(
                                RichText::new("Your PC screen. On your TV.")
                                    .size(13.0)
                                    .color(MUTED),
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                            ui.label(
                                RichText::new("No cables. No accounts. Just works.")
                                    .size(13.0)
                                    .color(MUTED),
                            );
                        });
                    });
                });
            }
        });
    }

    fn devices_card(&mut self, ui: &mut Ui) {
        card(ui, |ui| {
            let wide = ui.available_width() > 430.0;
            if wide {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("TVs found on your network")
                                .size(16.0)
                                .strong()
                                .color(TEXT),
                        );
                        ui.label(
                            RichText::new("Select a TV and press Connect.")
                                .size(12.0)
                                .color(MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let enabled =
                            !self.searching && !self.session_active && !self.connect_pending;
                        if action_button(ui, "Search", Some(Icon::Magnifier), true, enabled, 104.0)
                            .clicked()
                        {
                            self.search();
                        }
                        if icon_button(ui, Icon::Refresh, 36.0, enabled).clicked() {
                            self.search();
                        }
                    });
                });
            } else {
                ui.label(
                    RichText::new("TVs found on your network")
                        .size(16.0)
                        .strong()
                        .color(TEXT),
                );
                ui.label(
                    RichText::new("Select a TV and press Connect.")
                        .size(12.0)
                        .color(MUTED),
                );
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    let enabled = !self.searching && !self.session_active && !self.connect_pending;
                    if action_button(ui, "Search", Some(Icon::Magnifier), true, enabled, 104.0)
                        .clicked()
                    {
                        self.search();
                    }
                    if icon_button(ui, Icon::Refresh, 36.0, enabled).clicked() {
                        self.search();
                    }
                });
            }
            ui.add_space(8.0);
            ScrollArea::vertical()
                .id_salt("devices_list")
                .max_height(200.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if self.devices.is_empty() {
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new(
                                "Press Search to look for AirPlay and Google Cast devices.",
                            )
                            .size(13.0)
                            .color(FAINT),
                        );
                        ui.add_space(10.0);
                    }
                    let selectable = !self.session_active && !self.connect_pending;
                    for i in 0..self.devices.len() {
                        let selected = i == self.selected;
                        let device = self.devices[i].clone();
                        if device_row(ui, &device, selected).clicked() && selectable {
                            self.selected = i;
                            self.needs_pin = false;
                        }
                        ui.add_space(2.0);
                    }
                });

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(6.0);
            let idle = !self.searching && !self.session_active && !self.connect_pending;
            ui.label(
                RichText::new("Google Cast IP / hostname")
                    .size(12.0)
                    .strong()
                    .color(TEXT),
            );
            ui.horizontal(|ui| {
                let add_width = 74.0;
                let field_width = (ui.available_width() - add_width - 8.0).max(90.0);
                let response = ui.add_enabled(
                    idle,
                    egui::TextEdit::singleline(&mut self.manual_target)
                        .hint_text("192.168.1.50")
                        .desired_width(field_width)
                        .font(FontId::proportional(13.0)),
                );
                if response.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    && idle
                {
                    self.add_manual_cast();
                }
                if action_button(ui, "Add", None, false, idle, add_width).clicked() {
                    self.add_manual_cast();
                }
            });
            if let Some(error) = &self.manual_error {
                ui.add(egui::Label::new(RichText::new(error).size(11.0).color(RED)).truncate());
            }
        });
    }

    fn connection_card(&mut self, ui: &mut Ui) {
        let selected_device = self.devices.get(self.selected).cloned();
        let selected_is_cast = selected_device.as_ref().is_some_and(|d| d.is_cast());
        let show_airplay_pin = shows_airplay_pin(selected_device.as_ref());
        let session_active = self.session_active;
        let connect_pending = self.connect_pending;
        let streaming = self.streaming;
        let active_is_cast = self.active_is_cast;
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Connection").size(16.0).strong().color(TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if streaming {
                        if active_is_cast {
                            pill(ui, "Playing", GREEN, Color32::from_rgb(0xE8, 0xF7, 0xEE));
                        } else {
                            pill(ui, "Mirroring", GREEN, Color32::from_rgb(0xE8, 0xF7, 0xEE));
                        }
                    } else if session_active {
                        pill(
                            ui,
                            "Connecting",
                            ACCENT,
                            Color32::from_rgb(0xE8, 0xF0, 0xFD),
                        );
                    } else {
                        pill(
                            ui,
                            "Not connected",
                            MUTED,
                            Color32::from_rgb(0xF1, 0xF3, 0xF6),
                        );
                    }
                });
            });
            ui.add_space(6.0);

            let compact = ui.available_width() < 380.0;
            match &selected_device {
                Some(device) => {
                    ui.horizontal(|ui| {
                        let icon = if compact { 36.0 } else { 42.0 };
                        let (rect, _) = ui.allocate_exact_size(vec2(icon, icon), Sense::hover());
                        paint_icon(ui.painter(), Icon::Monitor, rect.shrink(3.0), ACCENT);
                        ui.add_space(8.0);
                        ui.vertical(|ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(device.name())
                                        .size(if compact { 15.0 } else { 16.5 })
                                        .strong()
                                        .color(TEXT),
                                )
                                .truncate(),
                            );
                            let mut line = format!("{}:{}", device.host(), device.port());
                            if let Some(model) = device.model() {
                                line.push_str(&format!("  ·  {model}"));
                            }
                            ui.add(
                                egui::Label::new(RichText::new(line).size(12.5).color(MUTED))
                                    .truncate(),
                            );
                            if !compact {
                                let line = if selected_is_cast {
                                    format!("Google Cast · Device ID: {}", device.device_id())
                                } else {
                                    format!("Device ID: {}", device.device_id())
                                };
                                ui.label(RichText::new(line).size(12.0).color(FAINT));
                            }
                        });
                    });
                }
                None => {
                    ui.label(
                        RichText::new("No TV selected — pick one from the list.")
                            .size(13.0)
                            .color(FAINT),
                    );
                }
            }

            ui.add_space(8.0);
            ui.label(
                RichText::new("Screen to mirror")
                    .size(13.0)
                    .strong()
                    .color(TEXT),
            );
            ui.label(
                RichText::new("Which monitor is sent to the TV.")
                    .size(11.0)
                    .color(FAINT),
            );
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                let selected_text = self
                    .displays
                    .get(self.selected_display)
                    .map(|d| d.label())
                    .unwrap_or_else(|| "No displays detected".to_string());
                let combo_width = (ui.available_width() - 42.0).max(120.0);
                ui.add_enabled_ui(!session_active, |ui| {
                    egui::ComboBox::from_id_salt("mirror_display")
                        .width(combo_width)
                        .selected_text(RichText::new(selected_text).size(13.0).color(TEXT))
                        .show_ui(ui, |ui| {
                            for (i, display) in self.displays.iter().enumerate() {
                                ui.selectable_value(&mut self.selected_display, i, display.label());
                            }
                        });
                });
                if icon_button(ui, Icon::Refresh, 32.0, true).clicked() {
                    self.refresh_displays();
                }
            });

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);

            if show_airplay_pin {
                if ui.available_width() > 430.0 {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("AirPlay code")
                                .size(13.0)
                                .strong()
                                .color(TEXT),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(
                                RichText::new("First time: enter the code shown on your TV.")
                                    .size(11.0)
                                    .color(FAINT),
                            );
                        });
                    });
                } else {
                    ui.label(
                        RichText::new("AirPlay code")
                            .size(13.0)
                            .strong()
                            .color(TEXT),
                    );
                    ui.label(
                        RichText::new("First time: enter the code shown on your TV.")
                            .size(11.0)
                            .color(FAINT),
                    );
                }
                ui.add_space(3.0);
                let text_edit = egui::TextEdit::singleline(&mut self.pin)
                    .hint_text("Enter 4-digit code (e.g. 1234)")
                    .font(FontId::proportional(14.0));
                let width = ui.available_width();
                let response = ui.add_sized([width, 34.0], text_edit);
                if self.focus_pin {
                    response.request_focus();
                    self.focus_pin = false;
                }
            } else {
                Frame::new()
                    .fill(Color32::from_rgb(0xFD, 0xF4, 0xE3))
                    .stroke(Stroke::new(1.0, AMBER))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::same(8))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(CAST_WARNING)
                                .size(12.0)
                                .color(Color32::from_rgb(0x8A, 0x5A, 0x00)),
                        );
                    });
                ui.add_space(6.0);
                // Cast-only system-audio choice; AirPlay always sends audio.
                let audio_supported = cfg!(target_os = "windows");
                let can_edit = !session_active && !connect_pending;
                ui.add_enabled(
                    can_edit && audio_supported,
                    egui::Checkbox::new(&mut self.cast_audio, "System audio (AAC)"),
                );
                let hint = if !audio_supported {
                    "Not supported on this platform; Cast stays video only."
                } else if self.cast_audio {
                    "Captured system sound; your local speakers are not muted."
                } else {
                    "Video only; enable to send system sound to the receiver."
                };
                ui.label(RichText::new(hint).size(11.0).color(FAINT));

                // Cast-only quality/latency choices, locked while a session is
                // starting or running (the selection is snapshotted on Connect).
                ui.add_space(6.0);
                ui.add_enabled_ui(can_edit, |ui| {
                    ui.label(
                        RichText::new("Cast quality")
                            .size(12.5)
                            .strong()
                            .color(TEXT),
                    );
                    egui::ComboBox::from_id_salt("cast_quality")
                        .width(ui.available_width())
                        .selected_text(
                            RichText::new(self.cast_quality.label())
                                .size(12.5)
                                .color(TEXT),
                        )
                        .show_ui(ui, |ui| {
                            for quality in [CastQuality::Balanced, CastQuality::High] {
                                ui.selectable_value(
                                    &mut self.cast_quality,
                                    quality,
                                    quality.label(),
                                );
                            }
                        });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Cast latency")
                            .size(12.5)
                            .strong()
                            .color(TEXT),
                    );
                    egui::ComboBox::from_id_salt("cast_latency")
                        .width(ui.available_width())
                        .selected_text(
                            RichText::new(self.cast_latency.label())
                                .size(12.5)
                                .color(TEXT),
                        )
                        .show_ui(ui, |ui| {
                            for latency in [CastLatency::Stable, CastLatency::Responsive] {
                                ui.selectable_value(
                                    &mut self.cast_latency,
                                    latency,
                                    latency.label(),
                                );
                            }
                        });
                });
                if self.cast_quality == CastQuality::High {
                    ui.label(
                        RichText::new(
                            "High needs more encoder and network capacity for 1080p; actual \
                             frame rate may fall on slower PCs.",
                        )
                        .size(11.0)
                        .color(Color32::from_rgb(0x8A, 0x5A, 0x00)),
                    );
                }
                if self.cast_latency == CastLatency::Responsive {
                    ui.label(
                        RichText::new(
                            "Lower delay can rebuffer more often. Cermin prepares 4 seconds of \
                             media first; the TV may buffer longer.",
                        )
                        .size(11.0)
                        .color(Color32::from_rgb(0x8A, 0x5A, 0x00)),
                    );
                }
            }

            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                let total = ui.available_width();
                let gap = 10.0;
                let connect_w = ((total - gap) * 0.56).max(140.0);
                let disconnect_w = (total - gap - connect_w).max(110.0);
                let can_connect = selected_device.is_some()
                    && !session_active
                    && !self.searching
                    && !connect_pending;
                if action_button(
                    ui,
                    "Connect",
                    Some(Icon::Monitor),
                    true,
                    can_connect,
                    connect_w,
                )
                .clicked()
                {
                    self.connect();
                }
                if action_button(ui, "Disconnect", None, false, session_active, disconnect_w)
                    .clicked()
                {
                    self.disconnect();
                }
            });
        });
    }

    fn volume_card(&mut self, ui: &mut Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                paint_icon(ui.painter(), Icon::Speaker, rect.shrink(1.0), TEXT);
                ui.add_space(6.0);
                ui.label(RichText::new("TV volume").size(15.0).strong().color(TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{} %", self.volume))
                            .size(16.0)
                            .strong()
                            .color(TEXT),
                    );
                });
            });
            ui.add_space(2.0);
            let mut volume = self.volume;
            let response = ui.add_sized(
                [ui.available_width(), 22.0],
                egui::Slider::new(&mut volume, 0..=100).show_value(false),
            );
            if response.changed() {
                self.volume = volume;
                rotten_protocol::set_tv_volume_percent(volume);
            }
        });
    }

    fn status_card(&mut self, ui: &mut Ui) {
        let title = self.status_title.clone();
        let desc = self.status_desc.clone();
        let color = self.status_color;
        card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                dot(ui, color, 11.0);
                ui.add_space(2.0);
                ui.label(RichText::new(title).size(14.5).strong().color(TEXT));
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(desc).size(12.5).color(MUTED)).truncate());
            });
        });
    }

    fn log_card(&mut self, ui: &mut Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Log").size(14.5).strong().color(TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icon_button(ui, Icon::Trash, 26.0, true).clicked() {
                        self.log.clear();
                    }
                });
            });
            ui.add_space(3.0);
            Frame::new()
                .fill(LOG_BG)
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ScrollArea::vertical()
                        .id_salt("log_view")
                        .max_height(96.0)
                        .auto_shrink([false, true])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            if self.log.is_empty() {
                                ui.label(
                                    RichText::new("No events yet.")
                                        .font(FontId::monospace(11.5))
                                        .color(FAINT),
                                );
                            }
                            for line in &self.log {
                                ui.label(
                                    RichText::new(line)
                                        .font(FontId::monospace(11.5))
                                        .color(MUTED),
                                );
                            }
                        });
                });
        });
    }
}

fn apply_style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Light);
    ctx.all_styles_mut(|style| {
        style.visuals = egui::Visuals::light();
        style.visuals.panel_fill = BG;
        style.visuals.window_fill = CARD;
        style.visuals.extreme_bg_color = Color32::WHITE;
        style.visuals.selection.bg_fill = ACCENT;
        style.visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, MUTED);
        style.visuals.widgets.inactive.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.hovered.corner_radius = CornerRadius::same(8);
        style.visuals.widgets.active.corner_radius = CornerRadius::same(8);
        style.spacing.item_spacing = vec2(8.0, 8.0);
        style.spacing.button_padding = vec2(12.0, 7.0);
        style.spacing.slider_width = 200.0;
    });
}

impl eframe::App for CerminApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.styled {
            apply_style(ctx);
            self.styled = true;
        }
        self.handle_events();
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        ui.painter()
            .rect_filled(ui.max_rect(), CornerRadius::ZERO, BG);
        ScrollArea::vertical()
            .id_salt("page")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                Frame::new()
                    .inner_margin(Margin {
                        left: 12,
                        right: 12,
                        top: 4,
                        bottom: 6,
                    })
                    .show(ui, |ui| {
                        self.draw_content(ui);
                    });
            });

        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}

impl CerminApp {
    fn draw_content(&mut self, ui: &mut Ui) {
        self.header(ui);
        ui.add_space(8.0);
        self.status_card(ui);
        ui.add_space(8.0);

        let total = ui.available_width();
        let gap = 10.0;
        // TV volume is an AirPlay RTSP control; Cast keeps no misleading knob.
        let show_volume = shows_tv_volume(self.selected_device());
        // Below ~700 px the two columns get cramped and controls start to
        // collide, so stack the cards vertically instead.
        let stacked = total < 700.0;
        if stacked {
            self.devices_card(ui);
            ui.add_space(8.0);
            self.connection_card(ui);
            if show_volume {
                ui.add_space(8.0);
                self.volume_card(ui);
            }
        } else {
            let mut left_width = (total - gap) * 0.46;
            let mut right_width = total - gap - left_width;
            if right_width < 360.0 {
                right_width = 360.0;
                left_width = total - gap - right_width;
            }
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(left_width);
                    self.devices_card(ui);
                });
                ui.add_space(gap);
                ui.vertical(|ui| {
                    ui.set_width(right_width);
                    self.connection_card(ui);
                    if show_volume {
                        ui.add_space(8.0);
                        self.volume_card(ui);
                    }
                });
            });
        }

        ui.add_space(8.0);
        self.log_card(ui);
    }
}
