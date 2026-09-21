#[cfg(target_os = "linux")]
pub use crate::linux::create_linux_backend;

use async_trait::async_trait;
use rotten_core::error::{Result, RottenError};

/// Information about an available display.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub index: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub is_virtual: bool,
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
        return crate::dxgi::create_windows_backend(Some(resolved_index), virtual_only);
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (resolved_index, virtual_only);
        Err(RottenError::Capture("unsupported platform".into()))
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
        };
        assert_eq!(physical.label(), r"#0 \\.\DISPLAY1 — 1920x1080");

        let virtual_display = DisplayInfo {
            index: 2,
            name: "IDD HDR Virtual Display".into(),
            width: 2560,
            height: 1440,
            is_virtual: true,
        };
        assert_eq!(
            virtual_display.label(),
            "#2 IDD HDR Virtual Display — 2560x1440 (virtual)"
        );
    }
}
