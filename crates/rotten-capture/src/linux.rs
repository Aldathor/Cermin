use async_trait::async_trait;
use rotten_core::error::{Result, RottenError};
use tracing::info;
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;

use crate::backend::{CaptureBackend, CaptureFrame, DisplayInfo};
use crate::virtual_display::is_virtual_display_name;

pub fn create_linux_backend(
    display_index: Option<u32>,
    virtual_only: bool,
) -> Result<Box<dyn CaptureBackend>> {
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        info!("Wayland detected; using X11 fallback via XWayland if available");
    }

    let backend = X11Capture::open(display_index, virtual_only)?;
    Ok(Box::new(backend))
}

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    let (conn, screen_num) = RustConnection::connect(None)
        .map_err(|e| RottenError::Capture(format!("X11 connect: {e}")))?;
    enumerate_randr_displays(&conn, screen_num)
}

struct MonitorRegion {
    root: u32,
    x: i16,
    y: i16,
    width: u32,
    height: u32,
    depth: u8,
    name: String,
    index: u32,
    is_virtual: bool,
}

struct X11Capture {
    conn: RustConnection,
    region: MonitorRegion,
}

impl X11Capture {
    fn open(display_index: Option<u32>, _virtual_only: bool) -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None)
            .map_err(|e| RottenError::Capture(format!("X11 connect: {e}")))?;
        let displays = enumerate_randr_displays(&conn, screen_num)?;
        let idx = display_index.unwrap_or(0);
        let selected = displays.iter().find(|d| d.index == idx).ok_or_else(|| {
            RottenError::Capture(format!(
                "display index {idx} not found ({} outputs total)",
                displays.len()
            ))
        })?;

        let region = monitor_region_for_display(&conn, screen_num, selected)?;
        info!(
            display_index = idx,
            width = region.width,
            height = region.height,
            name = %region.name,
            "X11 capture initialized"
        );

        Ok(Self { conn, region })
    }
}

#[async_trait]
impl CaptureBackend for X11Capture {
    fn displays(&self) -> Result<Vec<DisplayInfo>> {
        Ok(vec![DisplayInfo {
            index: self.region.index,
            name: self.region.name.clone(),
            width: self.region.width,
            height: self.region.height,
            is_virtual: self.region.is_virtual,
            adapter: None,
        }])
    }

    fn grab_frame(&mut self) -> Result<CaptureFrame> {
        let region = &self.region;
        let pixmap = self
            .conn
            .generate_id()
            .map_err(|e| RottenError::Capture(format!("pixmap id: {e}")))?;

        self.conn
            .create_pixmap(
                region.depth,
                pixmap,
                region.root,
                region.width as u16,
                region.height as u16,
            )
            .map_err(|e| RottenError::Capture(format!("create_pixmap: {e}")))?;

        let gc = self
            .conn
            .generate_id()
            .map_err(|e| RottenError::Capture(format!("gc id: {e}")))?;

        self.conn
            .create_gc(gc, pixmap, &Default::default())
            .map_err(|e| RottenError::Capture(format!("create_gc: {e}")))?;

        self.conn
            .copy_area(
                region.root,
                pixmap,
                gc,
                region.x,
                region.y,
                0,
                0,
                region.width as u16,
                region.height as u16,
            )
            .map_err(|e| RottenError::Capture(format!("copy_area: {e}")))?;

        let image = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                pixmap,
                0,
                0,
                region.width as u16,
                region.height as u16,
                !0,
            )
            .map_err(|e| RottenError::Capture(format!("get_image: {e}")))?
            .reply()
            .map_err(|e| RottenError::Capture(format!("get_image reply: {e}")))?;

        let _ = self.conn.free_pixmap(pixmap);
        let _ = self.conn.free_gc(gc);

        let rgba = bgrx_to_rgba(&image.data, region.width as usize, region.height as usize);

        Ok(CaptureFrame {
            rgba,
            width: region.width,
            height: region.height,
        })
    }

    fn backend_name(&self) -> &'static str {
        "x11"
    }
}

fn enumerate_randr_displays(conn: &RustConnection, screen_num: usize) -> Result<Vec<DisplayInfo>> {
    let setup = conn.setup();
    let screen = &setup.roots[screen_num];
    let root = screen.root;

    let resources = conn
        .randr_get_screen_resources_current(root)
        .map_err(|e| RottenError::Capture(format!("randr resources: {e}")))?
        .reply()
        .map_err(|e| RottenError::Capture(format!("randr resources reply: {e}")))?;

    let mut crtc_map = std::collections::HashMap::new();
    for &crtc_id in &resources.crtcs {
        let info = conn
            .randr_get_crtc_info(crtc_id, resources.config_timestamp)
            .map_err(|e| RottenError::Capture(format!("randr crtc: {e}")))?
            .reply()
            .map_err(|e| RottenError::Capture(format!("randr crtc reply: {e}")))?;
        crtc_map.insert(crtc_id, info);
    }

    let mut displays = Vec::new();
    for (index, &output_id) in resources.outputs.iter().enumerate() {
        let output = conn
            .randr_get_output_info(output_id, resources.config_timestamp)
            .map_err(|e| RottenError::Capture(format!("randr output: {e}")))?
            .reply()
            .map_err(|e| RottenError::Capture(format!("randr output reply: {e}")))?;

        if output.crtc == 0 {
            continue;
        }

        let name = String::from_utf8_lossy(&output.name).into_owned();
        let (width, height) = crtc_map
            .get(&output.crtc)
            .map(|crtc| (crtc.width as u32, crtc.height as u32))
            .unwrap_or((
                screen.width_in_pixels as u32,
                screen.height_in_pixels as u32,
            ));

        displays.push(DisplayInfo {
            index: index as u32,
            name: name.clone(),
            width,
            height,
            is_virtual: is_virtual_display_name(&name),
            adapter: None,
        });
    }

    if displays.is_empty() {
        displays.push(DisplayInfo {
            index: 0,
            name: format!("X11 display {screen_num}"),
            width: screen.width_in_pixels as u32,
            height: screen.height_in_pixels as u32,
            is_virtual: false,
            adapter: None,
        });
    }

    Ok(displays)
}

fn monitor_region_for_display(
    conn: &RustConnection,
    screen_num: usize,
    display: &DisplayInfo,
) -> Result<MonitorRegion> {
    let setup = conn.setup();
    let screen = &setup.roots[screen_num];
    let root = screen.root;

    let resources = conn
        .randr_get_screen_resources_current(root)
        .map_err(|e| RottenError::Capture(format!("randr resources: {e}")))?
        .reply()
        .map_err(|e| RottenError::Capture(format!("randr resources reply: {e}")))?;

    let output_id = resources
        .outputs
        .get(display.index as usize)
        .copied()
        .ok_or_else(|| RottenError::Capture(format!("output index {} missing", display.index)))?;

    let output = conn
        .randr_get_output_info(output_id, resources.config_timestamp)
        .map_err(|e| RottenError::Capture(format!("randr output: {e}")))?
        .reply()
        .map_err(|e| RottenError::Capture(format!("randr output reply: {e}")))?;

    let crtc_id = output.crtc;
    if crtc_id == 0 {
        return Err(RottenError::Capture(format!(
            "output {} is not active",
            display.name
        )));
    }

    let crtc = conn
        .randr_get_crtc_info(crtc_id, resources.config_timestamp)
        .map_err(|e| RottenError::Capture(format!("randr crtc: {e}")))?
        .reply()
        .map_err(|e| RottenError::Capture(format!("randr crtc reply: {e}")))?;

    Ok(MonitorRegion {
        root,
        x: crtc.x,
        y: crtc.y,
        width: crtc.width as u32,
        height: crtc.height as u32,
        depth: screen.root_depth,
        name: display.name.clone(),
        index: display.index,
        is_virtual: display.is_virtual,
    })
}

fn bgrx_to_rgba(bgrx: &[u8], width: usize, height: usize) -> Vec<u8> {
    let pixel_count = width * height;
    let mut rgba = vec![0u8; pixel_count * 4];
    for i in 0..pixel_count {
        let src = i * 4;
        if src + 3 >= bgrx.len() {
            break;
        }
        rgba[i * 4] = bgrx[src + 2];
        rgba[i * 4 + 1] = bgrx[src + 1];
        rgba[i * 4 + 2] = bgrx[src];
        rgba[i * 4 + 3] = 255;
    }
    rgba
}
