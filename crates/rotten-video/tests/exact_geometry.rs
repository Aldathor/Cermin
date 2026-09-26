#![cfg(feature = "software-encode-source")]

//! CPU-only geometry tests for `SoftwareEncoder::new_exact` (true visible size).
//!
//! Every roundtrip encodes with a real OpenH264 encoder and decodes with a real
//! OpenH264 decoder. Exact mode feeds the visible size as the coded source
//! size; OpenH264 pads the macroblock grid internally and writes SPS frame
//! cropping, so the decoder must report the visible size (1920x1080, never
//! 1088/1072) and the right/bottom visible pixels must survive. The legacy
//! AirPlay path must keep coding 1080 as 1088.

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use rotten_video::{EncodedFrame, Encoder as _, SoftwareEncoder};

const BITRATE_KBPS: u32 = 4000;
const FPS: u32 = 30;
const BACKGROUND: [u8; 4] = [32, 32, 32, 255];
const MARKER: [u8; 4] = [235, 235, 235, 255];
/// Side length of the bright square pinned to the bottom-right corner.
const MARKER_EDGE: usize = 16;

/// Flat dark frame with a bright marker flush with the bottom-right visible
/// corner, so a wrong crop either changes the decoded size or cuts the marker
/// off. `MARKER` luma is about 218, `BACKGROUND` about 44.
fn frame_with_corner_marker(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut rgba = vec![0u8; w * h * 4];
    for pixel in rgba.as_chunks_mut::<4>().0 {
        *pixel = BACKGROUND;
    }
    for y in h - MARKER_EDGE..h {
        for x in w - MARKER_EDGE..w {
            let offset = (y * w + x) * 4;
            rgba[offset..offset + 4].copy_from_slice(&MARKER);
        }
    }
    rgba
}

fn encode_frame(
    encoder: &mut SoftwareEncoder,
    width: u32,
    height: u32,
    pts_us: u64,
) -> EncodedFrame {
    let rgba = frame_with_corner_marker(width, height);
    encoder
        .encode(&rgba, width, height, pts_us)
        .expect("exact encode must not error")
        .expect("every frame must produce a bitstream")
}

/// Checks the `EncodedFrame` metadata, then decodes the access unit and checks
/// the visible dimensions and that the corner marker survived right/bottom.
fn assert_visible_roundtrip(frame: &EncodedFrame, width: u32, height: u32, pts_us: u64) {
    assert_eq!(frame.pts_us, pts_us, "timestamps must survive encode");
    assert_eq!(frame.display_width, width);
    assert_eq!(frame.display_height, height);
    assert_eq!(frame.coded_width, width.div_ceil(16) * 16);
    assert_eq!(frame.coded_height, height.div_ceil(16) * 16);
    assert!(frame.is_keyframe, "the first frame must be an IDR");
    assert!(!frame.data.is_empty(), "the bitstream must not be empty");

    let mut decoder = Decoder::new().expect("OpenH264 decoder from source");
    let decoded = decoder
        .decode(&frame.data)
        .expect("decoder must accept the access unit")
        .expect("the IDR must decode");
    assert_eq!(
        decoded.dimensions(),
        (width as usize, height as usize),
        "exact mode must decode the visible {width}x{height}, not the padded coded size"
    );

    let (stride, _, _) = decoded.strides();
    let rows = decoded.y();
    let luma = |px: usize, py: usize| rows[py * stride + px];
    let (w, h) = (width as usize, height as usize);
    for (x, y) in [
        (w - 1, h - 1),
        (w - 1, h - 8),
        (w - 8, h - 1),
        (w - 8, h - 8),
    ] {
        assert!(
            luma(x, y) > 140,
            "the corner marker must survive at the last visible pixel ({x},{y}): Y={}",
            luma(x, y)
        );
    }
    assert!(
        luma(0, 0) < 90,
        "the top-left must stay dark, got Y={}",
        luma(0, 0)
    );
    assert!(
        luma(w / 2, h / 2) < 90,
        "the center must stay dark, got Y={}",
        luma(w / 2, h / 2)
    );
}

#[test]
#[cfg_attr(
    feature = "software-encode-dll",
    ignore = "requires the official OpenH264 DLL"
)]
fn exact_1920x1080_decodes_visible_1080_not_1088() {
    let (width, height) = (1920, 1080);
    let mut encoder =
        SoftwareEncoder::new_exact(width, height, BITRATE_KBPS, FPS).expect("exact 1080p encoder");
    let frame = encode_frame(&mut encoder, width, height, 33_333);
    assert_visible_roundtrip(&frame, width, height, 33_333);
}

#[test]
#[cfg_attr(
    feature = "software-encode-dll",
    ignore = "requires the official OpenH264 DLL"
)]
fn exact_854x480_decodes_visible_non_macroblock_size() {
    let (width, height) = (854, 480);
    let mut encoder = SoftwareEncoder::new_exact(width, height, BITRATE_KBPS, FPS)
        .expect("exact 854x480 encoder");
    let frame = encode_frame(&mut encoder, width, height, 1_234_567);
    assert_visible_roundtrip(&frame, width, height, 1_234_567);
}

/// The smallest native frame, with the boundary rates, must construct and
/// decode at its actual size (16x16 is already macroblock aligned, so this
/// also covers the exact mode with no SPS crop).
#[test]
#[cfg_attr(
    feature = "software-encode-dll",
    ignore = "requires the official OpenH264 DLL"
)]
fn exact_16x16_minimum_constructs_and_decodes() {
    let (width, height) = (16u32, 16u32);
    let mut encoder = SoftwareEncoder::new_exact(width, height, 500, 60)
        .expect("16x16 at the native size/rate bounds must construct");

    // Left half dark, right half bright so a wrong crop or scale is visible.
    let (w, h) = (width as usize, height as usize);
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let pixel = if x >= w / 2 { MARKER } else { BACKGROUND };
            let offset = (y * w + x) * 4;
            rgba[offset..offset + 4].copy_from_slice(&pixel);
        }
    }

    let frame = encoder
        .encode(&rgba, width, height, 7)
        .expect("exact encode must not error")
        .expect("every frame must produce a bitstream");
    assert_eq!(frame.pts_us, 7);
    assert_eq!(frame.display_width, 16);
    assert_eq!(frame.display_height, 16);
    assert_eq!(frame.coded_width, 16);
    assert_eq!(frame.coded_height, 16);
    assert!(frame.is_keyframe, "the first frame must be an IDR");

    let mut decoder = Decoder::new().expect("OpenH264 decoder from source");
    let decoded = decoder
        .decode(&frame.data)
        .expect("decoder must accept the access unit")
        .expect("the IDR must decode");
    assert_eq!(
        decoded.dimensions(),
        (16, 16),
        "the smallest exact frame must decode at its actual size"
    );
    let (stride, _, _) = decoded.strides();
    let rows = decoded.y();
    let luma = |px: usize, py: usize| rows[py * stride + px];
    for y in 0..h {
        for x in 0..4 {
            assert!(luma(x, y) < 90, "left half must stay dark at ({x},{y})");
        }
        for x in 12..16 {
            assert!(luma(x, y) > 140, "right half must stay bright at ({x},{y})");
        }
    }
}

/// The AirPlay path must be untouched: `new` still macroblock-aligns, so a
/// 1080-line source is coded (and presented) as 1088.
#[test]
#[cfg_attr(
    feature = "software-encode-dll",
    ignore = "requires the official OpenH264 DLL"
)]
fn legacy_1920x1080_still_codes_1088() {
    let (width, height) = (1920, 1080);
    let mut encoder =
        SoftwareEncoder::new(width, height, BITRATE_KBPS, FPS).expect("legacy 1080p encoder");
    let rgba = frame_with_corner_marker(width, height);
    let frame = encoder
        .encode(&rgba, width, height, 66_666)
        .expect("legacy encode must not error")
        .expect("every frame must produce a bitstream");
    assert_eq!(frame.pts_us, 66_666);
    assert_eq!(frame.display_width, 1920);
    assert_eq!(frame.display_height, 1080);
    assert_eq!(frame.coded_width, 1920);
    assert_eq!(frame.coded_height, 1088);

    let mut decoder = Decoder::new().expect("OpenH264 decoder from source");
    let decoded = decoder
        .decode(&frame.data)
        .expect("decoder must accept the access unit")
        .expect("the IDR must decode");
    assert_eq!(
        decoded.dimensions(),
        (1920, 1088),
        "the legacy AirPlay path must keep coding 1080 lines as 1088"
    );
}

#[test]
fn exact_constructor_rejects_odd_zero_small_and_oversized_dimensions() {
    for (width, height) in [
        (15, 1080),
        (1920, 1081),
        (0, 1080),
        (1920, 0),
        (1, 1),
        (14, 1080),
        (1920, 14),
        (2, 2),
        (1922, 1080),
        (1920, 1082),
        (0, 0),
    ] {
        let Err(error) = SoftwareEncoder::new_exact(width, height, BITRATE_KBPS, FPS) else {
            panic!("{width}x{height} must be rejected");
        };
        let message = error.to_string();
        assert!(
            message.contains("exact encoder width") || message.contains("exact encoder height"),
            "unexpected error for {width}x{height}: {message}"
        );
    }
}

#[test]
fn exact_constructor_rejects_out_of_range_rates() {
    for (bitrate_kbps, fps) in [
        (BITRATE_KBPS, 0),
        (BITRATE_KBPS, 61),
        (BITRATE_KBPS, u32::MAX),
        (0, FPS),
        (499, FPS),
        (u32::MAX, FPS),
    ] {
        let Err(error) = SoftwareEncoder::new_exact(1920, 1080, bitrate_kbps, fps) else {
            panic!("{bitrate_kbps} kbps at {fps} fps must be rejected");
        };
        let message = error.to_string();
        assert!(
            message.contains("exact encoder fps") || message.contains("exact encoder bitrate"),
            "unexpected error for {bitrate_kbps} kbps at {fps} fps: {message}"
        );
    }
}

#[test]
#[cfg_attr(
    feature = "software-encode-dll",
    ignore = "requires the official OpenH264 DLL"
)]
fn exact_encode_rejects_incomplete_rgba_buffer() {
    let (width, height) = (64, 48);
    let mut encoder =
        SoftwareEncoder::new_exact(width, height, BITRATE_KBPS, FPS).expect("exact 64x48 encoder");
    let full_len = width as usize * height as usize * 4;

    let short = vec![0u8; full_len - 4];
    let Err(error) = encoder.encode(&short, width, height, 0) else {
        panic!("a short RGBA buffer must be rejected");
    };
    assert!(error.to_string().contains("RGBA"), "{error}");

    let long = vec![0u8; full_len + 4];
    assert!(
        encoder.encode(&long, width, height, 0).is_err(),
        "a buffer larger than the frame must be rejected"
    );
}

#[test]
#[cfg_attr(
    feature = "software-encode-dll",
    ignore = "requires the official OpenH264 DLL"
)]
fn exact_encode_rejects_resolution_change() {
    let (width, height) = (64, 48);
    let mut encoder =
        SoftwareEncoder::new_exact(width, height, BITRATE_KBPS, FPS).expect("exact 64x48 encoder");
    let rgba = frame_with_corner_marker(width, height);
    encoder
        .encode(&rgba, width, height, 0)
        .expect("first encode must not error")
        .expect("first frame must produce a bitstream");

    let (other_w, other_h) = (48, 64);
    let other = frame_with_corner_marker(other_w, other_h);
    let Err(error) = encoder.encode(&other, other_w, other_h, 33_333) else {
        panic!("a resolution change must be rejected, not silently resized");
    };
    let message = error.to_string();
    assert!(
        message.contains("64x48") && message.contains("reconnect"),
        "the error must name the fixed geometry and the reconnect remedy: {message}"
    );

    // The fixed geometry keeps working after a rejected change.
    encoder
        .encode(&rgba, width, height, 66_666)
        .expect("same-size encode after rejection must not error")
        .expect("frame must produce a bitstream");
}
