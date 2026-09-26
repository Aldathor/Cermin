use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use rotten_core::config::{MirrorCipherMode, MirrorConfig, StreamConfig, resolve_credentials_path};
use rotten_core::debug_log::{DEBUG_BUILD_ID, agent_log};
use rotten_core::device::{CastDevice, ReceiverDevice};
use rotten_discovery::{
    discover_cast_for, discover_devices, discover_for, discover_receivers_for, resolve_device,
};
use rotten_pairing::{PairingManager, format_pin};
use tracing::info;

use crate::cast::{CastLatency, CastQuality};
use crate::mirror::{run_mirror, until_stopped};

#[derive(Parser)]
#[command(name = "cermin-cli")]
#[command(about = "Mirror your PC display to Apple TV (AirPlay) or Google Cast receivers")]
#[command(version)]
pub struct Cli {
    /// Running without a subcommand starts auto mode: discover the receiver,
    /// mirror the screen with system audio, retry on disconnects.
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Write a boot marker and exit (diagnose startup hangs)
    Probe,
    /// Scan the LAN for AirPlay and Google Cast receivers
    Discover {
        /// Discovery timeout in seconds
        #[arg(long, default_value = "5")]
        timeout: u64,
        /// Restrict the scan to one protocol
        #[arg(long, value_enum, default_value = "all")]
        protocol: ProtocolArg,
    },
    /// Check a Google Cast receiver's control connection and status (no streaming)
    CastProbe {
        /// Google Cast hostname or IP address
        #[arg(short, long)]
        target: String,
        /// Google Cast control port
        #[arg(long, default_value = "8009")]
        port: u16,
    },
    /// Stream video (and system audio on Windows) to a Google Cast receiver
    /// (experimental)
    Cast {
        /// Google Cast hostname or IP (required if discovery finds several)
        #[arg(short, long)]
        target: Option<String>,
        /// Google Cast control port
        #[arg(long, default_value = "8009")]
        port: u16,
        /// Display index to capture (see `cermin-cli displays`; default: primary)
        #[arg(long)]
        display: Option<u32>,
        /// Use synthetic test pattern instead of screen capture (with the
        /// deterministic 440 Hz test pulse instead of system audio)
        #[arg(long)]
        test: bool,
        /// Do not capture system audio; video only (Windows defaults to AAC
        /// system sound)
        #[arg(long)]
        no_audio: bool,
        /// Local HTTP port serving the stream (0 = pick a free port)
        #[arg(long, default_value = "0")]
        http_port: u16,
        /// Stop after this many seconds (includes setup; useful with --test)
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        duration: Option<u64>,
        /// Cast video quality preset
        #[arg(long, value_enum, default_value = "balanced")]
        quality: CastQuality,
        /// Cast startup-latency preset (responsive lowers the initial buffer
        /// and may rebuffer more)
        #[arg(long, value_enum, default_value = "stable")]
        latency: CastLatency,
    },
    /// Measure the local capture -> scale -> H.264 -> mux pipeline with no
    /// audio capture, no TV/receiver traffic and no image saved
    CastBenchmark {
        /// Display index to capture (see `cermin-cli displays`; default: primary)
        #[arg(long)]
        display: Option<u32>,
        /// Benchmark duration in seconds (1-60)
        #[arg(long, default_value = "10", value_parser = clap::value_parser!(u64).range(1..=60))]
        duration: u64,
        /// Use the real-size synthetic test pattern instead of screen capture
        #[arg(long)]
        test: bool,
        /// Cast video quality preset to measure
        #[arg(long, value_enum, default_value = "balanced")]
        quality: CastQuality,
        /// Cast startup-latency preset to measure
        #[arg(long, value_enum, default_value = "stable")]
        latency: CastLatency,
    },
    /// List the displays (monitors) that can be mirrored
    Displays,
    /// Capture a few frames to verify screen capture without streaming
    CaptureProbe {
        /// Display index (see `displays`)
        #[arg(long, default_value = "0")]
        display: u32,
    },
    /// Pair with an Apple TV (stores credentials for future sessions)
    Pair {
        /// Apple TV hostname or IP address
        #[arg(short, long)]
        target: String,
        /// PIN shown on the Apple TV screen (prompted interactively if omitted)
        #[arg(short, long)]
        pin: Option<String>,
        /// AirPlay port
        #[arg(long, default_value = "7000")]
        port: u16,
        /// Force re-pairing even if credentials exist
        #[arg(long)]
        force: bool,
        /// Path to credentials file
        #[arg(long)]
        creds: Option<PathBuf>,
    },
    /// Mirror the PC screen to an Apple TV
    Mirror {
        /// Apple TV hostname or IP (skips mDNS discovery)
        #[arg(short, long)]
        target: Option<String>,
        /// 4-digit PIN for first-time pairing
        #[arg(short, long)]
        pin: Option<String>,
        /// AirPlay port
        #[arg(long, default_value = "7000")]
        port: u16,
        /// Stream width (0 = match the captured display; smaller values scale down)
        #[arg(long, default_value = "0")]
        width: u32,
        /// Stream height (0 = match the captured display; smaller values scale down)
        #[arg(long, default_value = "0")]
        height: u32,
        /// Frames per second
        #[arg(long, default_value = "30")]
        fps: u32,
        /// Video bitrate in kbps (0 = auto)
        #[arg(long, default_value = "0")]
        bitrate: u32,
        /// Hardware encoder: auto, nvenc, vaapi, none
        #[arg(long, default_value = "auto")]
        hwaccel: String,
        /// Use synthetic test pattern instead of screen capture
        #[arg(long)]
        test: bool,
        /// Enable audio streaming (experimental)
        #[arg(long)]
        audio: bool,
        /// Force new pairing
        #[arg(long)]
        pair: bool,
        /// Capture only virtual displays (extend mode)
        #[arg(long)]
        virtual_display: bool,
        /// Display index to capture (see `cermin-cli displays`; default: primary)
        #[arg(long)]
        display: Option<u32>,
        /// Path to credentials file
        #[arg(long)]
        creds: Option<PathBuf>,
        /// Verbose debug logging
        #[arg(long)]
        debug: bool,
        /// Send video frames without encryption (debug cipher issues)
        #[arg(long)]
        no_encrypt: bool,
        /// Video cipher: cha-cha (Apple TV default) or aes (legacy UxPlay-style)
        #[arg(long, value_enum, default_value = "cha-cha")]
        cipher: CipherArg,
    },
}

#[derive(Clone, Copy, ValueEnum, Default)]
pub enum ProtocolArg {
    #[default]
    All,
    Airplay,
    Cast,
}

#[derive(Clone, Copy, ValueEnum, Default)]
enum CipherArg {
    Aes,
    #[default]
    #[value(alias = "chacha")]
    ChaCha,
}

impl From<CipherArg> for MirrorCipherMode {
    fn from(v: CipherArg) -> Self {
        match v {
            CipherArg::Aes => MirrorCipherMode::AesCtr,
            CipherArg::ChaCha => MirrorCipherMode::ChaCha,
        }
    }
}

impl Cli {
    pub async fn run(self) -> anyhow::Result<()> {
        let stop = new_stop_flag();
        finish_on_shutdown(
            self.run_with_stop(stop.clone()),
            stop,
            tokio::signal::ctrl_c(),
        )
        .await
    }

    async fn run_with_stop(self, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
        let command = match self.command {
            Some(command) => command,
            None => return run_auto(stop).await,
        };
        match command {
            Commands::Probe => {}
            Commands::Discover { timeout, protocol } => {
                let timeout = Duration::from_secs(timeout);
                let receivers: Option<Vec<ReceiverDevice>> = match protocol {
                    ProtocolArg::All => {
                        until_stopped(discover_receivers_for(timeout), stop.clone()).await?
                    }
                    ProtocolArg::Airplay => until_stopped(discover_for(timeout), stop.clone())
                        .await?
                        .map(|devices| devices.into_iter().map(ReceiverDevice::AirPlay).collect()),
                    ProtocolArg::Cast => until_stopped(discover_cast_for(timeout), stop.clone())
                        .await?
                        .map(|devices| {
                            devices
                                .into_iter()
                                .map(ReceiverDevice::GoogleCast)
                                .collect()
                        }),
                };
                let Some(receivers) = receivers else {
                    return Ok(());
                };
                if receivers.is_empty() {
                    println!("No devices found.");
                } else {
                    println!("Found {} device(s):\n", receivers.len());
                    for device in &receivers {
                        println!(
                            "  [{}] {} — {}:{} ({})",
                            device.protocol_label(),
                            device.name(),
                            device.host(),
                            device.port(),
                            device.device_id()
                        );
                        if let Some(model) = device.model() {
                            println!("    model: {model}");
                        }
                    }
                }
            }
            Commands::CastProbe { target, port } => {
                let device = CastDevice::manual(&target, port)?;
                let Some(status) = until_stopped(probe_cast(&device), stop.clone()).await? else {
                    return Ok(());
                };
                print_cast_status(&status);
            }
            Commands::Cast {
                target,
                port,
                display,
                test,
                no_audio,
                http_port,
                duration,
                quality,
                latency,
            } => {
                let audio = cast_audio_enabled(no_audio);
                let device = if let Some(target) = target {
                    CastDevice::manual(&target, port)?
                } else {
                    let Some(devices) =
                        until_stopped(discover_cast_for(Duration::from_secs(5)), stop.clone())
                            .await?
                    else {
                        return Ok(());
                    };
                    match devices.len() {
                        0 => anyhow::bail!("no Google Cast devices found; pass --target <host>"),
                        1 => devices.into_iter().next().expect("single device"),
                        count => {
                            println!(
                                "Found {count} Google Cast devices; pass --target to choose one:\n"
                            );
                            for device in &devices {
                                println!("  {} — {}:{}", device.name, device.host, device.port);
                            }
                            anyhow::bail!(
                                "multiple Google Cast devices found; refusing to pick one at random"
                            );
                        }
                    }
                };

                if audio {
                    println!(
                        "Warning: experimental Google Cast path — H.264 video with AAC system audio,"
                    );
                } else {
                    println!(
                        "Warning: experimental Google Cast path — H.264 video only (system audio off),"
                    );
                }
                println!(
                    "         several seconds of buffering, and your local speakers are not muted."
                );
                println!("         Unencrypted media on the LAN; use only on trusted networks.");
                println!("Casting {} ({}:{})", device.name, device.host, device.port);
                println!(
                    "Preset: {} quality, {} latency (initial HLS buffer about {} seconds).",
                    quality.label(),
                    latency.label(),
                    latency.initial_buffer_secs()
                );

                let config = crate::cast::CastConfig {
                    display_index: display,
                    test_mode: test,
                    http_port,
                    audio,
                    quality,
                    latency,
                };
                // One stop flag is shared by the session and the optional
                // deadline, so Ctrl-C and --duration both take the cooperative
                // STOP/server/producer cleanup path.
                let deadline = duration.map(Duration::from_secs);
                run_cast_with_optional_deadline(
                    crate::cast::run_cast(
                        device,
                        config,
                        stop.clone(),
                        Some(Box::new(move || {
                            if audio {
                                println!(
                                    "Receiver reports PLAYING (video + system audio). Press Ctrl+C to stop."
                                );
                            } else {
                                println!(
                                    "Receiver reports PLAYING (video only). Press Ctrl+C to stop."
                                );
                            }
                        })),
                    ),
                    stop,
                    deadline,
                )
                .await?;
            }
            Commands::CastBenchmark {
                display,
                duration,
                test,
                quality,
                latency,
            } => {
                println!(
                    "Cast benchmark: measures the local capture + scale + H.264 + mux pipeline."
                );
                println!(
                    "No audio is captured, no TV or receiver is contacted, no HTTP server runs,"
                );
                println!("and no image or stream is saved anywhere.");
                println!(
                    "Preset: {} quality, {} latency (target {} kbps).",
                    quality.label(),
                    latency.label(),
                    quality.bitrate_kbps()
                );
                if test {
                    let (width, height) = quality.synthetic_dims();
                    println!(
                        "Using the same {width}x{height} synthetic test pattern as `cast --test` \
                         (no display)."
                    );
                }
                println!("Measuring for {duration}s; press Ctrl+C to stop early.");
                let summary = crate::cast::run_capture_benchmark_with_presets(
                    display,
                    test,
                    Duration::from_secs(duration),
                    stop,
                    quality,
                    latency,
                )
                .await?;
                println!("{}", summary.display_text());
            }
            Commands::Displays => {
                let displays = rotten_capture::list_displays()?;
                if displays.is_empty() {
                    println!("No displays found.");
                } else {
                    println!("Found {} display(s):\n", displays.len());
                    for d in &displays {
                        println!("  {}", d.label());
                        if let Some(adapter) = &d.adapter {
                            println!("      adapter: {adapter}");
                        }
                    }
                    println!("\nPick one with: cermin-cli mirror --display <index>");
                }
            }
            Commands::CaptureProbe { display } => {
                run_capture_probe(display)?;
            }
            Commands::Pair {
                target,
                pin,
                port,
                force,
                creds,
            } => {
                let Some(device) =
                    until_stopped(resolve_device(&target, port), stop.clone()).await?
                else {
                    return Ok(());
                };
                let pin = pin.map(|p| format_pin(&p)).transpose()?;
                let creds_path = resolve_credentials_path(creds);
                let mut manager = PairingManager::load(creds_path)?;
                let Some(stored) =
                    until_stopped(manager.pair(&device, pin.as_deref(), force), stop.clone())
                        .await?
                else {
                    return Ok(());
                };
                println!("Paired with {} ({})", device.name, stored.device_id);
            }
            Commands::Mirror {
                target,
                pin,
                port,
                width,
                height,
                fps,
                bitrate,
                hwaccel,
                test,
                audio,
                pair,
                virtual_display,
                display,
                creds,
                debug,
                no_encrypt,
                cipher,
            } => {
                // #region agent log
                agent_log(
                    "cli.rs:mirror",
                    "mirror command starting",
                    "H17",
                    serde_json::json!({
                        "buildId": DEBUG_BUILD_ID,
                        "target": target.as_deref(),
                        "test": test,
                        "noEncrypt": no_encrypt,
                        "cipher": match cipher {
                            CipherArg::Aes => "aes",
                            CipherArg::ChaCha => "chacha",
                        },
                    }),
                );
                // #endregion

                if debug {
                    tracing::subscriber::set_global_default(
                        tracing_subscriber::fmt().with_env_filter("debug").finish(),
                    )
                    .ok();
                }

                let device = if let Some(t) = target {
                    // #region agent log
                    agent_log(
                        "cli.rs:mirror",
                        "resolving target",
                        "H17",
                        serde_json::json!({ "target": &t, "port": port }),
                    );
                    // #endregion
                    let Some(device) =
                        until_stopped(resolve_device(&t, port), stop.clone()).await?
                    else {
                        return Ok(());
                    };
                    // #region agent log
                    agent_log(
                        "cli.rs:mirror",
                        "target resolved",
                        "H17",
                        serde_json::json!({
                            "name": device.name,
                            "host": device.host,
                        }),
                    );
                    // #endregion
                    device
                } else {
                    let Some(devices) = until_stopped(discover_devices(), stop.clone()).await?
                    else {
                        return Ok(());
                    };
                    devices
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("no AirPlay devices found; use --target"))?
                };

                info!(name = %device.name, host = %device.host, "selected device");

                let config = MirrorConfig {
                    stream: StreamConfig {
                        width,
                        height,
                        fps,
                        bitrate_kbps: bitrate,
                    },
                    pin: pin.map(|p| format_pin(&p)).transpose()?,
                    force_pair: pair,
                    test_mode: test,
                    audio,
                    hw_accel: rotten_core::config::HwAccel::from_str(&hwaccel),
                    credentials_path: resolve_credentials_path(creds),
                    display_index: display,
                    virtual_display_only: virtual_display,
                    no_encrypt,
                    cipher: cipher.into(),
                };

                run_mirror(device, config, stop, None).await?;
            }
        }
        Ok(())
    }
}

/// Auto mode (no subcommand — double-click friendly): find the receiver on the
/// network, mirror with system audio and retry forever on disconnects.
async fn run_auto(stop: Arc<AtomicBool>) -> anyhow::Result<()> {
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt().with_env_filter("info").finish(),
    )
    .ok();

    println!("Cermin — AirPlay screen mirroring with system audio");
    println!("=========================================================");
    println!("Keep this window open while mirroring; press Ctrl+C to stop.");
    println!("On first run you will be asked for the code shown on the TV.\n");

    while !stop.load(Ordering::Relaxed) {
        match run_auto_once(stop.clone()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!("\nauto: session ended: {e}");
                eprintln!("auto: rediscovering and retrying in 5 seconds (Ctrl+C to stop)...\n");
                until_stopped(
                    async {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        Ok::<_, anyhow::Error>(())
                    },
                    stop.clone(),
                )
                .await?;
            }
        }
    }
    Ok(())
}

async fn run_auto_once(stop: Arc<AtomicBool>) -> anyhow::Result<()> {
    let Some(device) = until_stopped(discover_receiver(), stop.clone()).await? else {
        return Ok(());
    };

    let credentials_path = resolve_credentials_path(None);
    let has_credentials = rotten_pairing::PairingManager::load(credentials_path.clone())
        .map(|m| m.has_credentials(&device.device_id))
        .unwrap_or(false);
    if has_credentials {
        println!(
            "auto: found '{}' at {}:{} — resuming saved session\n",
            device.name, device.host, device.port
        );
    } else {
        println!(
            "auto: found '{}' at {}:{}",
            device.name, device.host, device.port
        );
        println!("auto: first-time setup — enter the AirPlay code shown on the TV when prompted\n");
    }

    let config = MirrorConfig {
        stream: StreamConfig {
            width: 0,
            height: 0,
            fps: 30,
            bitrate_kbps: 0,
        },
        // No PIN here: pairing prompts interactively only when credentials are missing.
        pin: None,
        force_pair: false,
        test_mode: false,
        audio: true,
        hw_accel: rotten_core::config::HwAccel::from_str("auto"),
        credentials_path,
        display_index: None,
        virtual_display_only: false,
        no_encrypt: false,
        cipher: MirrorCipherMode::ChaCha,
    };

    run_mirror(device, config, stop, None).await?;
    Ok(())
}

/// Await a Cast session future, optionally requesting a cooperative stop after
/// `duration`. The timer is polled inline, not by a detached task: when it
/// fires it sets the session's existing stop flag and keeps awaiting the same
/// pinned future, so the receiver STOP, HTTP server shutdown, and producer
/// cleanup all complete before this returns. `None` preserves the indefinite
/// behavior, and a Ctrl-C that sets `stop` externally is observed identically.
async fn run_cast_with_optional_deadline<F, T, E>(
    future: F,
    stop: Arc<AtomicBool>,
    duration: Option<Duration>,
) -> std::result::Result<T, E>
where
    F: std::future::Future<Output = std::result::Result<T, E>>,
{
    let Some(duration) = duration else {
        return future.await;
    };
    tokio::pin!(future);
    match tokio::time::timeout(duration, &mut future).await {
        Ok(result) => result,
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            // The timed-out future was only borrowed above, so this is the
            // same session future continuing its cleanup.
            future.await
        }
    }
}

/// System audio is on by default on Windows, where WASAPI loopback and Media
/// Foundation AAC are available, and off elsewhere; `--no-audio` disables it
/// everywhere. `--test` keeps this setting but sends the synthetic pulse.
fn cast_audio_enabled(no_audio: bool) -> bool {
    cfg!(target_os = "windows") && !no_audio
}

/// Open the selected capture backend and grab a few frames, so a user can
/// verify screen capture (and see which backend is active) without streaming.
fn run_capture_probe(display: u32) -> anyhow::Result<()> {
    let mut backend = rotten_capture::create_capture_backend(Some(display), false)?;
    println!("Capture backend: {}", backend.backend_name());
    for info in backend.displays()? {
        println!("  {}", info.label());
        if let Some(adapter) = &info.adapter {
            println!("      adapter: {adapter}");
        }
    }

    let mut previous: Option<Vec<u8>> = None;
    for frame_index in 1..=3u32 {
        let started = std::time::Instant::now();
        let frame = backend.grab_frame()?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        let expected = frame.width as usize * frame.height as usize * 4;
        if frame.rgba.len() != expected {
            anyhow::bail!(
                "frame {frame_index} has {} RGBA bytes, expected {expected}",
                frame.rgba.len()
            );
        }
        let note = match previous.as_ref() {
            None => "",
            Some(previous) if previous == &frame.rgba => " (unchanged)",
            Some(_) => " (changed)",
        };
        println!(
            "  frame {frame_index}: {}x{} in {elapsed_ms:.0} ms{note}",
            frame.width, frame.height
        );
        previous = Some(frame.rgba);
    }
    println!("Capture probe OK.");
    Ok(())
}

/// Open the Cast control channel, request the receiver status, and close it.
/// The caller bounds this externally with `until_stopped`. `anyhow::Context`
/// keeps the underlying cause (connection refused, TLS, timeout) visible.
async fn probe_cast(device: &CastDevice) -> anyhow::Result<serde_json::Value> {
    let mut client = rotten_cast::CastClient::connect(&device.host, device.port)
        .await
        .with_context(|| format!("cannot connect to {}:{}", device.host, device.port))?;
    client
        .receiver_status()
        .await
        .context("receiver status request failed")
}

/// Receiver-supplied text is untrusted: strip control characters and bound it.
fn sanitized_label(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).take(80).collect()
}

/// Concise receiver application lines: name, app id, and whether the receiver
/// reports an idle screen. Arbitrary `statusText` (which can embed media URLs)
/// is deliberately not echoed.
fn cast_status_lines(value: &serde_json::Value) -> Vec<String> {
    let status = value.get("status").unwrap_or(value);
    let applications = status
        .get("applications")
        .and_then(|v| v.as_array())
        .filter(|apps| !apps.is_empty());
    let Some(applications) = applications else {
        return vec!["No applications running on the receiver (idle).".to_string()];
    };

    let mut lines = vec![format!("Receiver applications ({}):", applications.len())];
    for app in applications {
        let name = app
            .get("displayName")
            .and_then(|v| v.as_str())
            .or_else(|| {
                app.get("names")
                    .and_then(|v| v.as_array())
                    .and_then(|names| names.first())
                    .and_then(|v| v.as_str())
            })
            .map(sanitized_label)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "<unnamed>".to_string());
        let app_id = sanitized_label(
            app.get("appId")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
        );
        let state = match app.get("isIdleScreen").and_then(|v| v.as_bool()) {
            Some(true) => "idle screen",
            Some(false) => "running",
            None => "active",
        };
        lines.push(format!("  {name} — app {app_id} — {state}"));
    }
    lines
}

fn print_cast_status(value: &serde_json::Value) {
    for line in cast_status_lines(value) {
        println!("{line}");
    }
}

/// Shared stop flag for CLI or GUI session cancellation.
pub fn new_stop_flag() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
}

async fn discover_receiver() -> anyhow::Result<rotten_core::device::AirPlayDevice> {
    for attempt in 1..=6u32 {
        println!("auto: searching for the projector... (attempt {attempt})");
        let devices = discover_for(Duration::from_secs(5)).await?;
        if let Some(device) = devices
            .iter()
            .find(|d| {
                d.model.as_deref() == Some("LSP7")
                    || d.name.to_lowercase().contains("samsung")
                    || d.name.to_lowercase().contains("freestyle")
            })
            .or_else(|| devices.first())
        {
            return Ok(device.clone());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    anyhow::bail!("no AirPlay receiver found on the network")
}

// Request cooperative shutdown, then await the command so audio is restored
// before main exits. Discovery/retry/setup waits also observe the same flag.
async fn finish_on_shutdown(
    command: impl std::future::Future<Output = anyhow::Result<()>>,
    stop: Arc<AtomicBool>,
    shutdown: impl std::future::Future<Output = std::io::Result<()>>,
) -> anyhow::Result<()> {
    tokio::pin!(command);
    tokio::select! {
        result = &mut command => result,
        signal = shutdown => {
            stop.store(true, Ordering::Relaxed);
            let result = command.await;
            signal?;
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_waits_for_command_cleanup() {
        let stop = new_stop_flag();
        let command_stop = stop.clone();
        let cleaned = Arc::new(AtomicBool::new(false));
        let command_cleaned = cleaned.clone();
        finish_on_shutdown(
            async move {
                until_stopped(std::future::pending::<anyhow::Result<()>>(), command_stop).await?;
                tokio::task::yield_now().await;
                command_cleaned.store(true, Ordering::Relaxed);
                Ok(())
            },
            stop,
            async { Ok(()) },
        )
        .await
        .unwrap();
        assert!(cleaned.load(Ordering::Relaxed));
    }

    #[test]
    fn cast_probe_defaults_to_port_8009() {
        let cli =
            Cli::try_parse_from(["cermin-cli", "cast-probe", "--target", "192.168.1.50"]).unwrap();
        match cli.command {
            Some(Commands::CastProbe { target, port }) => {
                assert_eq!(target, "192.168.1.50");
                assert_eq!(port, 8009);
            }
            _ => panic!("expected cast-probe command"),
        }
    }

    #[test]
    fn capture_probe_defaults_to_the_primary_display() {
        let cli = Cli::try_parse_from(["cermin-cli", "capture-probe"]).unwrap();
        match cli.command {
            Some(Commands::CaptureProbe { display }) => assert_eq!(display, 0),
            _ => panic!("expected capture-probe command"),
        }

        let cli = Cli::try_parse_from(["cermin-cli", "capture-probe", "--display", "2"]).unwrap();
        match cli.command {
            Some(Commands::CaptureProbe { display }) => assert_eq!(display, 2),
            _ => panic!("expected capture-probe command"),
        }
    }

    #[test]
    fn cast_stream_parses_ports_and_options() {
        let cli = Cli::try_parse_from([
            "cermin-cli",
            "cast",
            "--target",
            "192.168.1.50",
            "--test",
            "--http-port",
            "9123",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Cast {
                target,
                port,
                display,
                test,
                no_audio,
                http_port,
                duration,
                quality,
                latency,
            }) => {
                assert_eq!(target.as_deref(), Some("192.168.1.50"));
                assert_eq!(port, 8009);
                assert_eq!(http_port, 9123);
                assert_eq!(display, None);
                assert!(test);
                assert!(!no_audio, "system audio defaults to the platform setting");
                assert_eq!(duration, None);
                assert_eq!(quality, CastQuality::Balanced);
                assert_eq!(latency, CastLatency::Stable);
            }
            _ => panic!("expected cast command"),
        }
    }

    /// Both Cast commands accept all four preset combinations and reject
    /// unknown values instead of silently falling back.
    #[test]
    fn cast_presets_parse_every_combination_and_reject_unknown_values() {
        for (quality, latency) in [
            (CastQuality::Balanced, CastLatency::Stable),
            (CastQuality::Balanced, CastLatency::Responsive),
            (CastQuality::High, CastLatency::Stable),
            (CastQuality::High, CastLatency::Responsive),
        ] {
            let cli = Cli::try_parse_from([
                "cermin-cli",
                "cast",
                "--target",
                "192.168.1.50",
                "--quality",
                quality.name(),
                "--latency",
                latency.name(),
            ])
            .unwrap();
            match cli.command {
                Some(Commands::Cast {
                    quality: parsed_quality,
                    latency: parsed_latency,
                    ..
                }) => {
                    assert_eq!(parsed_quality, quality);
                    assert_eq!(parsed_latency, latency);
                }
                _ => panic!("expected cast command"),
            }

            let cli = Cli::try_parse_from([
                "cermin-cli",
                "cast-benchmark",
                "--quality",
                quality.name(),
                "--latency",
                latency.name(),
            ])
            .unwrap();
            match cli.command {
                Some(Commands::CastBenchmark {
                    quality: parsed_quality,
                    latency: parsed_latency,
                    ..
                }) => {
                    assert_eq!(parsed_quality, quality);
                    assert_eq!(parsed_latency, latency);
                }
                _ => panic!("expected cast-benchmark command"),
            }
        }

        for args in [
            ["cermin-cli", "cast", "--quality", "ultra"],
            ["cermin-cli", "cast", "--latency", "turbo"],
            ["cermin-cli", "cast-benchmark", "--quality", "ultra"],
            ["cermin-cli", "cast-benchmark", "--latency", "turbo"],
        ] {
            assert!(
                Cli::try_parse_from(args).is_err(),
                "{args:?} must be rejected"
            );
        }
    }

    /// The preset CLI flags default to Balanced/Stable and the user-facing
    /// labels carry the documented targets.
    #[test]
    fn cast_preset_labels_match_the_documented_targets() {
        let cli = Cli::try_parse_from(["cermin-cli", "cast"]).unwrap();
        match cli.command {
            Some(Commands::Cast {
                quality, latency, ..
            }) => {
                assert_eq!(quality, CastQuality::Balanced);
                assert_eq!(latency, CastLatency::Stable);
                assert_eq!(quality.bitrate_kbps(), 4000);
                assert_eq!(latency.initial_buffer_secs(), 8);
                assert!(quality.label().contains("720p"));
                assert!(quality.label().contains("4 Mbps"));
            }
            _ => panic!("expected cast command"),
        }
        assert!(CastQuality::High.label().contains("1080p"));
        assert!(CastQuality::High.label().contains("8 Mbps"));
        assert!(CastLatency::Responsive.label().contains("experimental"));
    }

    #[test]
    fn cast_system_audio_defaults_to_platform_support_and_no_audio_disables_it() {
        assert_eq!(cast_audio_enabled(false), cfg!(target_os = "windows"));
        assert!(!cast_audio_enabled(true));

        let cli = Cli::try_parse_from([
            "cermin-cli",
            "cast",
            "--target",
            "192.168.1.50",
            "--no-audio",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Cast { no_audio, .. }) => assert!(no_audio),
            _ => panic!("expected cast command"),
        }
    }

    #[test]
    fn cast_duration_requires_at_least_one_second() {
        assert!(
            Cli::try_parse_from([
                "cermin-cli",
                "cast",
                "--target",
                "192.168.1.50",
                "--duration",
                "0",
            ])
            .is_err(),
            "zero duration must be rejected"
        );

        let cli = Cli::try_parse_from([
            "cermin-cli",
            "cast",
            "--target",
            "192.168.1.50",
            "--duration",
            "7",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Cast {
                duration: Some(7), ..
            }) => {}
            _ => panic!("expected cast command with duration 7"),
        }
    }

    #[test]
    fn cast_benchmark_defaults_to_primary_display_and_ten_seconds() {
        let cli = Cli::try_parse_from(["cermin-cli", "cast-benchmark"]).unwrap();
        match cli.command {
            Some(Commands::CastBenchmark {
                display,
                duration,
                test,
                quality,
                latency,
            }) => {
                assert_eq!(display, None);
                assert_eq!(duration, 10);
                assert!(!test);
                assert_eq!(quality, CastQuality::Balanced);
                assert_eq!(latency, CastLatency::Stable);
            }
            _ => panic!("expected cast-benchmark command"),
        }
    }

    #[test]
    fn cast_benchmark_requires_one_to_sixty_seconds() {
        for duration in ["0", "61"] {
            assert!(
                Cli::try_parse_from(["cermin-cli", "cast-benchmark", "--duration", duration])
                    .is_err(),
                "duration {duration} must be rejected"
            );
        }

        let cli = Cli::try_parse_from([
            "cermin-cli",
            "cast-benchmark",
            "--display",
            "2",
            "--duration",
            "60",
            "--test",
            "--quality",
            "high",
            "--latency",
            "responsive",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::CastBenchmark {
                display,
                duration,
                test,
                quality,
                latency,
            }) => {
                assert_eq!(display, Some(2));
                assert_eq!(duration, 60);
                assert!(test);
                assert_eq!(quality, CastQuality::High);
                assert_eq!(latency, CastLatency::Responsive);
            }
            _ => panic!("expected cast-benchmark command"),
        }
    }

    #[tokio::test]
    async fn duration_deadline_sets_stop_and_awaits_session_cleanup() {
        let stop = Arc::new(AtomicBool::new(false));
        let cleaned_up = Arc::new(AtomicBool::new(false));
        let stop_in_session = stop.clone();
        let cleanup_flag = cleaned_up.clone();
        let result = run_cast_with_optional_deadline(
            async move {
                // Fake session that only finishes after observing the
                // cooperative stop flag, like the real Cast backend.
                while !stop_in_session.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                cleanup_flag.store(true, Ordering::Relaxed);
                Ok::<_, anyhow::Error>("stopped")
            },
            stop.clone(),
            Some(Duration::from_millis(25)),
        )
        .await
        .unwrap();
        assert_eq!(result, "stopped");
        assert!(stop.load(Ordering::Relaxed));
        assert!(
            cleaned_up.load(Ordering::Relaxed),
            "the helper must await cleanup, not return at the deadline"
        );
    }

    #[tokio::test]
    async fn session_completion_before_the_deadline_does_not_set_stop() {
        let stop = Arc::new(AtomicBool::new(false));
        let result = run_cast_with_optional_deadline(
            async { Ok::<_, anyhow::Error>("done") },
            stop.clone(),
            Some(Duration::from_secs(60)),
        )
        .await
        .unwrap();
        assert_eq!(result, "done");
        assert!(!stop.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn no_duration_awaits_the_session_indefinitely() {
        let stop = Arc::new(AtomicBool::new(false));
        let result = run_cast_with_optional_deadline(
            async { Ok::<_, anyhow::Error>("natural") },
            stop.clone(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result, "natural");
        assert!(!stop.load(Ordering::Relaxed));
    }

    #[test]
    fn discover_protocol_defaults_to_all() {
        let cli = Cli::try_parse_from(["cermin-cli", "discover"]).unwrap();
        match cli.command {
            Some(Commands::Discover { timeout, protocol }) => {
                assert_eq!(timeout, 5);
                assert!(matches!(protocol, ProtocolArg::All));
            }
            _ => panic!("expected discover command"),
        }

        let cli = Cli::try_parse_from(["cermin-cli", "discover", "--protocol", "cast"]).unwrap();
        match cli.command {
            Some(Commands::Discover { protocol, .. }) => {
                assert!(matches!(protocol, ProtocolArg::Cast));
            }
            _ => panic!("expected discover command"),
        }
    }

    #[test]
    fn cast_status_lines_never_echo_media_urls() {
        let value = serde_json::json!({
            "type": "RECEIVER_STATUS",
            "status": {
                "applications": [{
                    "displayName": "Netflix",
                    "appId": "CA5E8412",
                    "statusText": "Playing http://192.168.1.50:8000/stream.m3u8",
                    "isIdleScreen": false
                }]
            }
        });
        let lines = cast_status_lines(&value);
        let joined = lines.join("\n");
        assert!(joined.contains("Netflix"));
        assert!(joined.contains("CA5E8412"));
        assert!(joined.contains("running"));
        assert!(!joined.contains("http"));
        assert!(!joined.contains("m3u8"));
        assert!(!joined.contains("statusText"));
    }

    #[test]
    fn cast_status_lines_handle_idle_and_sanitize_receiver_text() {
        let idle = serde_json::json!({ "status": { "applications": [] } });
        assert_eq!(
            cast_status_lines(&idle),
            vec!["No applications running on the receiver (idle).".to_string()]
        );

        let control = serde_json::json!({
            "applications": [{
                "displayName": "Bad\nName\u{7}",
                "appId": "A\tB",
                "isIdleScreen": true
            }]
        });
        let lines = cast_status_lines(&control);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("BadName"));
        assert!(lines[1].contains("app AB"));
        assert!(lines[1].contains("idle screen"));
        assert!(!lines[1].contains('\n'));
    }

    #[test]
    fn airplay_mirror_defaults_are_unchanged() {
        let cli =
            Cli::try_parse_from(["cermin-cli", "mirror", "--target", "192.168.1.10"]).unwrap();
        match cli.command {
            Some(Commands::Mirror {
                target,
                port,
                width,
                height,
                fps,
                audio,
                test,
                ..
            }) => {
                assert_eq!(target.as_deref(), Some("192.168.1.10"));
                assert_eq!(port, 7000);
                assert_eq!(width, 0);
                assert_eq!(height, 0);
                assert_eq!(fps, 30);
                assert!(!audio);
                assert!(!test);
            }
            _ => panic!("expected mirror command"),
        }
    }
}
