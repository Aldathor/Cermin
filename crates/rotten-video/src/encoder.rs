use rotten_core::config::HwAccel;
use rotten_core::debug_log::agent_log;
use rotten_core::error::{Result, RottenError};

#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
use openh264::OpenH264API;
#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, UsageType,
};
#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
use openh264::formats::YUVSource;

#[cfg(all(
    feature = "software-encode-source",
    not(feature = "software-encode-dll")
))]
fn create_openh264_api() -> Result<OpenH264API> {
    Ok(OpenH264API::from_source())
}

#[cfg(feature = "software-encode-dll")]
fn create_openh264_api() -> Result<OpenH264API> {
    use std::path::PathBuf;

    const DLL_NAME: &str = "openh264-2.6.0-win64.dll";
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(DLL_NAME));
        }
    }
    candidates.push(PathBuf::from(DLL_NAME));

    for path in candidates {
        if !path.exists() {
            continue;
        }
        // #region agent log
        agent_log(
            "encoder.rs:create_openh264_api",
            "loading openh264 dll",
            "H18",
            serde_json::json!({ "path": path.to_string_lossy() }),
        );
        // #endregion
        return OpenH264API::from_blob_path(&path)
            .map_err(|e| RottenError::Video(format!("openh264 dll {}: {e}", path.display())));
    }

    Err(RottenError::Video(format!(
        "missing {DLL_NAME} next to cermin.exe — copy it from the build output or https://www.openh264.org/"
    )))
}

/// Hardware encoder preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwEncoderKind {
    Software,
    Nvenc,
    Vaapi,
}

impl HwEncoderKind {
    pub fn resolve(pref: HwAccel) -> Self {
        match pref {
            HwAccel::Nvenc => Self::Nvenc,
            HwAccel::Vaapi => Self::Vaapi,
            HwAccel::None | HwAccel::Auto => Self::Software,
        }
    }
}

/// A single H.264 encoded frame with timing metadata.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub pts_us: u64,
    pub is_keyframe: bool,
    pub coded_width: u32,
    pub coded_height: u32,
    pub display_width: u32,
    pub display_height: u32,
}

const MAX_STREAM_WIDTH: u32 = 1920;
const MAX_STREAM_HEIGHT: u32 = 1088;

/// `new_exact` visible bounds: true 1080p, never the 1088 macroblock pad.
/// 16x16 is OpenH264's native minimum encode size.
#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
const EXACT_MIN_DIM: u32 = 16;
#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
const EXACT_MAX_WIDTH: u32 = 1920;
#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
const EXACT_MAX_HEIGHT: u32 = 1080;
/// `new_exact` rate bounds match the encoder's native support: 1..=60 fps
/// (OpenH264's own maximum) and the 500 kbps rate-control floor, with the
/// upper bitrate bounded so the bits-per-second value fits the encoder's
/// `i32` field. Exact mode rejects out-of-range values instead of clamping.
#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
const EXACT_MAX_FPS: u32 = 60;
#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
const EXACT_MIN_BITRATE_KBPS: u32 = 500;
#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
const EXACT_MAX_BITRATE_KBPS: u32 = i32::MAX as u32 / 1000;

/// How the encoder maps a capture frame onto the H.264 picture.
#[cfg_attr(
    not(any(feature = "software-encode-source", feature = "software-encode-dll")),
    allow(dead_code)
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeometryMode {
    /// AirPlay mirroring: macroblock-align the coded size (`fit_stream_dims`),
    /// padding 1080 to 1088 and scaling oversized frames down.
    LegacyAligned,
    /// Cast/HLS: feed the visible size as the source size and let OpenH264 pad
    /// the macroblock grid internally and crop the coded picture with SPS
    /// frame cropping, so decoders present the exact visible dimensions.
    ExactVisible,
}

/// Validates `new_exact` input. The legacy constructor keeps its permissive
/// clamp behavior; only the exact API rejects bad geometry outright.
#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
fn validate_exact_params(width: u32, height: u32, bitrate_kbps: u32, fps: u32) -> Result<()> {
    if !width.is_multiple_of(2) || !(EXACT_MIN_DIM..=EXACT_MAX_WIDTH).contains(&width) {
        return Err(RottenError::Video(format!(
            "exact encoder width must be even and {EXACT_MIN_DIM}..={EXACT_MAX_WIDTH}, got {width}"
        )));
    }
    if !height.is_multiple_of(2) || !(EXACT_MIN_DIM..=EXACT_MAX_HEIGHT).contains(&height) {
        return Err(RottenError::Video(format!(
            "exact encoder height must be even and {EXACT_MIN_DIM}..={EXACT_MAX_HEIGHT}, got {height}"
        )));
    }
    if !(1..=EXACT_MAX_FPS).contains(&fps) {
        return Err(RottenError::Video(format!(
            "exact encoder fps must be 1..={EXACT_MAX_FPS}, got {fps}"
        )));
    }
    if !(EXACT_MIN_BITRATE_KBPS..=EXACT_MAX_BITRATE_KBPS).contains(&bitrate_kbps) {
        return Err(RottenError::Video(format!(
            "exact encoder bitrate must be {EXACT_MIN_BITRATE_KBPS}..={EXACT_MAX_BITRATE_KBPS} kbps, got {bitrate_kbps}"
        )));
    }
    Ok(())
}

/// Build stamp for debug sessions; bump when verifying a new Windows binary.
pub const ENCODER_BUILD_ID: &str = rotten_core::debug_log::DEBUG_BUILD_ID;

/// Round down to a multiple of 16 (H.264 macroblock grid).
fn align16(v: u32) -> u32 {
    v & !15
}

/// Round up to a multiple of 16 (1080p → 1088 coded lines with bottom crop/pad).
fn align16_ceil(v: u32) -> u32 {
    v.div_ceil(16) * 16
}

pub fn fit_stream_dims(width: u32, height: u32) -> (u32, u32) {
    let w = align16(width);
    let h = align16_ceil(height);
    if w == 0 || h == 0 {
        return (16, 16);
    }
    if w <= MAX_STREAM_WIDTH && h <= MAX_STREAM_HEIGHT {
        return (w, h);
    }
    let scale = (MAX_STREAM_WIDTH as f64 / w as f64).min(MAX_STREAM_HEIGHT as f64 / h as f64);
    let nw = align16((((w as f64) * scale) as u32).max(16));
    let nh = align16_ceil((((h as f64) * scale) as u32).max(16));
    (nw, nh)
}

pub fn downscale_rgba(rgba: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let src_w = src_w as usize;
    let src_h = src_h as usize;
    let dst_w = dst_w as usize;
    let dst_h = dst_h as usize;
    let mut out = vec![0u8; dst_w * dst_h * 4];
    for y in 0..dst_h {
        let sy = y * src_h / dst_h;
        for x in 0..dst_w {
            let sx = x * src_w / dst_w;
            let src_i = (sy * src_w + sx) * 4;
            let dst_i = (y * dst_w + x) * 4;
            if src_i + 3 < rgba.len() {
                out[dst_i..dst_i + 4].copy_from_slice(&rgba[src_i..src_i + 4]);
            }
        }
    }
    out
}

/// I420 frame buffer fed straight to OpenH264 (no RGB intermediate copies).
///
/// The openh264 crate converts RGB to YUV with a scalar f32 loop per pixel;
/// doing the RGBA -> I420 conversion ourselves with 8-bit integer math on the
/// capture buffer removes a full-frame copy and most of the conversion cost.
#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
pub struct I420Source {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    width: usize,
    height: usize,
}

#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
#[inline(always)]
fn y_of(r: i32, g: i32, b: i32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8
}

#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
#[inline(always)]
fn u_of(r: i32, g: i32, b: i32) -> u8 {
    (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8
}

#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
#[inline(always)]
fn v_of(r: i32, g: i32, b: i32) -> u8 {
    (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8
}

#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
impl I420Source {
    pub fn new(width: usize, height: usize) -> Self {
        let chroma = (width / 2) * (height / 2);
        Self {
            y: vec![0u8; width * height],
            u: vec![0u8; chroma],
            v: vec![0u8; chroma],
            width,
            height,
        }
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Fills the buffer from an RGBA frame of `src_w` x `src_h` pixels, leaving
    /// any bottom/right rows black (YUV limited-range black). BT.601 limited
    /// range with the same coefficients openh264 uses. Block rows are split
    /// across a few threads; at 1080p this is the hottest per-frame CPU loop.
    pub fn fill_rgba(&mut self, rgba: &[u8], src_w: usize, src_h: usize) {
        let w = self.width;
        let half_w = w / 2;
        let bw = (src_w / 2).min(half_w);
        let bh = (src_h / 2).min(self.height / 2);
        let src_stride = src_w * 4;

        if bh > 0 && bw > 0 {
            let threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(6)
                .min(bh);
            let rows_per_chunk = bh.div_ceil(threads.max(1));
            let y_plane = &mut self.y[..bh * 2 * w];
            let u_plane = &mut self.u[..bh * half_w];
            let v_plane = &mut self.v[..bh * half_w];
            std::thread::scope(|scope| {
                let mut y_chunks = y_plane.chunks_mut(rows_per_chunk * 2 * w);
                let mut u_chunks = u_plane.chunks_mut(rows_per_chunk * half_w);
                let mut v_chunks = v_plane.chunks_mut(rows_per_chunk * half_w);
                let mut first_row = 0usize;
                while let (Some(y_c), Some(u_c), Some(v_c)) =
                    (y_chunks.next(), u_chunks.next(), v_chunks.next())
                {
                    let take = rows_per_chunk.min(bh - first_row);
                    if take == 0 {
                        break;
                    }
                    let rgba_ref: &[u8] = rgba;
                    scope.spawn(move || {
                        convert_block_rows(
                            rgba_ref, first_row, take, src_stride, src_w, w, half_w, bw, y_c, u_c,
                            v_c,
                        );
                    });
                    first_row += take;
                }
            });
        }

        // Pad the bottom rows (coded height is macroblock aligned, e.g. 1088).
        for y in src_h.min(self.height)..self.height {
            self.y[y * w..(y + 1) * w].fill(16);
        }
        for j in bh..self.height / 2 {
            self.u[j * half_w..(j + 1) * half_w].fill(128);
            self.v[j * half_w..(j + 1) * half_w].fill(128);
        }
    }
}

/// Converts `block_rows` 2x2 pixel blocks starting at `first_block_row` from an
/// RGBA plane into the given Y/U/V slice chunks.
#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
#[allow(clippy::too_many_arguments)]
fn convert_block_rows(
    rgba: &[u8],
    first_block_row: usize,
    block_rows: usize,
    src_stride: usize,
    src_w: usize,
    w: usize,
    half_w: usize,
    bw: usize,
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) {
    for jj in 0..block_rows {
        let j = first_block_row + jj;
        let row0 = &rgba[(2 * j) * src_stride..(2 * j) * src_stride + src_w * 4];
        let row1 = &rgba[(2 * j + 1) * src_stride..(2 * j + 1) * src_stride + src_w * 4];
        let (y_top, y_bottom) = y.split_at_mut(jj * 2 * w + w);
        let out_y0 = &mut y_top[jj * 2 * w..];
        let out_y1 = &mut y_bottom[..w];
        let out_u = &mut u[jj * half_w..(jj + 1) * half_w];
        let out_v = &mut v[jj * half_w..(jj + 1) * half_w];

        for i in 0..bw {
            let p00 = &row0[i * 8..i * 8 + 4];
            let p01 = &row0[i * 8 + 4..i * 8 + 8];
            let p10 = &row1[i * 8..i * 8 + 4];
            let p11 = &row1[i * 8 + 4..i * 8 + 8];
            let (r00, g00, b00) = (i32::from(p00[0]), i32::from(p00[1]), i32::from(p00[2]));
            let (r01, g01, b01) = (i32::from(p01[0]), i32::from(p01[1]), i32::from(p01[2]));
            let (r10, g10, b10) = (i32::from(p10[0]), i32::from(p10[1]), i32::from(p10[2]));
            let (r11, g11, b11) = (i32::from(p11[0]), i32::from(p11[1]), i32::from(p11[2]));

            out_y0[2 * i] = y_of(r00, g00, b00);
            out_y0[2 * i + 1] = y_of(r01, g01, b01);
            out_y1[2 * i] = y_of(r10, g10, b10);
            out_y1[2 * i + 1] = y_of(r11, g11, b11);

            let r = (r00 + r01 + r10 + r11) >> 2;
            let g = (g00 + g01 + g10 + g11) >> 2;
            let b = (b00 + b01 + b10 + b11) >> 2;
            out_u[i] = u_of(r, g, b);
            out_v[i] = v_of(r, g, b);
        }

        // Pad the right edge (only when downscaling width).
        for i in bw..half_w {
            out_y0[2 * i] = 16;
            out_y0[2 * i + 1] = 16;
            out_y1[2 * i] = 16;
            out_y1[2 * i + 1] = 16;
            out_u[i] = 128;
            out_v[i] = 128;
        }
    }
}

#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
impl YUVSource for I420Source {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

/// H.264 encoder trait.
pub trait EncoderTrait: Send {
    fn encode(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        pts_us: u64,
    ) -> Result<Option<EncodedFrame>>;
    fn force_keyframe(&mut self);
    fn kind(&self) -> HwEncoderKind;
}

/// Software H.264 encoder (OpenH264).
pub struct SoftwareEncoder {
    #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
    encoder: Encoder,
    #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
    yuv_buf: Option<I420Source>,
    width: u32,
    height: u32,
    frame_count: u64,
    force_idr: bool,
    bitrate_kbps: u32,
    geometry: GeometryMode,
}

impl SoftwareEncoder {
    /// Creates a legacy AirPlay-geometry encoder: the coded size is
    /// macroblock-aligned (`fit_stream_dims`), so 1080 becomes 1088 and
    /// oversized frames are scaled down.
    #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
    pub fn new(width: u32, height: u32, bitrate_kbps: u32, fps: u32) -> Result<Self> {
        Self::with_geometry(
            width,
            height,
            bitrate_kbps,
            fps,
            GeometryMode::LegacyAligned,
        )
    }

    /// Creates an exact-visible encoder: `width` x `height` stay the real coded
    /// source size, so OpenH264's internal macroblock padding is cropped back
    /// to the visible dimensions by SPS frame cropping (1920x1080 decodes as
    /// 1920x1080, never 1920x1088).
    ///
    /// `width`/`height` must be even and within 16..=1920 by 16..=1080, `fps`
    /// must be 1..=60 and `bitrate_kbps` 500..=2147483 (the encoder's native
    /// limits, so nothing is silently clipped or clamped). Every `encode` call
    /// must pass exactly these dimensions and a full `width * height * 4` RGBA
    /// buffer; a different resolution is an error and the caller must
    /// reconnect.
    #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
    pub fn new_exact(width: u32, height: u32, bitrate_kbps: u32, fps: u32) -> Result<Self> {
        validate_exact_params(width, height, bitrate_kbps, fps)?;
        Self::with_geometry(width, height, bitrate_kbps, fps, GeometryMode::ExactVisible)
    }

    #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
    fn with_geometry(
        width: u32,
        height: u32,
        bitrate_kbps: u32,
        fps: u32,
        geometry: GeometryMode,
    ) -> Result<Self> {
        // Tuning knobs (dev): thread count and rate-control mode.
        let threads = std::env::var("CERMIN_ENCODER_THREADS")
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get().min(4))
                    .unwrap_or(1)
                    .max(1) as u16
            });
        let rc_name: &str = match std::env::var("CERMIN_ENCODER_RC").ok().as_deref() {
            Some("quality") => "quality",
            Some("buffer") => "buffer",
            _ => "bitrate",
        };
        #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
        let encoder = {
            let bps = (bitrate_kbps.max(500) as u32) * 1000;
            let fps_hz = fps.max(1) as f32;
            let rc = match rc_name {
                "quality" => RateControlMode::Quality,
                "buffer" => RateControlMode::Bufferbased,
                _ => RateControlMode::Bitrate,
            };
            let config = EncoderConfig::new()
                .bitrate(BitRate::from_bps(bps))
                .usage_type(UsageType::ScreenContentRealTime)
                .max_frame_rate(FrameRate::from_hz(fps_hz))
                .rate_control_mode(rc)
                .complexity(Complexity::Low)
                .num_threads(threads)
                .scene_change_detect(true)
                .adaptive_quantization(false)
                .background_detection(false)
                .intra_frame_period(IntraFramePeriod::from_num_frames((fps.max(1) * 5).max(30)));
            let api = create_openh264_api()?;
            Encoder::with_api_config(api, config)
                .map_err(|e| RottenError::Video(format!("openh264 encoder init: {e}")))?
        };

        let enc = Self {
            #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
            encoder,
            #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
            yuv_buf: None,
            width,
            height,
            frame_count: 0,
            force_idr: true,
            bitrate_kbps,
            geometry,
        };
        // #region agent log
        agent_log(
            "encoder.rs:new",
            "openh264 encoder initialized",
            "H8",
            serde_json::json!({
                "width": width,
                "height": height,
                "bitrateKbps": bitrate_kbps,
                "fps": fps,
                "complexity": "low",
                "rateControl": rc_name,
                "threads": threads,
                "buildId": ENCODER_BUILD_ID,
            }),
        );
        // #endregion
        Ok(enc)
    }

    #[cfg(not(any(feature = "software-encode-source", feature = "software-encode-dll")))]
    pub fn new(_width: u32, _height: u32, _bitrate_kbps: u32, _fps: u32) -> Result<Self> {
        Err(RottenError::Video(
            "software encoder not enabled (rebuild with encode-source or encode-dll)".into(),
        ))
    }

    #[cfg(not(any(feature = "software-encode-source", feature = "software-encode-dll")))]
    pub fn new_exact(_width: u32, _height: u32, _bitrate_kbps: u32, _fps: u32) -> Result<Self> {
        Err(RottenError::Video(
            "software encoder not enabled (rebuild with encode-source or encode-dll)".into(),
        ))
    }

    pub fn from_hw_pref(
        width: u32,
        height: u32,
        bitrate_kbps: u32,
        fps: u32,
        pref: HwAccel,
    ) -> Result<Box<dyn EncoderTrait>> {
        let kind = HwEncoderKind::resolve(pref);
        match kind {
            HwEncoderKind::Nvenc => {
                return Err(RottenError::Video(
                    "NVENC hardware encoding is not implemented yet; use --hwaccel none or auto"
                        .into(),
                ));
            }
            HwEncoderKind::Vaapi => {
                return Err(RottenError::Video(
                    "VAAPI hardware encoding is not implemented yet; use --hwaccel none or auto"
                        .into(),
                ));
            }
            HwEncoderKind::Software => {}
        }
        Ok(Box::new(Self::new(width, height, bitrate_kbps, fps)?))
    }

    /// H.264 needs even width/height; do not macroblock-align here (fit_stream_dims handles that).
    fn even_dim(v: u32) -> u32 {
        v & !1
    }
}

impl EncoderTrait for SoftwareEncoder {
    fn encode(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        pts_us: u64,
    ) -> Result<Option<EncodedFrame>> {
        let (display_w, display_h, coded_w, coded_h) = match self.geometry {
            GeometryMode::LegacyAligned => {
                let display_w = Self::even_dim(width);
                let display_h = Self::even_dim(height);
                if display_w == 0 || display_h == 0 {
                    return Ok(None);
                }
                let (coded_w, coded_h) = fit_stream_dims(display_w, display_h);
                if coded_w != self.width || coded_h != self.height {
                    self.width = coded_w;
                    self.height = coded_h;
                    self.force_idr = true;
                }
                (display_w, display_h, coded_w, coded_h)
            }
            GeometryMode::ExactVisible => {
                if width != self.width || height != self.height {
                    return Err(RottenError::Video(format!(
                        "exact encoder geometry is fixed at {}x{} (got {width}x{height}); \
                         reconnect the stream to change resolution",
                        self.width, self.height
                    )));
                }
                let expected = width as usize * height as usize * 4;
                if rgba.len() != expected {
                    return Err(RottenError::Video(format!(
                        "exact encoder needs {expected} RGBA bytes for {width}x{height}, got {}",
                        rgba.len()
                    )));
                }
                // OpenH264 pads the macroblock grid internally and SPS-crops
                // back to these visible dimensions.
                (width, height, align16_ceil(width), align16_ceil(height))
            }
        };

        self.frame_count += 1;

        // #region agent log
        if self.frame_count == 1 {
            agent_log(
                "encoder.rs:encode",
                "encode started",
                "H9",
                serde_json::json!({
                    "displayW": display_w,
                    "displayH": display_h,
                    "codedW": coded_w,
                    "codedH": coded_h,
                    "codedWMod16": coded_w % 16,
                    "codedHMod16": coded_h % 16,
                    "rgbaBytes": rgba.len(),
                    "frameCount": self.frame_count,
                }),
            );
        }
        // #endregion

        let encode_start = std::time::Instant::now();
        let conv_start = std::time::Instant::now();

        #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
        {
            // Legacy pads/scales into the coded size; exact feeds the visible
            // size and lets OpenH264 pad the macroblock grid internally.
            let (buf_w, buf_h) = match self.geometry {
                GeometryMode::LegacyAligned => (coded_w as usize, coded_h as usize),
                GeometryMode::ExactVisible => (display_w as usize, display_h as usize),
            };
            let needs_alloc = match self.yuv_buf.as_ref() {
                Some(b) => b.dimensions() != (buf_w, buf_h),
                None => true,
            };
            if needs_alloc {
                self.yuv_buf = Some(I420Source::new(buf_w, buf_h));
            }
            let yuv = self.yuv_buf.as_mut().expect("yuv buffer");

            match self.geometry {
                GeometryMode::LegacyAligned if coded_w == display_w && coded_h >= display_h => {
                    // Fast path: no scaling, just pad the macroblock-aligned bottom rows.
                    let mode = if coded_h > display_h {
                        "pad-bottom"
                    } else {
                        "none"
                    };
                    // #region agent log
                    if self.frame_count == 1 {
                        agent_log(
                            "encoder.rs:encode",
                            "fitting rgba to coded size",
                            "H111",
                            serde_json::json!({
                                "mode": mode,
                                "fromW": display_w,
                                "fromH": display_h,
                                "toW": coded_w,
                                "toH": coded_h,
                            }),
                        );
                    }
                    // #endregion
                    yuv.fill_rgba(rgba, display_w as usize, display_h as usize);
                }
                GeometryMode::LegacyAligned => {
                    let scaled = downscale_rgba(rgba, display_w, display_h, coded_w, coded_h);
                    if self.frame_count == 1 {
                        // #region agent log
                        agent_log(
                            "encoder.rs:encode",
                            "fitting rgba to coded size",
                            "H111",
                            serde_json::json!({
                                "mode": "downscale",
                                "fromW": display_w,
                                "fromH": display_h,
                                "toW": coded_w,
                                "toH": coded_h,
                            }),
                        );
                        // #endregion
                    }
                    yuv.fill_rgba(&scaled, buf_w, buf_h);
                }
                GeometryMode::ExactVisible => {
                    if self.frame_count == 1 {
                        // #region agent log
                        agent_log(
                            "encoder.rs:encode",
                            "encoding exact visible frame",
                            "H111",
                            serde_json::json!({
                                "mode": "exact",
                                "fromW": display_w,
                                "fromH": display_h,
                                "codedW": coded_w,
                                "codedH": coded_h,
                            }),
                        );
                        // #endregion
                    }
                    yuv.fill_rgba(rgba, display_w as usize, display_h as usize);
                }
            }

            if self.force_idr {
                self.encoder.force_intra_frame();
                self.force_idr = false;
            }

            let conv_ms = conv_start.elapsed().as_millis();
            let h264_start = std::time::Instant::now();
            let bitstream = self
                .encoder
                .encode(yuv)
                .map_err(|e| RottenError::Video(format!("openh264 encode: {e}")))?;
            let h264_ms = h264_start.elapsed().as_millis();
            let data = bitstream.to_vec();
            if data.is_empty() {
                return Ok(None);
            }

            let is_keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);

            let duration_ms = encode_start.elapsed().as_millis();

            // #region agent log
            if self.frame_count <= 3 || self.frame_count % 30 == 0 {
                agent_log(
                    "encoder.rs:encode",
                    "openh264 frame encoded",
                    "H10",
                    serde_json::json!({
                        "frameCount": self.frame_count,
                        "h264Bytes": data.len(),
                        "keyframe": is_keyframe,
                        "codedW": coded_w,
                        "codedH": coded_h,
                        "durationMs": duration_ms,
                        "convMs": conv_ms,
                        "h264Ms": h264_ms,
                    }),
                );
            }
            // #endregion

            return Ok(Some(EncodedFrame {
                data,
                pts_us,
                is_keyframe,
                coded_width: coded_w,
                coded_height: coded_h,
                display_width: display_w,
                display_height: display_h,
            }));
        }

        #[cfg(not(any(feature = "software-encode-source", feature = "software-encode-dll")))]
        {
            let _ = (rgba, pts_us);
            Err(RottenError::Video(
                "software encoder not enabled (rebuild with encode-source or encode-dll)".into(),
            ))
        }
    }

    fn force_keyframe(&mut self) {
        self.force_idr = true;
    }

    fn kind(&self) -> HwEncoderKind {
        HwEncoderKind::Software
    }
}

/// Deferred encoder init so OpenH264 setup runs only on a blocking thread.
pub struct LazyEncoder {
    inner: Option<Box<dyn EncoderTrait>>,
    init_width: u32,
    init_height: u32,
    bitrate_kbps: u32,
    fps: u32,
    hw_accel: HwAccel,
}

impl LazyEncoder {
    pub fn new(width: u32, height: u32, bitrate_kbps: u32, fps: u32, hw_accel: HwAccel) -> Self {
        let (coded_w, coded_h) = fit_stream_dims(width, height);
        Self {
            inner: None,
            init_width: coded_w,
            init_height: coded_h,
            bitrate_kbps,
            fps,
            hw_accel,
        }
    }

    fn ensure(&mut self) -> Result<&mut dyn EncoderTrait> {
        if self.inner.is_none() {
            // #region agent log
            agent_log(
                "encoder.rs:lazy",
                "lazy encoder init starting",
                "H13",
                serde_json::json!({
                    "codedW": self.init_width,
                    "codedH": self.init_height,
                    "bitrateKbps": self.bitrate_kbps,
                    "fps": self.fps,
                    "buildId": ENCODER_BUILD_ID,
                }),
            );
            // #endregion
            let enc = SoftwareEncoder::from_hw_pref(
                self.init_width,
                self.init_height,
                self.bitrate_kbps,
                self.fps,
                self.hw_accel,
            )?;
            self.inner = Some(enc);
            // #region agent log
            agent_log(
                "encoder.rs:lazy",
                "lazy encoder init finished",
                "H13",
                serde_json::json!({
                    "codedW": self.init_width,
                    "codedH": self.init_height,
                    "buildId": ENCODER_BUILD_ID,
                }),
            );
            // #endregion
        }
        Ok(self.inner.as_mut().expect("lazy encoder").as_mut())
    }

    pub fn encode(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        pts_us: u64,
    ) -> Result<Option<EncodedFrame>> {
        self.ensure()?.encode(rgba, width, height, pts_us)
    }
}

pub fn auto_bitrate_kbps(width: u32, height: u32, fps: u32) -> u32 {
    let pixels = width as u64 * height as u64 * fps as u64;
    ((pixels / 1000) as u32).clamp(2000, 30000)
}

#[cfg(test)]
mod tests {
    #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
    use super::I420Source;
    use super::fit_stream_dims;
    #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
    use openh264::formats::YUVSource;

    #[test]
    fn ultrawide_fits_macroblock_grid() {
        let (w, h) = fit_stream_dims(3440, 1440);
        assert_eq!(w, 1920);
        assert_eq!(h, 816);
        assert_eq!(w % 16, 0);
        assert_eq!(h % 16, 0);
    }

    #[test]
    fn hd1080_rounds_height_up_to_1088() {
        let (w, h) = fit_stream_dims(1920, 1080);
        assert_eq!(w, 1920);
        assert_eq!(h, 1088);
    }

    /// Pure red, green and blue must round-trip to the same limited-range YUV
    /// values the openh264 crate's scalar RGB converter produces.
    #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
    #[test]
    fn i420_primaries_match_scalar_converter() {
        let w = 16usize;
        let h = 16usize;
        let colors: [(u8, u8, u8); 3] = [(255, 0, 0), (0, 255, 0), (0, 0, 255)];
        // One solid color per 2x2 block so chroma has a single source color.
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = colors[((y / 2) * (w / 2) + (x / 2)) % colors.len()];
                let i = (y * w + x) * 4;
                rgba[i..i + 4].copy_from_slice(&[r, g, b, 255]);
            }
        }
        let mut src = I420Source::new(w, h);
        src.fill_rgba(&rgba, w, h);

        // Expected values from openh264's write_yuv_scalar formulas.
        let scalar = |rgb: (u8, u8, u8)| -> (u8, u8, u8) {
            let (r, g, b) = (f32::from(rgb.0), f32::from(rgb.1), f32::from(rgb.2));
            let y =
                (0.09765625f32.mul_add(b, 0.2578125f32.mul_add(r, 0.50390625 * g)) + 16.0) as u8;
            let u =
                (0.4375f32.mul_add(b, (-0.1484375f32).mul_add(r, -0.2890625 * g)) + 128.0) as u8;
            let v =
                ((-0.0703125f32).mul_add(b, 0.4375f32.mul_add(r, -0.3671875 * g)) + 128.0) as u8;
            (y, u, v)
        };

        let y = src.y();
        let u = src.u();
        let v = src.v();
        for y0 in 0..h {
            for x0 in 0..w {
                let (r, g, b) = colors[((y0 / 2) * (w / 2) + (x0 / 2)) % colors.len()];
                let (ey, _, _) = scalar((r, g, b));
                let got = y[y0 * w + x0];
                assert!(
                    got.abs_diff(ey) <= 1,
                    "Y mismatch at ({x0},{y0}) for {:?}: {got} vs {ey}",
                    (r, g, b)
                );
            }
        }
        for by in 0..h / 2 {
            for bx in 0..w / 2 {
                let (r, g, b) = colors[(by * (w / 2) + bx) % colors.len()];
                let (_, eu, ev) = scalar((r, g, b));
                let gu = u[by * (w / 2) + bx];
                let gv = v[by * (w / 2) + bx];
                assert!(
                    gu.abs_diff(eu) <= 1,
                    "U mismatch for {:?}: {gu} vs {eu}",
                    (r, g, b)
                );
                assert!(
                    gv.abs_diff(ev) <= 1,
                    "V mismatch for {:?}: {gv} vs {ev}",
                    (r, g, b)
                );
            }
        }
    }

    /// Padding below the visible height must be limited-range black (Y=16, U=V=128).
    #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
    #[test]
    fn i420_pads_bottom_black() {
        let (w, h) = (16usize, 16usize);
        let rgba = vec![200u8; w * 10 * 4];
        let mut src = I420Source::new(w, h);
        src.fill_rgba(&rgba, w, 10);
        let y = src.y();
        for row in 10..h {
            assert!(y[row * w..(row + 1) * w].iter().all(|&p| p == 16));
        }
        for row in 5..h / 2 {
            assert!(
                src.u()[row * (w / 2)..(row + 1) * (w / 2)]
                    .iter()
                    .all(|&p| p == 128)
            );
        }
    }
}
