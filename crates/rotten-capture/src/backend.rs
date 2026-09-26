#[cfg(target_os = "linux")]
pub use crate::linux::create_linux_backend;

use async_trait::async_trait;
use rotten_core::error::{Result, RottenError};

/// One-shot message about an automatic capture decision, e.g. the GDI fallback.
///
/// A session UI can call [`take_capture_notice`] once after capture starts to
/// show the user why a slower backend is active.
static CAPTURE_NOTICE: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
    std::sync::OnceLock::new();

fn set_capture_notice(notice: String) {
    let slot = CAPTURE_NOTICE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some(notice);
}

/// Take the pending capture notice, if any. Consumed once per process.
pub fn take_capture_notice() -> Option<String> {
    let slot = CAPTURE_NOTICE.get()?;
    let mut guard = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.take()
}

/// Information about an available display.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub index: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub is_virtual: bool,
    /// Graphics adapter that drives the display, when the platform reports it.
    pub adapter: Option<String>,
}

impl DisplayInfo {
    /// Human-readable label for pickers and listings, e.g.
    /// `#1 \\.\DISPLAY2 — 2560x1440 (virtual)`.
    pub fn label(&self) -> String {
        let suffix = if self.is_virtual { " (virtual)" } else { "" };
        format!(
            "#{} {} — {}x{}{}",
            self.index, self.name, self.width, self.height, suffix
        )
    }
}

/// A captured frame in RGBA format.
#[derive(Debug, Clone)]
pub struct CaptureFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Platform-agnostic screen capture interface.
#[async_trait]
pub trait CaptureBackend: Send {
    fn displays(&self) -> Result<Vec<DisplayInfo>>;
    /// Blocking frame grab (safe to call from `spawn_blocking`).
    fn grab_frame(&mut self) -> Result<CaptureFrame>;
    async fn capture_frame(&mut self) -> Result<CaptureFrame> {
        self.grab_frame()
    }
    fn backend_name(&self) -> &'static str;
}

/// Create the best available capture backend for the current platform.
pub fn create_capture_backend(
    display_index: Option<u32>,
    virtual_only: bool,
) -> Result<Box<dyn CaptureBackend>> {
    #[cfg(target_os = "windows")]
    ensure_process_dpi_aware();

    let resolved_index = if virtual_only {
        let displays = list_displays()?;
        crate::virtual_display::select_virtual_display(&displays, display_index).ok_or_else(|| {
            RottenError::Capture(
                "no virtual displays found — install a virtual display driver and extend the desktop"
                    .into(),
            )
        })?
    } else {
        display_index.unwrap_or(0)
    };

    #[cfg(target_os = "linux")]
    {
        return create_linux_backend(Some(resolved_index), virtual_only);
    }
    #[cfg(target_os = "windows")]
    {
        return create_windows_backend(resolved_index, virtual_only);
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (resolved_index, virtual_only);
        Err(RottenError::Capture("unsupported platform".into()))
    }
}

/// Make the process per-monitor DPI aware so capture and enumeration report
/// physical pixels. The GUI already sets this, so a failure there is expected
/// and ignored; a console build becomes aware here.
#[cfg(target_os = "windows")]
fn ensure_process_dpi_aware() {
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };

    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// Explicit backend choice from `CERMIN_CAPTURE_BACKEND`.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendPreference {
    Auto,
    Dxgi,
    Gdi,
}

#[cfg(target_os = "windows")]
fn parse_backend_preference(value: &str) -> Option<BackendPreference> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(BackendPreference::Auto),
        "dxgi" => Some(BackendPreference::Dxgi),
        "gdi" => Some(BackendPreference::Gdi),
        _ => None,
    }
}

#[cfg(target_os = "windows")]
fn backend_preference() -> Result<BackendPreference> {
    match std::env::var("CERMIN_CAPTURE_BACKEND") {
        Ok(value) => parse_backend_preference(&value).ok_or_else(|| {
            RottenError::Capture(format!(
                "CERMIN_CAPTURE_BACKEND={value:?} is not one of auto, dxgi, gdi"
            ))
        }),
        Err(_) => Ok(BackendPreference::Auto),
    }
}

#[cfg(target_os = "windows")]
fn create_windows_backend(
    resolved_index: u32,
    virtual_only: bool,
) -> Result<Box<dyn CaptureBackend>> {
    match backend_preference()? {
        BackendPreference::Gdi => {
            crate::gdi::create_windows_gdi_backend(Some(resolved_index), virtual_only)
        }
        BackendPreference::Dxgi => {
            crate::dxgi::create_windows_backend(Some(resolved_index), virtual_only)
        }
        BackendPreference::Auto => {
            match crate::dxgi::create_windows_backend(Some(resolved_index), virtual_only) {
                Ok(backend) => Ok(backend),
                Err(error) if crate::dxgi::is_duplication_unsupported(&error) => {
                    fall_back_to_gdi(resolved_index, virtual_only, error)
                }
                Err(error) => Err(error),
            }
        }
    }
}

/// Desktop Duplication is unavailable (hybrid GPU, Remote Desktop, ...): ask
/// for the integrated GPU next launch and use the GDI backend for this session.
#[cfg(target_os = "windows")]
fn fall_back_to_gdi(
    resolved_index: u32,
    virtual_only: bool,
    dxgi_error: RottenError,
) -> Result<Box<dyn CaptureBackend>> {
    let preference_written = match crate::hybrid::ensure_integrated_gpu_preference() {
        Ok(true) => {
            tracing::info!(
                target: "cermin",
                "requested the integrated GPU for future launches; restart Cermin to use the faster DXGI capture"
            );
            true
        }
        Ok(false) => false,
        Err(error) => {
            tracing::warn!(
                target: "cermin",
                %error,
                "could not set the integrated-GPU preference"
            );
            false
        }
    };
    tracing::warn!(
        target: "cermin",
        %dxgi_error,
        "DXGI desktop duplication is unavailable; using the slower GDI capture fallback"
    );
    match crate::gdi::create_windows_gdi_backend(Some(resolved_index), virtual_only) {
        Ok(backend) => {
            tracing::info!(
                target: "cermin",
                backend = backend.backend_name(),
                "GDI capture fallback active"
            );
            set_capture_notice(if preference_written {
                "DXGI screen capture is unavailable (hybrid GPU); using the slower GDI fallback. \
                 Cermin asked Windows to use the integrated GPU — restart Cermin to get the \
                 faster DXGI capture."
                    .to_string()
            } else {
                "DXGI screen capture is unavailable (hybrid GPU or remote session); using the \
                 slower GDI fallback."
                    .to_string()
            });
            Ok(backend)
        }
        Err(gdi_error) => Err(RottenError::Capture(format!(
            "{dxgi_error}; the GDI capture fallback also failed: {gdi_error}"
        ))),
    }
}

/// Enumerate all capture targets on this platform.
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    #[cfg(target_os = "linux")]
    {
        return crate::linux::list_displays();
    }
    #[cfg(target_os = "windows")]
    {
        ensure_process_dpi_aware();
        return crate::dxgi::list_displays();
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(RottenError::Capture("unsupported platform".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_label_includes_index_resolution_and_virtual_marker() {
        let physical = DisplayInfo {
            index: 0,
            name: r"\\.\DISPLAY1".into(),
            width: 1920,
            height: 1080,
            is_virtual: false,
            adapter: Some("Intel(R) UHD Graphics".into()),
        };
        assert_eq!(physical.label(), r"#0 \\.\DISPLAY1 — 1920x1080");

        let virtual_display = DisplayInfo {
            index: 2,
            name: "IDD HDR Virtual Display".into(),
            width: 2560,
            height: 1440,
            is_virtual: true,
            adapter: None,
        };
        assert_eq!(
            virtual_display.label(),
            "#2 IDD HDR Virtual Display — 2560x1440 (virtual)"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn backend_preference_parsing_accepts_only_known_names() {
        assert_eq!(
            parse_backend_preference("auto"),
            Some(BackendPreference::Auto)
        );
        assert_eq!(
            parse_backend_preference(" DXGI "),
            Some(BackendPreference::Dxgi)
        );
        assert_eq!(
            parse_backend_preference("Gdi"),
            Some(BackendPreference::Gdi)
        );
        assert_eq!(parse_backend_preference("vulkan"), None);
        assert_eq!(parse_backend_preference(""), None);
    }
}
