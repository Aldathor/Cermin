#![cfg(feature = "software-encode-source")]

//! CPU-only end-to-end test of the Cast HLS delivery path.
//!
//! It runs the real components with no display, receiver or runtime DLL:
//! synthetic RGBA frames are encoded by `SoftwareEncoder` (OpenH264 built from
//! source, an IDR forced every 30 frames), muxed into MPEG-TS segments by
//! `HlsMuxer`, stored in `HlsStore`, served by the loopback `HttpServer`,
//! fetched back over plain HTTP with a minimal Tokio client and then demuxed by
//! test-local MPEG-TS/PES/Annex B readers. Every segment is decoded with an
//! OpenH264 decoder created fresh for that segment, which proves the segments
//! are independently decodable and validates dimensions, non-flat pixels, real
//! inter-frame motion (a moving high-contrast marker survives encode/decode),
//! frame counts and timestamp/payload continuity across the HTTP boundary.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openh264::decoder::{DecodedYUV, Decoder};
use openh264::formats::YUVSource;
use rotten_cast::hls::{HlsMuxer, HlsStore, Segment};
use rotten_cast::http::HttpServer;
use rotten_video::{Encoder as _, SoftwareEncoder, SyntheticSource};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const FPS: u32 = 30;
const BITRATE_KBPS: u32 = 2000;
/// Forced IDR cadence: at 30 fps one segment covers exactly one second.
const IDR_PERIOD: usize = 30;
/// Frames pushed: 0..=239 fills eight one-second segments; frame 240's IDR
/// seals the eighth, so the store advertises the full eight-second buffer.
const FRAMES: usize = 241;
/// The muxer offsets PTS by one second so the first PCR stays in range.
const PTS_OFFSET_90KHZ: u64 = 90_000;
const PTS_33BIT_MASK: u64 = (1 << 33) - 1;
const TS_PACKET_SIZE: usize = 188;
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// High-contrast marker painted into the generated input frames (test-only
/// shaping): a bright square that jumps to a new position every frame.
const MARKER_SIZE: usize = 12;
const MARKER_X_STEP: usize = 4;
const MARKER_Y_STEP: usize = 5;
/// Minimum Y delta that counts as a real picture change rather than codec noise.
const MOTION_PIXEL_DELTA: u8 = 32;
/// A frame transition counts as changed when at least this many Y pixels move
/// by more than `MOTION_PIXEL_DELTA`.
const MOTION_MIN_CHANGED_PIXELS: usize = 20;
/// Of the 29 inter-frame transitions inside one 30-AU segment at least this
/// many must show real motion; repeated IDRs or stale pictures score ~0.
const MOTION_MIN_CHANGED_TRANSITIONS: usize = 20;

const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

#[tokio::test]
#[cfg_attr(
    feature = "software-encode-dll",
    ignore = "requires the official OpenH264 DLL"
)]
async fn synthetic_frames_survive_hls_segments_and_decode_independently() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let encoded = encode_pipeline();
        assert!(
            encoded.store.ready(),
            "store must advertise the initial eight-second buffer"
        );

        let store = Arc::new(Mutex::new(encoded.store));
        let mut server = HttpServer::start(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            Arc::clone(&store),
        )
        .await
        .expect("loopback cast HTTP server must start");
        let addr = server.local_addr();

        // Master: actual advertised URL, CORS/length headers and codec string.
        let master_path = path_of(server.url());
        let master = http_get(addr, &master_path).await;
        assert_manifest_response(&master, &encoded.codec);
        let base = &master_path[..=master_path.rfind('/').expect("directory in master URL")];

        // Live: the advertised segment list must match what was published.
        let live = http_get(addr, &format!("{base}live.m3u8")).await;
        assert_manifest_headers(&live);
        let playlist_text = body_text(&live);
        assert!(playlist_text.starts_with("#EXTM3U\n"), "{playlist_text}");
        assert!(
            playlist_text.contains("#EXT-X-VERSION:3"),
            "{playlist_text}"
        );
        assert!(
            playlist_text.contains("#EXT-X-TARGETDURATION:2\n"),
            "{playlist_text}"
        );
        let playlist = parse_live_playlist(&playlist_text);
        assert_eq!(playlist.media_sequence, encoded.segments[0].sequence);
        assert!(playlist.entries.len() >= 8, "{playlist_text}");
        assert_eq!(playlist.entries.len(), encoded.segments.len());
        for (entry, segment) in playlist.entries.iter().zip(&encoded.segments) {
            assert_eq!(entry.sequence, segment.sequence);
            assert!((entry.duration - segment.duration).abs() < 0.001);
        }
        assert!(
            playlist
                .entries
                .windows(2)
                .all(|pair| pair[1].sequence == pair[0].sequence + 1),
            "media sequence numbers must increase monotonically: {playlist_text}"
        );

        // Every advertised segment is fetched, demuxed without the muxer's own
        // helpers and decoded with a decoder created for that segment alone.
        let mut all_packets = Vec::new();
        let mut expected_au = 0usize;
        let mut decoded_frames = 0usize;
        for (index, entry) in playlist.entries.iter().enumerate() {
            let expected = &encoded.segments[index];
            let response = http_get(addr, &format!("{base}{}.ts", entry.sequence)).await;
            let packets = assert_segment_response(&response, expected);
            let video_pid = video_pid_from_psi(&packets);
            let access_units = demux_access_units(&packets, video_pid);
            assert_eq!(
                access_units.len(),
                IDR_PERIOD,
                "segment {} must cover exactly one forced-IDR interval",
                entry.sequence
            );
            assert!(
                access_units[0].random_access,
                "segment {} must start with the random access indicator",
                entry.sequence
            );
            assert_independent_segment_start(entry.sequence, &access_units[0]);
            for access_unit in &access_units {
                let expected_pts = expected_pts_ticks(encoded.frame_pts[expected_au]);
                assert_eq!(
                    access_unit.pts, expected_pts,
                    "segment {} AU {expected_au} must continue the published timeline",
                    entry.sequence
                );
                expected_au += 1;
                assert_access_unit_payload(entry.sequence, access_unit);
            }
            decoded_frames += decode_segment(entry.sequence, &access_units);
            all_packets.extend(packets);
        }
        assert_eq!(
            expected_au,
            FRAMES - 1,
            "every published AU must be served once"
        );
        assert_eq!(decoded_frames, FRAMES - 1, "every AU must decode");
        // Continuity counters span segment boundaries: concatenating the
        // fetched segments must be gapless per PID.
        assert_continuity(&all_packets);

        server.shutdown().await.expect("HTTP shutdown");
        assert!(server.is_finished());
        TcpListener::bind(addr)
            .await
            .expect("HTTP listener must release its port after shutdown");
    })
    .await
    .expect("cast pipeline must finish without wall-clock stalls");
}

struct EncodeResult {
    codec: String,
    segments: Vec<Segment>,
    frame_pts: Vec<u64>,
    store: HlsStore,
}

/// Replaces the generated pixels with a controlled motion pattern: a flat dark
/// background plus one bright square whose position changes every frame. Only
/// the test input is shaped; production `SyntheticSource` is untouched.
fn paint_motion_pattern(rgba: &mut [u8], index: usize) {
    debug_assert_eq!(rgba.len(), (WIDTH * HEIGHT * 4) as usize);
    for pixel in rgba.as_chunks_mut::<4>().0.iter_mut() {
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

/// Encodes synthetic frames, pushes the real Annex B access units through the
/// HLS muxer and publishes every sealed segment to the store.
fn encode_pipeline() -> EncodeResult {
    let mut source = SyntheticSource::new(WIDTH, HEIGHT);
    let mut encoder =
        SoftwareEncoder::new(WIDTH, HEIGHT, BITRATE_KBPS, FPS).expect("OpenH264 source encoder");
    let mut muxer = HlsMuxer::new();
    let mut store = HlsStore::new();
    let mut segments = Vec::new();
    let mut frame_pts = Vec::with_capacity(FRAMES);
    let mut keyframes = Vec::new();
    // Fake monotonic wall clock for the store: every sealed one-second segment
    // advances it by its own duration. Real `publish` timestamps would all be
    // within one wall second, and the store refreshes its advertised snapshot
    // at most once per second, so readiness could never be reached offline
    // without sleeping.
    let clock_epoch = Instant::now();
    let mut clock_elapsed = Duration::ZERO;

    for index in 0..FRAMES {
        if index % IDR_PERIOD == 0 {
            encoder.force_keyframe();
        }
        let (mut rgba, width, height) = source.next_frame().expect("synthetic frame");
        paint_motion_pattern(&mut rgba, index);
        let pts_us = index as u64 * 1_000_000 / u64::from(FPS);
        frame_pts.push(pts_us);
        let frame = encoder
            .encode(&rgba, width, height, pts_us)
            .expect("software encode")
            .expect("every frame must produce a bitstream");
        assert!(!frame.data.is_empty(), "frame {index} must not be empty");
        if frame.is_keyframe {
            keyframes.push(index);
        }
        if let Some(segment) = muxer.push(&frame.data, pts_us).expect("mux access unit") {
            let codec = muxer.codec().expect("SPS seen before the first seal");
            clock_elapsed += Duration::from_secs_f64(segment.duration);
            store
                .publish_at(segment.clone(), codec, clock_epoch + clock_elapsed)
                .expect("publish sealed segment");
            segments.push(segment);
        }
    }

    for index in (0..FRAMES).step_by(IDR_PERIOD) {
        assert!(
            keyframes.contains(&index),
            "forced IDR at frame {index} must be a keyframe"
        );
    }
    assert!(
        segments.len() >= 8,
        "eight sealed one-second segments expected, got {}",
        segments.len()
    );
    let advertised: f64 = segments.iter().map(|segment| segment.duration).sum();
    assert!(
        advertised >= 8.0,
        "the sealed segments must cover at least eight seconds, got {advertised}"
    );
    assert!(
        store.ready(),
        "store must advertise the initial eight-second buffer"
    );
    EncodeResult {
        codec: muxer.codec().expect("codec string").to_owned(),
        segments,
        frame_pts,
        store,
    }
}

/// 90 kHz PTS as it must appear in the MPEG-TS for a microsecond timestamp.
fn expected_pts_ticks(pts_us: u64) -> u64 {
    (((pts_us as u128) * 9 / 100) as u64 + PTS_OFFSET_90KHZ) & PTS_33BIT_MASK
}

// --- minimal HTTP/1.1 client -------------------------------------------------

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn content_length(&self) -> usize {
        self.header("content-length")
            .expect("Content-Length header")
            .parse()
            .expect("numeric Content-Length")
    }
}

/// Simple HTTP/1.1 GET with connect/write/read deadlines; the server closes
/// every connection, so `read_to_end` yields the complete representation.
async fn http_get(addr: SocketAddr, path: &str) -> HttpResponse {
    let mut stream = tokio::time::timeout(HTTP_TIMEOUT, TcpStream::connect(addr))
        .await
        .expect("HTTP connect timed out")
        .expect("HTTP connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    tokio::time::timeout(HTTP_TIMEOUT, stream.write_all(request.as_bytes()))
        .await
        .expect("HTTP write timed out")
        .expect("HTTP write");
    let mut raw = Vec::new();
    tokio::time::timeout(HTTP_TIMEOUT, stream.read_to_end(&mut raw))
        .await
        .expect("HTTP read timed out")
        .expect("HTTP read");
    parse_http_response(&raw)
}

fn parse_http_response(raw: &[u8]) -> HttpResponse {
    let head_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP header terminator")
        + 4;
    let head = std::str::from_utf8(&raw[..head_end]).expect("HTTP head is UTF-8");
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .expect("HTTP status line")
        .split_whitespace()
        .nth(1)
        .expect("HTTP status code")
        .parse()
        .expect("numeric HTTP status");
    let headers = lines
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, value) = line.split_once(':').expect("HTTP header separator");
            (name.trim().to_owned(), value.trim().to_owned())
        })
        .collect();
    HttpResponse {
        status,
        headers,
        body: raw[head_end..].to_vec(),
    }
}

/// Request path (including token) of an advertised loopback URL.
fn path_of(url: &str) -> String {
    let rest = url.strip_prefix("http://").expect("loopback HTTP URL");
    let (_authority, path) = rest.split_once('/').expect("URL path");
    format!("/{path}")
}

fn body_text(response: &HttpResponse) -> String {
    std::str::from_utf8(&response.body)
        .expect("UTF-8 body")
        .to_owned()
}

fn assert_manifest_response(response: &HttpResponse, codec: &str) {
    assert_manifest_headers(response);
    let text = body_text(response);
    assert!(text.starts_with("#EXTM3U\n"), "{text}");
    assert!(text.contains("#EXT-X-VERSION:3"), "{text}");
    assert!(text.contains(&format!("CODECS=\"{codec}\"")), "{text}");
}

fn assert_manifest_headers(response: &HttpResponse) {
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("content-type"),
        Some("application/vnd.apple.mpegurl")
    );
    assert_eq!(
        response.header("cache-control"),
        Some("no-cache, no-store, must-revalidate")
    );
    assert_eq!(response.header("access-control-allow-origin"), Some("*"));
    assert_eq!(
        response.header("access-control-allow-methods"),
        Some("GET, HEAD, OPTIONS")
    );
    assert_eq!(
        response.content_length(),
        response.body.len(),
        "manifest response must not be truncated"
    );
}

fn assert_segment_response(response: &HttpResponse, expected: &Segment) -> Vec<TsPacket> {
    assert_eq!(response.status, 200, "segment {} status", expected.sequence);
    assert_eq!(response.header("content-type"), Some("video/mp2t"));
    assert_eq!(response.header("cache-control"), Some("public, max-age=60"));
    assert_eq!(response.header("access-control-allow-origin"), Some("*"));
    assert_eq!(response.header("accept-ranges"), Some("bytes"));
    assert_eq!(
        response.content_length(),
        response.body.len(),
        "segment {} response must not be incomplete",
        expected.sequence
    );
    assert_eq!(
        response.body, expected.data,
        "segment {} served bytes must match the published segment",
        expected.sequence
    );
    assert_eq!(response.body.len() % TS_PACKET_SIZE, 0);
    parse_ts_packets(&response.body)
}

struct PlaylistEntry {
    duration: f64,
    sequence: u64,
}

struct LivePlaylist {
    media_sequence: u64,
    entries: Vec<PlaylistEntry>,
}

fn parse_live_playlist(text: &str) -> LivePlaylist {
    let mut media_sequence = None;
    let mut entries = Vec::new();
    let mut pending_duration = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = Some(value.parse().expect("media sequence"));
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            pending_duration = Some(
                value
                    .trim_end_matches(',')
                    .parse::<f64>()
                    .expect("EXTINF duration"),
            );
        } else if let Some(name) = line.strip_suffix(".ts") {
            entries.push(PlaylistEntry {
                duration: pending_duration.take().expect("EXTINF before segment URI"),
                sequence: name.parse().expect("segment sequence"),
            });
        }
    }
    LivePlaylist {
        media_sequence: media_sequence.expect("#EXT-X-MEDIA-SEQUENCE"),
        entries,
    }
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

fn assert_independent_segment_start(sequence: u64, access_unit: &AccessUnit) {
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

fn assert_access_unit_payload(sequence: u64, access_unit: &AccessUnit) {
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
/// that segment, proving the segment is independently decodable. The Y planes
/// are compared in memory so a stream that repeats or freezes pictures fails
/// even though every frame still decodes.
fn decode_segment(sequence: u64, access_units: &[AccessUnit]) -> usize {
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
        motion.transitions,
        IDR_PERIOD - 1,
        "segment {sequence}: {frames} decoded frames must yield {} inter-frame transitions",
        IDR_PERIOD - 1
    );
    assert!(
        motion.changed_transitions >= MOTION_MIN_CHANGED_TRANSITIONS,
        "segment {sequence}: only {} of {} decoded frame transitions show real motion; \
         the stream repeated IDRs or served stale pictures",
        motion.changed_transitions,
        motion.transitions
    );
    frames
}

fn assert_decoded_frame(frame: &DecodedYUV<'_>, sequence: u64) {
    assert_eq!(
        frame.dimensions(),
        (WIDTH as usize, HEIGHT as usize),
        "segment {sequence} decoded the wrong dimensions"
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
fn motion_metric_scores_repeated_frames_as_static() {
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
