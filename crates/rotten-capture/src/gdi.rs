//! GDI `BitBlt` screen capture fallback.
//!
//! Used when DXGI Desktop Duplication is unavailable: the documented Microsoft
//! Hybrid limitation (duplication is not supported against the discrete GPU),
//! Remote Desktop sessions, and some virtual machines. DXGI is still used to
//! locate the selected monitor, but the pixels come from a plain `BitBlt` of the
//! composited desktop, which works regardless of which GPU the process uses.
//!
//! The trade-off is a CPU copy of the whole monitor per frame, so this backend
//! is slower than Desktop Duplication. It exists to make capture work at all on
//! systems where duplication is refused.

use std::ffi::c_void;
use std::mem::size_of;

use async_trait::async_trait;
use rotten_core::debug_log::agent_log;
use rotten_core::error::{Result, RottenError};
use tracing::info;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HDC, HGDIOBJ, RGBQUAD, ROP_CODE,
    ReleaseDC, SRCCOPY, SelectObject,
};

use crate::backend::{CaptureBackend, CaptureFrame, DisplayInfo};
use crate::dxgi::{self, OutputTarget};

/// Hard ceiling for one DIB frame, checked before any FFI call or raw slice.
/// 256 MiB comfortably covers 8K (7680x4320x4 ≈ 127 MiB) while rejecting
/// absurd or overflowing geometry.
const MAX_DIB_BYTES: usize = 256 * 1024 * 1024;

/// Validates a DIB geometry and returns its pixel byte length.
///
/// `BITMAPINFOHEADER` dimensions are signed 32-bit and both the `BitBlt`
/// arguments and the pixel slice length derive from these values, so reject
/// unrepresentable or oversized sizes before touching GDI or raw memory.
fn checked_frame_len(width: u32, height: u32) -> Result<usize> {
    if width == 0 || height == 0 {
        return Err(RottenError::Capture(format!(
            "invalid capture size {width}x{height}"
        )));
    }
    if width > i32::MAX as u32 || height > i32::MAX as u32 {
        return Err(RottenError::Capture(format!(
            "capture size {width}x{height} does not fit the 32-bit GDI dimensions"
        )));
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|&len| len <= MAX_DIB_BYTES)
        .ok_or_else(|| {
            RottenError::Capture(format!(
                "capture size {width}x{height} exceeds the {MAX_DIB_BYTES}-byte GDI fallback limit"
            ))
        })
}

pub fn create_windows_gdi_backend(
    display_index: Option<u32>,
    _virtual_only: bool,
) -> Result<Box<dyn CaptureBackend>> {
    Ok(Box::new(GdiCapture::open(display_index.unwrap_or(0))?))
}

struct GdiCapture {
    target: OutputTarget,
    dib: Dib,
}

// SAFETY: the raw DIB pointer inside `Dib` is only read while building a frame
// on the thread that owns this backend. The capture pipeline moves the backend
// between threads but never uses it concurrently.
unsafe impl Send for GdiCapture {}

/// Offscreen 32-bit top-down DIB section selected into a private memory DC.
///
/// Owns every GDI handle of one capture surface and restores/deletes them on
/// drop, so the create and grab failure paths cannot leak a bitmap or DC.
struct Dib {
    dc: HDC,
    bitmap: HGDIOBJ,
    previous: HGDIOBJ,
    bits: *mut c_void,
    len: usize,
}

impl Dib {
    fn create(width: u32, height: u32) -> Result<Self> {
        let len = checked_frame_len(width, height)?;
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return Err(RottenError::Capture(
                    "GetDC returned no desktop device context".into(),
                ));
            }
            let dc = CreateCompatibleDC(screen);
            if dc.is_invalid() {
                ReleaseDC(None, screen);
                return Err(RottenError::Capture(
                    "CreateCompatibleDC failed for the GDI fallback".into(),
                ));
            }

            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    // Negative height requests a top-down DIB, matching the
                    // top-down RGBA the rest of the pipeline expects.
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                bmiColors: [RGBQUAD::default()],
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(bitmap) => HGDIOBJ(bitmap.0),
                Err(error) => {
                    let _ = DeleteDC(dc);
                    ReleaseDC(None, screen);
                    return Err(RottenError::Capture(format!(
                        "CreateDIBSection failed for the GDI fallback: {error}"
                    )));
                }
            };
            if bits.is_null() {
                let _ = DeleteObject(bitmap);
                let _ = DeleteDC(dc);
                ReleaseDC(None, screen);
                return Err(RottenError::Capture(
                    "CreateDIBSection returned no pixel memory".into(),
                ));
            }
            let previous = SelectObject(dc, bitmap);
            if previous.is_invalid() {
                let _ = DeleteObject(bitmap);
                let _ = DeleteDC(dc);
                ReleaseDC(None, screen);
                return Err(RottenError::Capture(
                    "SelectObject failed to select the GDI fallback bitmap".into(),
                ));
            }
            ReleaseDC(None, screen);

            Ok(Self {
                dc,
                bitmap,
                previous,
                bits,
                len,
            })
        }
    }

    fn dc(&self) -> HDC {
        self.dc
    }

    fn byte_len(&self) -> usize {
        self.len
    }

    /// Flushes batched GDI drawing and returns the DIB pixel memory.
    ///
    /// `CreateDIBSection` batches GDI output, so `BitBlt` may still be queued
    /// when it returns; `GdiFlush` on this (the owning) thread is required
    /// before the CPU reads the pixels.
    fn flush_and_read(&self) -> Result<&[u8]> {
        // SAFETY: GdiFlush only flushes this thread's GDI batch and must run
        // before the pixel memory is read.
        if !unsafe { GdiFlush() }.as_bool() {
            return Err(RottenError::Capture(
                "GdiFlush failed; GDI fallback pixels may be stale".into(),
            ));
        }
        // SAFETY: `bits` is the non-null base of a DIB section of `len` bytes
        // validated in `create`, and callers only read it on the owning thread.
        Ok(unsafe { std::slice::from_raw_parts(self.bits as *const u8, self.len) })
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap);
            let _ = DeleteDC(self.dc);
        }
    }
}

impl GdiCapture {
    fn open(display_index: u32) -> Result<Self> {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            );
            let factory: IDXGIFactory1 = CreateDXGIFactory1()
                .map_err(|e| RottenError::Capture(format!("DXGI factory: {e}")))?;
            let target = dxgi::find_output_target(&factory, display_index)?;
            if target.width == 0 || target.height == 0 {
                return Err(RottenError::Capture(format!(
                    "display {} has no capturable area",
                    target.device_name
                )));
            }
            let dib = Dib::create(target.width, target.height)?;

            info!(
                display_index,
                width = target.width,
                height = target.height,
                name = %target.device_name,
                adapter = %target.adapter_name,
                "GDI capture initialized"
            );

            Ok(Self { target, dib })
        }
    }
}

#[async_trait]
impl CaptureBackend for GdiCapture {
    fn displays(&self) -> Result<Vec<DisplayInfo>> {
        Ok(vec![DisplayInfo {
            index: self.target.index,
            name: self.target.device_name.clone(),
            width: self.target.width,
            height: self.target.height,
            is_virtual: self.target.is_virtual,
            adapter: Some(self.target.adapter_name.clone()),
        }])
    }

    fn grab_frame(&mut self) -> Result<CaptureFrame> {
        let width = self.target.width as usize;
        let height = self.target.height as usize;
        // Re-validate the fixed geometry before the FFI call and the slice
        // read; `Dib::create` already accepted it, so this is an invariant.
        let expected_len = checked_frame_len(self.target.width, self.target.height)?;
        debug_assert_eq!(expected_len, self.dib.byte_len());
        let rop = ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0);
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return Err(RottenError::Capture(
                    "GetDC returned no desktop device context".into(),
                ));
            }
            let result = BitBlt(
                self.dib.dc(),
                0,
                0,
                self.target.width as i32,
                self.target.height as i32,
                screen,
                self.target.left,
                self.target.top,
                rop,
            );
            ReleaseDC(None, screen);
            result.map_err(|e| {
                RottenError::Capture(format!(
                    "BitBlt failed for {} on adapter {}: {e}",
                    self.target.device_name, self.target.adapter_name
                ))
            })?;

            // The DIB contract requires GdiFlush before the CPU reads pixels
            // that a batched GDI call such as BitBlt just wrote.
            let source = self.dib.flush_and_read()?;
            let rgba = dxgi::bgra_to_rgba(source, width * 4, width, height);

            static ACQUIRED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = ACQUIRED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n == 1 {
                agent_log(
                    "gdi.rs:grab_frame",
                    "first GDI frame acquired",
                    "H1",
                    serde_json::json!({ "width": width, "height": height }),
                );
            }

            Ok(CaptureFrame {
                rgba,
                width: self.target.width,
                height: self.target.height,
            })
        }
    }

    fn backend_name(&self) -> &'static str {
        "gdi"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_WIDTH: u32 = 5;
    const TEST_HEIGHT: u32 = 4;

    /// A top-down BGRA pattern with distinct channel values in every pixel, so
    /// a stale read, a wrong channel order or a bottom-up DIB all mismatch.
    fn test_pattern() -> Vec<u8> {
        let mut pattern = Vec::with_capacity((TEST_WIDTH * TEST_HEIGHT * 4) as usize);
        for y in 0..TEST_HEIGHT as usize {
            for x in 0..TEST_WIDTH as usize {
                pattern.extend_from_slice(&[
                    (10 + x * 30) as u8,
                    (20 + y * 50) as u8,
                    (30 + (x + y) * 10) as u8,
                    255,
                ]);
            }
        }
        pattern
    }

    #[test]
    fn checked_frame_len_enforces_gdi_dimension_and_byte_limits() {
        assert_eq!(
            checked_frame_len(64, 48).expect("valid geometry"),
            64 * 48 * 4
        );
        assert!(checked_frame_len(0, 48).is_err());
        assert!(checked_frame_len(64, 0).is_err());
        let unrepresentable = i32::MAX as u32 + 1;
        assert!(checked_frame_len(unrepresentable, 1).is_err());
        assert!(checked_frame_len(1, unrepresentable).is_err());
        // 8193 * 8192 * 4 is just above the 256 MiB cap.
        assert!(checked_frame_len(8193, 8192).is_err());
    }

    /// CPU/GDI-memory-only regression for the `CreateDIBSection` contract:
    /// `BitBlt` between two offscreen DIB sections, then `GdiFlush` before the
    /// pixel bytes are read back. The desktop DC is never a source, so this
    /// test neither depends on nor captures real screen contents.
    #[test]
    fn flushed_offscreen_bitblt_reads_fresh_top_down_pixels() {
        let source = Dib::create(TEST_WIDTH, TEST_HEIGHT).expect("source DIB section");
        let target = Dib::create(TEST_WIDTH, TEST_HEIGHT).expect("target DIB section");
        let pattern = test_pattern();

        // Direct writes through the pointer `CreateDIBSection` returned are the
        // documented way to seed a DIB; CPU writes need no GdiFlush.
        unsafe {
            std::slice::from_raw_parts_mut(source.bits as *mut u8, source.byte_len())
                .copy_from_slice(&pattern);
        }

        unsafe {
            BitBlt(
                target.dc(),
                0,
                0,
                TEST_WIDTH as i32,
                TEST_HEIGHT as i32,
                source.dc(),
                0,
                0,
                SRCCOPY,
            )
            .expect("BitBlt between offscreen DIB sections");
        }

        let pixels = target.flush_and_read().expect("flushed destination pixels");
        assert_eq!(
            pixels.len(),
            pattern.len(),
            "the flushed DIB must expose the whole frame"
        );
        for (index, (actual, expected)) in pixels
            .as_chunks::<4>()
            .0
            .iter()
            .zip(pattern.as_chunks::<4>().0)
            .enumerate()
        {
            // Compare RGB; the DIB alpha byte is GDI's business, not the RGB
            // contract this fallback depends on.
            assert_eq!(
                &actual[..3],
                &expected[..3],
                "pixel {index} must be the freshly BitBlt-ed top-down value"
            );
        }

        // The pipeline conversion must turn top-down BGRA into RGBA with opaque
        // alpha, proving the negative-height DIB orientation is preserved.
        let rgba = crate::dxgi::bgra_to_rgba(
            pixels,
            (TEST_WIDTH * 4) as usize,
            TEST_WIDTH as usize,
            TEST_HEIGHT as usize,
        );
        for (index, (pixel, bgra)) in rgba
            .as_chunks::<4>()
            .0
            .iter()
            .zip(pattern.as_chunks::<4>().0)
            .enumerate()
        {
            let expected = [bgra[2], bgra[1], bgra[0], 255];
            assert_eq!(
                pixel, &expected,
                "pixel {index} must convert to opaque RGBA in top-down order"
            );
        }
    }
}
