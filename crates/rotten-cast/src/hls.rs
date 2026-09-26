//! H.264 Annex B to MPEG-TS muxing and a bounded in-memory HLS segment store.
//!
//! [`HlsMuxer`] accepts complete H.264 Annex B access units with monotonic
//! presentation timestamps and seals MPEG-TS segments on real IDR (NAL type 5)
//! boundaries. [`HlsMuxer::with_aac`] additionally accepts complete AAC-LC
//! 44.1 kHz stereo ADTS frames ([`HlsMuxer::push_audio`]) and advertises the
//! combined `avc1...,mp4a.40.2` codec string; audio PES packets and the second
//! PMT elementary stream are multiplexed into the same transport stream while
//! PCR stays on the video PID. Segments are kept in memory by [`HlsStore`] and
//! rendered as a live playlist; desktop pixels never touch the filesystem.
//!
//! The muxer is deliberately strict: it only emits decodable segments
//! (SPS/PPS present, AUD inserted when missing), rejects codec changes and
//! timestamp regressions instead of producing an invalid timeline, and fails
//! loudly when a segment would exceed the selected profile's target duration or
//! the 4 MiB size cap including audio (capture is too slow) rather than drifting
//! the playlist.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Segmenting profile for the HLS muxer and store.
///
/// [`HlsProfile::Stable`] keeps the original 2-second target, ~1-second
/// segments and 8 seconds of initial readiness. [`HlsProfile::Responsive`]
/// uses a 1-second target duration, seals about every 500 ms and permits
/// playback setup after 4 advertised seconds. Both profiles retain the same 12-second
/// media history and share the payload and segment budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HlsProfile {
    /// 2-second target duration, ~1-second segments, 8-second initial ready.
    #[default]
    Stable,
    /// 1-second target duration, ~500 ms segments, 4-second initial ready.
    Responsive,
}

impl HlsProfile {
    /// The playlist `EXT-X-TARGETDURATION` value, in whole seconds.
    pub fn target_duration_secs(self) -> u64 {
        match self {
            Self::Stable => 2,
            Self::Responsive => 1,
        }
    }

    /// Hard limit for a completed segment, in microseconds.
    pub fn target_duration_us(self) -> u64 {
        self.target_duration_secs() * 1_000_000
    }

    /// Nominal media duration of one segment, in microseconds: the interval at
    /// which the encoder is asked to force an IDR.
    pub fn segment_duration_us(self) -> u64 {
        match self {
            Self::Stable => 1_000_000,
            Self::Responsive => 500_000,
        }
    }

    /// Minimum open-segment age before an IDR may seal it, in microseconds.
    pub fn min_segment_duration_us(self) -> u64 {
        match self {
            Self::Stable => 900_000,
            Self::Responsive => 450_000,
        }
    }

    /// Advertised media duration required before the manifests answer 200.
    pub fn ready_duration_us(self) -> u64 {
        match self {
            Self::Stable => 8_000_000,
            Self::Responsive => 4_000_000,
        }
    }

    /// Minimum interval between two changed advertised snapshots.
    ///
    /// Deliberately not computed from whole seconds: the responsive target of
    /// one second would integer-divide to zero and advertise on every publish.
    pub fn snapshot_min_interval(self) -> Duration {
        match self {
            Self::Stable => Duration::from_millis(1_000),
            Self::Responsive => Duration::from_millis(500),
        }
    }
}

/// Master playlist bandwidth for stores created with [`HlsStore::new`].
const DEFAULT_BANDWIDTH_BPS: u64 = 8_000_000;
/// Upper bound accepted by [`HlsStore::with_profile`].
const MAX_BANDWIDTH_BPS: u64 = 100_000_000;
/// Maximum size of a single MPEG-TS segment.
const MAX_SEGMENT_BYTES: usize = 4 * 1024 * 1024;
/// Maximum accepted Annex B access unit size, checked before splitting or
/// copying so hostile or broken input cannot allocate megabytes.
const MAX_ACCESS_UNIT_BYTES: usize = 2 * 1024 * 1024;
/// Maximum NAL units per access unit; bounds the NAL vector and the growth
/// from normalizing every NAL to a 4-byte start code.
const MAX_NALS_PER_ACCESS_UNIT: usize = 512;
/// Maximum size of a cached SPS/PPS NAL, bounded because it is repeated on
/// every IDR access unit.
const MAX_CONFIG_NAL_BYTES: usize = 4 * 1024;
/// Steady retained and advertised window, in media time; both profiles keep the
/// same 12-second history.
const WINDOW_DURATION_US: u64 = 12_000_000;
/// Payload budget across active and retired segments.
const MAX_STORE_BYTES: usize = 128 * 1024 * 1024;
/// Retained-segment budget across active and retired segments.
const MAX_STORE_SEGMENTS: usize = 64;

const TS_PACKET_SIZE: usize = 188;
const PTS33_MASK: u64 = (1 << 33) - 1;
/// 1 second of 90 kHz ticks added to PTS so the first PCR stays in range.
const PES_PTS_OFFSET: u64 = 90_000;
/// PCR is held 300 ms (27 000 ticks) behind the access unit PTS for decoder
/// lead-in.
const PCR_DECODE_LEAD: u64 = 27_000;

/// Fixed AAC-LC 44.1 kHz stereo codec string appended to `avc1...` in AAC mode.
const AAC_CODEC: &str = "mp4a.40.2";
/// ADTS `profile` field value for AAC-LC (the field encodes `object_type - 1`).
const ADTS_PROFILE_LC: u8 = 1;
/// ADTS `sampling_frequency_index` for 44100 Hz.
const ADTS_FREQ_INDEX_44100: u8 = 4;
/// ADTS `channel_configuration` for stereo.
const ADTS_CHANNEL_CONFIG_STEREO: u8 = 2;
/// ADTS header without the optional CRC (protection_absent = 1).
const ADTS_HEADER_BYTES: usize = 7;
/// ADTS header with the optional CRC (protection_absent = 0).
const ADTS_HEADER_BYTES_CRC: usize = 9;
/// `frame_length` is 13 bits: a complete ADTS frame can never exceed this.
const MAX_ADTS_FRAME_BYTES: usize = 8191;
/// Refresh the video PCR from audio when it has not advanced for 40 ms.
const PCR_MAX_INTERVAL_90K: u64 = 3_600;

const PID_PAT: u16 = 0x0000;
const PID_PMT: u16 = 0x0100;
const PID_VIDEO: u16 = 0x0101;
const PID_AUDIO: u16 = 0x0102;
const STREAM_TYPE_H264: u8 = 0x1B;
const STREAM_TYPE_AAC: u8 = 0x0F;
const PES_STREAM_ID_VIDEO: u8 = 0xE0;
const PES_STREAM_ID_AUDIO: u8 = 0xC0;

const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

const AUD_NAL: [u8; 2] = [0x09, 0xF0];
const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// A sealed MPEG-TS media segment.
#[derive(Debug, Clone)]
pub struct Segment {
    /// Monotonic media sequence number, starting at 0 for a session.
    pub sequence: u64,
    /// Segment duration in seconds (the playlist `EXTINF` value).
    pub duration: f64,
    /// MPEG-TS payload; always a whole number of 188-byte packets.
    pub data: Vec<u8>,
}

/// A single Annex B NAL unit borrowed from the pushed access unit.
struct Nal<'a> {
    nal_type: u8,
    bytes: &'a [u8],
}

/// State of the segment currently being buffered.
struct PendingSegment {
    first_pts_us: u64,
    data: Vec<u8>,
}

impl PendingSegment {
    fn new(first_pts_us: u64) -> Self {
        Self {
            first_pts_us,
            data: Vec::new(),
        }
    }
}

/// Fixed ADTS header fields that must not change midstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AacConfig {
    protection_absent: bool,
    profile: u8,
    frequency_index: u8,
    channel_configuration: u8,
}

/// Converts H.264 Annex B access units (and optionally AAC-LC ADTS frames)
/// into HLS-ready MPEG-TS segments.
pub struct HlsMuxer {
    profile: HlsProfile,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    codec: Option<String>,
    aac: bool,
    aac_config: Option<AacConfig>,
    next_sequence: u64,
    last_pts_us: Option<u64>,
    pending: Option<PendingSegment>,
    // The last PCR written to the video PID (video PES packets and PCR-only
    // refreshes share it) so a later refresh can never regress the clock.
    last_pcr_90k: Option<u64>,
    // Per-PID continuity counters persist across contiguous segments: MPEG-TS
    // continuity is a property of the elementary stream, not of an HLS segment
    // boundary, and we never emit a discontinuity indicator.
    cc_pat: u8,
    cc_pmt: u8,
    cc_video: u8,
    cc_audio: u8,
}

impl HlsMuxer {
    /// Creates an empty video-only muxer.
    pub fn new() -> Self {
        Self::with_audio(false)
    }

    /// Creates an empty muxer that also accepts AAC-LC 44.1 kHz stereo ADTS
    /// frames through [`HlsMuxer::push_audio`] and advertises the combined
    /// `avc1...,mp4a.40.2` codec string once an SPS has been seen.
    pub fn with_aac() -> Self {
        Self::with_audio(true)
    }

    /// Creates an empty muxer with the given segmenting profile; `aac` enables
    /// the AAC input path exactly like [`HlsMuxer::with_aac`].
    ///
    /// The profile fixes the segment target and seal limits for the whole
    /// session; there is no midstream switch.
    pub fn with_profile(profile: HlsProfile, aac: bool) -> Self {
        Self {
            profile,
            sps: None,
            pps: None,
            codec: None,
            aac,
            aac_config: None,
            next_sequence: 0,
            last_pts_us: None,
            pending: None,
            last_pcr_90k: None,
            cc_pat: 0,
            cc_pmt: 0,
            cc_video: 0,
            cc_audio: 0,
        }
    }

    fn with_audio(aac: bool) -> Self {
        Self::with_profile(HlsProfile::Stable, aac)
    }

    /// Pushes one complete H.264 Annex B access unit with a monotonic
    /// presentation timestamp in microseconds.
    ///
    /// Returns the previous segment when this access unit's IDR seals it.
    /// Input before the first decodable IDR is ignored. Non-monotonic
    /// timestamps (globally, across video and audio, on an AAC muxer), config
    /// changes, oversize segments and segments that exceed the profile's target
    /// duration are reported as errors.
    pub fn push(&mut self, annex_b: &[u8], pts_us: u64) -> Result<Option<Segment>> {
        if let Some(last) = self.last_pts_us
            && pts_us < last
        {
            bail!(
                "non-monotonic presentation timestamp {pts_us}us after {last}us; refusing to build an invalid timeline"
            );
        }
        self.last_pts_us = Some(pts_us);

        // Reject unreasonable input before splitting or copying anything.
        if annex_b.len() > MAX_ACCESS_UNIT_BYTES {
            bail!(
                "access unit is {} bytes, over the {} MiB input cap; refusing to buffer it",
                annex_b.len(),
                MAX_ACCESS_UNIT_BYTES / (1024 * 1024)
            );
        }
        let nals = split_annex_b(annex_b);
        if nals.is_empty() {
            bail!("access unit contains no Annex B NAL units");
        }
        if nals.len() > MAX_NALS_PER_ACCESS_UNIT {
            bail!(
                "access unit has {} NAL units, over the {MAX_NALS_PER_ACCESS_UNIT} NAL cap",
                nals.len()
            );
        }

        if let Some(sps) = nals.iter().find(|nal| nal.nal_type == NAL_SPS) {
            self.update_sps(sps.bytes)?;
        }
        if let Some(pps) = nals.iter().find(|nal| nal.nal_type == NAL_PPS) {
            self.update_pps(pps.bytes)?;
        }

        let has_idr = nals.iter().any(|nal| nal.nal_type == NAL_IDR);
        let decodable = self.sps.is_some() && self.pps.is_some();
        if self.pending.is_none() && !(has_idr && decodable) {
            // Pre-IDR input (or an IDR without cached SPS/PPS) cannot start a
            // decodable segment; ignore it instead of advertising garbage.
            return Ok(None);
        }

        let mut sealed = None;
        if let Some(pending) = &self.pending {
            let elapsed_us = pts_us.saturating_sub(pending.first_pts_us);
            if has_idr && elapsed_us >= self.profile.min_segment_duration_us() {
                sealed = self.seal(pts_us)?;
            } else if elapsed_us > self.profile.target_duration_us() {
                bail!(
                    "segment already spans {:.3}s without a sealable IDR (target {}s); capture is too slow",
                    elapsed_us as f64 / 1e6,
                    self.profile.target_duration_secs()
                );
            }
        }

        let prepared = self.prepare_access_unit(&nals)?;
        self.append_access_unit(prepared, pts_us, has_idr)?;
        Ok(sealed)
    }

    /// Pushes one complete AAC-LC 44.1 kHz stereo ADTS frame with a monotonic
    /// presentation timestamp in microseconds.
    ///
    /// Only valid on a muxer created with [`HlsMuxer::with_aac`]. In AAC mode
    /// the timestamp is globally monotonic across video and audio, so a frame
    /// older than the last submitted sample is rejected as late audio. Audio
    /// arriving before a decodable video IDR opens a segment is discarded (it
    /// advances the global watermark but never creates an audio-only segment).
    ///
    /// The frame is appended to the segment that is open when it arrives.
    /// Callers submit globally ascending timestamps and, when an audio frame's
    /// PTS exactly equals the sealing IDR's PTS, submit the video first; the
    /// audio then belongs to the new segment, so every frame lands in the
    /// segment its first sample PTS falls in. A frame with a PTS below the
    /// sealing IDR PTS is already enqueued and stays assigned to the preceding
    /// segment exactly once. The 4 MiB and profile target-duration segment
    /// bounds include audio.
    pub fn push_audio(&mut self, adts: &[u8], pts_us: u64) -> Result<()> {
        if !self.aac {
            bail!("audio requires an AAC muxer; construct it with HlsMuxer::with_aac()");
        }
        if let Some(last) = self.last_pts_us
            && pts_us < last
        {
            bail!(
                "non-monotonic presentation timestamp {pts_us}us after {last}us (late audio); refusing to build an invalid timeline"
            );
        }
        self.last_pts_us = Some(pts_us);

        let (_, config) = parse_adts_frame(adts)?;
        match self.aac_config {
            Some(cached) if cached != config => {
                bail!(
                    "AAC ADTS configuration changed midstream; refusing to continue an invalid timeline"
                );
            }
            None => self.aac_config = Some(config),
            _ => {}
        }

        let Some(pending) = self.pending.as_ref() else {
            // Before the first decodable IDR there is no segment to attach the
            // frame to; drop it instead of opening an audio-only segment.
            return Ok(());
        };
        let elapsed_us = pts_us.saturating_sub(pending.first_pts_us);
        if elapsed_us > self.profile.target_duration_us() {
            bail!(
                "audio at {:.3}s into the open segment, over the {}s target duration (capture is too slow; the playlist target must not drift)",
                elapsed_us as f64 / 1e6,
                self.profile.target_duration_secs()
            );
        }

        self.append_audio(adts, pts_us)
    }

    /// The `avc1.PPCCLL` codec string derived from the first SPS, if seen,
    /// combined with `,mp4a.40.2` on an AAC muxer.
    pub fn codec(&self) -> Option<&str> {
        self.codec.as_deref()
    }

    /// True once a segment is open or has been sealed: SPS/PPS may no longer
    /// change after this point.
    fn started(&self) -> bool {
        self.pending.is_some() || self.next_sequence > 0
    }

    fn update_sps(&mut self, nal: &[u8]) -> Result<()> {
        if nal.len() > MAX_CONFIG_NAL_BYTES {
            bail!(
                "SPS is {} bytes, over the {MAX_CONFIG_NAL_BYTES}-byte configuration cap",
                nal.len()
            );
        }
        let codec =
            parse_codec_string(nal).context("SPS too short to derive an avc1 codec string")?;
        match (&self.sps, &self.codec) {
            (Some(cached), Some(cached_codec)) if self.started() => {
                if cached.as_slice() != nal {
                    bail!(
                        "H.264 SPS changed midstream ({cached_codec} -> {codec}); refusing to continue an invalid timeline"
                    );
                }
            }
            _ => {
                self.sps = Some(nal.to_vec());
                self.codec = Some(if self.aac {
                    format!("{codec},{AAC_CODEC}")
                } else {
                    codec
                });
            }
        }
        Ok(())
    }

    fn update_pps(&mut self, nal: &[u8]) -> Result<()> {
        if nal.len() > MAX_CONFIG_NAL_BYTES {
            bail!(
                "PPS is {} bytes, over the {MAX_CONFIG_NAL_BYTES}-byte configuration cap",
                nal.len()
            );
        }
        match &self.pps {
            Some(cached) if self.started() => {
                if cached.as_slice() != nal {
                    bail!("H.264 PPS changed midstream; refusing to continue an invalid timeline");
                }
            }
            _ => {
                self.pps = Some(nal.to_vec());
            }
        }
        Ok(())
    }

    /// Builds the bytes for one access unit: insert an AUD when missing,
    /// repeat cached SPS/PPS on IDR access units, and normalize start codes.
    fn prepare_access_unit(&self, nals: &[Nal<'_>]) -> Result<Vec<u8>> {
        let has_aud = nals.iter().any(|nal| nal.nal_type == NAL_AUD);
        let has_idr = nals.iter().any(|nal| nal.nal_type == NAL_IDR);
        let has_sps = nals.iter().any(|nal| nal.nal_type == NAL_SPS);
        let has_pps = nals.iter().any(|nal| nal.nal_type == NAL_PPS);

        let mut out = Vec::new();
        if !has_aud {
            append_nal(&mut out, &AUD_NAL);
        }
        if has_idr {
            if !has_sps {
                append_nal(
                    &mut out,
                    self.sps.as_deref().context("IDR without cached SPS")?,
                );
            }
            if !has_pps {
                append_nal(
                    &mut out,
                    self.pps.as_deref().context("IDR without cached PPS")?,
                );
            }
        }
        for nal in nals {
            append_nal(&mut out, nal.bytes);
        }
        Ok(out)
    }

    /// Appends one prepared access unit as a single PES packet, writing
    /// PAT/PMT first when the segment is empty.
    ///
    /// The exact packetized size is checked before a single byte is appended,
    /// so a rejected access unit never leaves a partially extended segment.
    fn append_access_unit(&mut self, prepared: Vec<u8>, pts_us: u64, is_idr: bool) -> Result<()> {
        if self.pending.is_none() {
            self.pending = Some(PendingSegment::new(pts_us));
        }
        let aac = self.aac;
        // Disjoint field borrows: the continuity counters and the last PCR live
        // on the muxer so they can carry across contiguous segments.
        let Self {
            pending,
            last_pcr_90k,
            cc_pat,
            cc_pmt,
            cc_video,
            ..
        } = self;
        let pending = pending.as_mut().expect("pending segment set above");

        let psi_bytes = if pending.data.is_empty() {
            2 * TS_PACKET_SIZE
        } else {
            0
        };
        let growth = packetized_pes_len(prepared.len(), true);
        let projected = pending.data.len() + psi_bytes + growth;
        if projected > MAX_SEGMENT_BYTES {
            bail!(
                "segment would grow to {projected} bytes, over the {} MiB cap ({} bytes buffered); drop frames or lower the bitrate",
                MAX_SEGMENT_BYTES / (1024 * 1024),
                pending.data.len()
            );
        }

        if pending.data.is_empty() {
            append_psi(&mut pending.data, PID_PAT, &pat_section(), cc_pat);
            append_psi(&mut pending.data, PID_PMT, &pmt_section(aac), cc_pmt);
        }

        let pts_90k = (pts_90k(pts_us) + PES_PTS_OFFSET) & PTS33_MASK;
        let pcr_90k = pts_90k.wrapping_sub(PCR_DECODE_LEAD) & PTS33_MASK;

        append_pes(
            &mut pending.data,
            PID_VIDEO,
            PES_STREAM_ID_VIDEO,
            cc_video,
            pts_90k,
            Some(pcr_90k),
            is_idr,
            &prepared,
        );
        *last_pcr_90k = Some(pcr_90k);
        debug_assert!(
            pending.data.len() <= MAX_SEGMENT_BYTES,
            "exact size check above"
        );
        Ok(())
    }

    /// Appends one ADTS frame as a single audio PES packet to the open segment.
    ///
    /// When the last video PCR is already 40 ms old (slow video), an
    /// adaptation-only PCR packet on the video PID refreshes the shared clock
    /// first. Its exact size is checked before a single byte is appended, so a
    /// rejected frame never leaves a partially extended segment.
    fn append_audio(&mut self, adts: &[u8], pts_us: u64) -> Result<()> {
        let pts_90k = (pts_90k(pts_us) + PES_PTS_OFFSET) & PTS33_MASK;
        let pcr_90k = pts_90k.wrapping_sub(PCR_DECODE_LEAD) & PTS33_MASK;
        let emit_pcr = self.last_pcr_90k.is_some_and(|last| {
            let gap = pcr_90k.wrapping_sub(last) & PTS33_MASK;
            (PCR_MAX_INTERVAL_90K..(1 << 32)).contains(&gap)
        });
        let growth =
            packetized_pes_len(adts.len(), false) + if emit_pcr { TS_PACKET_SIZE } else { 0 };

        let Self {
            pending,
            last_pcr_90k,
            cc_video,
            cc_audio,
            ..
        } = self;
        let pending = pending.as_mut().expect("caller checked an open segment");
        let projected = pending.data.len() + growth;
        if projected > MAX_SEGMENT_BYTES {
            bail!(
                "audio would grow the segment to {projected} bytes, over the {} MiB cap ({} bytes buffered); drop frames or lower the bitrate",
                MAX_SEGMENT_BYTES / (1024 * 1024),
                pending.data.len()
            );
        }

        if emit_pcr {
            // `cc_video` holds the *next* payload counter, so the adaptation-
            // only packet repeats the last transmitted payload counter
            // (next - 1 mod 16) and leaves the pending one for the next
            // payload packet (MPEG-TS 2.4.3.3: only packets with a payload
            // increment the counter).
            append_pcr_only(&mut pending.data, (*cc_video + 15) & 0x0F, pcr_90k);
            *last_pcr_90k = Some(pcr_90k);
        }
        append_pes(
            &mut pending.data,
            PID_AUDIO,
            PES_STREAM_ID_AUDIO,
            cc_audio,
            pts_90k,
            None,
            false,
            adts,
        );
        debug_assert!(
            pending.data.len() <= MAX_SEGMENT_BYTES,
            "exact size check above"
        );
        Ok(())
    }

    /// Completes the open segment at `seal_pts_us`. The caller starts the next
    /// segment with the IDR access unit carrying that timestamp.
    fn seal(&mut self, seal_pts_us: u64) -> Result<Option<Segment>> {
        let Some(pending) = self.pending.as_ref() else {
            return Ok(None);
        };
        let duration_us = seal_pts_us.saturating_sub(pending.first_pts_us);
        if duration_us > self.profile.target_duration_us() {
            bail!(
                "completed segment is {:.3}s, over the {}s target duration (capture is too slow; the playlist target must not drift)",
                duration_us as f64 / 1e6,
                self.profile.target_duration_secs()
            );
        }
        if pending.data.len() > MAX_SEGMENT_BYTES {
            bail!(
                "completed segment is {} bytes, over the {} MiB cap",
                pending.data.len(),
                MAX_SEGMENT_BYTES / (1024 * 1024)
            );
        }

        let pending = self.pending.take().expect("checked above");
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        Ok(Some(Segment {
            sequence,
            duration: duration_us as f64 / 1_000_000.0,
            data: pending.data,
        }))
    }
}

impl Default for HlsMuxer {
    fn default() -> Self {
        Self::new()
    }
}

/// One admitted segment plus the metadata that bounds how long it is kept.
struct RetainedSegment {
    segment: Segment,
    /// Validated duration as integer microseconds; all playlist math uses this.
    duration_us: u64,
    /// Largest advertised snapshot duration (microseconds) that contained this
    /// segment while it was in the window.
    max_containing_us: u64,
    /// `None` while the segment is inside the active window; `Some` once it is
    /// retired, holding the instant its retention promise expires.
    retire_at: Option<Instant>,
    /// Whether any committed snapshot ever advertised this segment.
    ever_advertised: bool,
}

/// One advertised playlist entry: metadata only, never a payload copy.
struct AdvertisedEntry {
    sequence: u64,
    duration_us: u64,
}

/// An advertised snapshot committed at most once per the profile's snapshot
/// interval ([`HlsProfile::snapshot_min_interval`]).
struct AdvertisedSnapshot {
    entries: Vec<AdvertisedEntry>,
    duration_us: u64,
}

impl AdvertisedSnapshot {
    fn start_sequence(&self) -> u64 {
        self.entries.first().map_or(0, |entry| entry.sequence)
    }

    /// Entries are always contiguous, so a range check is exact.
    fn contains(&self, sequence: u64) -> bool {
        self.entries
            .first()
            .is_some_and(|first| first.sequence <= sequence)
            && self
                .entries
                .last()
                .is_some_and(|last| sequence <= last.sequence)
    }
}

/// Bounded in-memory store for sealed segments plus live playlist rendering.
///
/// The active window always carries at least 12 seconds of media time (as much
/// as exists before that), for every profile. Snapshots are committed at most
/// once per the profile's snapshot interval and are the only thing readiness,
/// `MEDIA-SEQUENCE` and `EXTINF` ever read; published-but-unadvertised segments
/// stay fetchable but are never listed. Segments removed from the window are
/// retired until `removal + segment duration + the largest advertised window
/// that contained them`, so a client following an older playlist can still
/// finish fetching it.
pub struct HlsStore {
    /// Segmenting profile fixed for this session.
    profile: HlsProfile,
    /// Master playlist `BANDWIDTH` hint fixed for this session, in bits/s.
    bandwidth_bps: u64,
    /// Segments inside the sliding window, oldest first.
    active: VecDeque<RetainedSegment>,
    /// Segments retired from the window but still inside a retention promise.
    retired: VecDeque<RetainedSegment>,
    total_bytes: usize,
    codec: Option<String>,
    last_sequence: Option<u64>,
    last_publish_at: Option<Instant>,
    snapshot: Option<AdvertisedSnapshot>,
    snapshot_at: Option<Instant>,
    max_bytes: usize,
    max_segments: usize,
}

impl HlsStore {
    /// Creates an empty store with the stable profile and an 8 Mbit/s master
    /// playlist bandwidth hint.
    pub fn new() -> Self {
        Self::with_profile(HlsProfile::Stable, DEFAULT_BANDWIDTH_BPS)
            .expect("the default store profile is valid")
    }

    /// Creates an empty store with the given profile and master playlist
    /// bandwidth hint in bits/s.
    ///
    /// The bandwidth must be positive and at most 100 Mbit/s; the profile and
    /// bandwidth are immutable for the session. Both profiles keep the same
    /// 12-second media history and payload/segment budgets.
    pub fn with_profile(profile: HlsProfile, bandwidth_bps: u64) -> Result<Self> {
        if bandwidth_bps == 0 {
            bail!("refusing an HLS store with a zero bandwidth hint");
        }
        if bandwidth_bps > MAX_BANDWIDTH_BPS {
            bail!("HLS bandwidth hint {bandwidth_bps} bps is over the {MAX_BANDWIDTH_BPS} bps cap");
        }
        Ok(Self {
            profile,
            bandwidth_bps,
            active: VecDeque::new(),
            retired: VecDeque::new(),
            total_bytes: 0,
            codec: None,
            last_sequence: None,
            last_publish_at: None,
            snapshot: None,
            snapshot_at: None,
            max_bytes: MAX_STORE_BYTES,
            max_segments: MAX_STORE_SEGMENTS,
        })
    }

    /// Publishes a sealed segment using the process monotonic clock.
    pub fn publish(&mut self, segment: Segment, codec: &str) -> Result<()> {
        self.publish_at(segment, codec, Instant::now())
    }

    /// [`Self::publish`] with a caller-supplied monotonic clock.
    ///
    /// This deterministic-clock version lets offline tests simulate production
    /// and retention without sleeping. Successful calls must carry
    /// non-decreasing instants; a regression is rejected before any state
    /// changes.
    ///
    /// Rejects codec changes, sequence gaps or overflow, invalid, empty or
    /// oversized segments, and admission that would exceed the payload or
    /// segment budget while advertised, window and unexpired segments cannot be
    /// evicted. A rejected publish never changes the codec, the advertised
    /// snapshot, the retained window or the last sequence number.
    pub fn publish_at(&mut self, segment: Segment, codec: &str, now: Instant) -> Result<()> {
        if let Some(last) = self.last_publish_at
            && now < last
        {
            bail!(
                "publish clock went backwards: {now:?} is before the last successful publish {last:?}"
            );
        }
        if codec.is_empty() {
            bail!("refusing to publish a segment with an empty codec string");
        }
        if let Some(current) = &self.codec
            && current != codec
        {
            bail!(
                "codec changed midstream ({current} -> {codec}); refusing to advertise mixed segments"
            );
        }
        if segment.data.is_empty() {
            bail!("refusing to publish an empty segment");
        }
        if segment.data.len() > MAX_SEGMENT_BYTES {
            bail!(
                "segment is {} bytes, over the {} MiB cap",
                segment.data.len(),
                MAX_SEGMENT_BYTES / (1024 * 1024)
            );
        }
        let duration_us = validated_duration_us(segment.duration, self.profile)?;
        match self.last_sequence {
            Some(last) if last == u64::MAX => {
                bail!(
                    "media sequence overflow at u64::MAX; refusing to publish sequence {}",
                    segment.sequence
                );
            }
            Some(last) if segment.sequence != last + 1 => {
                bail!(
                    "non-contiguous segment sequence {} after {}",
                    segment.sequence,
                    last
                );
            }
            _ => {}
        }

        // Expired retention (and never-advertised segments a snapshot has
        // skipped) is safe to drop before the budget check; advertised or
        // unexpired segments are never trimmed to make room.
        self.prune(now);
        let retained = self.active.len() + self.retired.len();
        if retained + 1 > self.max_segments {
            bail!(
                "cast HLS store cannot admit sequence {}: {retained} retained segments reach the {}-segment budget while advertised, window and unexpired segments cannot be evicted (producer outpacing retention)",
                segment.sequence,
                self.max_segments
            );
        }
        if self.total_bytes + segment.data.len() > self.max_bytes {
            bail!(
                "cast HLS store cannot admit sequence {}: {} retained bytes plus {} new bytes exceed the {} byte budget while advertised, window and unexpired segments cannot be evicted (lower the bitrate or wait for retention to expire)",
                segment.sequence,
                self.total_bytes,
                segment.data.len(),
                self.max_bytes
            );
        }

        // Admission cannot fail from here; commit every field together.
        self.total_bytes += segment.data.len();
        if self.codec.is_none() {
            self.codec = Some(codec.to_owned());
        }
        self.last_sequence = Some(segment.sequence);
        self.last_publish_at = Some(now);
        self.active.push_back(RetainedSegment {
            segment,
            duration_us,
            max_containing_us: 0,
            retire_at: None,
            ever_advertised: false,
        });

        if self.snapshot_at.is_none_or(|at| {
            now.saturating_duration_since(at) >= self.profile.snapshot_min_interval()
        }) {
            // Retire only at the actual commit boundary: while the previous
            // snapshot still advertises a segment, its retention clock must
            // not start early. The active window may therefore hold more than
            // 12s of pending media between two commits; the budget check above
            // still bounds it.
            self.slide(now);
            self.commit_snapshot(now);
        }
        // A commit can release segments from the advertised range, so prune
        // again after it, once the promises it dropped are no longer protected.
        self.prune(now);
        Ok(())
    }

    /// True once the advertised snapshot carries at least the profile's
    /// readiness duration ([`HlsProfile::ready_duration_us`]) of media.
    /// Readiness uses the snapshot, never pending segments, and once true it
    /// stays true: the window only grows or slides forward.
    pub fn ready(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.duration_us >= self.profile.ready_duration_us())
    }

    /// Render the live media playlist, or `None` before the stream is ready.
    pub(crate) fn playlist(&self) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        if snapshot.duration_us < self.profile.ready_duration_us() {
            return None;
        }
        let mut out = String::new();
        out.push_str("#EXTM3U\n");
        out.push_str("#EXT-X-VERSION:3\n");
        out.push_str(&format!(
            "#EXT-X-TARGETDURATION:{}\n",
            self.profile.target_duration_secs()
        ));
        out.push_str(&format!(
            "#EXT-X-MEDIA-SEQUENCE:{}\n",
            snapshot.start_sequence()
        ));
        for entry in &snapshot.entries {
            // Integer microseconds rendered with six decimals: the advertised
            // timeline never accumulates floating-point rounding.
            out.push_str(&format!(
                "#EXTINF:{}.{:06},\n",
                entry.duration_us / 1_000_000,
                entry.duration_us % 1_000_000
            ));
            out.push_str(&format!("{}.ts\n", entry.sequence));
        }
        Some(out)
    }

    /// Codec string declared by the master playlist.
    pub(crate) fn codec(&self) -> Option<&str> {
        self.codec.as_deref()
    }

    /// Master playlist bandwidth hint fixed for this session, in bits/s.
    pub(crate) fn bandwidth_bps(&self) -> u64 {
        self.bandwidth_bps
    }

    /// Segment with the given media sequence while its payload is retained.
    ///
    /// This serves completed-but-not-yet-advertised segments too, for
    /// compatibility with callers that fetch a segment right after publishing
    /// it; the playlist and readiness never advertise them.
    pub(crate) fn segment(&self, sequence: u64) -> Option<&Segment> {
        self.active
            .iter()
            .chain(self.retired.iter())
            .find(|entry| entry.segment.sequence == sequence)
            .map(|entry| &entry.segment)
    }

    /// Bounded numeric store snapshot for the HTTP diagnostics.
    pub(crate) fn stats(&self) -> StoreStats {
        self.stats_at(Instant::now())
    }

    /// [`Self::stats`] with a caller-supplied clock.
    pub(crate) fn stats_at(&self, now: Instant) -> StoreStats {
        StoreStats {
            advertised_us: self
                .snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.duration_us),
            retained_segments: self.active.len() + self.retired.len(),
            retained_bytes: self.total_bytes,
            publication_age: self.snapshot_at.map(|at| now.saturating_duration_since(at)),
        }
    }

    /// Index of the oldest segment of the newest contiguous suffix that still
    /// carries at least [`WINDOW_DURATION_US`]; 0 while less media exists.
    fn window_start(&self) -> usize {
        let mut sum = 0u64;
        let mut start = 0usize;
        for (index, entry) in self.active.iter().enumerate().rev() {
            sum = sum.saturating_add(entry.duration_us);
            start = index;
            if sum >= WINDOW_DURATION_US {
                break;
            }
        }
        start
    }

    /// Moves the oldest segments out of the active window once at least the
    /// whole window of newer media remains. A removed segment keeps its payload
    /// until `now + its duration + the largest window that contained it`.
    fn slide(&mut self, now: Instant) {
        let start = self.window_start();
        for _ in 0..start {
            let Some(mut entry) = self.active.pop_front() else {
                break;
            };
            entry.retire_at = Some(
                now + Duration::from_micros(
                    entry.duration_us.saturating_add(entry.max_containing_us),
                ),
            );
            self.retired.push_back(entry);
        }
    }

    /// Builds the advertised snapshot from the whole active window and records
    /// the containing window duration on every advertised segment.
    fn commit_snapshot(&mut self, now: Instant) {
        if self.active.is_empty() {
            return;
        }
        let mut entries = Vec::with_capacity(self.active.len());
        let mut duration_us = 0u64;
        for entry in &self.active {
            duration_us = duration_us.saturating_add(entry.duration_us);
            entries.push(AdvertisedEntry {
                sequence: entry.segment.sequence,
                duration_us: entry.duration_us,
            });
        }
        for entry in &mut self.active {
            entry.max_containing_us = entry.max_containing_us.max(duration_us);
            entry.ever_advertised = true;
        }
        self.snapshot = Some(AdvertisedSnapshot {
            entries,
            duration_us,
        });
        self.snapshot_at = Some(now);
    }

    /// Drops retired segments whose retention promise expired, plus segments a
    /// committed snapshot skipped before they were ever advertised. Active
    /// window segments, current snapshot segments and unexpired retired
    /// segments are never evicted.
    fn prune(&mut self, now: Instant) {
        let snapshot = self.snapshot.as_ref();
        self.retired.retain(|entry| {
            if let Some(snapshot) = snapshot {
                if snapshot.contains(entry.segment.sequence) {
                    return true;
                }
                if !entry.ever_advertised && snapshot.start_sequence() > entry.segment.sequence {
                    return false;
                }
            }
            entry.retire_at.is_some_and(|retire_at| retire_at > now)
        });
        self.total_bytes = self
            .active
            .iter()
            .map(|entry| entry.segment.data.len())
            .chain(self.retired.iter().map(|entry| entry.segment.data.len()))
            .sum();
    }

    /// Test-only constructor with smaller budgets; production always uses
    /// [`MAX_STORE_BYTES`] and [`MAX_STORE_SEGMENTS`].
    #[cfg(test)]
    fn with_limits(max_bytes: usize, max_segments: usize) -> Self {
        Self {
            max_bytes,
            max_segments,
            ..Self::new()
        }
    }
}

impl Default for HlsStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Bounded numeric snapshot of [`HlsStore`], used only by HTTP diagnostics.
///
/// No segment bytes, tokens, URLs or request data can appear here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StoreStats {
    /// Advertised snapshot duration in integer microseconds (0 before the
    /// first commit).
    pub(crate) advertised_us: u64,
    /// Segments currently retaining payload (active window plus retired).
    pub(crate) retained_segments: usize,
    /// Total retained payload bytes.
    pub(crate) retained_bytes: usize,
    /// Age of the current advertised snapshot, `None` before the first commit.
    /// It keeps growing while production is stalled: the store never invents
    /// freshness without new media.
    pub(crate) publication_age: Option<Duration>,
}

/// Validates a published duration against the profile target and converts it
/// to integer microseconds.
fn validated_duration_us(duration: f64, profile: HlsProfile) -> Result<u64> {
    if !duration.is_finite() || duration <= 0.0 {
        bail!("refusing to publish a segment with invalid duration {duration}");
    }
    let micros = (duration * 1_000_000.0).round();
    if micros < 1.0 {
        bail!("segment duration {duration} rounds to zero microseconds");
    }
    if micros > profile.target_duration_us() as f64 {
        bail!(
            "segment duration {duration:.3}s exceeds the {}s target duration",
            profile.target_duration_secs()
        );
    }
    Ok(micros as u64)
}

/// Splits Annex B data into NAL units, normalizing away the start codes and
/// stripping trailing zero padding that belongs to a 4-byte start code.
fn split_annex_b(buf: &[u8]) -> Vec<Nal<'_>> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= buf.len() {
        if buf[index] == 0 && buf[index + 1] == 0 && buf[index + 2] == 1 {
            starts.push(index + 3);
            index += 3;
        } else {
            index += 1;
        }
    }

    let mut nals = Vec::with_capacity(starts.len());
    for (position, &start) in starts.iter().enumerate() {
        let end = match starts.get(position + 1) {
            Some(&next) => {
                let mut end = next - 3;
                if end > start && buf[end - 1] == 0 {
                    end -= 1;
                }
                end
            }
            None => {
                let mut end = buf.len();
                while end > start && buf[end - 1] == 0 {
                    end -= 1;
                }
                end
            }
        };
        if end <= start {
            continue;
        }
        nals.push(Nal {
            nal_type: buf[start] & 0x1F,
            bytes: &buf[start..end],
        });
    }
    nals
}

/// Removes emulation prevention bytes from an EBSP.
fn unescape_rbsp(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut zeros = 0usize;
    for &byte in ebsp {
        if zeros >= 2 && byte == 0x03 {
            zeros = 0;
            continue;
        }
        out.push(byte);
        if byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

/// Derives the RFC 6381 `avc1.PPCCLL` string from an SPS NAL unit.
fn parse_codec_string(sps_nal: &[u8]) -> Option<String> {
    let rbsp = unescape_rbsp(sps_nal.get(1..)?);
    let [profile, constraints, level, ..] = rbsp.as_slice() else {
        return None;
    };
    Some(format!("avc1.{profile:02x}{constraints:02x}{level:02x}"))
}

/// Validates exactly one complete fixed-profile AAC-LC 44.1 kHz stereo ADTS
/// frame and returns its header length and fixed configuration.
///
/// Rejects anything that is not exactly one frame: wrong sync/layer, a non-LC
/// profile, a non-44100 Hz sampling frequency index, a non-stereo channel
/// configuration, more than one raw data block, a `frame_length` that does not
/// match the pushed bytes (trailing garbage or a truncated frame) and frames
/// over the 13-bit length cap.
fn parse_adts_frame(adts: &[u8]) -> Result<(usize, AacConfig)> {
    if adts.len() > MAX_ADTS_FRAME_BYTES {
        bail!(
            "ADTS frame is {} bytes, over the {MAX_ADTS_FRAME_BYTES}-byte (13-bit frame_length) cap",
            adts.len()
        );
    }
    if adts.len() < ADTS_HEADER_BYTES {
        bail!(
            "ADTS frame is {} bytes, shorter than the {ADTS_HEADER_BYTES}-byte header",
            adts.len()
        );
    }
    if adts[0] != 0xFF || adts[1] & 0xF0 != 0xF0 {
        bail!("ADTS frame is missing the 0xFFF sync word");
    }
    if adts[1] & 0x06 != 0 {
        bail!("ADTS layer must be 0");
    }
    let protection_absent = adts[1] & 0x01 != 0;
    let header_len = if protection_absent {
        ADTS_HEADER_BYTES
    } else {
        ADTS_HEADER_BYTES_CRC
    };
    if adts.len() < header_len {
        bail!(
            "ADTS frame is {} bytes, shorter than its {header_len}-byte header",
            adts.len()
        );
    }
    let profile = (adts[2] >> 6) & 0x03;
    if profile != ADTS_PROFILE_LC {
        bail!(
            "ADTS profile object type {} is not AAC-LC (this muxer only accepts AAC-LC)",
            profile + 1
        );
    }
    let frequency_index = (adts[2] >> 2) & 0x0F;
    if frequency_index != ADTS_FREQ_INDEX_44100 {
        bail!("ADTS sampling_frequency_index {frequency_index} is not 44100 Hz");
    }
    let channel_configuration = ((adts[2] & 0x01) << 2) | ((adts[3] >> 6) & 0x03);
    if channel_configuration != ADTS_CHANNEL_CONFIG_STEREO {
        bail!("ADTS channel_configuration {channel_configuration} is not stereo");
    }
    let frame_len =
        (((adts[3] & 0x03) as usize) << 11) | ((adts[4] as usize) << 3) | ((adts[5] as usize) >> 5);
    if frame_len != adts.len() {
        bail!(
            "ADTS frame_length {frame_len} does not match the {} bytes pushed (trailing garbage or truncated frame)",
            adts.len()
        );
    }
    if frame_len <= header_len {
        bail!("ADTS frame carries no AAC payload");
    }
    if adts[6] & 0x03 != 0 {
        bail!("ADTS number_of_raw_data_blocks_in_frame must be 0 (one frame per ADTS frame)");
    }
    Ok((
        header_len,
        AacConfig {
            protection_absent,
            profile,
            frequency_index,
            channel_configuration,
        },
    ))
}

/// Appends `nal` with a 4-byte Annex B start code.
fn append_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(nal);
}

/// Converts microseconds to 33-bit-wrapped 90 kHz ticks.
fn pts_90k(pts_us: u64) -> u64 {
    (((pts_us as u128) * 9 / 100) as u64) & PTS33_MASK
}

/// Encodes a 33-bit PTS into the 5-byte PES field.
fn encode_pts(pts: u64) -> [u8; 5] {
    [
        0x21 | (((pts >> 29) as u8) & 0x0E),
        ((pts >> 22) & 0xFF) as u8,
        0x01 | (((pts >> 14) as u8) & 0xFE),
        ((pts >> 7) & 0xFF) as u8,
        0x01 | (((pts << 1) as u8) & 0xFE),
    ]
}

/// Writes a 33-bit PCR base plus a zero extension into 6 bytes.
fn write_pcr(out: &mut [u8], pcr_90k: u64) {
    let base = pcr_90k & PTS33_MASK;
    out[0] = (base >> 25) as u8;
    out[1] = (base >> 17) as u8;
    out[2] = (base >> 9) as u8;
    out[3] = (base >> 1) as u8;
    out[4] = (((base & 1) as u8) << 7) | 0x7E;
    out[5] = 0;
}

/// CRC-32/MPEG-2: poly 0x04C11DB7, init all ones, no reflection, no final XOR.
fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// PAT: single program, PMT on [`PID_PMT`].
fn pat_section() -> [u8; 16] {
    let mut section = [0u8; 16];
    section[0] = 0x00;
    section[1] = 0xB0; // section_syntax_indicator, reserved, length high bits
    section[2] = 0x0D; // 13 bytes after the section_length field
    section[3] = 0x00;
    section[4] = 0x01; // transport_stream_id
    section[5] = 0xC1; // reserved, version 0, current_next
    section[6] = 0x00; // section_number
    section[7] = 0x00; // last_section_number
    section[8] = 0x00;
    section[9] = 0x01; // program_number
    section[10] = 0xE0 | ((PID_PMT >> 8) as u8 & 0x1F);
    section[11] = (PID_PMT & 0xFF) as u8;
    let crc = crc32_mpeg2(&section[..12]);
    section[12..16].copy_from_slice(&crc.to_be_bytes());
    section
}

/// PMT: H.264 on [`PID_VIDEO`] plus, on an AAC muxer, AAC on [`PID_AUDIO`].
/// PCR stays on the video PID in both modes.
fn pmt_section(aac: bool) -> Vec<u8> {
    // 13 fixed bytes after the section_length field, 5 per elementary stream
    // and 4 CRC bytes.
    let section_length = 13 + 5 * if aac { 2 } else { 1 };
    let mut section = vec![0u8; 3 + section_length];
    section[0] = 0x02;
    section[1] = 0xB0;
    section[2] = section_length as u8;
    section[3] = 0x00;
    section[4] = 0x01; // program_number
    section[5] = 0xC1; // reserved, version 0, current_next
    section[6] = 0x00; // section_number
    section[7] = 0x00; // last_section_number
    section[8] = 0xE0 | ((PID_VIDEO >> 8) as u8 & 0x1F);
    section[9] = (PID_VIDEO & 0xFF) as u8; // PCR_PID
    section[10] = 0xF0; // reserved, program_info_length high bits
    section[11] = 0x00;
    section[12] = STREAM_TYPE_H264;
    section[13] = 0xE0 | ((PID_VIDEO >> 8) as u8 & 0x1F);
    section[14] = (PID_VIDEO & 0xFF) as u8; // elementary_PID
    section[15] = 0xF0; // reserved, ES_info_length high bits
    section[16] = 0x00;
    if aac {
        section[17] = STREAM_TYPE_AAC;
        section[18] = 0xE0 | ((PID_AUDIO >> 8) as u8 & 0x1F);
        section[19] = (PID_AUDIO & 0xFF) as u8; // elementary_PID
        section[20] = 0xF0; // reserved, ES_info_length high bits
        section[21] = 0x00;
    }
    let crc = crc32_mpeg2(&section[..section.len() - 4]);
    let crc_at = section.len() - 4;
    section[crc_at..].copy_from_slice(&crc.to_be_bytes());
    section
}

/// Appends one PSI section as a single payload-only packet with 0xFF stuffing.
fn append_psi(out: &mut Vec<u8>, pid: u16, section: &[u8], cc: &mut u8) {
    let mut packet = [0xFFu8; TS_PACKET_SIZE];
    packet[0] = 0x47;
    packet[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
    packet[2] = (pid & 0xFF) as u8;
    packet[3] = 0x10 | (*cc & 0x0F);
    *cc = (*cc + 1) & 0x0F;
    packet[4] = 0x00; // pointer_field
    packet[5..5 + section.len()].copy_from_slice(section);
    out.extend_from_slice(&packet);
}

/// Exact number of transport-stream bytes used by one PES packet carrying
/// `payload_len` bytes. With `with_pcr` the first packet holds 176 bytes after
/// the 12-byte header plus PCR adaptation field; without it the first packet
/// holds 184. Every following packet holds 184.
fn packetized_pes_len(payload_len: usize, with_pcr: bool) -> usize {
    let pes_len = payload_len + 14; // 6-byte PES header + 3 flag/len bytes + 5 PTS bytes
    let first_capacity = if with_pcr {
        TS_PACKET_SIZE - 12
    } else {
        TS_PACKET_SIZE - 4
    };
    let packets = if pes_len <= first_capacity {
        1
    } else {
        1 + (pes_len - first_capacity).div_ceil(TS_PACKET_SIZE - 4)
    };
    packets * TS_PACKET_SIZE
}

/// Appends one `payload` as a PES packet on `pid`, split into 188-byte TS
/// packets.
///
/// The first packet carries an adaptation field with the PCR (and the random
/// access indicator for video IDRs) when `pcr` is `Some`; audio PES packets
/// pass `None` and always carry a mandatory bounded PES length. The final
/// packet is padded with adaptation stuffing so no PES payload byte is ever
/// dropped or duplicated.
#[allow(clippy::too_many_arguments)] // one packet writer for both elementary streams
fn append_pes(
    out: &mut Vec<u8>,
    pid: u16,
    stream_id: u8,
    cc: &mut u8,
    pts: u64,
    pcr: Option<u64>,
    random_access: bool,
    payload: &[u8],
) {
    let mut pes = Vec::with_capacity(payload.len() + 14);
    pes.extend_from_slice(&[0x00, 0x00, 0x01, stream_id]);
    let pes_len = payload.len() + 8; // 3 flag bytes + 5 PTS bytes + payload
    if pes_len <= u16::MAX as usize {
        pes.extend_from_slice(&(pes_len as u16).to_be_bytes());
    } else {
        pes.extend_from_slice(&[0x00, 0x00]); // unbounded length is legal for video
    }
    pes.push(0x80); // marker bits, no scrambling
    pes.push(0x80); // PTS only, no DTS (no B-frames)
    pes.push(5);
    pes.extend_from_slice(&encode_pts(pts));
    pes.extend_from_slice(payload);

    let mut offset = 0usize;
    let mut first = true;
    while offset < pes.len() {
        let mut packet = [0xFFu8; TS_PACKET_SIZE];
        packet[0] = 0x47;
        packet[1] = if first { 0x40 } else { 0x00 } | ((pid >> 8) as u8 & 0x1F);
        packet[2] = (pid & 0xFF) as u8;

        let remaining = pes.len() - offset;
        let capacity = if first && pcr.is_some() {
            TS_PACKET_SIZE - 12
        } else {
            TS_PACKET_SIZE - 4
        };
        let take = remaining.min(capacity);
        let adaptation = TS_PACKET_SIZE - 4 - take;
        let afc = if adaptation == 0 { 0b01 } else { 0b11 };
        packet[3] = (afc << 4) | (*cc & 0x0F);
        *cc = (*cc + 1) & 0x0F;

        let mut index = 4;
        if afc == 0b11 {
            let af_len = adaptation - 1;
            packet[index] = af_len as u8;
            if first && let Some(pcr_90k) = pcr {
                packet[index + 1] = 0x10 | if random_access { 0x40 } else { 0x00 }; // PCR flag, RAI
                write_pcr(&mut packet[index + 2..index + 8], pcr_90k);
            } else if af_len > 0 {
                packet[index + 1] = 0x00; // adaptation flags
            }
            index = 4 + adaptation; // stuffing bytes stay 0xFF
        }
        packet[index..index + take].copy_from_slice(&pes[offset..offset + take]);
        out.extend_from_slice(&packet);
        offset += take;
        first = false;
    }
}

/// Appends an adaptation-only packet that refreshes the video PID's PCR
/// without a payload. `cc` must be the last transmitted payload continuity
/// counter: MPEG-TS 2.4.3.3 only increments the counter for packets carrying a
/// payload, so this packet repeats that value and the caller's pending counter
/// stays untouched for the next payload packet.
fn append_pcr_only(out: &mut Vec<u8>, cc: u8, pcr_90k: u64) {
    let mut packet = [0xFFu8; TS_PACKET_SIZE];
    packet[0] = 0x47;
    packet[1] = (PID_VIDEO >> 8) as u8 & 0x1F; // payload_unit_start clear
    packet[2] = (PID_VIDEO & 0xFF) as u8;
    packet[3] = 0x20 | (cc & 0x0F); // adaptation only, cc unchanged
    packet[4] = (TS_PACKET_SIZE - 5) as u8; // flags + 6 PCR bytes + stuffing
    packet[5] = 0x10; // PCR flag
    write_pcr(&mut packet[6..12], pcr_90k);
    out.extend_from_slice(&packet);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Independent readers below deliberately avoid the production helpers, so
    // the tests validate real TS bytes rather than echoing the writer.

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xAC];
    const PPS: &[u8] = &[0x68, 0xCE, 0x38, 0x80];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x21, 0x11];
    const P_FRAME: &[u8] = &[0x41, 0x9A, 0x22, 0x33];
    const AUD: &[u8] = &[0x09, 0xF0];

    fn start_code() -> [u8; 4] {
        [0x00, 0x00, 0x00, 0x01]
    }

    fn nal(bytes: &[u8]) -> Vec<u8> {
        let mut out = start_code().to_vec();
        out.extend_from_slice(bytes);
        out
    }

    fn config_au(idr: bool) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&nal(SPS));
        out.extend_from_slice(&nal(PPS));
        out.extend_from_slice(&nal(if idr { IDR } else { P_FRAME }));
        out
    }

    fn au_with_aud(idr: bool) -> Vec<u8> {
        let mut out = nal(AUD);
        out.extend_from_slice(&nal(SPS));
        out.extend_from_slice(&nal(PPS));
        out.extend_from_slice(&nal(if idr { IDR } else { P_FRAME }));
        out
    }

    fn p_au() -> Vec<u8> {
        nal(P_FRAME)
    }

    fn expected_au_payload(idr: bool, with_config: bool) -> Vec<u8> {
        let mut out = nal(AUD);
        if with_config {
            out.extend_from_slice(&nal(SPS));
            out.extend_from_slice(&nal(PPS));
        }
        out.extend_from_slice(&nal(if idr { IDR } else { P_FRAME }));
        out
    }

    fn ticks_for_test(pts_us: u64) -> u64 {
        (((pts_us as u128) * 9 / 100) as u64 + 90_000) & ((1u64 << 33) - 1)
    }

    /// Payload of the synthetic ADTS frames; real AAC bytes are not needed to
    /// validate TS framing, so these are opaque.
    const AAC_PAYLOAD: &[u8] = &[0x21, 0x10, 0x04, 0x60, 0x8C, 0x1C];

    fn adts_frame(payload: &[u8]) -> Vec<u8> {
        adts_frame_with_protection(payload, true)
    }

    /// Builds exactly one structurally valid AAC-LC 44.1 kHz stereo ADTS frame.
    fn adts_frame_with_protection(payload: &[u8], protection_absent: bool) -> Vec<u8> {
        let header_len = if protection_absent { 7 } else { 9 };
        let frame_len = header_len + payload.len();
        assert!(
            frame_len <= MAX_ADTS_FRAME_BYTES,
            "test ADTS frame too long"
        );
        let mut frame = Vec::with_capacity(frame_len);
        frame.push(0xFF);
        // MPEG-4, layer 0, protection_absent.
        frame.push(0xF0 | u8::from(protection_absent));
        // profile AAC-LC (1), 44100 Hz index (4), stereo high channel bit (0).
        frame.push((1 << 6) | (4 << 2));
        frame.push(0x80 | ((frame_len >> 11) as u8 & 0x03));
        frame.push(((frame_len >> 3) & 0xFF) as u8);
        frame.push((((frame_len & 0x07) as u8) << 5) | 0x1F);
        frame.push(0xFC); // buffer fullness low bits, number_of_raw_data_blocks 0
        if !protection_absent {
            frame.extend_from_slice(&[0xAB, 0xCD]); // CRC contents are not inspected
        }
        frame.extend_from_slice(payload);
        frame
    }

    #[derive(Debug)]
    struct ParsedPacket {
        pid: u16,
        pusi: bool,
        afc: u8,
        cc: u8,
        random_access: bool,
        pcr_base: Option<u64>,
        pcr_extension: u64,
        payload: Vec<u8>,
    }

    fn parse_packets(data: &[u8]) -> Vec<ParsedPacket> {
        let (chunks, remainder) = data.as_chunks::<188>();
        assert!(
            remainder.is_empty(),
            "TS data is not a whole number of packets"
        );
        let mut packets = Vec::new();
        for chunk in chunks {
            assert_eq!(chunk[0], 0x47, "packet is missing the sync byte");
            let pusi = chunk[1] & 0x40 != 0;
            let pid = (((chunk[1] & 0x1F) as u16) << 8) | chunk[2] as u16;
            let afc = (chunk[3] >> 4) & 0x03;
            let cc = chunk[3] & 0x0F;
            let mut random_access = false;
            let mut pcr_base = None;
            let mut pcr_extension = 0u64;
            let mut index = 4;
            if afc & 0x02 != 0 {
                let af_len = chunk[4] as usize;
                assert!(5 + af_len <= 188, "adaptation field overruns the packet");
                if af_len >= 1 {
                    let flags = chunk[5];
                    random_access = flags & 0x40 != 0;
                    if flags & 0x10 != 0 {
                        assert!(af_len >= 7, "PCR flag set without room for six PCR bytes");
                        let bytes = &chunk[6..12];
                        let base = ((bytes[0] as u64) << 25)
                            | ((bytes[1] as u64) << 17)
                            | ((bytes[2] as u64) << 9)
                            | ((bytes[3] as u64) << 1)
                            | ((bytes[4] as u64) >> 7);
                        pcr_extension = (((bytes[4] as u64) & 1) << 8) | bytes[5] as u64;
                        pcr_base = Some(base);
                    }
                }
                index = 5 + af_len;
            }
            let payload = if afc & 0x01 != 0 {
                chunk[index..].to_vec()
            } else {
                Vec::new()
            };
            packets.push(ParsedPacket {
                pid,
                pusi,
                afc,
                cc,
                random_access,
                pcr_base,
                pcr_extension,
                payload,
            });
        }
        packets
    }

    fn assert_continuity(packets: &[ParsedPacket]) {
        let mut last: std::collections::HashMap<u16, u8> = std::collections::HashMap::new();
        for packet in packets {
            if packet.afc & 0x01 == 0 {
                continue;
            }
            if let Some(previous) = last.insert(packet.pid, packet.cc) {
                assert_eq!(
                    packet.cc,
                    (previous + 1) & 0x0F,
                    "continuity counter gap on PID {:#06x}",
                    packet.pid
                );
            }
        }
    }

    /// Independent CRC check: running the CRC over data plus its CRC is zero.
    fn crc32_ok(section: &[u8]) -> bool {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in section {
            crc ^= (byte as u32) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04C1_1DB7
                } else {
                    crc << 1
                };
            }
        }
        crc == 0
    }

    fn find_section(packets: &[ParsedPacket], pid: u16) -> Vec<u8> {
        let mut section = Vec::new();
        for packet in packets.iter().filter(|packet| packet.pid == pid) {
            if packet.pusi {
                section.clear();
                let pointer = packet.payload[0] as usize;
                section.extend_from_slice(&packet.payload[1 + pointer..]);
            } else {
                section.extend_from_slice(&packet.payload);
            }
            if section.len() >= 3 {
                let length = (((section[1] & 0x0F) as usize) << 8) | section[2] as usize;
                if section.len() >= 3 + length {
                    section.truncate(3 + length);
                    break;
                }
            }
        }
        section
    }

    #[derive(Debug)]
    struct ParsedPes {
        pts: u64,
        pcr_base: Option<u64>,
        pcr_extension: u64,
        random_access: bool,
        body: Vec<u8>,
    }

    fn parse_pes(packets: &[ParsedPacket]) -> Vec<ParsedPes> {
        let mut result = Vec::new();
        let mut current: Vec<u8> = Vec::new();
        let mut current_pcr = None;
        let mut current_extension = 0u64;
        let mut current_rai = false;
        for packet in packets.iter().filter(|packet| packet.pid == PID_VIDEO) {
            if packet.pusi && !current.is_empty() {
                result.push(decode_pes(
                    &current,
                    current_pcr,
                    current_extension,
                    current_rai,
                ));
                current.clear();
                current_pcr = None;
                current_extension = 0;
                current_rai = false;
            }
            if current.is_empty() {
                current_pcr = packet.pcr_base;
                current_extension = packet.pcr_extension;
                current_rai = packet.random_access;
            }
            current.extend_from_slice(&packet.payload);
        }
        if !current.is_empty() {
            result.push(decode_pes(
                &current,
                current_pcr,
                current_extension,
                current_rai,
            ));
        }
        result
    }

    fn decode_pes(
        data: &[u8],
        pcr_base: Option<u64>,
        pcr_extension: u64,
        random_access: bool,
    ) -> ParsedPes {
        assert_eq!(&data[..3], &[0x00, 0x00, 0x01], "missing PES start code");
        assert_eq!(data[3], 0xE0, "unexpected PES stream id");
        let pes_len = u16::from_be_bytes([data[4], data[5]]) as usize;
        if pes_len != 0 {
            assert_eq!(pes_len, data.len() - 6, "PES packet length mismatch");
        }
        assert_eq!(data[6] & 0xC0, 0x80, "PES marker bits missing");
        assert_eq!(data[7] & 0xC0, 0x80, "PTS_DTS flags missing");
        let header_len = data[8] as usize;
        assert!(header_len >= 5, "PTS flag set without PTS bytes");
        let bytes = &data[9..14];
        let pts = (((bytes[0] as u64) >> 1 & 0x07) << 30)
            | ((bytes[1] as u64) << 22)
            | ((bytes[2] as u64) >> 1) << 15
            | ((bytes[3] as u64) << 7)
            | ((bytes[4] as u64) >> 1);
        ParsedPes {
            pts,
            pcr_base,
            pcr_extension,
            random_access,
            body: data[9 + header_len..].to_vec(),
        }
    }

    /// Independent audio PES reader: stream_id 0xC0 with a mandatory bounded
    /// 16-bit PES length and a PTS-only header.
    fn parse_audio_pes(packets: &[ParsedPacket]) -> Vec<ParsedPes> {
        let mut result = Vec::new();
        let mut current: Vec<u8> = Vec::new();
        for packet in packets.iter().filter(|packet| packet.pid == PID_AUDIO) {
            if packet.pusi && !current.is_empty() {
                result.push(decode_audio_pes(&current));
                current.clear();
            }
            current.extend_from_slice(&packet.payload);
        }
        if !current.is_empty() {
            result.push(decode_audio_pes(&current));
        }
        result
    }

    fn decode_audio_pes(data: &[u8]) -> ParsedPes {
        assert_eq!(&data[..3], &[0x00, 0x00, 0x01], "missing PES start code");
        assert_eq!(data[3], 0xC0, "audio PES stream id must be 0xC0");
        let pes_len = u16::from_be_bytes([data[4], data[5]]) as usize;
        assert_ne!(pes_len, 0, "audio PES length must be present");
        assert_eq!(pes_len, data.len() - 6, "audio PES packet length mismatch");
        assert_eq!(data[6] & 0xC0, 0x80, "PES marker bits missing");
        assert_eq!(data[7] & 0xC0, 0x80, "PTS_DTS flags missing");
        assert_eq!(data[8], 5, "audio PES must carry a PTS-only header");
        let bytes = &data[9..14];
        let pts = (((bytes[0] as u64) >> 1 & 0x07) << 30)
            | ((bytes[1] as u64) << 22)
            | ((bytes[2] as u64) >> 1) << 15
            | ((bytes[3] as u64) << 7)
            | ((bytes[4] as u64) >> 1);
        ParsedPes {
            pts,
            pcr_base: None,
            pcr_extension: 0,
            random_access: false,
            body: data[14..].to_vec(),
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct PmtStream {
        stream_type: u8,
        pid: u16,
    }

    /// Independent PMT elementary-stream reader, including section_length and
    /// ES_info_length consistency.
    fn pmt_streams(pmt: &[u8]) -> Vec<PmtStream> {
        assert!(pmt.len() >= 12, "PMT too short");
        assert_eq!(pmt[0], 0x02, "PMT table id");
        let section_length = (((pmt[1] & 0x0F) as usize) << 8) | pmt[2] as usize;
        assert_eq!(pmt.len(), 3 + section_length, "PMT section_length mismatch");
        let program_info_length = (((pmt[10] & 0x0F) as usize) << 8) | pmt[11] as usize;
        let mut index = 12 + program_info_length;
        let end = pmt.len() - 4;
        let mut streams = Vec::new();
        while index + 5 <= end {
            let stream_type = pmt[index];
            let pid = (((pmt[index + 1] & 0x1F) as u16) << 8) | pmt[index + 2] as u16;
            let es_info_length =
                (((pmt[index + 3] & 0x0F) as usize) << 8) | pmt[index + 4] as usize;
            index += 5 + es_info_length;
            streams.push(PmtStream { stream_type, pid });
        }
        assert_eq!(index, end, "ES loop must end exactly at the CRC");
        streams
    }

    /// Demuxes video and audio PES packets in transport order, which is the
    /// order the muxer received the samples in.
    fn demux_all_pes(packets: &[ParsedPacket]) -> Vec<(u16, ParsedPes)> {
        let mut result = Vec::new();
        let mut current: Option<(u16, Vec<u8>)> = None;
        let mut current_pcr = None;
        let mut current_extension = 0u64;
        let mut current_rai = false;
        for packet in packets
            .iter()
            .filter(|packet| packet.pid == PID_VIDEO || packet.pid == PID_AUDIO)
        {
            if packet.pusi {
                if let Some((pid, data)) = current.take() {
                    result.push((
                        pid,
                        decode_any_pes(pid, &data, current_pcr, current_extension, current_rai),
                    ));
                }
                current = Some((packet.pid, packet.payload.clone()));
                current_pcr = packet.pcr_base;
                current_extension = packet.pcr_extension;
                current_rai = packet.random_access;
            } else if let Some((_, data)) = current.as_mut() {
                data.extend_from_slice(&packet.payload);
            }
        }
        if let Some((pid, data)) = current {
            result.push((
                pid,
                decode_any_pes(pid, &data, current_pcr, current_extension, current_rai),
            ));
        }
        result
    }

    fn decode_any_pes(
        pid: u16,
        data: &[u8],
        pcr_base: Option<u64>,
        pcr_extension: u64,
        random_access: bool,
    ) -> ParsedPes {
        if pid == PID_AUDIO {
            decode_audio_pes(data)
        } else {
            decode_pes(data, pcr_base, pcr_extension, random_access)
        }
    }

    /// Minimal independent Annex B splitter used to inspect PES payloads.
    fn test_nal_types(buf: &[u8]) -> Vec<u8> {
        let mut starts = Vec::new();
        let mut index = 0;
        while index + 3 <= buf.len() {
            if buf[index] == 0 && buf[index + 1] == 0 && buf[index + 2] == 1 {
                starts.push(index + 3);
                index += 3;
            } else {
                index += 1;
            }
        }
        let mut types = Vec::new();
        for (position, &start) in starts.iter().enumerate() {
            let end = match starts.get(position + 1) {
                Some(&next) => next - 3,
                None => buf.len(),
            };
            if end > start {
                types.push(buf[start] & 0x1F);
            }
        }
        types
    }

    /// Builds a ~1 s segment at 30 fps and returns it together with the
    /// timestamps of the access units it contains.
    fn segment_for_test(idr_pts: u64, idr_duration_us: u64) -> (Segment, Vec<u64>) {
        let mut timestamps = vec![idr_pts];
        let mut muxer = HlsMuxer::new();
        let mut time = idr_pts;
        muxer.push(&config_au(true), time).unwrap();
        while time + 33_333 < idr_pts + idr_duration_us {
            time += 33_333;
            timestamps.push(time);
            muxer.push(&p_au(), time).unwrap();
        }
        let seal = idr_pts + idr_duration_us;
        let segment = muxer
            .push(&config_au(true), seal)
            .unwrap()
            .expect("second IDR seals the first segment");
        (segment, timestamps)
    }

    #[test]
    fn profiles_expose_documented_durations() {
        assert_eq!(HlsProfile::default(), HlsProfile::Stable);

        assert_eq!(HlsProfile::Stable.target_duration_secs(), 2);
        assert_eq!(HlsProfile::Stable.target_duration_us(), 2_000_000);
        assert_eq!(HlsProfile::Stable.segment_duration_us(), 1_000_000);
        assert_eq!(HlsProfile::Stable.min_segment_duration_us(), 900_000);
        assert_eq!(HlsProfile::Stable.ready_duration_us(), 8_000_000);
        assert_eq!(
            HlsProfile::Stable.snapshot_min_interval(),
            Duration::from_secs(1)
        );

        assert_eq!(HlsProfile::Responsive.target_duration_secs(), 1);
        assert_eq!(HlsProfile::Responsive.target_duration_us(), 1_000_000);
        assert_eq!(HlsProfile::Responsive.segment_duration_us(), 500_000);
        assert_eq!(HlsProfile::Responsive.min_segment_duration_us(), 450_000);
        assert_eq!(HlsProfile::Responsive.ready_duration_us(), 4_000_000);
        assert_eq!(
            HlsProfile::Responsive.snapshot_min_interval(),
            Duration::from_millis(500),
            "a whole-second divide would round the responsive cadence to zero"
        );
        assert_ne!(
            HlsProfile::Responsive.snapshot_min_interval(),
            Duration::ZERO
        );
    }

    /// The stable profile is the historical byte-for-byte path: the explicit
    /// constructor and the defaults must produce identical TS bytes.
    #[test]
    fn stable_profile_matches_the_default_muxer_bytes() {
        let t0 = 5_000_000u64;
        let mut default = HlsMuxer::new();
        let mut explicit = HlsMuxer::with_profile(HlsProfile::Stable, false);
        default.push(&config_au(true), t0).unwrap();
        explicit.push(&config_au(true), t0).unwrap();
        let mut time = t0;
        while time + 33_333 < t0 + 1_000_000 {
            time += 33_333;
            default.push(&p_au(), time).unwrap();
            explicit.push(&p_au(), time).unwrap();
        }
        let from_default = default
            .push(&config_au(true), t0 + 1_000_000)
            .unwrap()
            .unwrap();
        let from_explicit = explicit
            .push(&config_au(true), t0 + 1_000_000)
            .unwrap()
            .unwrap();
        assert_eq!(from_default.sequence, from_explicit.sequence);
        assert_eq!(from_default.duration, from_explicit.duration);
        assert_eq!(from_default.data, from_explicit.data);
        assert_eq!(default.codec(), explicit.codec());

        // The AAC path is identical too.
        let mut default = HlsMuxer::with_aac();
        let mut explicit = HlsMuxer::with_profile(HlsProfile::Stable, true);
        default.push(&config_au(true), 0).unwrap();
        explicit.push(&config_au(true), 0).unwrap();
        default
            .push_audio(&adts_frame(AAC_PAYLOAD), 100_000)
            .unwrap();
        explicit
            .push_audio(&adts_frame(AAC_PAYLOAD), 100_000)
            .unwrap();
        let from_default = default.push(&config_au(true), 1_000_000).unwrap().unwrap();
        let from_explicit = explicit.push(&config_au(true), 1_000_000).unwrap().unwrap();
        assert_eq!(from_default.data, from_explicit.data);
        assert_eq!(default.codec(), Some("avc1.640028,mp4a.40.2"));
    }

    /// Golden fingerprint of the stable one-second fixture: the muxed TS bytes
    /// of the default profile must not drift.
    #[test]
    fn stable_fixture_bytes_are_unchanged() {
        let (segment, _timestamps) = segment_for_test(5_000_000, 1_000_000);
        let mut fingerprint = 0xcbf2_9ce4_8422_2325u64;
        for &byte in &segment.data {
            fingerprint ^= u64::from(byte);
            fingerprint = fingerprint.wrapping_mul(0x0000_0100_0000_01b3);
        }
        assert_eq!(segment.duration, 1.0);
        assert_eq!(segment.data.len(), 6_204);
        assert_eq!(
            fingerprint, 0x152b_728d_5299_82b0,
            "the stable TS fixture bytes must not change"
        );
    }

    #[test]
    fn responsive_idr_seals_at_half_a_second_but_not_before() {
        let mut muxer = HlsMuxer::with_profile(HlsProfile::Responsive, false);
        let t0 = 1_000_000u64;
        muxer.push(&config_au(true), t0).unwrap();
        assert!(
            muxer
                .push(&config_au(true), t0 + 400_000)
                .unwrap()
                .is_none(),
            "an IDR before the 450 ms seal minimum must not seal"
        );
        let segment = muxer
            .push(&config_au(true), t0 + 500_000)
            .unwrap()
            .expect("an IDR at 500 ms seals the responsive segment");
        assert_eq!(segment.sequence, 0);
        assert!((segment.duration - 0.5).abs() < 1e-9);
        assert!(
            muxer.pending.is_some(),
            "the sealing IDR opens the next segment"
        );
    }

    #[test]
    fn responsive_profile_rejects_media_past_one_second() {
        // An open video segment without a sealable IDR.
        let mut muxer = HlsMuxer::with_profile(HlsProfile::Responsive, false);
        muxer.push(&config_au(true), 0).unwrap();
        muxer.push(&p_au(), 900_000).unwrap();
        let error = muxer.push(&p_au(), 1_000_001).unwrap_err();
        assert!(
            error.to_string().contains("target 1s"),
            "unexpected: {error}"
        );

        // A segment sealed by an IDR past the 1s target.
        let mut muxer = HlsMuxer::with_profile(HlsProfile::Responsive, false);
        muxer.push(&config_au(true), 0).unwrap();
        let error = muxer.push(&config_au(true), 1_200_000).unwrap_err();
        assert!(
            error.to_string().contains("completed segment"),
            "unexpected: {error}"
        );
        assert!(
            error.to_string().contains("1s target duration"),
            "unexpected: {error}"
        );

        // Audio past the 1s target.
        let mut muxer = HlsMuxer::with_profile(HlsProfile::Responsive, true);
        muxer.push(&config_au(true), 0).unwrap();
        muxer.push_audio(&adts_frame(AAC_PAYLOAD), 900_000).unwrap();
        let error = muxer
            .push_audio(&adts_frame(AAC_PAYLOAD), 1_000_001)
            .unwrap_err();
        assert!(
            error.to_string().contains("1s target duration"),
            "unexpected: {error}"
        );

        // Exactly the target duration stays accepted (the cap is exclusive).
        let mut muxer = HlsMuxer::with_profile(HlsProfile::Responsive, true);
        muxer.push(&config_au(true), 0).unwrap();
        muxer
            .push_audio(&adts_frame(AAC_PAYLOAD), 1_000_000)
            .unwrap();
        muxer.push(&p_au(), 1_000_000).unwrap();
    }

    #[test]
    fn derives_codec_from_sps() {
        let mut muxer = HlsMuxer::new();
        assert_eq!(muxer.codec(), None);
        assert!(muxer.push(&config_au(true), 0).unwrap().is_none());
        assert_eq!(muxer.codec(), Some("avc1.640028"));
    }

    #[test]
    fn ignores_input_until_decodable_idr() {
        let mut muxer = HlsMuxer::new();
        assert!(muxer.push(&p_au(), 0).unwrap().is_none());
        assert!(muxer.push(&nal(SPS), 1_000).unwrap().is_none());
        assert!(muxer.push(&nal(PPS), 2_000).unwrap().is_none());
        assert!(
            muxer.pending.is_none(),
            "no segment may start before an IDR"
        );
        assert!(muxer.push(&config_au(true), 100_000).unwrap().is_none());
        assert!(muxer.pending.is_some());
    }

    #[test]
    fn seals_only_on_idr_after_min_duration() {
        let mut muxer = HlsMuxer::new();
        let t0 = 5_000_000u64;
        assert!(muxer.push(&config_au(true), t0).unwrap().is_none());
        assert!(muxer.push(&p_au(), t0 + 500_000).unwrap().is_none());
        assert!(muxer.push(&p_au(), t0 + 1_400_000).unwrap().is_none());
        let segment = muxer
            .push(&config_au(true), t0 + 1_600_000)
            .unwrap()
            .expect("IDR must seal the open segment");
        assert_eq!(segment.sequence, 0);
        assert!((segment.duration - 1.6).abs() < 1e-9);
        assert!(
            muxer.pending.is_some(),
            "the sealing IDR opens the next segment"
        );
        assert!(muxer.push(&p_au(), t0 + 1_633_333).unwrap().is_none());
        assert!(muxer.push(&p_au(), t0 + 1_666_666).unwrap().is_none());
    }

    #[test]
    fn early_idr_does_not_seal() {
        let mut muxer = HlsMuxer::new();
        let t0 = 0u64;
        assert!(muxer.push(&config_au(true), t0).unwrap().is_none());
        assert!(
            muxer
                .push(&config_au(true), t0 + 300_000)
                .unwrap()
                .is_none()
        );
        let segment = muxer
            .push(&config_au(true), t0 + 1_000_000)
            .unwrap()
            .expect("IDR after 1s seals");
        assert_eq!(segment.sequence, 0);
        assert!((segment.duration - 1.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_codec_change_midstream() {
        let mut muxer = HlsMuxer::new();
        muxer.push(&config_au(true), 0).unwrap();
        let mut changed = Vec::new();
        changed.extend_from_slice(&nal(&[0x67, 0x42, 0x00, 0x1F, 0xAC]));
        changed.extend_from_slice(&nal(PPS));
        changed.extend_from_slice(&nal(IDR));
        let error = muxer.push(&changed, 1_000_000).unwrap_err();
        assert!(
            error.to_string().contains("SPS changed midstream"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_timestamp_regression() {
        let mut muxer = HlsMuxer::new();
        muxer.push(&config_au(true), 10_000_000).unwrap();
        let error = muxer.push(&p_au(), 9_000_000).unwrap_err();
        assert!(
            error.to_string().contains("non-monotonic"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_segments_over_target_duration() {
        let mut muxer = HlsMuxer::new();
        muxer.push(&config_au(true), 0).unwrap();
        muxer.push(&p_au(), 1_500_000).unwrap();
        let error = muxer.push(&p_au(), 2_100_000).unwrap_err();
        assert!(
            error.to_string().contains("capture is too slow"),
            "unexpected error: {error}"
        );

        let mut muxer = HlsMuxer::new();
        muxer.push(&config_au(true), 0).unwrap();
        let error = muxer.push(&config_au(true), 2_500_000).unwrap_err();
        assert!(
            error.to_string().contains("target duration"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_oversized_segment_before_unbounded_growth() {
        let mut muxer = HlsMuxer::new();
        muxer.push(&config_au(true), 0).unwrap();
        let big_nals = vec![0x41u8; 512 * 1024];
        let mut time = 0u64;
        let mut error = None;
        for _ in 0..16 {
            time += 33_333;
            let before = muxer.pending.as_ref().unwrap().data.len();
            match muxer.push(&nal(&big_nals), time) {
                Ok(_) => {}
                Err(err) => {
                    assert_eq!(
                        muxer.pending.as_ref().unwrap().data.len(),
                        before,
                        "a rejected access unit must not partially extend the segment"
                    );
                    error = Some(err);
                    break;
                }
            }
        }
        let error = error.expect("oversized segment must be rejected");
        assert!(
            error.to_string().contains("MiB"),
            "unexpected error: {error}"
        );
        assert!(muxer.pending.as_ref().unwrap().data.len() <= MAX_SEGMENT_BYTES);
    }

    #[test]
    fn rejects_hostile_input_before_allocating() {
        // Oversized access unit: rejected before splitting/copying.
        let mut muxer = HlsMuxer::new();
        let too_big = vec![0x41u8; MAX_ACCESS_UNIT_BYTES + 5];
        let error = muxer.push(&nal(&too_big), 0).unwrap_err();
        assert!(
            error.to_string().contains("access unit"),
            "unexpected error: {error}"
        );
        assert!(muxer.pending.is_none());

        // Many tiny NALs: the NAL-count cap bounds vector/normalization growth.
        let mut au = Vec::new();
        for _ in 0..MAX_NALS_PER_ACCESS_UNIT + 1 {
            au.extend_from_slice(&[0x00, 0x00, 0x01, 0x41]);
        }
        let error = muxer.push(&au, 1).unwrap_err();
        assert!(
            error.to_string().contains("NAL cap"),
            "unexpected error: {error}"
        );
        assert!(muxer.pending.is_none());

        // Oversized SPS: bounded because it is repeated on every IDR.
        let oversized_sps = vec![0x67u8; MAX_CONFIG_NAL_BYTES + 1];
        let error = muxer.push(&nal(&oversized_sps), 2).unwrap_err();
        assert!(
            error.to_string().contains("SPS"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn continuity_and_pts_span_contiguous_segments() {
        let t0 = 3_000_000u64;
        let frame_us = 33_333u64;
        let mut muxer = HlsMuxer::new();
        let mut time = t0;
        muxer.push(&config_au(true), time).unwrap();
        let mut current_pts = vec![time];
        let mut built: Vec<(Segment, Vec<u64>)> = Vec::new();

        for _ in 0..3 {
            let next_idr = time + 1_000_000;
            while time + frame_us < next_idr {
                time += frame_us;
                muxer.push(&p_au(), time).unwrap();
                current_pts.push(time);
            }
            time = next_idr;
            let segment = muxer
                .push(&config_au(true), time)
                .unwrap()
                .expect("IDR seals the previous segment");
            built.push((segment, std::mem::take(&mut current_pts)));
            current_pts.push(time);
        }

        assert_eq!(built.len(), 3, "three contiguous segments expected");
        assert_eq!(
            built
                .iter()
                .map(|(segment, _)| segment.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        let expected_pts: Vec<u64> = built
            .iter()
            .flat_map(|(_, pts)| pts.iter().copied())
            .collect();

        // Parse the concatenated timeline as a receiver would when following
        // the playlist across segment boundaries.
        let mut concatenated = Vec::new();
        for (segment, _) in &built {
            concatenated.extend_from_slice(&segment.data);
        }
        let packets = parse_packets(&concatenated);
        assert_continuity(&packets);

        let cc_of = |pid: u16| -> Vec<u8> {
            packets
                .iter()
                .filter(|packet| packet.pid == pid)
                .map(|packet| packet.cc)
                .collect()
        };
        assert_eq!(
            cc_of(PID_PAT),
            vec![0, 1, 2],
            "PAT continuity must not reset at segment boundaries"
        );
        assert_eq!(
            cc_of(PID_PMT),
            vec![0, 1, 2],
            "PMT continuity must not reset at segment boundaries"
        );
        let video_cc = cc_of(PID_VIDEO);
        assert!(video_cc.len() > 16, "video stream should wrap the counter");
        assert!(
            video_cc
                .windows(2)
                .all(|pair| pair[1] == (pair[0] + 1) & 0x0F),
            "video continuity must not reset at segment boundaries"
        );

        let pes = parse_pes(&packets);
        assert_eq!(pes.len(), expected_pts.len());
        for (parsed, expected) in pes.iter().zip(&expected_pts) {
            assert_eq!(parsed.pts, ticks_for_test(*expected));
        }
        assert!(
            pes.windows(2).all(|pair| {
                let delta = pair[1].pts.wrapping_sub(pair[0].pts) & PTS33_MASK;
                (1..=90_001).contains(&delta)
            }),
            "PTS must advance monotonically across segment boundaries"
        );
    }

    #[test]
    fn ts_structure_is_valid() {
        let t0 = 5_000_000u64;
        let (segment, timestamps) = segment_for_test(t0, 1_000_000);
        assert!(segment.data.len() < MAX_SEGMENT_BYTES);
        assert_eq!(segment.data.len() % 188, 0);

        let packets = parse_packets(&segment.data);
        assert_continuity(&packets);

        let pat = find_section(&packets, PID_PAT);
        assert_eq!(pat[0], 0x00);
        assert_eq!(pat[1] & 0x80, 0x80, "PAT section_syntax_indicator missing");
        assert!(crc32_ok(&pat), "PAT CRC32/MPEG2 is wrong");
        let pmt_pid = (((pat[10] & 0x1F) as u16) << 8) | pat[11] as u16;
        assert_eq!(pmt_pid, PID_PMT);

        let pmt = find_section(&packets, PID_PMT);
        assert_eq!(pmt[0], 0x02);
        assert!(crc32_ok(&pmt), "PMT CRC32/MPEG2 is wrong");
        let program = ((pmt[3] as u16) << 8) | pmt[4] as u16;
        assert_eq!(program, 1);
        let pcr_pid = (((pmt[8] & 0x1F) as u16) << 8) | pmt[9] as u16;
        assert_eq!(pcr_pid, PID_VIDEO);
        assert_eq!(pmt[12], 0x1B, "H.264 stream type must be 0x1b");
        let elementary_pid = (((pmt[13] & 0x1F) as u16) << 8) | pmt[14] as u16;
        assert_eq!(elementary_pid, PID_VIDEO);

        let pes = parse_pes(&packets);
        assert_eq!(
            pes.len(),
            timestamps.len(),
            "one PES packet per access unit"
        );
        for (index, packet_pes) in pes.iter().enumerate() {
            assert_eq!(packet_pes.pts, ticks_for_test(timestamps[index]));
            if index == 0 {
                assert_eq!(packet_pes.body, expected_au_payload(true, true));
            } else {
                assert_eq!(packet_pes.body, expected_au_payload(false, false));
            }
        }
        assert!(pes.windows(2).all(|pair| pair[1].pts > pair[0].pts));

        for packet_pes in &pes {
            let pcr = packet_pes
                .pcr_base
                .expect("every access unit starts with a PCR");
            assert_eq!(packet_pes.pcr_extension, 0, "PCR extension should be zero");
            let lead = packet_pes.pts.wrapping_sub(pcr) & PTS33_MASK;
            assert_eq!(lead, 27_000, "PCR must lead PTS by 300ms");
        }
        for pair in pes.windows(2) {
            let first = pair[0].pcr_base.unwrap();
            let second = pair[1].pcr_base.unwrap();
            let gap = second.wrapping_sub(first) & PTS33_MASK;
            assert!(
                gap <= 3_600,
                "PCR repetition {gap} ticks exceeds 40ms at 30fps"
            );
        }
        assert!(
            pes[0].random_access,
            "IDR access unit needs the random access indicator"
        );
        assert!(!pes[1].random_access);
    }

    #[test]
    fn segments_start_with_decodable_idr_and_insert_aud() {
        let (segment, _timestamps) = segment_for_test(20_000_000, 1_000_000);
        let packets = parse_packets(&segment.data);
        let pes = parse_pes(&packets);
        for packet_pes in &pes {
            let types = test_nal_types(&packet_pes.body);
            assert_eq!(types.first(), Some(&NAL_AUD), "AUD must be present first");
        }
        let first_types = test_nal_types(&pes[0].body);
        assert_eq!(&first_types[..4], &[NAL_AUD, NAL_SPS, NAL_PPS, NAL_IDR]);

        // An access unit that already contains an AUD must not gain another.
        let mut muxer = HlsMuxer::new();
        muxer.push(&au_with_aud(true), 0).unwrap();
        muxer.push(&p_au(), 33_333).unwrap();
        let segment = muxer.push(&au_with_aud(true), 1_000_000).unwrap().unwrap();
        let pes = parse_pes(&parse_packets(&segment.data));
        for packet_pes in &pes {
            let auds = test_nal_types(&packet_pes.body)
                .iter()
                .filter(|&&nal_type| nal_type == NAL_AUD)
                .count();
            assert_eq!(auds, 1, "exactly one AUD expected");
        }
    }

    #[test]
    fn supports_33bit_pts_wrap() {
        // 95_443_720_000us is 8_589_934_800 90 kHz ticks: just past the
        // 33-bit wrap at 8_589_934_592.
        let t0 = 95_443_720_000u64;
        assert!(
            (t0 as u128 * 9 / 100) > (1u128 << 33),
            "test must cross the 33-bit wrap"
        );
        let expected0 = ticks_for_test(t0);
        let expected1 = ticks_for_test(t0 + 1_000_000);
        assert!(
            expected0 < expected1,
            "timeline must advance across the wrap"
        );

        let mut muxer = HlsMuxer::new();
        muxer.push(&config_au(true), t0).unwrap();
        let first = muxer
            .push(&config_au(true), t0 + 1_000_000)
            .unwrap()
            .unwrap();
        let first_pes = parse_pes(&parse_packets(&first.data));
        assert_eq!(first_pes.len(), 1);
        assert_eq!(first_pes[0].pts, expected0);

        let second = muxer
            .push(&config_au(true), t0 + 2_000_000)
            .unwrap()
            .unwrap();
        let second_pes = parse_pes(&parse_packets(&second.data));
        assert_eq!(second_pes.len(), 1);
        assert_eq!(second_pes[0].pts, expected1);
        let delta = second_pes[0].pts.wrapping_sub(first_pes[0].pts) & PTS33_MASK;
        assert_eq!(delta, 90_000);
        let pcr = first_pes[0].pcr_base.expect("PCR present");
        assert_eq!(
            first_pes[0].pts.wrapping_sub(pcr) & PTS33_MASK,
            27_000,
            "wrap-safe decode lead"
        );
    }

    fn test_segment(sequence: u64, duration: f64) -> Segment {
        Segment {
            sequence,
            duration,
            data: vec![(0x40 + sequence % 0x40) as u8; 376],
        }
    }

    /// Publishes `sequence` stamped `base + seconds` on a fake monotonic clock.
    fn publish_at_seconds(
        store: &mut HlsStore,
        sequence: u64,
        duration: f64,
        seconds: u64,
        base: Instant,
    ) {
        store
            .publish_at(
                test_segment(sequence, duration),
                "avc1.640028",
                base + Duration::from_secs(seconds),
            )
            .expect("publish test segment");
    }

    fn advertised_sequences(store: &HlsStore) -> Vec<u64> {
        store.snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
            snapshot
                .entries
                .iter()
                .map(|entry| entry.sequence)
                .collect()
        })
    }

    #[test]
    fn store_becomes_ready_at_eight_advertised_seconds() {
        let mut store = HlsStore::new();
        assert!(!store.ready());
        assert!(store.playlist().is_none());
        let base = Instant::now();
        let durations = [0.9, 1.0, 1.033333, 2.0, 2.0, 1.1];
        let mut advertised_us = 0u64;
        for (sequence, duration) in durations.iter().enumerate() {
            publish_at_seconds(
                &mut store,
                sequence as u64,
                *duration,
                sequence as u64,
                base,
            );
            advertised_us += validated_duration_us(*duration, HlsProfile::Stable).unwrap();
            assert_eq!(
                store.ready(),
                advertised_us >= HlsProfile::Stable.ready_duration_us(),
                "readiness after {advertised_us}us advertised"
            );
        }
        assert!(store.ready());
        let playlist = store.playlist().unwrap();
        assert!(playlist.contains("#EXT-X-TARGETDURATION:2\n"), "{playlist}");
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0\n"), "{playlist}");
        assert!(playlist.contains("#EXTINF:0.900000,\n"), "{playlist}");
        assert!(playlist.contains("#EXTINF:1.033333,\n"), "{playlist}");
        assert!(playlist.contains("#EXTINF:1.100000,\n"), "{playlist}");
        assert_eq!(playlist.matches("#EXTINF:").count(), durations.len());
        assert!(!playlist.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn store_defaults_to_stable_and_eight_megabits() {
        for store in [HlsStore::new(), HlsStore::default()] {
            assert_eq!(store.profile, HlsProfile::Stable);
            assert_eq!(store.bandwidth_bps(), 8_000_000);
        }

        assert!(HlsStore::with_profile(HlsProfile::Responsive, 0).is_err());
        let over = HlsStore::with_profile(HlsProfile::Responsive, 100_000_001)
            .err()
            .expect("over-cap bandwidth must be rejected");
        assert!(over.to_string().contains("cap"), "unexpected: {over}");
        assert!(HlsStore::with_profile(HlsProfile::Stable, 100_000_000).is_ok());
        let store = HlsStore::with_profile(HlsProfile::Responsive, 16_000_000).unwrap();
        assert_eq!(store.profile, HlsProfile::Responsive);
        assert_eq!(store.bandwidth_bps(), 16_000_000);
    }

    #[test]
    fn responsive_store_readies_at_four_advertised_seconds() {
        let mut store = HlsStore::with_profile(HlsProfile::Responsive, 16_000_000).unwrap();
        let base = Instant::now();
        let mut advertised_us = 0u64;
        for sequence in 0..8u64 {
            store
                .publish_at(
                    test_segment(sequence, 0.5),
                    "avc1.640028",
                    base + Duration::from_micros(sequence * 500_000),
                )
                .unwrap();
            advertised_us += 500_000;
            assert_eq!(
                store.ready(),
                advertised_us >= HlsProfile::Responsive.ready_duration_us(),
                "readiness after {advertised_us}us advertised"
            );
        }
        assert!(store.ready());
        let playlist = store.playlist().unwrap();
        assert!(playlist.contains("#EXT-X-TARGETDURATION:1\n"), "{playlist}");
        assert_eq!(
            playlist.matches("#EXTINF:0.500000,\n").count(),
            8,
            "{playlist}"
        );
        assert!(!playlist.contains("#EXT-X-ENDLIST"), "{playlist}");
    }

    #[test]
    fn responsive_store_rejects_segments_over_one_second() {
        let mut store = HlsStore::with_profile(HlsProfile::Responsive, 8_000_000).unwrap();
        let base = Instant::now();
        let error = store
            .publish_at(test_segment(0, 1.5), "avc1.640028", base)
            .unwrap_err();
        assert!(
            error.to_string().contains("1s target duration"),
            "unexpected: {error}"
        );
        assert_eq!(
            store.last_sequence, None,
            "a rejected publish must not admit"
        );
        assert_eq!(
            validated_duration_us(1.0, HlsProfile::Responsive).unwrap(),
            1_000_000,
            "exactly the responsive target is still valid"
        );
        store
            .publish_at(test_segment(0, 1.0), "avc1.640028", base)
            .unwrap();
        assert_eq!(store.last_sequence, Some(0));
    }

    #[test]
    fn responsive_snapshot_cadence_is_half_a_second() {
        let mut store = HlsStore::with_profile(HlsProfile::Responsive, 16_000_000).unwrap();
        let base = Instant::now();
        for sequence in 0..9u64 {
            store
                .publish_at(
                    test_segment(sequence, 0.5),
                    "avc1.640028",
                    base + Duration::from_micros(sequence * 500_000),
                )
                .unwrap();
        }
        assert!(store.ready());
        assert_eq!(advertised_sequences(&store), (0..9).collect::<Vec<_>>());
        let committed = store.playlist().unwrap();

        // 250 ms after the commit the advertised snapshot must not change.
        store
            .publish_at(
                test_segment(9, 0.5),
                "avc1.640028",
                base + Duration::from_micros(4_250_000),
            )
            .unwrap();
        assert_eq!(advertised_sequences(&store), (0..9).collect::<Vec<_>>());
        assert_eq!(store.playlist().unwrap(), committed);
        assert!(
            store.segment(9).is_some(),
            "pending segments stay fetchable"
        );

        // At the full 500 ms interval the next commit coalesces both pending
        // segments.
        store
            .publish_at(
                test_segment(10, 0.5),
                "avc1.640028",
                base + Duration::from_micros(4_500_000),
            )
            .unwrap();
        assert_eq!(advertised_sequences(&store), (0..11).collect::<Vec<_>>());
    }

    #[test]
    fn responsive_retention_expires_exactly_at_the_grace_deadline() {
        let mut store = HlsStore::with_profile(HlsProfile::Responsive, 8_000_000).unwrap();
        let base = Instant::now();
        {
            let mut publish = |sequence: u64, at_micros: u64| {
                store
                    .publish_at(
                        test_segment(sequence, 0.5),
                        "avc1.640028",
                        base + Duration::from_micros(at_micros),
                    )
                    .expect("publish responsive test segment");
            };
            for sequence in 0..=24u64 {
                publish(sequence, sequence * 500_000);
            }
        }
        let retired = store
            .retired
            .iter()
            .find(|entry| entry.segment.sequence == 0)
            .expect("sequence 0 retires once 12s of newer media exist");
        assert_eq!(retired.max_containing_us, 12_000_000);
        assert_eq!(
            retired.retire_at,
            Some(base + Duration::from_micros(24_500_000))
        );
        assert!(store.segment(0).is_some(), "retired but unexpired");

        let publish = |store: &mut HlsStore, sequence: u64, at_micros: u64| {
            store
                .publish_at(
                    test_segment(sequence, 0.5),
                    "avc1.640028",
                    base + Duration::from_micros(at_micros),
                )
                .expect("publish responsive test segment");
        };
        // One microsecond before the deadline it is still served...
        publish(&mut store, 25, 24_499_999);
        assert!(
            store.segment(0).is_some(),
            "retention must hold until the exact deadline"
        );
        // ...and the first publish at the deadline prunes it.
        publish(&mut store, 26, 24_500_000);
        assert!(
            store.segment(0).is_none(),
            "retention must expire at the exact deadline"
        );
        assert!(store.segment(2).is_some(), "newer retired segments stay");
    }

    /// The responsive history must keep the 12-second media window across a
    /// 20-minute fake clock with 0.45/0.5/0.533333 s jittered segments, and the
    /// retained set must stay inside the 64-segment and 128 MiB budgets.
    #[test]
    fn responsive_window_stays_twelve_seconds_within_budgets_for_twenty_minutes() {
        let mut store = HlsStore::with_profile(HlsProfile::Responsive, 8_000_000).unwrap();
        let base = Instant::now();
        let cycle = [0.45_f64, 0.5, 0.533333];
        let target_us = 20 * 60 * 1_000_000u64;
        let mut sequence = 0u64;
        let mut media_us = 0u64;
        let mut at = Duration::ZERO;
        while media_us < target_us {
            let duration = cycle[sequence as usize % cycle.len()];
            let duration_us = validated_duration_us(duration, HlsProfile::Responsive).unwrap();
            // Up to 5 ms of publish jitter on top of the media duration.
            at += Duration::from_micros(duration_us + (sequence % 11) * 500);
            store
                .publish_at(test_segment(sequence, duration), "avc1.640028", base + at)
                .expect("the responsive window must fit the retention budget");
            media_us += duration_us;
            sequence += 1;

            if media_us >= WINDOW_DURATION_US {
                let retained_us: u64 = store.active.iter().map(|entry| entry.duration_us).sum();
                assert!(
                    retained_us >= WINDOW_DURATION_US,
                    "retained {retained_us}us after {media_us}us published"
                );
            }
            let retained = store.active.len() + store.retired.len();
            assert!(
                retained <= MAX_STORE_SEGMENTS,
                "{retained} retained segments at media {media_us}us"
            );
            assert!(
                store.total_bytes <= MAX_STORE_BYTES,
                "{} retained bytes at media {media_us}us",
                store.total_bytes
            );
        }
        assert!(media_us >= target_us);
        assert!(store.ready());
        assert!(store.active.len() + store.retired.len() <= MAX_STORE_SEGMENTS);
        assert!(store.total_bytes <= MAX_STORE_BYTES);
    }

    #[test]
    fn store_never_slides_below_twelve_seconds() {
        let mut store = HlsStore::new();
        let base = Instant::now();
        let mut published_us = 0u64;
        for sequence in 0..30u64 {
            publish_at_seconds(&mut store, sequence, 1.0, sequence, base);
            published_us += 1_000_000;
            let retained_us: u64 = store.active.iter().map(|entry| entry.duration_us).sum();
            if published_us >= WINDOW_DURATION_US {
                assert!(
                    retained_us >= WINDOW_DURATION_US,
                    "retained {retained_us}us after {published_us}us published"
                );
                assert!(retained_us < WINDOW_DURATION_US + HlsProfile::Stable.target_duration_us());
            } else {
                assert_eq!(retained_us, published_us);
            }
        }
        // Recently retired segments are kept for delayed clients while their
        // retention holds; the oldest one has already expired. The advertised
        // window still starts where the 12s suffix starts.
        assert!(
            store.segment(17).is_some(),
            "a recently retired segment is retained"
        );
        assert!(
            store.segment(0).is_none(),
            "retention expired for the oldest segment"
        );
        assert!(store.retired.iter().all(|entry| entry.retire_at.is_some()));
        assert!(
            store
                .playlist()
                .unwrap()
                .contains("#EXT-X-MEDIA-SEQUENCE:18\n")
        );
    }

    #[test]
    fn advertised_segment_is_retained_for_window_plus_duration() {
        let mut store = HlsStore::new();
        let base = Instant::now();
        for sequence in 0..13u64 {
            publish_at_seconds(&mut store, sequence, 2.0, 2 * sequence, base);
        }
        assert!(store.ready());
        let retired = store
            .retired
            .iter()
            .find(|entry| entry.segment.sequence == 0)
            .expect("sequence 0 must be retired once 12s of newer media exist");
        assert_eq!(retired.max_containing_us, 12_000_000);
        assert_eq!(retired.retire_at, Some(base + Duration::from_secs(26)));
        assert!(store.segment(0).is_some(), "retired but unexpired");
        assert!(
            !store.snapshot.as_ref().unwrap().contains(0),
            "the current playlist no longer lists it"
        );

        // The next publish at or after the deadline prunes it.
        publish_at_seconds(&mut store, 13, 2.0, 26, base);
        assert!(
            store.segment(0).is_none(),
            "an expired retirement must be evicted"
        );
    }

    #[test]
    fn snapshot_cadence_coalesces_burst_publications() {
        let mut store = HlsStore::new();
        let base = Instant::now();
        for sequence in 0..8u64 {
            publish_at_seconds(&mut store, sequence, 1.0, sequence, base);
        }
        assert!(store.ready());
        assert_eq!(advertised_sequences(&store), (0..8).collect::<Vec<_>>());

        // Two publishes on the same clock: the first is eligible and commits,
        // the second is stored but must not change the advertised snapshot.
        publish_at_seconds(&mut store, 8, 1.0, 8, base);
        let after_first_burst = store.playlist().unwrap();
        publish_at_seconds(&mut store, 9, 1.0, 8, base);
        assert_eq!(advertised_sequences(&store), (0..9).collect::<Vec<_>>());
        assert_eq!(store.playlist().unwrap(), after_first_burst);
        assert!(
            store.segment(9).is_some(),
            "completed pending segments stay fetchable"
        );

        // The next eligible publish coalesces the pending segment and the new
        // one into a single snapshot.
        publish_at_seconds(&mut store, 10, 1.0, 9, base);
        assert_eq!(advertised_sequences(&store), (0..11).collect::<Vec<_>>());
        let playlist = store.playlist().unwrap();
        assert_eq!(playlist.matches("#EXTINF:1.000000,\n").count(), 11);
        assert!(playlist.contains("\n9.ts\n") && playlist.contains("\n10.ts\n"));
    }

    /// Regression: retirement must be timed from the actual snapshot commit,
    /// not from any publish that happens to fall inside the commit interval.
    /// A segment advertised by the old snapshot must not start its expiry
    /// clock (or disappear) early.
    #[test]
    fn retirement_clock_starts_at_the_commit_boundary() {
        let mut store = HlsStore::new();
        let base = Instant::now();
        for sequence in 0..12u64 {
            publish_at_seconds(&mut store, sequence, 1.0, sequence, base);
        }
        assert_eq!(advertised_sequences(&store), (0..12).collect::<Vec<_>>());
        assert!(store.retired.is_empty(), "12s fits the suffix exactly");
        let committed_playlist = store.playlist().unwrap();

        // A publish 500ms inside the snapshot interval is stored but must not
        // retire anything: the old snapshot still promises sequences 0..=11.
        store
            .publish_at(
                test_segment(12, 1.0),
                "avc1.640028",
                base + Duration::from_millis(11_500),
            )
            .unwrap();
        assert_eq!(advertised_sequences(&store), (0..12).collect::<Vec<_>>());
        assert_eq!(store.playlist().unwrap(), committed_playlist);
        assert!(store.retired.is_empty(), "no retirement before the commit");
        assert!(store.segment(0).is_some());
        assert_eq!(store.last_sequence, Some(12));
        assert_eq!(
            store.active.len(),
            13,
            "pending media may exceed the 12s suffix between commits"
        );

        // The next eligible publish commits: only now do sequences 0 and 1
        // retire, and their clock starts at this commit, not at t11.5.
        publish_at_seconds(&mut store, 13, 1.0, 12, base);
        assert_eq!(advertised_sequences(&store), (2..14).collect::<Vec<_>>());
        let retired_at = |sequence: u64| {
            store
                .retired
                .iter()
                .find(|entry| entry.segment.sequence == sequence)
                .and_then(|entry| entry.retire_at)
        };
        assert_eq!(retired_at(0), Some(base + Duration::from_secs(25)));
        assert_eq!(retired_at(1), Some(base + Duration::from_secs(25)));
        assert!(
            retired_at(0) != Some(base + Duration::from_millis(24_500)),
            "the t11.5 publish must not time the retention"
        );

        // The old manifest URI is still available just before the deadline,
        // even after another publish that cannot free it early...
        publish_at_seconds(&mut store, 14, 1.0, 24, base);
        store
            .publish_at(
                test_segment(15, 1.0),
                "avc1.640028",
                base + Duration::from_secs(24) + Duration::from_millis(999),
            )
            .unwrap();
        assert!(
            store.segment(0).is_some(),
            "the old manifest URI must still be available at t24.999"
        );

        // ...and expires at the first eligible publish at or after t25.
        publish_at_seconds(&mut store, 16, 1.0, 25, base);
        assert!(store.segment(0).is_none(), "expired at the t25 commit");
        assert!(store.segment(3).is_some(), "newer retired segments stay");
    }

    #[test]
    fn accelerated_hour_keeps_integer_microsecond_precision() {
        let mut store = HlsStore::new();
        let base = Instant::now();
        let cycle = [0.9_f64, 1.0, 1.033333, 2.0];
        let mut expected_us: Vec<u64> = Vec::new();
        let mut media_us = 0u64;
        let mut at = Duration::ZERO;
        while media_us < 3_600_000_000 {
            let sequence = expected_us.len() as u64;
            let duration = cycle[sequence as usize % cycle.len()];
            let duration_us = validated_duration_us(duration, HlsProfile::Stable).unwrap();
            at += Duration::from_micros(duration_us);
            store
                .publish_at(test_segment(sequence, duration), "avc1.640028", base + at)
                .unwrap();
            expected_us.push(duration_us);
            media_us += duration_us;
        }
        assert!(media_us >= 3_600_000_000);
        assert!(store.ready());

        // The advertised window is exact integer microseconds: its duration is
        // the sum of the recorded per-segment durations over the contiguous
        // range it lists, with no floating-point accumulation.
        let snapshot = store.snapshot.as_ref().unwrap();
        let start = snapshot.entries.first().unwrap().sequence as usize;
        let end = snapshot.entries.last().unwrap().sequence as usize;
        assert_eq!(snapshot.entries.len(), end - start + 1);
        let expected_window: u64 = expected_us[start..=end].iter().sum();
        assert_eq!(snapshot.duration_us, expected_window);
        assert!(snapshot.duration_us >= WINDOW_DURATION_US);
        assert!(
            snapshot.duration_us < WINDOW_DURATION_US + HlsProfile::Stable.target_duration_us()
        );
        for (entry, expected) in snapshot.entries.iter().zip(&expected_us[start..=end]) {
            assert_eq!(entry.duration_us, *expected);
        }

        // EXTINF renders each integer microsecond duration with six decimals.
        let playlist = store.playlist().unwrap();
        for entry in &snapshot.entries {
            assert!(
                playlist.contains(&format!(
                    "#EXTINF:{}.{:06},\n",
                    entry.duration_us / 1_000_000,
                    entry.duration_us % 1_000_000
                )),
                "{playlist}"
            );
        }
        // The store stays bounded across an accelerated hour.
        assert!(store.active.len() + store.retired.len() <= MAX_STORE_SEGMENTS);
        assert!(store.total_bytes <= MAX_STORE_BYTES);
    }

    #[test]
    fn delayed_client_following_an_old_manifest_keeps_working() {
        let mut store = HlsStore::new();
        let base = Instant::now();
        for sequence in 0..=600u64 {
            publish_at_seconds(&mut store, sequence, 1.0, sequence, base);
        }
        let manifest = store.playlist().unwrap();
        assert!(manifest.contains("\n596.ts\n"), "{manifest}");

        // Eight seconds later sequence 596 has left the active window but is
        // still inside its retention promise (removed at +608s, expires +621s).
        for sequence in 601..=608u64 {
            publish_at_seconds(&mut store, sequence, 1.0, sequence, base);
        }
        assert!(
            store.segment(596).is_some(),
            "a client that fetched the old manifest 8s ago must still be served"
        );

        // The producer keeps running to 20 minutes; once retention expires and
        // a publish triggers pruning, the old segment is gone.
        for sequence in 609..1200u64 {
            publish_at_seconds(&mut store, sequence, 1.0, sequence, base);
        }
        assert!(store.segment(596).is_none());
        assert!(store.ready());
        assert!(store.segment(1199).is_some());
    }

    #[test]
    fn store_reports_bounded_stats() {
        let mut store = HlsStore::new();
        let base = Instant::now();
        for sequence in 0..8u64 {
            publish_at_seconds(&mut store, sequence, 1.0, sequence, base);
        }
        let stats = store.stats_at(base + Duration::from_secs(12));
        assert_eq!(stats.advertised_us, 8_000_000);
        assert_eq!(stats.retained_segments, 8);
        assert_eq!(stats.retained_bytes, 8 * 376);
        assert_eq!(stats.publication_age, Some(Duration::from_secs(5)));
        let stalled = store.stats_at(base + Duration::from_secs(60));
        assert_eq!(
            stalled.publication_age,
            Some(Duration::from_secs(53)),
            "the publication age keeps growing while production is stalled"
        );
    }

    #[test]
    fn store_rejects_invalid_publishes_without_mutating_state() {
        let mut store = HlsStore::new();
        let base = Instant::now();
        store
            .publish_at(test_segment(0, 1.0), "avc1.640028", base)
            .unwrap();

        let gap = store
            .publish_at(
                test_segment(2, 1.0),
                "avc1.640028",
                base + Duration::from_secs(1),
            )
            .unwrap_err();
        assert!(gap.to_string().contains("contiguous"), "unexpected: {gap}");

        let codec = store
            .publish_at(
                test_segment(1, 1.0),
                "avc1.42c01f",
                base + Duration::from_secs(1),
            )
            .unwrap_err();
        assert!(
            codec.to_string().contains("codec changed"),
            "unexpected: {codec}"
        );

        let long = store
            .publish_at(
                test_segment(1, 2.5),
                "avc1.640028",
                base + Duration::from_secs(1),
            )
            .unwrap_err();
        assert!(
            long.to_string().contains("target duration"),
            "unexpected: {long}"
        );

        let zero = store
            .publish_at(
                test_segment(1, 0.0),
                "avc1.640028",
                base + Duration::from_secs(1),
            )
            .unwrap_err();
        assert!(
            zero.to_string().contains("invalid duration"),
            "unexpected: {zero}"
        );

        let empty = store
            .publish_at(
                Segment {
                    sequence: 1,
                    duration: 1.0,
                    data: Vec::new(),
                },
                "avc1.640028",
                base + Duration::from_secs(1),
            )
            .unwrap_err();
        assert!(
            empty.to_string().contains("empty segment"),
            "unexpected: {empty}"
        );

        let oversized = store
            .publish_at(
                Segment {
                    sequence: 1,
                    duration: 1.0,
                    data: vec![0x41; MAX_SEGMENT_BYTES + 1],
                },
                "avc1.640028",
                base + Duration::from_secs(1),
            )
            .unwrap_err();
        assert!(
            oversized.to_string().contains("MiB"),
            "unexpected: {oversized}"
        );

        // A rejected publish must not change the codec, the advertised
        // snapshot or the accepted sequence.
        assert_eq!(store.codec.as_deref(), Some("avc1.640028"));
        assert_eq!(store.last_sequence, Some(0));
        assert_eq!(advertised_sequences(&store), vec![0]);
        assert!(!store.ready());
        store
            .publish_at(
                test_segment(1, 1.0),
                "avc1.640028",
                base + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(store.last_sequence, Some(1));

        // A clock regression is rejected before any state changes.
        let backwards = store
            .publish_at(
                test_segment(2, 1.0),
                "avc1.640028",
                base + Duration::from_millis(1),
            )
            .unwrap_err();
        assert!(
            backwards.to_string().contains("backwards"),
            "unexpected: {backwards}"
        );
        assert_eq!(store.last_sequence, Some(1));
        assert_eq!(advertised_sequences(&store), vec![0, 1]);
    }

    #[test]
    fn store_rejects_admission_when_protected_segments_fill_the_budget() {
        let mut store = HlsStore::with_limits(MAX_STORE_BYTES, 4);
        let base = Instant::now();
        for sequence in 0..4u64 {
            publish_at_seconds(&mut store, sequence, 1.0, sequence, base);
        }
        let advertised = advertised_sequences(&store);
        let bytes = store.total_bytes;

        let error = store
            .publish_at(
                test_segment(4, 1.0),
                "avc1.640028",
                base + Duration::from_secs(4),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("segment budget"),
            "unexpected: {error}"
        );
        assert_eq!(store.active.len() + store.retired.len(), 4);
        assert_eq!(store.total_bytes, bytes);
        assert_eq!(advertised_sequences(&store), advertised);
        assert!(store.segment(0).is_some());
        // The rejected publish must not advance the accepted sequence either.
        let gap = store
            .publish_at(
                test_segment(5, 1.0),
                "avc1.640028",
                base + Duration::from_secs(4),
            )
            .unwrap_err();
        assert!(gap.to_string().contains("contiguous"), "unexpected: {gap}");
    }

    #[test]
    fn store_rejects_admission_when_protected_bytes_fill_the_budget() {
        let mut store = HlsStore::with_limits(1_200, MAX_STORE_SEGMENTS);
        let base = Instant::now();
        for sequence in 0..3u64 {
            publish_at_seconds(&mut store, sequence, 1.0, sequence, base);
        }
        assert_eq!(store.total_bytes, 3 * 376);
        let error = store
            .publish_at(
                test_segment(3, 1.0),
                "avc1.640028",
                base + Duration::from_secs(3),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("byte budget"),
            "unexpected: {error}"
        );
        assert_eq!(store.total_bytes, 3 * 376);
        assert!(store.segment(0).is_some());
        assert!(store.segment(2).is_some());
        assert!(store.snapshot.is_some(), "the window must be preserved");
    }

    #[test]
    fn store_rejects_sequence_overflow() {
        let mut store = HlsStore::new();
        store.last_sequence = Some(u64::MAX - 1);
        let base = Instant::now();
        store
            .publish_at(
                Segment {
                    sequence: u64::MAX,
                    duration: 1.0,
                    data: vec![0x41; 376],
                },
                "avc1.640028",
                base,
            )
            .unwrap();
        let error = store
            .publish_at(
                test_segment(0, 1.0),
                "avc1.640028",
                base + Duration::from_secs(1),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("overflow"),
            "unexpected: {error}"
        );
        assert_eq!(store.last_sequence, Some(u64::MAX));
    }

    #[test]
    fn aac_mode_reports_combined_codec() {
        let mut muxer = HlsMuxer::with_aac();
        assert_eq!(muxer.codec(), None);
        muxer.push(&config_au(true), 0).unwrap();
        assert_eq!(muxer.codec(), Some("avc1.640028,mp4a.40.2"));
    }

    #[test]
    fn aac_pmt_advertises_both_streams_with_valid_crc() {
        let mut muxer = HlsMuxer::with_aac();
        muxer.push(&config_au(true), 0).unwrap();
        muxer.push_audio(&adts_frame(AAC_PAYLOAD), 100_000).unwrap();
        let segment = muxer.push(&config_au(true), 1_000_000).unwrap().unwrap();

        let packets = parse_packets(&segment.data);
        assert_continuity(&packets);
        let pat = find_section(&packets, PID_PAT);
        assert!(crc32_ok(&pat), "PAT CRC32/MPEG2 is wrong");
        let pmt = find_section(&packets, PID_PMT);
        assert_eq!(pmt[0], 0x02);
        assert!(crc32_ok(&pmt), "PMT CRC32/MPEG2 is wrong");
        let pcr_pid = (((pmt[8] & 0x1F) as u16) << 8) | pmt[9] as u16;
        assert_eq!(pcr_pid, PID_VIDEO, "PCR must stay on the video PID");
        assert_eq!(
            pmt_streams(&pmt),
            vec![
                PmtStream {
                    stream_type: STREAM_TYPE_H264,
                    pid: PID_VIDEO,
                },
                PmtStream {
                    stream_type: STREAM_TYPE_AAC,
                    pid: PID_AUDIO,
                },
            ]
        );
        assert!(
            packets
                .iter()
                .filter(|packet| packet.pid == PID_AUDIO)
                .all(|packet| packet.pcr_base.is_none()),
            "audio PES packets must not carry a PCR"
        );
    }

    #[test]
    fn audio_before_first_decodable_idr_is_discarded() {
        let mut muxer = HlsMuxer::with_aac();
        let frame = adts_frame(AAC_PAYLOAD);
        // Audio may be submitted first (its PTS is below the first video
        // frame): it is dropped without error and never opens a segment.
        muxer.push_audio(&frame, 1_000).unwrap();
        assert!(muxer.pending.is_none(), "audio must not open a segment");
        muxer.push(&nal(SPS), 2_000).unwrap();
        muxer.push(&nal(PPS), 3_000).unwrap();
        muxer.push_audio(&frame, 4_000).unwrap();
        assert!(muxer.pending.is_none(), "no decodable IDR yet");

        muxer.push(&config_au(true), 5_000).unwrap();
        muxer.push_audio(&frame, 5_023).unwrap();
        let segment = muxer.push(&config_au(true), 1_005_000).unwrap().unwrap();
        let audio = parse_audio_pes(&parse_packets(&segment.data));
        assert_eq!(
            audio.len(),
            1,
            "only the post-IDR frame belongs to the segment"
        );
        assert_eq!(audio[0].pts, ticks_for_test(5_023));
        assert_eq!(audio[0].body, frame);
    }

    #[test]
    fn audio_before_sealing_idr_stays_in_the_preceding_segment() {
        let mut muxer = HlsMuxer::with_aac();
        muxer.push(&config_au(true), 0).unwrap();
        let first_frame = adts_frame(&[0x01, 0x02, 0x03]);
        let second_frame = adts_frame(&[0x04, 0x05, 0x06]);
        muxer.push_audio(&first_frame, 100_000).unwrap();
        muxer.push_audio(&second_frame, 500_000).unwrap();
        let first = muxer.push(&config_au(true), 1_000_000).unwrap().unwrap();

        let third_frame = adts_frame(&[0x07, 0x08, 0x09]);
        muxer.push_audio(&third_frame, 1_100_000).unwrap();
        let second = muxer.push(&config_au(true), 2_000_000).unwrap().unwrap();

        let first_audio = parse_audio_pes(&parse_packets(&first.data));
        assert_eq!(first_audio.len(), 2);
        assert_eq!(first_audio[0].body, first_frame);
        assert_eq!(first_audio[1].body, second_frame);
        let second_audio = parse_audio_pes(&parse_packets(&second.data));
        assert_eq!(second_audio.len(), 1);
        assert_eq!(second_audio[0].body, third_frame);
    }

    #[test]
    fn aac_mode_rejects_late_audio_and_late_video() {
        let mut muxer = HlsMuxer::with_aac();
        let frame = adts_frame(AAC_PAYLOAD);
        muxer.push(&config_au(true), 10_000_000).unwrap();

        // Audio older than the last video sample is detected as late.
        let error = muxer.push_audio(&frame, 9_000_000).unwrap_err();
        assert!(error.to_string().contains("late audio"), "{error}");

        muxer.push_audio(&frame, 11_000_000).unwrap();
        // Video older than the last audio sample is a global regression.
        let error = muxer.push(&p_au(), 10_500_000).unwrap_err();
        assert!(error.to_string().contains("non-monotonic"), "{error}");

        muxer.push(&p_au(), 12_000_000).unwrap();
        let error = muxer.push_audio(&frame, 11_500_000).unwrap_err();
        assert!(error.to_string().contains("non-monotonic"), "{error}");
    }

    #[test]
    fn rejects_malformed_adts_frames() {
        let valid = adts_frame(AAC_PAYLOAD);
        let cases: Vec<(&str, &str, Vec<u8>)> = vec![
            ("sync", "sync word", {
                let mut frame = valid.clone();
                frame[0] = 0xFE;
                frame
            }),
            ("layer", "layer", {
                let mut frame = valid.clone();
                frame[1] |= 0x06;
                frame
            }),
            ("profile", "AAC-LC", {
                let mut frame = valid.clone();
                frame[2] = 0x10; // AAC Main instead of AAC-LC
                frame
            }),
            ("frequency", "44100", {
                let mut frame = valid.clone();
                frame[2] = (1 << 6) | (3 << 2); // 48000 Hz index 3
                frame
            }),
            ("channels", "stereo", {
                let mut frame = valid.clone();
                frame[2] = (1 << 6) | (4 << 2) | 0x01; // channel configuration 4
                frame[3] &= 0x3F;
                frame
            }),
            ("blocks", "raw_data_blocks", {
                let mut frame = valid.clone();
                frame[6] |= 0x01;
                frame
            }),
            ("trailing", "frame_length", {
                let mut frame = valid.clone();
                frame.push(0x00);
                frame
            }),
            ("truncated", "frame_length", {
                let mut frame = valid.clone();
                frame.pop();
                frame
            }),
            ("header", "header", valid[..3].to_vec()),
            ("oversize", "8191", vec![0u8; MAX_ADTS_FRAME_BYTES + 1]),
        ];

        let mut time = 0u64;
        for (name, expected, frame) in cases {
            let mut muxer = HlsMuxer::with_aac();
            time += 1_000_000;
            let error = muxer.push_audio(&frame, time).unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "{name}: unexpected error {error}"
            );
            assert!(
                muxer.pending.is_none(),
                "{name}: a rejected frame must not open a segment"
            );
        }
    }

    #[test]
    fn rejects_adts_configuration_change() {
        let mut muxer = HlsMuxer::with_aac();
        muxer.push(&config_au(true), 0).unwrap();
        muxer.push_audio(&adts_frame(AAC_PAYLOAD), 100_000).unwrap();
        let crc_protected = adts_frame_with_protection(AAC_PAYLOAD, false);
        let error = muxer.push_audio(&crc_protected, 200_000).unwrap_err();
        assert!(
            error.to_string().contains("configuration changed"),
            "{error}"
        );
    }

    #[test]
    fn accepts_crc_and_protection_absent_adts_frames() {
        for protection_absent in [true, false] {
            let mut muxer = HlsMuxer::with_aac();
            muxer.push(&config_au(true), 0).unwrap();
            let frame = adts_frame_with_protection(&[0xDE, 0xAD, 0xBE, 0xEF], protection_absent);
            muxer.push_audio(&frame, 100_000).unwrap();
            let segment = muxer.push(&config_au(true), 1_000_000).unwrap().unwrap();
            let audio = parse_audio_pes(&parse_packets(&segment.data));
            assert_eq!(audio.len(), 1);
            assert_eq!(audio[0].pts, ticks_for_test(100_000));
            assert_eq!(audio[0].body, frame, "the whole ADTS frame must round-trip");
        }
    }

    #[test]
    fn aac_timeline_continuity_spans_three_segments() {
        struct AacSegment {
            segment: Segment,
            video_pts: Vec<u64>,
            audio: Vec<(u64, Vec<u8>)>,
        }

        let t0 = 7_000_000u64;
        let mut muxer = HlsMuxer::with_aac();
        let mut video_time = t0;
        let mut audio_time = t0;
        muxer.push(&config_au(true), video_time).unwrap();
        let mut current_video = vec![video_time];
        let mut current_audio: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut segments: Vec<AacSegment> = Vec::new();

        for _ in 0..3 {
            let seal = video_time + 1_000_000;
            while video_time + 33_333 < seal {
                video_time += 33_333;
                while audio_time + 23_220 <= video_time {
                    audio_time += 23_220;
                    let frame = adts_frame(&[0x21, (audio_time & 0xFF) as u8, 0x04]);
                    muxer.push_audio(&frame, audio_time).unwrap();
                    current_audio.push((audio_time, frame));
                }
                muxer.push(&p_au(), video_time).unwrap();
                current_video.push(video_time);
            }
            while audio_time + 23_220 < seal {
                audio_time += 23_220;
                let frame = adts_frame(&[0x21, (audio_time & 0xFF) as u8, 0x04]);
                muxer.push_audio(&frame, audio_time).unwrap();
                current_audio.push((audio_time, frame));
            }
            video_time = seal;
            let segment = muxer.push(&config_au(true), video_time).unwrap().unwrap();
            segments.push(AacSegment {
                segment,
                video_pts: std::mem::take(&mut current_video),
                audio: std::mem::take(&mut current_audio),
            });
            current_video.push(video_time);
        }
        assert_eq!(segments.len(), 3);

        let mut concatenated = Vec::new();
        for entry in &segments {
            concatenated.extend_from_slice(&entry.segment.data);
        }
        let packets = parse_packets(&concatenated);
        assert_continuity(&packets);

        // Every segment advertises the video and the audio stream.
        for entry in &segments {
            let pmt = find_section(&parse_packets(&entry.segment.data), PID_PMT);
            assert!(crc32_ok(&pmt), "PMT CRC32/MPEG2 is wrong");
            assert_eq!(
                pmt_streams(&pmt),
                vec![
                    PmtStream {
                        stream_type: STREAM_TYPE_H264,
                        pid: PID_VIDEO,
                    },
                    PmtStream {
                        stream_type: STREAM_TYPE_AAC,
                        pid: PID_AUDIO,
                    },
                ]
            );
        }

        let expected_video: Vec<u64> = segments
            .iter()
            .flat_map(|entry| entry.video_pts.iter().copied())
            .collect();
        let expected_audio: Vec<(u64, Vec<u8>)> = segments
            .iter()
            .flat_map(|entry| entry.audio.iter().cloned())
            .collect();

        let video_pes = parse_pes(&packets);
        assert_eq!(video_pes.len(), expected_video.len());
        for (parsed, expected) in video_pes.iter().zip(&expected_video) {
            assert_eq!(parsed.pts, ticks_for_test(*expected));
        }
        let audio_pes = parse_audio_pes(&packets);
        assert_eq!(audio_pes.len(), expected_audio.len());
        for (parsed, (pts, frame)) in audio_pes.iter().zip(&expected_audio) {
            assert_eq!(parsed.pts, ticks_for_test(*pts));
            assert_eq!(&parsed.body, frame, "ADTS frames must round-trip");
        }

        assert!(video_pes.windows(2).all(|pair| pair[1].pts > pair[0].pts));
        assert!(audio_pes.windows(2).all(|pair| pair[1].pts > pair[0].pts));
        let ordered = demux_all_pes(&packets);
        assert_eq!(ordered.len(), expected_video.len() + expected_audio.len());
        assert!(
            ordered.windows(2).all(|pair| pair[1].1.pts > pair[0].1.pts),
            "PES packets must follow the globally ascending submission order"
        );

        // Audio continuity counters persist across segment boundaries.
        let audio_cc: Vec<u8> = packets
            .iter()
            .filter(|packet| packet.pid == PID_AUDIO && packet.afc & 0x01 != 0)
            .map(|packet| packet.cc)
            .collect();
        assert!(audio_cc.len() > 16, "the audio counter should wrap");
        assert!(
            audio_cc
                .windows(2)
                .all(|pair| pair[1] == (pair[0] + 1) & 0x0F),
            "audio continuity must not reset at segment boundaries"
        );
    }

    #[test]
    fn aac_segment_size_cap_includes_audio() {
        let mut muxer = HlsMuxer::with_aac();
        muxer.push(&config_au(true), 0).unwrap();
        let big = vec![0x41u8; 96 * 1024];
        let mut time = 0u64;

        // Pack the open segment with large access units until the next one
        // would cross the 4 MiB cap, then let audio cross it instead.
        loop {
            time += 33_333;
            let before = muxer.pending.as_ref().unwrap().data.len();
            match muxer.push(&nal(&big), time) {
                Ok(_) => {
                    if muxer.pending.as_ref().unwrap().data.len() >= MAX_SEGMENT_BYTES - 120 * 1024
                    {
                        break;
                    }
                }
                Err(error) => {
                    assert_eq!(
                        muxer.pending.as_ref().unwrap().data.len(),
                        before,
                        "a rejected access unit must not partially extend the segment"
                    );
                    assert!(error.to_string().contains("MiB"), "{error}");
                    break;
                }
            }
        }

        let mut audio_error = None;
        for _ in 0..40 {
            time += 23_220;
            let frame = adts_frame(&vec![0x21u8; 8_000]);
            let before = muxer.pending.as_ref().unwrap().data.len();
            match muxer.push_audio(&frame, time) {
                Ok(()) => assert!(
                    muxer.pending.as_ref().unwrap().data.len() > before,
                    "accepted audio must extend the segment"
                ),
                Err(error) => {
                    assert_eq!(
                        muxer.pending.as_ref().unwrap().data.len(),
                        before,
                        "a rejected ADTS frame must not partially extend the segment"
                    );
                    audio_error = Some(error);
                    break;
                }
            }
        }
        let error = audio_error.expect("audio must hit the 4 MiB segment cap");
        assert!(error.to_string().contains("MiB"), "{error}");
        assert!(muxer.pending.as_ref().unwrap().data.len() <= MAX_SEGMENT_BYTES);
    }

    #[test]
    fn rejects_audio_past_the_target_duration() {
        let mut muxer = HlsMuxer::with_aac();
        muxer.push(&config_au(true), 0).unwrap();
        muxer
            .push_audio(&adts_frame(AAC_PAYLOAD), 1_500_000)
            .unwrap();
        muxer
            .push_audio(&adts_frame(AAC_PAYLOAD), 1_900_000)
            .unwrap();
        let before = muxer.pending.as_ref().unwrap().data.len();
        let error = muxer
            .push_audio(&adts_frame(AAC_PAYLOAD), 2_100_000)
            .unwrap_err();
        assert!(error.to_string().contains("target duration"), "{error}");
        assert_eq!(muxer.pending.as_ref().unwrap().data.len(), before);
    }

    #[test]
    fn refreshes_slow_video_pcr_with_adaptation_only_packets() {
        let mut muxer = HlsMuxer::with_aac();
        let t0 = 1_000_000u64;
        muxer.push(&config_au(true), t0).unwrap();
        let mut video_time = t0;
        let mut audio_time = t0;
        let mut submitted = vec![t0];

        // 5 fps video with 44100 Hz audio: audio must keep the PCR moving.
        while video_time < t0 + 900_000 {
            while audio_time + 23_220 < video_time + 200_000 {
                audio_time += 23_220;
                muxer
                    .push_audio(&adts_frame(AAC_PAYLOAD), audio_time)
                    .unwrap();
                submitted.push(audio_time);
            }
            video_time += 200_000;
            muxer.push(&p_au(), video_time).unwrap();
            submitted.push(video_time);
        }
        let segment = muxer
            .push(&config_au(true), t0 + 1_000_000)
            .unwrap()
            .unwrap();
        let packets = parse_packets(&segment.data);
        assert_continuity(&packets);

        // Decoder-like continuity check over every video packet: only a
        // payload packet increments the counter, so an adaptation-only packet
        // must repeat the immediately preceding packet's counter.
        let mut previous_cc: Option<u8> = None;
        let mut pcr_values = Vec::new();
        let mut adaptation_only = 0usize;
        for packet in packets.iter().filter(|packet| packet.pid == PID_VIDEO) {
            if packet.afc & 0x01 == 0 {
                assert_eq!(packet.afc, 0b10, "PCR refresh must be adaptation-only");
                assert!(packet.payload.is_empty());
                assert_eq!(
                    Some(packet.cc),
                    previous_cc,
                    "adaptation-only packet must repeat the previous continuity counter"
                );
                adaptation_only += 1;
                pcr_values.push(packet.pcr_base.expect("PCR refresh must carry a PCR"));
            } else {
                if let Some(previous) = previous_cc {
                    assert_eq!(
                        packet.cc,
                        (previous + 1) & 0x0F,
                        "payload packet must increment the continuity counter"
                    );
                }
                if let Some(pcr) = packet.pcr_base {
                    pcr_values.push(pcr);
                }
            }
            previous_cc = Some(packet.cc);
        }
        assert!(
            adaptation_only > 0,
            "slow video must be topped up with PCR refreshes"
        );

        // PCR advances within 40 ms plus one audio frame and never regresses.
        for pair in pcr_values.windows(2) {
            let gap = pair[1].wrapping_sub(pair[0]);
            assert!(gap > 0, "PCR must never regress");
            assert!(
                gap <= PCR_MAX_INTERVAL_90K + 2_100,
                "PCR gap {gap} ticks exceeds 40 ms"
            );
        }

        // Every PCR uses the same 90 kHz clock and the same 300 ms lead as the
        // video PES packets.
        let submitted_ticks: std::collections::HashSet<u64> =
            submitted.iter().map(|pts| ticks_for_test(*pts)).collect();
        for pcr in &pcr_values {
            let pts = pcr.wrapping_add(PCR_DECODE_LEAD) & PTS33_MASK;
            assert!(
                submitted_ticks.contains(&pts),
                "PCR must be derived from a submitted video/audio PTS"
            );
        }
    }

    /// Independent expectations for the adaptation-only rule, including the
    /// 15 -> 0 wrap: payload cc N, two PCR-only packets repeating N, next
    /// payload cc N+1.
    #[test]
    fn adaptation_only_pcr_repeats_last_payload_counter_including_wrap() {
        for (start_cc, expected) in [(7u8, vec![7u8, 7, 7, 8]), (15, vec![15, 15, 15, 0])] {
            let mut muxer = HlsMuxer::with_aac();
            // Deterministic first payload counter; in-crate tests may seed it.
            muxer.cc_video = start_cc;
            let t0 = 1_000_000u64;
            muxer.push(&config_au(true), t0).unwrap();
            // Four audio frames ~23 ms apart while the video is stalled: the
            // 40 ms refresh threshold is crossed twice (at ~46 ms and ~93 ms),
            // so two PCR-only packets are emitted before the next payload.
            for step in 1..=4u64 {
                muxer
                    .push_audio(&adts_frame(AAC_PAYLOAD), t0 + step * 23_220)
                    .unwrap();
            }
            muxer.push(&p_au(), t0 + 100_000).unwrap();

            let packets = parse_packets(&muxer.pending.as_ref().unwrap().data);
            let video: Vec<&ParsedPacket> = packets
                .iter()
                .filter(|packet| packet.pid == PID_VIDEO)
                .collect();
            let cc_sequence: Vec<u8> = video.iter().map(|packet| packet.cc).collect();
            assert_eq!(cc_sequence, expected, "seeded first counter {start_cc}");

            // Decoder-like check comparing every packet with the previous one.
            let mut previous_cc: Option<u8> = None;
            for packet in &video {
                if packet.afc & 0x01 == 0 {
                    assert_eq!(
                        Some(packet.cc),
                        previous_cc,
                        "adaptation-only must repeat the previous counter"
                    );
                } else if let Some(previous) = previous_cc {
                    assert_eq!(
                        packet.cc,
                        (previous + 1) & 0x0F,
                        "payload must increment the previous counter"
                    );
                }
                previous_cc = Some(packet.cc);
            }
        }
    }

    #[test]
    fn video_only_muxer_rejects_audio_and_stays_video_only() {
        let mut muxer = HlsMuxer::new();
        muxer.push(&config_au(true), 0).unwrap();
        let error = muxer
            .push_audio(&adts_frame(AAC_PAYLOAD), 1_000)
            .unwrap_err();
        assert!(error.to_string().contains("with_aac"), "{error}");

        let segment = muxer.push(&config_au(true), 1_000_000).unwrap().unwrap();
        let packets = parse_packets(&segment.data);
        let pmt = find_section(&packets, PID_PMT);
        assert!(crc32_ok(&pmt));
        assert_eq!(
            pmt_streams(&pmt),
            vec![PmtStream {
                stream_type: STREAM_TYPE_H264,
                pid: PID_VIDEO,
            }],
            "a video-only muxer must not advertise an audio stream"
        );
        assert!(packets.iter().all(|packet| packet.pid != PID_AUDIO));
        assert_eq!(muxer.codec(), Some("avc1.640028"));
    }
}
