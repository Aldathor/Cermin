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
    thread: Option<std::thread::JoinHandle<std::result::Result<(), String>>>,
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
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("wasapi-loopback".into())
            .spawn(move || {
                let mut ready_tx = Some(ready_tx);
                let result = capture::run_loopback(tx, stop_thread, mute_thread, || {
                    if let Some(ready_tx) = ready_tx.take() {
                        let _ = ready_tx.send(Ok(()));
                    }
                });
                if let Err(e) = &result {
                    if let Some(ready_tx) = ready_tx.take() {
                        let _ = ready_tx.send(Err(e.clone()));
                    }
                    warn!("WASAPI loopback capture ended: {e}");
                }
                result
            })
            .map_err(|e| rotten_core::error::RottenError::Protocol(e.to_string()))?;
        // Construct the owner before awaiting readiness so cancellation also
        // signals the worker to stop and restore the endpoint's mute state.
        let mirror = Self {
            device_name: device.name.clone(),
            stop,
            mute_request,
            thread: Some(thread),
        };
        ready_rx
            .await
            .map_err(|_| {
                rotten_core::error::RottenError::Capture(
                    "audio capture worker exited during startup".into(),
                )
            })?
            .map_err(rotten_core::error::RottenError::Capture)?;
        info!(device = %device.name, "system audio loopback capture started");
        Ok((mirror, rx))
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
    pub async fn start(
        device: &AirPlayDevice,
    ) -> Result<(Self, tokio::sync::mpsc::Receiver<Vec<u8>>)> {
        warn!(
            device = %device.name,
            "audio mirroring capture is only implemented on Windows; sending silence"
        );
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        Ok((
            Self {
                device_name: device.name.clone(),
            },
            rx,
        ))
    }

    #[cfg(target_os = "windows")]
    pub async fn stop(mut self) -> Result<()> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.mute_request
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            tokio::task::spawn_blocking(move || thread.join())
                .await
                .map_err(|e| {
                    rotten_core::error::RottenError::Capture(format!(
                        "could not join audio capture worker: {e}"
                    ))
                })?
                .map_err(|_| {
                    rotten_core::error::RottenError::Capture("audio capture worker panicked".into())
                })?
                .map_err(rotten_core::error::RottenError::Capture)?;
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

#[cfg(target_os = "windows")]
impl Drop for AudioMirror {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.mute_request
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn mock_mirror(
        thread: std::thread::JoinHandle<std::result::Result<(), String>>,
    ) -> AudioMirror {
        AudioMirror {
            device_name: "test".into(),
            stop: Arc::new(AtomicBool::new(false)),
            mute_request: Arc::new(AtomicBool::new(true)),
            thread: Some(thread),
        }
    }

    #[test]
    fn dropping_mirror_requests_stop_and_mute_restore() {
        let mirror = mock_mirror(std::thread::spawn(|| Ok(())));
        let stop = mirror.stop.clone();
        let mute = mirror.mute_request.clone();
        drop(mirror);
        assert!(stop.load(Ordering::Relaxed));
        assert!(!mute.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn stop_reports_worker_errors() {
        let mirror = mock_mirror(std::thread::spawn(|| Err("device disconnected".into())));
        assert!(
            mirror
                .stop()
                .await
                .unwrap_err()
                .to_string()
                .contains("device disconnected")
        );
    }

    #[tokio::test]
    async fn stop_reports_worker_panics() {
        let mirror = mock_mirror(std::thread::spawn(|| panic!("capture worker panic")));
        assert!(
            mirror
                .stop()
                .await
                .unwrap_err()
                .to_string()
                .contains("panicked")
        );
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
