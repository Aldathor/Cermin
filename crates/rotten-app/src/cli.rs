use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use rotten_core::config::{MirrorCipherMode, MirrorConfig, StreamConfig, resolve_credentials_path};
use rotten_core::debug_log::{DEBUG_BUILD_ID, agent_log};
use rotten_discovery::{discover_devices, discover_for, resolve_device};
use rotten_pairing::{PairingManager, format_pin};
use tracing::info;

use crate::mirror::run_mirror;

#[derive(Parser)]
#[command(name = "cermin-cli")]
#[command(about = "Mirror or extend your PC display to Apple TV via AirPlay")]
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
    /// Scan the LAN for AirPlay receivers (Apple TVs)
    Discover {
        /// Discovery timeout in seconds
        #[arg(long, default_value = "5")]
        timeout: u64,
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
        /// Stream width
        #[arg(long, default_value = "1920")]
        width: u32,
        /// Stream height
        #[arg(long, default_value = "1080")]
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
        /// Display index to capture
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
        let command = match self.command {
            Some(command) => command,
            None => return run_auto().await,
        };
        match command {
            Commands::Probe => {}
            Commands::Discover { timeout } => {
                let devices = discover_for(Duration::from_secs(timeout)).await?;
                if devices.is_empty() {
                    println!("No AirPlay devices found.");
                } else {
                    println!("Found {} AirPlay device(s):\n", devices.len());
                    for d in &devices {
                        println!("  {} — {}:{} ({})", d.name, d.host, d.port, d.device_id);
                        if let Some(model) = &d.model {
                            println!("    model: {model}");
                        }
                    }
                }
            }
            Commands::Pair {
                target,
                pin,
                port,
                force,
                creds,
            } => {
                let device = resolve_device(&target, port).await?;
                let pin = pin.map(|p| format_pin(&p)).transpose()?;
                let creds_path = resolve_credentials_path(creds);
                let mut manager = PairingManager::load(creds_path)?;
                let stored = manager.pair(&device, pin.as_deref(), force).await?;
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
                    let device = resolve_device(&t, port).await?;
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
                    let devices = discover_devices().await?;
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

                run_mirror(device, config, new_stop_flag(), None).await?;
            }
        }
        Ok(())
    }
}

/// Auto mode (no subcommand — double-click friendly): find the receiver on the
/// network, mirror with system audio and retry forever on disconnects.
async fn run_auto() -> anyhow::Result<()> {
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt().with_env_filter("info").finish(),
    )
    .ok();

    println!("Cermin — AirPlay screen mirroring with system audio");
    println!("=========================================================");
    println!("Keep this window open while mirroring; press Ctrl+C to stop.");
    println!("On first run you will be asked for the code shown on the TV.\n");

    loop {
        match run_auto_once().await {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!("\nauto: session ended: {e}");
                eprintln!("auto: rediscovering and retrying in 5 seconds (Ctrl+C to stop)...\n");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn run_auto_once() -> anyhow::Result<()> {
    let device = discover_receiver().await?;

    let credentials_path = resolve_credentials_path(None);
    let has_credentials = rotten_pairing::PairingManager::load(credentials_path.clone())
        .map(|m| m.has_credentials(&device.device_id))
        .unwrap_or(false);
    if has_credentials {
        println!("auto: found '{}' at {}:{} — resuming saved session\n", device.name, device.host, device.port);
    } else {
        println!("auto: found '{}' at {}:{}", device.name, device.host, device.port);
        println!("auto: first-time setup — enter the AirPlay code shown on the TV when prompted\n");
    }

    let config = MirrorConfig {
        stream: StreamConfig {
            width: 1280,
            height: 720,
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

    run_mirror(device, config, new_stop_flag(), None).await?;
    Ok(())
}

/// CLI sessions run until the process exits (Ctrl+C), so the flag is never set.
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
