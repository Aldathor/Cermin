#![cfg(feature = "software-encode-source")]

//! CPU-only responsive-HLS test for `SoftwareEncoder::new_exact`.
//!
//! A real OpenH264 source encoder codes a non-macroblock even geometry
//! (96x54: OpenH264 pads to 96x64 internally and SPS-crops back to 96x54).
//! 121 access units are muxed into real MPEG-TS segments with
//! `HlsProfile::Responsive`, published on a fake wall clock until the store is
//! ready, then every sealed segment is demuxed independently and decoded with
//! a fresh OpenH264 decoder. HTTP serving is deliberately out of scope here:
//! the stable-profile pipeline test covers the HTTP path, so this test can
//! assert segment-level exact geometry, PTS/PES completeness and real
//! inter-frame motion without a server.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use openh264::decoder::{DecodedYUV, Decoder};
use openh264::formats::YUVSource;
use rotten_cast::hls::{HlsMuxer, HlsProfile, HlsStore, Segment};
use rotten_video::{Encoder as _, SoftwareEncoder};

const WIDTH: u32 = 96;
const HEIGHT: u32 = 54;
const FPS: u32 = 30;
const BITRATE_KBPS: u32 = 2000;
/// Forced IDR cadence: the responsive profile's 500 ms segment at 30 fps.
const IDR_PERIOD: usize = 15;
/// Eight sealed segments (eight IDR seals) need the ninth IDR to start as the
/// next unsealed segment, so `FRAMES = 8 * IDR_PERIOD + 1`.
const FRAMES: usize = 8 * IDR_PERIOD + 1;
const SEGMENTS: usize = 8;
const PTS_OFFSET_90KHZ: u64 = 90_000;
const PTS_33BIT_MASK: u64 = (1 << 33) - 1;
const TS_PACKET_SIZE: usize = 188;

const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

/// High-contrast marker painted into the generated input frames (test-only
/// shaping): a bright square that moves every frame.
const MARKER_SIZE: usize = 8;
const MARKER_X_STEP: usize = 4;
const MARKER_Y_STEP: usize = 3;
/// Minimum Y delta that counts as a real picture change rather than codec noise.
const MOTION_PIXEL_DELTA: u8 = 32;
/// A frame transition counts as changed when at least this many Y pixels move
/// by more than `MOTION_PIXEL_DELTA`.
const MOTION_MIN_CHANGED_PIXELS: usize = 20;
/// Of the 14 inter-frame transitions inside one sealed segment at least this
/// many must show real motion; repeated IDRs or stale pictures score ~0.
const MOTION_MIN_CHANGED_TRANSITIONS: usize = 10;

#[test]
#[cfg_attr(
    feature = "software-encode-dll",
    ignore = "requires the official OpenH264 DLL"
)]
fn responsive_exact_segments_decode_geometry_pts_and_motion() {
    let encoded = encode_pipeline();
    assert_eq!(
        encoded.segments.len(),
        SEGMENTS,
        "121 frames at a 15-frame IDR period must seal eight segments"
    );
    assert!(
        encoded.store.ready(),
        "the responsive store must be ready after the advertised four seconds"
    );
    let advertised: f64 = encoded
        .segments
        .iter()
        .map(|segment| segment.duration)
        .sum();
    assert!(
        (advertised - 4.0).abs() < 1e-9,
        "eight 500 ms segments must advertise four seconds, got {advertised}"
    );

    let mut all_packets = Vec::new();
    let mut expected_au = 0usize;
    let mut decoded_frames = 0usize;
    for (index, segment) in encoded.segments.iter().enumerate() {
        assert_eq!(
            segment.data.len() % TS_PACKET_SIZE,
            0,
            "segment {index} must be whole MPEG-TS packets"
        );
        let packets = parse_ts_packets(&segment.data);
        let video_pid = video_pid_from_psi(&packets);
        let access_units = demux_access_units(&packets, video_pid);
        assert_eq!(
            access_units.len(),
            IDR_PERIOD,
            "segment {index} must carry exactly one {IDR_PERIOD}-frame GOP"
        );
        assert!(
            access_units[0].random_access,
            "segment {index} must start with the random access indicator"
        );
        assert_independent_segment_start(index, &access_units[0]);
        for access_unit in &access_units {
            assert_eq!(
                access_unit.pts,
                expected_pts_ticks(encoded.frame_pts[expected_au]),
                "segment {index} AU {expected_au} must continue the published timeline"
            );
            expected_au += 1;
            assert_access_unit_payload(index, access_unit);
        }
        decoded_frames += decode_segment(index, &access_units);
        all_packets.extend(packets);
    }
    assert_eq!(
        expected_au,
        FRAMES - 1,
        "every sealed access unit must be served once"
    );
    assert_eq!(
        decoded_frames,
        FRAMES - 1,
        "every sealed access unit must decode"
    );
    // Continuity counters span segment boundaries: concatenating the sealed
    // segments must be gapless per PID.
    assert_continuity(&all_packets);
}

struct EncodeResult {
    segments: Vec<Segment>,
    frame_pts: Vec<u64>,
    store: HlsStore,
}

/// Paints a flat dark background plus one bright square whose position changes
/// every frame. Only the test input is shaped.
fn paint_motion_pattern(rgba: &mut [u8], index: usize) {
    debug_assert_eq!(rgba.len(), (WIDTH * HEIGHT * 4) as usize);
    for pixel in rgba.as_chunks_mut::<4>().0 {
        *pixel = [32, 32, 32, 255];
    }
    let width = WIDTH as usize;
    let x0 = (index * MARKER_X_STEP) % (width - MARKER_SIZE + 1);
    let y0 = (index * MARKER_Y_STEP) % (HEIGHT as usize - MARKER_SIZE + 1);
    for y in y0..y0 + MARKER_SIZE {
        for x in x0..x0 + MARKER_SIZE {
            let offset = (y * width + x) * 4;
            rgba[offset..offset + 4].copy_from_slice(&[235, 235, 235, 255]);
        }
    }
}

/// Encodes real access units, muxes them with the responsive profile and
/// publishes every sealed segment on a fake monotonic clock.
fn encode_pipeline() -> EncodeResult {
    let profile = HlsProfile::Responsive;
    let mut encoder =
        SoftwareEncoder::new_exact(WIDTH, HEIGHT, BITRATE_KBPS, FPS).expect("exact 96x54 encoder");
    let mut muxer = HlsMuxer::with_profile(profile, false);
    let mut store = HlsStore::with_profile(profile, 16_000_000).expect("responsive HLS store");
    let mut segments = Vec::new();
    let mut frame_pts = Vec::with_capacity(FRAMES);
    // Fake monotonic wall clock: every sealed segment advances it by its own
    // duration so readiness is reached offline without sleeping. The muxer's
    // 500 ms segments make it match the responsive snapshot interval.
    let clock_epoch = Instant::now();
    let mut clock_elapsed = Duration::ZERO;

    for index in 0..FRAMES {
        if index % IDR_PERIOD == 0 {
            encoder.force_keyframe();
        }
        let mut rgba = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
        paint_motion_pattern(&mut rgba, index);
        let pts_us = index as u64 * 1_000_000 / u64::from(FPS);
        frame_pts.push(pts_us);
        let frame = encoder
            .encode(&rgba, WIDTH, HEIGHT, pts_us)
            .expect("exact software encode")
            .expect("every frame must produce a bitstream");
        assert_eq!(frame.display_width, WIDTH);
        assert_eq!(frame.display_height, HEIGHT);
        assert_eq!(frame.coded_width, WIDTH.div_ceil(16) * 16);
        assert_eq!(frame.coded_height, HEIGHT.div_ceil(16) * 16);
        if let Some(segment) = muxer.push(&frame.data, pts_us).expect("mux access unit") {
            let codec = muxer.codec().expect("SPS seen before the first seal");
            clock_elapsed += Duration::from_secs_f64(segment.duration);
            store
                .publish_at(segment.clone(), codec, clock_epoch + clock_elapsed)
                .expect("publish sealed segment");
            segments.push(segment);
        }
    }

    EncodeResult {
        segments,
        frame_pts,
        store,
    }
}

/// 90 kHz PTS as it must appear in the MPEG-TS for a microsecond timestamp.
fn expected_pts_ticks(pts_us: u64) -> u64 {
    (((pts_us as u128) * 9 / 100) as u64 + PTS_OFFSET_90KHZ) & PTS_33BIT_MASK
}

// --- independent MPEG-TS demuxing -------------------------------------------

struct TsPacket {
    pid: u16,
    payload_unit_start: bool,
    random_access: bool,
    continuity: u8,
    payload: Vec<u8>,
}

fn parse_ts_packets(data: &[u8]) -> Vec<TsPacket> {
    let (chunks, remainder) = data.as_chunks::<TS_PACKET_SIZE>();
    assert!(remainder.is_empty(), "TS data must be whole packets");
    let mut packets = Vec::new();
    for packet in chunks {
        assert_eq!(packet[0], 0x47, "TS sync byte");
        let payload_unit_start = packet[1] & 0x40 != 0;
        let pid = (((packet[1] & 0x1F) as u16) << 8) | packet[2] as u16;
        let adaptation_control = (packet[3] >> 4) & 0x03;
        let continuity = packet[3] & 0x0F;
        let mut index = 4;
        let mut random_access = false;
        if adaptation_control & 0x02 != 0 {
            let length = packet[4] as usize;
            assert!(5 + length <= TS_PACKET_SIZE, "adaptation field overruns");
            if length >= 1 {
                random_access = packet[5] & 0x40 != 0;
            }
            index = 5 + length;
        }
        let payload = if adaptation_control & 0x01 != 0 {
            packet[index..].to_vec()
        } else {
            Vec::new()
        };
        packets.push(TsPacket {
            pid,
            payload_unit_start,
            random_access,
            continuity,
            payload,
        });
    }
    packets
}

fn assert_continuity(packets: &[TsPacket]) {
    let mut last: HashMap<u16, u8> = HashMap::new();
    for packet in packets {
        // Adaptation-only packets carry no continuity counter increment.
        if packet.payload.is_empty() {
            continue;
        }
        if let Some(previous) = last.insert(packet.pid, packet.continuity) {
            assert_eq!(
                packet.continuity,
                (previous + 1) & 0x0F,
                "continuity counter gap on PID {:#06x}",
                packet.pid
            );
        }
    }
}

/// Reassembles the PAT/PMT sections with a pointer-field parser.
fn collect_section(packets: &[TsPacket], pid: u16) -> Vec<u8> {
    let mut section: Vec<u8> = Vec::new();
    let mut started = false;
    for packet in packets.iter().filter(|packet| packet.pid == pid) {
        if packet.payload_unit_start {
            assert!(!packet.payload.is_empty(), "empty payload unit start");
            let pointer = packet.payload[0] as usize;
            assert!(pointer < packet.payload.len(), "pointer field overruns");
            section.clear();
            section.extend_from_slice(&packet.payload[1 + pointer..]);
            started = true;
        } else if started {
            section.extend_from_slice(&packet.payload);
        }
        if started && section.len() >= 3 {
            let length = (((section[1] & 0x0F) as usize) << 8) | section[2] as usize;
            if section.len() >= 3 + length {
                section.truncate(3 + length);
                return section;
            }
        }
    }
    panic!("PSI section on PID {pid:#06x} is incomplete");
}

/// Finds the H.264 elementary PID by reading the served PAT and PMT.
fn video_pid_from_psi(packets: &[TsPacket]) -> u16 {
    let pat = collect_section(packets, 0x0000);
    assert!(pat.len() >= 12, "PAT too short");
    assert_eq!(pat[0], 0x00, "PAT table id");
    assert_eq!(&pat[8..10], &[0x00, 0x01], "single program number 1");
    let pmt_pid = (((pat[10] & 0x1F) as u16) << 8) | pat[11] as u16;

    let pmt = collect_section(packets, pmt_pid);
    assert!(pmt.len() >= 15, "PMT too short");
    assert_eq!(pmt[0], 0x02, "PMT table id");
    let pcr_pid = (((pmt[8] & 0x1F) as u16) << 8) | pmt[9] as u16;
    assert_eq!(pmt[12], 0x1B, "H.264 stream type");
    let video_pid = (((pmt[13] & 0x1F) as u16) << 8) | pmt[14] as u16;
    assert_eq!(pcr_pid, video_pid, "PCR must be on the video PID");
    video_pid
}

struct AccessUnit {
    pts: u64,
    random_access: bool,
    /// Complete Annex B access unit from one PES packet.
    body: Vec<u8>,
}

/// Groups video TS payloads into PES packets; each PES is one access unit.
fn demux_access_units(packets: &[TsPacket], video_pid: u16) -> Vec<AccessUnit> {
    let mut units = Vec::new();
    let mut current: Option<(bool, Vec<u8>)> = None;
    for packet in packets.iter().filter(|packet| packet.pid == video_pid) {
        if packet.payload_unit_start {
            if let Some((random_access, bytes)) = current.take() {
                units.push(parse_pes(&bytes, random_access));
            }
            current = Some((packet.random_access, packet.payload.clone()));
        } else if let Some((_, bytes)) = current.as_mut() {
            bytes.extend_from_slice(&packet.payload);
        } else {
            panic!("video PID data before the first payload unit start");
        }
    }
    if let Some((random_access, bytes)) = current {
        units.push(parse_pes(&bytes, random_access));
    }
    units
}

fn parse_pes(data: &[u8], random_access: bool) -> AccessUnit {
    assert!(data.len() >= 14, "PES packet too short");
    assert_eq!(&data[..3], &[0x00, 0x00, 0x01], "PES start code");
    assert_eq!(data[3], 0xE0, "video PES stream id");
    let declared = u16::from_be_bytes([data[4], data[5]]) as usize;
    assert_ne!(declared, 0, "video PES length must be bounded");
    assert_eq!(
        declared,
        data.len() - 6,
        "PES length must match the received payload"
    );
    assert_eq!(data[6] & 0xC0, 0x80, "PES marker bits");
    assert_eq!(data[7] & 0xC0, 0x80, "PES PTS flag");
    assert_eq!(data[8], 5, "PTS-only PES header");
    let bytes = &data[9..14];
    let pts = (((bytes[0] as u64 >> 1) & 0x07) << 30)
        | ((bytes[1] as u64) << 22)
        | ((bytes[2] as u64 >> 1) << 15)
        | ((bytes[3] as u64) << 7)
        | (bytes[4] as u64 >> 1);
    AccessUnit {
        pts,
        random_access,
        body: data[14..].to_vec(),
    }
}

/// Splits Annex B bytes into NAL units, tolerating 3- and 4-byte start codes.
fn annex_b_nals(data: &[u8]) -> Vec<Vec<u8>> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            starts.push(index + 3);
            index += 3;
        } else {
            index += 1;
        }
    }
    let mut nals = Vec::new();
    for (position, &start) in starts.iter().enumerate() {
        let mut end = starts
            .get(position + 1)
            .map(|next| next - 3)
            .unwrap_or(data.len());
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            nals.push(data[start..end].to_vec());
        }
    }
    nals
}

fn assert_independent_segment_start(sequence: usize, access_unit: &AccessUnit) {
    let nals = annex_b_nals(&access_unit.body);
    let types: Vec<u8> = nals.iter().map(|nal| nal[0] & 0x1F).collect();
    assert_eq!(
        types.first(),
        Some(&NAL_AUD),
        "segment {sequence}: AUD must come first"
    );
    assert!(
        types.contains(&NAL_SPS),
        "segment {sequence}: SPS must be repeated for independent decoding"
    );
    assert!(
        types.contains(&NAL_PPS),
        "segment {sequence}: PPS must be repeated for independent decoding"
    );
    assert!(
        types.contains(&NAL_IDR),
        "segment {sequence}: first access unit must be an IDR"
    );
}

fn assert_access_unit_payload(sequence: usize, access_unit: &AccessUnit) {
    let nals = annex_b_nals(&access_unit.body);
    assert!(!nals.is_empty(), "segment {sequence}: empty access unit");
    assert_eq!(
        nals[0][0] & 0x1F,
        NAL_AUD,
        "segment {sequence}: AU must start with an AUD"
    );
    assert_eq!(
        nals.iter().filter(|nal| nal[0] & 0x1F == NAL_AUD).count(),
        1,
        "segment {sequence}: exactly one AUD per AU"
    );
    assert!(
        nals.iter().any(|nal| matches!(nal[0] & 0x1F, 1..=5)),
        "segment {sequence}: AU must contain VCL data"
    );
}

// --- independent per-segment decoding ----------------------------------------

/// Accumulates in-memory Y-plane changes between consecutive decoded frames.
#[derive(Default)]
struct MotionStats {
    previous: Option<Vec<u8>>,
    transitions: usize,
    changed_transitions: usize,
}

impl MotionStats {
    /// Records a decoded frame, copying its Y plane out of the decoder buffer
    /// so the next frame can be compared with the retained one.
    fn observe_frame(&mut self, frame: &DecodedYUV<'_>) {
        let (stride, _, _) = frame.strides();
        let y = frame.y();
        let width = WIDTH as usize;
        let height = HEIGHT as usize;
        let mut plane = Vec::with_capacity(width * height);
        for row in 0..height {
            plane.extend_from_slice(&y[row * stride..row * stride + width]);
        }
        self.observe_plane(&plane);
    }

    /// Records one row-major Y plane and classifies the transition from the
    /// previous frame as a real change when enough pixels jump by more than
    /// `MOTION_PIXEL_DELTA`. A repeated or stale picture scores zero.
    fn observe_plane(&mut self, plane: &[u8]) {
        assert_eq!(
            plane.len(),
            (WIDTH * HEIGHT) as usize,
            "Y plane must cover the whole frame"
        );
        if let Some(previous) = &self.previous {
            let changed = previous
                .iter()
                .zip(plane)
                .filter(|(before, after)| before.abs_diff(**after) > MOTION_PIXEL_DELTA)
                .count();
            self.transitions += 1;
            if changed >= MOTION_MIN_CHANGED_PIXELS {
                self.changed_transitions += 1;
            }
        }
        self.previous = Some(plane.to_vec());
    }
}

/// Decodes every access unit of one segment with a decoder created fresh for
/// that segment, proving the segment is independently decodable and that the
/// exact geometry and the moving marker survive.
fn decode_segment(sequence: usize, access_units: &[AccessUnit]) -> usize {
    let mut decoder = Decoder::new().expect("OpenH264 decoder from source");
    let mut motion = MotionStats::default();
    let mut frames = 0usize;
    for access_unit in access_units {
        let decoded = decoder.decode(&access_unit.body).unwrap_or_else(|error| {
            panic!(
                "segment {sequence}: decoder rejected AU at PTS {}: {error}",
                access_unit.pts
            )
        });
        if let Some(frame) = decoded {
            assert_decoded_frame(&frame, sequence);
            motion.observe_frame(&frame);
            frames += 1;
        }
    }
    for frame in decoder
        .flush_remaining()
        .expect("decoder flush after the segment")
    {
        assert_decoded_frame(&frame, sequence);
        motion.observe_frame(&frame);
        frames += 1;
    }
    assert_eq!(
        frames, IDR_PERIOD,
        "segment {sequence} must decode {IDR_PERIOD} frames"
    );
    assert_eq!(
        motion.transitions,
        IDR_PERIOD - 1,
        "segment {sequence}: {frames} decoded frames must yield {} inter-frame transitions",
        IDR_PERIOD - 1
    );
    assert!(
        motion.changed_transitions >= MOTION_MIN_CHANGED_TRANSITIONS,
        "segment {sequence}: only {} of {} decoded frame transitions show real marker motion; \
         the stream repeated IDRs or served stale pictures",
        motion.changed_transitions,
        motion.transitions
    );
    frames
}

fn assert_decoded_frame(frame: &DecodedYUV<'_>, sequence: usize) {
    assert_eq!(
        frame.dimensions(),
        (WIDTH as usize, HEIGHT as usize),
        "segment {sequence} decoded the wrong geometry (exact 96x54 expected, not the 96x64 pad)"
    );
    let (stride, _, _) = frame.strides();
    let y = frame.y();
    let mut minimum = u8::MAX;
    let mut maximum = 0u8;
    for row in 0..HEIGHT as usize {
        let pixels = &y[row * stride..row * stride + WIDTH as usize];
        minimum = minimum.min(*pixels.iter().min().expect("row pixels"));
        maximum = maximum.max(*pixels.iter().max().expect("row pixels"));
    }
    assert!(
        maximum - minimum > 20,
        "segment {sequence} decoded a flat frame (Y {minimum}..{maximum})"
    );
}

/// Demonstrates that the temporal metric rejects exactly what a repeated-IDR,
/// stale-picture or keyframe-only regression produces: identical Y planes for
/// every decoded frame of a segment.
#[test]
fn motion_metric_scores_static_frames_as_negative() {
    let stale = vec![64u8; (WIDTH * HEIGHT) as usize];
    let mut motion = MotionStats::default();
    for _ in 0..IDR_PERIOD {
        motion.observe_plane(&stale);
    }
    assert_eq!(motion.transitions, IDR_PERIOD - 1);
    assert_eq!(
        motion.changed_transitions, 0,
        "identical consecutive frames must never count as motion"
    );
    assert!(
        motion.changed_transitions < MOTION_MIN_CHANGED_TRANSITIONS,
        "a stale stream must fail the per-segment motion assertion"
    );
}
