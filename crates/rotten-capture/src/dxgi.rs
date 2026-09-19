use async_trait::async_trait;
use rotten_core::debug_log::agent_log;
use rotten_core::error::{Result, RottenError};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::info;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter,
    IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication,
};
use windows::core::Interface;

use crate::backend::{CaptureBackend, CaptureFrame, DisplayInfo};
use crate::virtual_display::is_virtual_display_name;

pub fn create_windows_backend(
    display_index: Option<u32>,
    virtual_only: bool,
) -> Result<Box<dyn CaptureBackend>> {
    let backend = DxgiCapture::open(display_index.unwrap_or(0), virtual_only)?;
    Ok(Box::new(backend))
}

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| RottenError::Capture(format!("DXGI factory: {e}")))?;
        enumerate_outputs(&factory)
    }
}

struct DxgiCapture {
    device: ID3D11Device,
    duplication: IDXGIOutputDuplication,
    device_name: String,
    adapter_name: String,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
    display_index: u32,
    virtual_only: bool,
    last_frame: Option<(Arc<Vec<u8>>, u32, u32)>,
}

struct AcquiredFrame {
    duplication: IDXGIOutputDuplication,
    released: bool,
}

impl AcquiredFrame {
    fn new(duplication: &IDXGIOutputDuplication) -> Self {
        Self {
            duplication: duplication.clone(),
            released: false,
        }
    }

    fn release(mut self) {
        self.released = true;
        let _ = unsafe { self.duplication.ReleaseFrame() };
    }
}

impl Drop for AcquiredFrame {
    fn drop(&mut self) {
        if !self.released {
            let _ = unsafe { self.duplication.ReleaseFrame() };
        }
    }
}

impl DxgiCapture {
    fn open(display_index: u32, virtual_only: bool) -> Result<Self> {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            );

            let factory: IDXGIFactory1 = CreateDXGIFactory1()
                .map_err(|e| RottenError::Capture(format!("DXGI factory: {e}")))?;

            let (adapter1, output, adapter_name) = find_output(&factory, display_index)?;

            let adapter: IDXGIAdapter = adapter1
                .cast()
                .map_err(|e| RottenError::Capture(format!("IDXGIAdapter cast: {e}")))?;

            let mut device = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .map_err(|e| RottenError::Capture(format!("D3D11 device: {e}")))?;

            let device = device.ok_or_else(|| RottenError::Capture("no D3D11 device".into()))?;

            let output1: IDXGIOutput1 = output
                .cast()
                .map_err(|e| RottenError::Capture(format!("IDXGIOutput1: {e}")))?;

            let duplication = output1
                .DuplicateOutput(&device)
                .map_err(|e| RottenError::Capture(format!("DuplicateOutput: {e}")))?;

            let output_desc = output1
                .GetDesc()
                .map_err(|e| RottenError::Capture(format!("GetDesc: {e}")))?;

            let width =
                (output_desc.DesktopCoordinates.right - output_desc.DesktopCoordinates.left) as u32;
            let height =
                (output_desc.DesktopCoordinates.bottom - output_desc.DesktopCoordinates.top) as u32;

            let device_name = output_device_name(&output_desc.DeviceName);

            info!(display_index, width, height, %device_name, "DXGI capture initialized");

            Ok(Self {
                device,
                duplication,
                device_name,
                adapter_name,
                staging: None,
                width,
                height,
                display_index,
                virtual_only,
                last_frame: None,
            })
        }
    }

    fn ensure_staging(&mut self, width: u32, height: u32) -> Result<ID3D11Texture2D> {
        if let Some(ref tex) = self.staging {
            unsafe {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut desc);
                if desc.Width == width && desc.Height == height {
                    return Ok(tex.clone());
                }
            }
            self.staging = None;
        }

        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };

            let mut staging = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .map_err(|e| RottenError::Capture(format!("staging texture: {e}")))?;

            let staging = staging.ok_or_else(|| RottenError::Capture("no staging".into()))?;
            self.staging = Some(staging.clone());
            Ok(staging)
        }
    }

    fn try_acquire_frame(&mut self) -> Result<CaptureFrame> {
        unsafe {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut desktop_resource = None;

            self.duplication
                .AcquireNextFrame(16, &mut frame_info, &mut desktop_resource)
                .map_err(|e| {
                    if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                        RottenError::Capture(format!(
                            "AcquireNextFrame: DXGI_ERROR_WAIT_TIMEOUT ({e})"
                        ))
                    } else {
                        RottenError::Capture(format!("AcquireNextFrame: {e}"))
                    }
                })?;

            let acquired = AcquiredFrame::new(&self.duplication);

            let resource = desktop_resource
                .ok_or_else(|| RottenError::Capture("no desktop resource".into()))?;
            let texture: ID3D11Texture2D = resource
                .cast()
                .map_err(|e| RottenError::Capture(format!("texture cast: {e}")))?;

            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);

            let staging = self.ensure_staging(desc.Width, desc.Height)?;
            let context = self
                .device
                .GetImmediateContext()
                .map_err(|e| RottenError::Capture(format!("context: {e}")))?;

            context.CopyResource(&staging, &texture);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| RottenError::Capture(format!("Map: {e}")))?;

            let row_pitch = mapped.RowPitch as usize;
            let width = desc.Width as usize;
            let height = desc.Height as usize;
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, row_pitch * height);
            let rgba = bgra_to_rgba(src, row_pitch, width, height);

            context.Unmap(&staging, 0);
            acquired.release();

            Ok(CaptureFrame {
                rgba,
                width: desc.Width,
                height: desc.Height,
            })
        }
    }
}

fn enumerate_outputs(factory: &IDXGIFactory1) -> Result<Vec<DisplayInfo>> {
    unsafe {
        let mut displays = Vec::new();
        let mut global_index = 0u32;
        for adapter_idx in 0..16 {
            let adapter = match factory.EnumAdapters1(adapter_idx) {
                Ok(a) => a,
                Err(_) => break,
            };
            let adapter_desc = adapter
                .GetDesc1()
                .map_err(|e| RottenError::Capture(format!("adapter GetDesc1: {e}")))?;
            let adapter_name = String::from_utf16_lossy(
                &adapter_desc.Description[..adapter_desc
                    .Description
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(adapter_desc.Description.len())],
            );

            for output_idx in 0..16 {
                let output = match adapter.EnumOutputs(output_idx) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                let output1: IDXGIOutput1 = output
                    .cast()
                    .map_err(|e| RottenError::Capture(format!("IDXGIOutput1: {e}")))?;
                let output_desc = output1
                    .GetDesc()
                    .map_err(|e| RottenError::Capture(format!("GetDesc: {e}")))?;
                let device_name = output_device_name(&output_desc.DeviceName);
                let width = (output_desc.DesktopCoordinates.right
                    - output_desc.DesktopCoordinates.left) as u32;
                let height = (output_desc.DesktopCoordinates.bottom
                    - output_desc.DesktopCoordinates.top) as u32;
                let is_virtual =
                    is_virtual_display_name(&device_name) || is_virtual_display_name(&adapter_name);

                displays.push(DisplayInfo {
                    index: global_index,
                    name: if device_name.is_empty() {
                        format!("{adapter_name} #{output_idx}")
                    } else {
                        device_name.clone()
                    },
                    width,
                    height,
                    is_virtual,
                });
                global_index += 1;
            }
        }
        Ok(displays)
    }
}

fn find_output(
    factory: &IDXGIFactory1,
    display_index: u32,
) -> Result<(IDXGIAdapter1, IDXGIOutput, String)> {
    unsafe {
        let mut global_index = 0u32;
        for adapter_idx in 0..16 {
            let adapter = match factory.EnumAdapters1(adapter_idx) {
                Ok(a) => a,
                Err(_) => break,
            };
            let adapter_desc = adapter
                .GetDesc1()
                .map_err(|e| RottenError::Capture(format!("adapter GetDesc1: {e}")))?;
            let adapter_name = String::from_utf16_lossy(
                &adapter_desc.Description[..adapter_desc
                    .Description
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(adapter_desc.Description.len())],
            );

            for output_idx in 0..16 {
                let output = match adapter.EnumOutputs(output_idx) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                if global_index == display_index {
                    return Ok((adapter, output, adapter_name));
                }
                global_index += 1;
            }
        }
        Err(RottenError::Capture(format!(
            "display index {display_index} not found ({global_index} outputs total)"
        )))
    }
}

fn output_device_name(device_name: &[u16; 32]) -> String {
    String::from_utf16_lossy(
        &device_name[..device_name
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(device_name.len())],
    )
}

#[async_trait]
impl CaptureBackend for DxgiCapture {
    fn displays(&self) -> Result<Vec<DisplayInfo>> {
        Ok(vec![DisplayInfo {
            index: self.display_index,
            name: self.device_name.clone(),
            width: self.width,
            height: self.height,
            is_virtual: is_virtual_display_name(&self.device_name)
                || is_virtual_display_name(&self.adapter_name),
        }])
    }

    fn grab_frame(&mut self) -> Result<CaptureFrame> {
        match self.try_acquire_frame() {
            Ok(frame) => {
                static ACQUIRED: AtomicU64 = AtomicU64::new(0);
                let n = ACQUIRED.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 {
                    agent_log(
                        "dxgi.rs:grab_frame",
                        "first DXGI frame acquired",
                        "H1",
                        serde_json::json!({
                            "width": frame.width,
                            "height": frame.height,
                        }),
                    );
                }
                let rgba = Arc::new(frame.rgba);
                self.last_frame = Some((rgba.clone(), frame.width, frame.height));
                Ok(CaptureFrame {
                    rgba: (*rgba).clone(),
                    width: frame.width,
                    height: frame.height,
                })
            }
            Err(e) if is_dxgi_wait_timeout(&e) => {
                static TIMEOUTS: AtomicU64 = AtomicU64::new(0);
                let n = TIMEOUTS.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 || n % 60 == 0 {
                    agent_log(
                        "dxgi.rs:grab_frame",
                        "DXGI wait timeout — reusing cached frame",
                        "H1",
                        serde_json::json!({
                            "timeoutCount": n,
                            "hasLastFrame": self.last_frame.is_some(),
                            "width": self.width,
                            "height": self.height,
                        }),
                    );
                }
                if let Some((rgba, width, height)) = &self.last_frame {
                    return Ok(CaptureFrame {
                        rgba: rgba.as_ref().clone(),
                        width: *width,
                        height: *height,
                    });
                }
                Ok(blank_frame(self.width, self.height))
            }
            Err(e) => Err(e),
        }
    }

    fn backend_name(&self) -> &'static str {
        "dxgi"
    }
}

fn is_dxgi_wait_timeout(err: &RottenError) -> bool {
    let RottenError::Capture(msg) = err else {
        return false;
    };
    msg.contains("0x887A0027") || msg.contains("DXGI_ERROR_WAIT_TIMEOUT")
}

fn blank_frame(width: u32, height: u32) -> CaptureFrame {
    CaptureFrame {
        rgba: vec![0u8; (width as usize) * (height as usize) * 4],
        width,
        height,
    }
}

fn bgra_to_rgba(src: &[u8], row_pitch: usize, width: usize, height: usize) -> Vec<u8> {
    let mut rgba = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let src_off = y * row_pitch + x * 4;
            let dst_off = (y * width + x) * 4;
            if src_off + 3 < src.len() && dst_off + 3 < rgba.len() {
                rgba[dst_off] = src[src_off + 2];
                rgba[dst_off + 1] = src[src_off + 1];
                rgba[dst_off + 2] = src[src_off];
                rgba[dst_off + 3] = 255;
            }
        }
    }
    rgba
}
