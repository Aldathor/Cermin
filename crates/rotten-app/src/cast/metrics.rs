//! Bounded, in-memory diagnostics for the Cast capture -> encode -> mux producer.
//!
//! Nothing in this module retains or prints frame contents. The pixel-change
//! detector keeps one `u32` sample hash per fixed-grid cell (32x18, independent
//! of the capture resolution) and compares it with the previous frame, so a
//! static desktop shows up as `changed_frames == 0` even while capture and
//! encoding keep up. Only counts and millisecond timings ever leave the
//! process: no sample values, hashes, image bytes, URLs or tokens.
//!
//! Wall time is read once from a single [`Instant`]; the per-stage totals are
//! exclusive accumulators (a measured interval is recorded into exactly one
//! stage), so no duration is counted twice.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use anyhow::bail;

use super::{CastLatency, CastQuality};

/// How often a running producer emits a periodic tracing report.
pub(crate) const REPORT_INTERVAL: Duration = Duration::from_secs(2);

/// Fixed fingerprint grid; independent of the captured resolution.
const SAMPLE_GRID_WIDTH: usize = 32;
const SAMPLE_GRID_HEIGHT: usize = 18;

/// Encoder implementation compiled into this binary.
///
/// `encode-dll` wins when both encoder features are enabled, matching the
/// symbol resolution in `rotten-video`.
pub(crate) fn encoder_kind() -> &'static str {
    #[cfg(feature = "encode-dll")]
    {
        "openh264-dll"
    }
    #[cfg(all(feature = "encode-source", not(feature = "encode-dll")))]
    {
        "openh264-source"
    }
    #[cfg(not(any(feature = "encode-source", feature = "encode-dll")))]
    {
        "none"
    }
}

/// Build profile reported next to the encoder kind.
pub(crate) fn build_profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

/// Total, maximum and sample count for one exclusive pipeline stage.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StageStats {
    /// Sum of every recorded interval, in milliseconds.
    pub total_ms: f64,
    /// Longest recorded interval, in milliseconds.
    pub max_ms: f64,
    /// Number of recorded intervals.
    pub samples: u64,
}

impl StageStats {
    pub(crate) fn record(&mut self, elapsed: Duration) {
        let ms = elapsed.as_secs_f64() * 1000.0;
        self.total_ms += ms;
        self.max_ms = self.max_ms.max(ms);
        self.samples += 1;
    }

    /// Mean stage time in milliseconds; `0.0` when nothing was recorded.
    pub fn mean_ms(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.total_ms / self.samples as f64
        }
    }

    fn brief(&self) -> String {
        format!(
            "{:.1}/{:.1} ms avg/max ({} samples)",
            self.mean_ms(),
            self.max_ms,
            self.samples
        )
    }
}

/// One measured run of the capture -> encode -> HLS mux producer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PipelineSummary {
    /// `encode-dll` wins; otherwise the portable source encoder.
    pub encoder_kind: &'static str,
    /// `debug` or `release` for this binary.
    pub build_profile: &'static str,
    /// Selected quality preset (`balanced` or `high`).
    pub quality: &'static str,
    /// Selected latency preset (`stable` or `responsive`).
    pub latency: &'static str,
    /// Video bitrate the encoder was configured with, in kbps.
    pub target_bitrate_kbps: u32,
    /// HLS target duration of the selected latency profile, in seconds.
    pub hls_target_duration_secs: u64,
    /// Actual capture backend (`dxgi`, `gdi`, `x11`), the synthetic test
    /// source, or `not started` when nothing was opened.
    pub backend_name: &'static str,
    /// Captured frame size, fixed at the first frame (0 when none was taken).
    pub capture_width: u32,
    /// Captured frame height, fixed at the first frame (0 when none was taken).
    pub capture_height: u32,
    /// Encoded stream width, fixed at the first frame (0 when none was taken).
    pub stream_width: u32,
    /// Encoded stream height, fixed at the first frame (0 when none was taken).
    pub stream_height: u32,
    /// Wall-clock seconds from producer start to stop.
    pub wall_secs: f64,
    /// Successful desktop/synthetic grabs.
    pub captured_frames: u64,
    /// Access units returned by the H.264 encoder.
    pub encoded_frames: u64,
    /// Actual IDR NAL units (type 5) found in the encoded access units.
    pub idr_nals: u64,
    /// Segments sealed by the muxer and published into the local store.
    pub sealed_segments: u64,
    /// Frames compared by the fixed-grid fingerprint.
    pub sampled_frames: u64,
    /// Sampled frames whose grid samples differed from the previous frame.
    pub changed_frames: u64,
    /// Timing of each successful grab (the max reveals a stalled backend).
    pub capture: StageStats,
    /// RGBA downscales performed before encoding.
    pub scale: StageStats,
    /// Encoder calls, including OpenH264's internal RGBA -> I420 conversion.
    pub h264: StageStats,
    /// Audio drain/encode passes (always zero samples in a benchmark).
    pub audio: StageStats,
    /// Interleaver flush plus HLS mux/store publication.
    pub mux: StageStats,
    /// Longest gap between consecutive capture timestamps.
    pub max_capture_gap_ms: f64,
}

impl PipelineSummary {
    /// An all-zero summary for the default Balanced/Stable presets, used by
    /// tests and kept as the reference for [`Self::empty_for`].
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self::empty_for(CastQuality::Balanced, CastLatency::Stable)
    }

    /// An all-zero summary that still reports the selected presets, so a
    /// cancelled benchmark names the choices the user asked for.
    pub(crate) fn empty_for(quality: CastQuality, latency: CastLatency) -> Self {
        Self {
            encoder_kind: encoder_kind(),
            build_profile: build_profile(),
            quality: quality.name(),
            latency: latency.name(),
            target_bitrate_kbps: quality.bitrate_kbps(),
            hls_target_duration_secs: latency.hls_profile().target_duration_secs(),
            backend_name: "not started",
            capture_width: 0,
            capture_height: 0,
            stream_width: 0,
            stream_height: 0,
            wall_secs: 0.0,
            captured_frames: 0,
            encoded_frames: 0,
            idr_nals: 0,
            sealed_segments: 0,
            sampled_frames: 0,
            changed_frames: 0,
            capture: StageStats::default(),
            scale: StageStats::default(),
            h264: StageStats::default(),
            audio: StageStats::default(),
            mux: StageStats::default(),
            max_capture_gap_ms: 0.0,
        }
    }

    /// Captured frames per wall-clock second; `0.0` for an empty run.
    pub fn captured_fps(&self) -> f64 {
        self.rate(self.captured_frames)
    }

    /// Encoder outputs per wall-clock second; `0.0` for an empty run.
    pub fn encoded_fps(&self) -> f64 {
        self.rate(self.encoded_frames)
    }

    /// Samples that changed per wall-clock second; `0.0` for an empty run.
    pub fn changed_fps(&self) -> f64 {
        self.rate(self.changed_frames)
    }

    fn rate(&self, count: u64) -> f64 {
        if self.wall_secs.is_finite() && self.wall_secs > 0.0 {
            count as f64 / self.wall_secs
        } else {
            0.0
        }
    }

    /// Compact single-line report for periodic tracing.
    pub fn one_line(&self) -> String {
        format!(
            "preset {}/{} ({} kbps, HLS target {} s); backend {}, {}x{} -> {}x{}; {:.1} fps \
             captured, {:.1} fps encoded, {:.1} fps changed; capture {}; scale {}; h264+I420 {}; \
             audio {}; mux {}; idr {}; sealed {}; max capture gap {:.1} ms; wall {:.1} s",
            self.quality,
            self.latency,
            self.target_bitrate_kbps,
            self.hls_target_duration_secs,
            self.backend_name,
            self.capture_width,
            self.capture_height,
            self.stream_width,
            self.stream_height,
            self.captured_fps(),
            self.encoded_fps(),
            self.changed_fps(),
            self.capture.brief(),
            self.scale.brief(),
            self.h264.brief(),
            self.audio.brief(),
            self.mux.brief(),
            self.idr_nals,
            self.sealed_segments,
            self.max_capture_gap_ms,
            self.wall_secs,
        )
    }

    /// Multi-line report for the final CLI summary.
    pub fn display_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Cast pipeline summary (encoder {}, {} build)",
            self.encoder_kind, self.build_profile
        );
        let _ = writeln!(
            out,
            "  preset: {} quality, {} latency | target {} kbps | HLS target {} s",
            self.quality, self.latency, self.target_bitrate_kbps, self.hls_target_duration_secs
        );
        let _ = writeln!(
            out,
            "  backend {} | capture {}x{} | stream {}x{}",
            self.backend_name,
            self.capture_width,
            self.capture_height,
            self.stream_width,
            self.stream_height
        );
        let _ = writeln!(
            out,
            "  wall {:.2} s | captured {} ({:.1} fps) | encoded {} ({:.1} fps) | IDR NALs {} | \
             sealed segments {}",
            self.wall_secs,
            self.captured_frames,
            self.captured_fps(),
            self.encoded_frames,
            self.encoded_fps(),
            self.idr_nals,
            self.sealed_segments
        );
        let _ = writeln!(
            out,
            "  sampled pixel change: {} of {} frames changed (fixed grid, in memory only)",
            self.changed_frames, self.sampled_frames
        );
        let _ = writeln!(out, "  capture {}", self.capture.brief());
        let _ = writeln!(out, "  scale   {}", self.scale.brief());
        let _ = writeln!(out, "  h264    {}", self.h264.brief());
        let _ = writeln!(out, "  audio   {}", self.audio.brief());
        let _ = writeln!(out, "  mux     {}", self.mux.brief());
        let _ = write!(
            out,
            "  max inter-capture gap {:.1} ms",
            self.max_capture_gap_ms
        );
        out
    }
}

/// Cheap fixed-grid fingerprint of a frame's pixels.
struct Fingerprint {
    current: Vec<u32>,
    previous: Vec<u32>,
    has_previous: bool,
}

impl Fingerprint {
    fn new() -> Self {
        Self {
            current: vec![0; SAMPLE_GRID_WIDTH * SAMPLE_GRID_HEIGHT],
            previous: vec![0; SAMPLE_GRID_WIDTH * SAMPLE_GRID_HEIGHT],
            has_previous: false,
        }
    }

    /// Samples `rgba` on the fixed grid and returns whether the samples differ
    /// from the previous frame. The caller must have validated the frame
    /// first; malformed dimensions or a short buffer are an error here, never
    /// a panic.
    fn record(&mut self, rgba: &[u8], width: u32, height: u32) -> anyhow::Result<bool> {
        if width == 0 || height == 0 {
            bail!("cannot fingerprint a zero-sized frame ({width}x{height})");
        }
        let expected = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4));
        let Some(expected) = expected else {
            bail!("cannot fingerprint a frame with overflowing dimensions ({width}x{height})");
        };
        if u64::try_from(rgba.len()).unwrap_or(u64::MAX) < expected {
            bail!(
                "cannot fingerprint a short RGBA buffer ({} of {expected} bytes) for {width}x{height}",
                rgba.len()
            );
        }

        for grid_y in 0..SAMPLE_GRID_HEIGHT {
            let y = (grid_y as u64 * u64::from(height) / SAMPLE_GRID_HEIGHT as u64) as usize;
            for grid_x in 0..SAMPLE_GRID_WIDTH {
                let x = (grid_x as u64 * u64::from(width) / SAMPLE_GRID_WIDTH as u64) as usize;
                let index = ((y as u64 * u64::from(width) + x as u64) * 4) as usize;
                let cell = grid_y * SAMPLE_GRID_WIDTH + grid_x;
                self.current[cell] = hash_pixel(&rgba[index..index + 4]);
            }
        }

        let changed = self.has_previous && self.current != self.previous;
        std::mem::swap(&mut self.current, &mut self.previous);
        self.has_previous = true;
        Ok(changed)
    }
}

/// Counts Annex B NAL units of type 5 (IDR) in one encoded access unit. Both
/// the 3-byte and 4-byte start code forms are recognized.
pub(crate) fn idr_nal_count(annex_b: &[u8]) -> u64 {
    let mut count = 0u64;
    let mut index = 0usize;
    while index + 3 < annex_b.len() {
        if annex_b[index] == 0 && annex_b[index + 1] == 0 && annex_b[index + 2] == 1 {
            if annex_b[index + 3] & 0x1F == 5 {
                count += 1;
            }
            index += 4;
        } else {
            index += 1;
        }
    }
    count
}

#[inline]
fn hash_pixel(rgba: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for &byte in &rgba[..4] {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Mutable accumulator owned by one blocking producer run.
pub(crate) struct PipelineMetrics {
    started: Instant,
    last_report: Instant,
    quality: CastQuality,
    latency: CastLatency,
    backend_name: &'static str,
    capture_dims: Option<(u32, u32)>,
    stream_dims: Option<(u32, u32)>,
    captured_frames: u64,
    encoded_frames: u64,
    idr_nals: u64,
    sealed_segments: u64,
    sampled_frames: u64,
    changed_frames: u64,
    capture: StageStats,
    scale: StageStats,
    h264: StageStats,
    audio: StageStats,
    mux: StageStats,
    last_capture_pts_us: Option<u64>,
    max_capture_gap_us: u64,
    fingerprint: Fingerprint,
}

impl PipelineMetrics {
    pub(crate) fn new(quality: CastQuality, latency: CastLatency) -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last_report: now,
            quality,
            latency,
            backend_name: "not started",
            capture_dims: None,
            stream_dims: None,
            captured_frames: 0,
            encoded_frames: 0,
            idr_nals: 0,
            sealed_segments: 0,
            sampled_frames: 0,
            changed_frames: 0,
            capture: StageStats::default(),
            scale: StageStats::default(),
            h264: StageStats::default(),
            audio: StageStats::default(),
            mux: StageStats::default(),
            last_capture_pts_us: None,
            max_capture_gap_us: 0,
            fingerprint: Fingerprint::new(),
        }
    }

    /// Records which capture backend actually produced the frames.
    pub(crate) fn set_backend(&mut self, backend_name: &'static str) {
        self.backend_name = backend_name;
    }

    /// Records the fixed source and stream dimensions established by the
    /// first captured frame.
    pub(crate) fn set_source_dims(&mut self, capture: (u32, u32), stream: (u32, u32)) {
        self.capture_dims = Some(capture);
        self.stream_dims = Some(stream);
    }

    /// Records one successful grab. The timestamp is the frame's capture PTS
    /// on the shared session clock, so the gap is not skewed by encode time.
    pub(crate) fn record_capture(&mut self, elapsed: Duration, pts_us: u64) {
        self.capture.record(elapsed);
        self.captured_frames += 1;
        if let Some(last) = self.last_capture_pts_us {
            self.max_capture_gap_us = self.max_capture_gap_us.max(pts_us.saturating_sub(last));
        }
        self.last_capture_pts_us = Some(pts_us);
    }

    pub(crate) fn record_scale(&mut self, elapsed: Duration) {
        self.scale.record(elapsed);
    }

    pub(crate) fn record_h264(&mut self, elapsed: Duration) {
        self.h264.record(elapsed);
    }

    pub(crate) fn record_audio(&mut self, elapsed: Duration) {
        self.audio.record(elapsed);
    }

    pub(crate) fn record_mux(&mut self, elapsed: Duration) {
        self.mux.record(elapsed);
    }

    /// Samples a validated frame before scaling or encoding.
    pub(crate) fn record_sampled_frame(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> anyhow::Result<()> {
        self.sampled_frames += 1;
        if self.fingerprint.record(rgba, width, height)? {
            self.changed_frames += 1;
        }
        Ok(())
    }

    /// Records an encoder output. IDR NALs are counted from the real Annex B
    /// bytes, not from the encoder's keyframe flag.
    pub(crate) fn record_encoded_frame(&mut self, annex_b: &[u8]) {
        self.encoded_frames += 1;
        self.idr_nals += idr_nal_count(annex_b);
    }

    pub(crate) fn record_sealed_segment(&mut self) {
        self.sealed_segments += 1;
    }

    /// Snapshot for the final result (or an early clean stop).
    pub(crate) fn finish(&self) -> PipelineSummary {
        self.snapshot()
    }

    /// Snapshot due only after [`REPORT_INTERVAL`]; `None` otherwise.
    pub(crate) fn take_periodic(&mut self) -> Option<PipelineSummary> {
        let now = Instant::now();
        if now.duration_since(self.last_report) < REPORT_INTERVAL {
            return None;
        }
        self.last_report = now;
        Some(self.snapshot())
    }

    fn snapshot(&self) -> PipelineSummary {
        PipelineSummary {
            encoder_kind: encoder_kind(),
            build_profile: build_profile(),
            quality: self.quality.name(),
            latency: self.latency.name(),
            target_bitrate_kbps: self.quality.bitrate_kbps(),
            hls_target_duration_secs: self.latency.hls_profile().target_duration_secs(),
            backend_name: self.backend_name,
            capture_width: self.capture_dims.map_or(0, |dims| dims.0),
            capture_height: self.capture_dims.map_or(0, |dims| dims.1),
            stream_width: self.stream_dims.map_or(0, |dims| dims.0),
            stream_height: self.stream_dims.map_or(0, |dims| dims.1),
            wall_secs: self.started.elapsed().as_secs_f64(),
            captured_frames: self.captured_frames,
            encoded_frames: self.encoded_frames,
            idr_nals: self.idr_nals,
            sealed_segments: self.sealed_segments,
            sampled_frames: self.sampled_frames,
            changed_frames: self.changed_frames,
            capture: self.capture,
            scale: self.scale,
            h264: self.h264,
            audio: self.audio,
            mux: self.mux,
            max_capture_gap_ms: self.max_capture_gap_us as f64 / 1000.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, value: u8) -> Vec<u8> {
        vec![value; (width * height * 4) as usize]
    }

    #[test]
    fn stage_stats_track_total_max_and_a_zero_sample_mean() {
        let mut stats = StageStats::default();
        assert_eq!(stats.mean_ms(), 0.0);
        stats.record(Duration::from_millis(10));
        stats.record(Duration::from_millis(30));
        assert_eq!(stats.samples, 2);
        assert!((stats.total_ms - 40.0).abs() < 1e-9);
        assert!((stats.max_ms - 30.0).abs() < 1e-9);
        assert!((stats.mean_ms() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn summary_rates_are_zero_for_an_empty_run_and_finite_when_timed() {
        let empty = PipelineSummary::empty();
        assert_eq!(empty.captured_fps(), 0.0);
        assert_eq!(empty.encoded_fps(), 0.0);
        assert_eq!(empty.changed_fps(), 0.0);
        assert!(!empty.one_line().contains("NaN"));
        assert!(!empty.display_text().contains("NaN"));

        let timed = PipelineSummary {
            wall_secs: 10.0,
            captured_frames: 300,
            encoded_frames: 250,
            changed_frames: 5,
            sampled_frames: 300,
            ..PipelineSummary::empty()
        };
        assert!((timed.captured_fps() - 30.0).abs() < 1e-9);
        assert!((timed.encoded_fps() - 25.0).abs() < 1e-9);
        assert!((timed.changed_fps() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn source_identity_is_reported_in_the_summary() {
        let mut metrics = PipelineMetrics::new(CastQuality::Balanced, CastLatency::Stable);
        metrics.set_backend("dxgi");
        metrics.set_source_dims((1920, 1080), (1280, 720));
        let summary = metrics.finish();
        assert_eq!(summary.backend_name, "dxgi");
        assert_eq!(
            (summary.capture_width, summary.capture_height),
            (1920, 1080)
        );
        assert_eq!((summary.stream_width, summary.stream_height), (1280, 720));
        assert_eq!(summary.quality, "balanced");
        assert_eq!(summary.latency, "stable");
        assert_eq!(summary.target_bitrate_kbps, 4000);
        assert_eq!(summary.hls_target_duration_secs, 2);

        let text = summary.display_text();
        assert!(text.contains("dxgi"), "{text}");
        assert!(text.contains("1920x1080"), "{text}");
        assert!(text.contains("1280x720"), "{text}");
        assert!(text.contains(encoder_kind()), "{text}");
        assert!(text.contains("balanced quality, stable latency"), "{text}");
        assert!(
            summary.one_line().contains("balanced/stable"),
            "{}",
            summary.one_line()
        );

        let empty = PipelineSummary::empty();
        assert_eq!((empty.capture_width, empty.capture_height), (0, 0));
        assert_eq!((empty.stream_width, empty.stream_height), (0, 0));
        assert!(empty.display_text().contains("not started"));
    }

    #[test]
    fn preset_identity_is_reported_in_the_summary() {
        let summary = PipelineMetrics::new(CastQuality::High, CastLatency::Responsive).finish();
        assert_eq!(summary.quality, "high");
        assert_eq!(summary.latency, "responsive");
        assert_eq!(summary.target_bitrate_kbps, 8000);
        assert_eq!(summary.hls_target_duration_secs, 1);
        assert!(summary.one_line().contains("high/responsive (8000 kbps"));
        assert!(
            summary
                .display_text()
                .contains("high quality, responsive latency"),
            "{}",
            summary.display_text()
        );

        let empty = PipelineSummary::empty_for(CastQuality::High, CastLatency::Responsive);
        assert_eq!(empty.quality, "high");
        assert_eq!(empty.latency, "responsive");
        assert_eq!(empty.target_bitrate_kbps, 8000);
        assert_eq!(empty.hls_target_duration_secs, 1);
    }

    #[test]
    fn fingerprint_counts_changed_and_unchanged_frames() {
        let mut metrics = PipelineMetrics::new(CastQuality::Balanced, CastLatency::Stable);
        metrics
            .record_sampled_frame(&solid(64, 64, 0), 64, 64)
            .expect("first frame");
        metrics
            .record_sampled_frame(&solid(64, 64, 0), 64, 64)
            .expect("identical frame");
        metrics
            .record_sampled_frame(&solid(64, 64, 200), 64, 64)
            .expect("changed frame");
        let summary = metrics.finish();
        assert_eq!(summary.sampled_frames, 3);
        assert_eq!(summary.changed_frames, 1);
    }

    #[test]
    fn fingerprint_rejects_malformed_frames_without_panicking() {
        let mut fingerprint = Fingerprint::new();
        let error = fingerprint.record(&[], 0, 64).unwrap_err();
        assert!(error.to_string().contains("zero-sized"), "{error}");
        let error = fingerprint.record(&[0u8; 16], 64, 64).unwrap_err();
        assert!(error.to_string().contains("short RGBA"), "{error}");
        let error = fingerprint.record(&[], u32::MAX, u32::MAX).unwrap_err();
        assert!(error.to_string().contains("overflow"), "{error}");
        assert!(fingerprint.record(&solid(64, 64, 0), 64, 64).is_ok());
    }

    #[test]
    fn idr_nal_count_handles_three_and_four_byte_start_codes() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&[0, 0, 1, 0x67]); // SPS, 3-byte start code
        stream.extend_from_slice(&[0, 0, 0, 1, 0x68]); // PPS, 4-byte start code
        stream.extend_from_slice(&[0, 0, 1, 0x41]); // non-IDR slice
        stream.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88]); // IDR, 4-byte
        stream.extend_from_slice(&[0, 0, 1, 0x65, 0x01]); // IDR, 3-byte
        stream.extend_from_slice(&[0, 0, 1, 0x41]); // non-IDR slice
        assert_eq!(idr_nal_count(&stream), 2);
        assert_eq!(idr_nal_count(&[]), 0);
        assert_eq!(idr_nal_count(&[0, 0, 1]), 0);
    }

    #[test]
    fn encoder_kind_reports_the_compiled_features() {
        let kind = encoder_kind();
        #[cfg(feature = "encode-dll")]
        assert_eq!(kind, "openh264-dll");
        #[cfg(all(feature = "encode-source", not(feature = "encode-dll")))]
        assert_eq!(kind, "openh264-source");
        #[cfg(not(any(feature = "encode-source", feature = "encode-dll")))]
        assert_eq!(kind, "none");
        assert!(matches!(build_profile(), "debug" | "release"));
    }

    #[test]
    fn periodic_reports_start_after_two_seconds() {
        assert_eq!(REPORT_INTERVAL, Duration::from_secs(2));
        let mut metrics = PipelineMetrics::new(CastQuality::Balanced, CastLatency::Stable);
        assert!(metrics.take_periodic().is_none());
    }
}
