use rotten_core::device::AirPlayDevice;
use rotten_core::error::Result;
use tracing::{info, warn};

#[cfg(target_os = "windows")]
mod capture;

/// System audio mirroring via WASAPI loopback (Windows); stub elsewhere.
pub struct AudioMirror {
    device_name: String,
    #[cfg(target_os = "windows")]
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(target_os = "windows")]
    mute_request: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(target_os = "windows")]
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioMirror {
    /// Start loopback capture and return the handle plus a PCM receiver that
    /// yields interleaved 44.1 kHz stereo S16 frames.
    #[cfg(target_os = "windows")]
    pub async fn start(
        device: &AirPlayDevice,
    ) -> Result<(Self, tokio::sync::mpsc::Receiver<Vec<u8>>)> {
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = stop.clone();
        let mute_request = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mute_thread = mute_request.clone();
        let thread = std::thread::Builder::new()
            .name("wasapi-loopback".into())
            .spawn(move || {
                if let Err(e) = capture::run_loopback(tx, stop_thread, mute_thread) {
                    warn!("WASAPI loopback capture ended: {e}");
                }
            })
            .map_err(|e| rotten_core::error::RottenError::Protocol(e.to_string()))?;
        info!(device = %device.name, "system audio loopback capture started");
        Ok((
            Self {
                device_name: device.name.clone(),
                stop,
                mute_request,
                thread: Some(thread),
            },
            rx,
        ))
    }

    /// Mute (`true`) or restore (`false`) the captured render endpoint while
    /// mirroring, so the laptop does not echo the TV audio. The capture thread
    /// restores the original mute state on stop.
    #[cfg(target_os = "windows")]
    pub fn set_local_mute(&self, mute: bool) {
        self.mute_request
            .store(mute, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(not(target_os = "windows"))]
    pub fn set_local_mute(&self, _mute: bool) {}

    #[cfg(not(target_os = "windows"))]
    pub async fn start(device: &AirPlayDevice) -> Result<Self> {
        warn!(
            device = %device.name,
            "audio mirroring capture is only implemented on Windows; sending silence"
        );
        Ok(Self {
            device_name: device.name.clone(),
        })
    }

    #[cfg(target_os = "windows")]
    pub async fn stop(mut self) -> Result<()> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.mute_request
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
        info!(device = %self.device_name, "audio mirror stopped");
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    pub async fn stop(self) -> Result<()> {
        info!(device = %self.device_name, "audio mirror stopped");
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub mod platform_audio {
    /// PulseAudio/PipeWire loopback capture hook (stub).
    pub fn list_audio_sources() -> Vec<String> {
        vec!["default".into()]
    }
}

#[cfg(target_os = "windows")]
pub mod platform_audio {
    /// WASAPI loopback capture hook (stub).
    pub fn list_audio_sources() -> Vec<String> {
        vec!["default".into()]
    }
}
