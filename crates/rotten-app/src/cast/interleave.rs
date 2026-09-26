//! PTS-watermarked interleaving of the encoded Cast video and AAC tracks.
//!
//! The HLS muxer seals a segment when a video IDR arrives and assigns each
//! audio frame to the segment that is open when it is pushed, so every audio
//! frame strictly before that IDR must be fed before it. Both encoders also
//! introduce latency, and the audio timeline deliberately runs about 100 ms
//! behind the shared clock. The interleaver therefore buffers both tracks
//! separately and releases the globally earliest queued timestamp only once
//! both the latest seen video input PTS and the latest returned AAC PTS have
//! reached it. That keeps each sealed segment's audio complete, keeps
//! timestamps monotonic, and bounds memory when either encoder stalls.
//!
//! Video wins an equal-PTS tie: an audio frame whose timestamp equals the
//! sealing IDR belongs to the *new* segment that IDR opens, while every frame
//! strictly before it is fed first and stays in the closing segment.

use std::collections::VecDeque;

use anyhow::{Context, Result, bail};
use rotten_cast::hls::{HlsMuxer, Segment};

use crate::cast_audio::AacFrame;

/// Unsubmitted span of either track beyond which the session fails instead of
/// retaining more media the receiver will never play.
const MAX_BUFFER_SPAN_US: u64 = 2_000_000;
/// Unsubmitted bytes of either track beyond which the session fails.
const MAX_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// One encoded H.264 access unit bound for the muxer.
pub(crate) struct VideoAu {
    pub(crate) pts_us: u64,
    pub(crate) data: Vec<u8>,
}

/// Orders encoded video and AAC frames for the muxer by PTS watermark.
pub(crate) struct AudioVideoInterleaver {
    audio_enabled: bool,
    video: VecDeque<VideoAu>,
    audio: VecDeque<AacFrame>,
    video_watermark: Option<u64>,
    audio_watermark: Option<u64>,
    video_bytes: usize,
    audio_bytes: usize,
    last_submitted: Option<u64>,
    submitted_video: u64,
    submitted_audio: u64,
}

impl AudioVideoInterleaver {
    pub(crate) fn new(audio_enabled: bool) -> Self {
        Self {
            audio_enabled,
            video: VecDeque::new(),
            audio: VecDeque::new(),
            video_watermark: None,
            audio_watermark: None,
            video_bytes: 0,
            audio_bytes: 0,
            last_submitted: None,
            submitted_video: 0,
            submitted_audio: 0,
        }
    }

    /// Records one video grab. `encoded` is `None` when the H.264 encoder
    /// returned no access unit for the input; the watermark still advances on
    /// the input PTS so audio is never stalled behind a swallowed frame (the
    /// encoder emits nothing later for that timestamp). A produced access unit
    /// must carry exactly the input PTS: any other timestamp would unsort the
    /// queue the muxer receives.
    pub(crate) fn push_video(&mut self, input_pts_us: u64, encoded: Option<VideoAu>) -> Result<()> {
        if let Some(watermark) = self.video_watermark
            && input_pts_us < watermark
        {
            bail!(
                "video input timestamp {input_pts_us}us regressed below the {watermark}us watermark"
            );
        }
        self.video_watermark = Some(input_pts_us);
        let Some(access_unit) = encoded else {
            return Ok(());
        };
        if access_unit.pts_us != input_pts_us {
            bail!(
                "encoded video timestamp {}us does not match its input {input_pts_us}us; refusing to build a non-monotonic queue",
                access_unit.pts_us
            );
        }
        if let Some(front) = self.video.front() {
            let span = access_unit.pts_us.saturating_sub(front.pts_us);
            if span > MAX_BUFFER_SPAN_US {
                bail!(
                    "unsubmitted video spans {:.1} s, over the {} s buffer cap; the HLS muxer or the AAC encoder stalled",
                    span as f64 / 1e6,
                    MAX_BUFFER_SPAN_US / 1_000_000
                );
            }
        }
        if self.video_bytes + access_unit.data.len() > MAX_BUFFER_BYTES {
            bail!(
                "unsubmitted video exceeds the {} MiB buffer cap; the HLS muxer stalled",
                MAX_BUFFER_BYTES / (1024 * 1024)
            );
        }
        self.video_bytes += access_unit.data.len();
        self.video.push_back(access_unit);
        Ok(())
    }

    /// Records AAC frames as the encoder returns them. Frames must arrive with
    /// nondecreasing timestamps; buffered media is capped so a stalled muxer
    /// cannot grow memory without bound.
    pub(crate) fn push_audio(&mut self, frames: Vec<AacFrame>) -> Result<()> {
        if !self.audio_enabled {
            bail!("AAC frames reached a video-only Cast interleaver");
        }
        for frame in frames {
            if let Some(watermark) = self.audio_watermark
                && frame.pts_us < watermark
            {
                bail!(
                    "AAC timestamp {}us regressed below the {}us watermark",
                    frame.pts_us,
                    watermark
                );
            }
            self.audio_watermark = Some(frame.pts_us);
            if let Some(front) = self.audio.front() {
                let span = frame.pts_us.saturating_sub(front.pts_us);
                if span > MAX_BUFFER_SPAN_US {
                    bail!(
                        "unsubmitted AAC spans {:.1} s, over the {} s buffer cap; the HLS muxer stalled",
                        span as f64 / 1e6,
                        MAX_BUFFER_SPAN_US / 1_000_000
                    );
                }
            }
            if self.audio_bytes + frame.data.len() > MAX_BUFFER_BYTES {
                bail!(
                    "unsubmitted AAC exceeds the {} MiB buffer cap; the HLS muxer stalled",
                    MAX_BUFFER_BYTES / (1024 * 1024)
                );
            }
            self.audio_bytes += frame.data.len();
            self.audio.push_back(frame);
        }
        Ok(())
    }

    /// Submits every queued item whose PTS is covered by both track watermarks,
    /// in globally nondecreasing order. Video wins an equal-PTS tie, so an
    /// audio frame at an IDR timestamp is appended after the IDR opens the new
    /// segment; all strictly earlier audio is submitted first and closes the
    /// previous segment complete. Returns the segments sealed by the muxer.
    pub(crate) fn flush(&mut self, muxer: &mut HlsMuxer) -> Result<Vec<Segment>> {
        let limit = match (
            self.audio_enabled,
            self.video_watermark,
            self.audio_watermark,
        ) {
            (false, Some(video), _) => Some(video),
            (true, Some(video), Some(audio)) => Some(video.min(audio)),
            _ => None,
        };
        let Some(limit) = limit else {
            return Ok(Vec::new());
        };
        let mut sealed = Vec::new();
        loop {
            let audio_pts = self.audio.front().map(|frame| frame.pts_us);
            let video_pts = self.video.front().map(|access_unit| access_unit.pts_us);
            let (is_audio, pts) = match (audio_pts, video_pts) {
                (Some(audio), Some(video)) => {
                    if audio < video {
                        (true, audio)
                    } else {
                        (false, video)
                    }
                }
                (Some(audio), None) => (true, audio),
                (None, Some(video)) => (false, video),
                (None, None) => break,
            };
            if pts > limit {
                break;
            }
            if let Some(last) = self.last_submitted
                && pts < last
            {
                bail!("interleaver timestamp regression: {pts}us after {last}us is not monotonic");
            }
            if is_audio {
                let frame = self.audio.pop_front().expect("front was just inspected");
                self.audio_bytes = self.audio_bytes.saturating_sub(frame.data.len());
                muxer
                    .push_audio(&frame.data, frame.pts_us)
                    .context("muxing an AAC frame into HLS")?;
                self.submitted_audio += 1;
            } else {
                let access_unit = self.video.pop_front().expect("front was just inspected");
                self.video_bytes = self.video_bytes.saturating_sub(access_unit.data.len());
                if let Some(segment) = muxer
                    .push(&access_unit.data, access_unit.pts_us)
                    .context("muxing an H.264 access unit into HLS")?
                {
                    sealed.push(segment);
                }
                self.submitted_video += 1;
            }
            self.last_submitted = Some(pts);
        }
        Ok(sealed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xAC];
    const PPS: &[u8] = &[0x68, 0xCE, 0x38, 0x80];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x21, 0x11];
    const P_FRAME: &[u8] = &[0x41, 0x9A, 0x22, 0x33];

    fn annex_b(frame: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in [SPS, PPS, frame] {
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            out.extend_from_slice(nal);
        }
        out
    }

    fn video(pts_us: u64, idr: bool) -> VideoAu {
        VideoAu {
            pts_us,
            data: annex_b(if idr { IDR } else { P_FRAME }),
        }
    }

    /// A minimal valid 44.1 kHz stereo AAC-LC ADTS frame (7-byte header plus
    /// one payload byte, matching `frame_length`).
    fn aac(pts_us: u64) -> AacFrame {
        AacFrame {
            data: vec![0xFF, 0xF1, 0x50, 0x80, 0x01, 0x00, 0xFC, 0x00],
            pts_us,
        }
    }

    fn video_only() -> (AudioVideoInterleaver, HlsMuxer) {
        (AudioVideoInterleaver::new(false), HlsMuxer::new())
    }

    fn with_audio() -> (AudioVideoInterleaver, HlsMuxer) {
        (AudioVideoInterleaver::new(true), HlsMuxer::with_aac())
    }

    /// Decoder-free audio presence check: a standard MPEG-TS audio PES start
    /// code or an ADTS `0xFFFx` sync word inside the segment.
    fn segment_contains_audio(data: &[u8]) -> bool {
        data.windows(4)
            .any(|window| window == [0x00, 0x00, 0x01, 0xC0])
            || data
                .windows(2)
                .any(|window| window[0] == 0xFF && window[1] & 0xF6 == 0xF0)
    }

    #[test]
    fn video_only_mode_submits_every_encoded_frame() {
        let (mut interleaver, mut muxer) = video_only();
        interleaver
            .push_video(0, Some(video(0, true)))
            .expect("queue video");
        interleaver
            .push_video(33_333, Some(video(33_333, false)))
            .expect("queue video");
        let sealed = interleaver.flush(&mut muxer).expect("flush video");
        assert!(sealed.is_empty());
        assert!(interleaver.video.is_empty());
        assert_eq!(interleaver.submitted_video, 2);
    }

    #[test]
    fn audio_waits_for_the_video_watermark_and_video_waits_for_audio() {
        let (mut interleaver, mut muxer) = with_audio();
        // No video input yet: audio may queue but must not be submitted.
        interleaver
            .push_audio(vec![aac(0), aac(23_219)])
            .expect("queue AAC");
        interleaver.flush(&mut muxer).expect("flush");
        assert_eq!(interleaver.audio.len(), 2);
        assert_eq!(interleaver.submitted_audio, 0);

        // The video input watermark reaches 0: only audio up to 0 releases.
        interleaver
            .push_video(0, Some(video(0, true)))
            .expect("queue video");
        interleaver.flush(&mut muxer).expect("flush");
        assert_eq!(interleaver.audio.len(), 1);
        assert_eq!(interleaver.submitted_audio, 1);
        assert_eq!(interleaver.submitted_video, 1);

        // A video encoder that produced nothing for a 500 ms input must not
        // release the 500 ms frame while AAC has only returned to 23 ms.
        interleaver
            .push_video(500_000, Some(video(500_000, false)))
            .expect("queue video");
        interleaver.flush(&mut muxer).expect("flush");
        assert_eq!(interleaver.submitted_audio, 2);
        assert_eq!(
            interleaver.video.len(),
            1,
            "video beyond the AAC watermark must stay queued"
        );
        assert_eq!(interleaver.video.front().unwrap().pts_us, 500_000);

        // Once AAC reaches the video timestamp, the frame releases.
        interleaver
            .push_audio(vec![aac(500_000)])
            .expect("queue AAC");
        interleaver.flush(&mut muxer).expect("flush");
        assert!(interleaver.video.is_empty());
        assert_eq!(interleaver.submitted_video, 2);
    }

    #[test]
    fn delayed_aac_defers_the_sealing_idr_and_the_closed_segment_has_audio() {
        let (mut interleaver, mut muxer) = with_audio();
        // The segment opens at 1 s. The first AAC frame arrives just before.
        interleaver
            .push_video(1_000_000, Some(video(1_000_000, true)))
            .expect("queue IDR");
        interleaver
            .push_audio(vec![aac(999_000)])
            .expect("queue AAC");
        assert!(interleaver.flush(&mut muxer).expect("flush").is_empty());
        assert_eq!(
            interleaver.video.len(),
            1,
            "the opening IDR must stay queued"
        );

        // Video reaches the sealing IDR at 2 s while AAC only returned to
        // 1.499 s: nothing may seal yet.
        for pts in [1_033_000u64, 1_500_000, 1_999_000, 2_000_000] {
            interleaver
                .push_video(pts, Some(video(pts, pts == 2_000_000)))
                .expect("queue video");
        }
        let first_batch: Vec<AacFrame> = (999_000..=1_499_000).step_by(10_000).map(aac).collect();
        interleaver.push_audio(first_batch).expect("queue AAC");
        assert!(
            interleaver.flush(&mut muxer).expect("flush").is_empty(),
            "no segment may seal while AAC lags behind the IDR"
        );
        assert!(
            interleaver
                .video
                .iter()
                .any(|access_unit| access_unit.pts_us == 2_000_000),
            "the sealing IDR must be held back"
        );

        // Once AAC passes the IDR, the gap closes and the segment seals with
        // all audio that belongs to it.
        let second_batch: Vec<AacFrame> =
            (1_509_000..=2_399_000).step_by(10_000).map(aac).collect();
        interleaver.push_audio(second_batch).expect("queue AAC");
        let sealed = interleaver.flush(&mut muxer).expect("flush");
        assert_eq!(sealed.len(), 1, "exactly the closing IDR seals a segment");
        assert_eq!(sealed[0].sequence, 0);
        assert!(
            segment_contains_audio(&sealed[0].data),
            "the sealed segment must carry the AAC frames fed before its IDR"
        );
    }

    #[test]
    fn submission_order_is_monotonic_across_both_tracks() {
        let (mut interleaver, mut muxer) = with_audio();
        interleaver
            .push_video(0, Some(video(0, true)))
            .expect("queue video");
        interleaver
            .push_video(100_000, Some(video(100_000, false)))
            .expect("queue video");
        interleaver
            .push_video(200_000, None)
            .expect("advance the video watermark");
        interleaver
            .push_audio(vec![aac(50_000), aac(150_000), aac(200_000)])
            .expect("queue AAC");
        interleaver.flush(&mut muxer).expect("flush");
        // Both tracks must be fully drained: equal-to-watermark items release.
        assert!(interleaver.video.is_empty());
        assert!(interleaver.audio.is_empty());
        assert_eq!(interleaver.submitted_video, 2);
        assert_eq!(interleaver.submitted_audio, 3);
    }

    #[test]
    fn regressions_are_rejected_per_track() {
        let (mut interleaver, _muxer) = with_audio();
        interleaver
            .push_audio(vec![aac(10_000)])
            .expect("first AAC");
        let error = interleaver.push_audio(vec![aac(9_000)]).unwrap_err();
        assert!(error.to_string().contains("regressed"), "{error}");
        interleaver
            .push_video(10_000, Some(video(10_000, true)))
            .expect("first video input");
        let error = interleaver
            .push_video(9_000, None)
            .expect_err("video input regression must fail");
        assert!(error.to_string().contains("regressed"), "{error}");
    }

    #[test]
    fn audio_is_rejected_on_a_video_only_interleaver() {
        let (mut interleaver, _muxer) = video_only();
        let error = interleaver.push_audio(vec![aac(0)]).unwrap_err();
        assert!(error.to_string().contains("video-only"), "{error}");
    }

    #[test]
    fn the_unsubmitted_span_is_capped() {
        let (mut interleaver, _muxer) = with_audio();
        interleaver
            .push_video(0, Some(video(0, true)))
            .expect("queue video");
        // AAC never returns, so nothing submits; the span cap must trip.
        let error = interleaver
            .push_video(3_000_000, Some(video(3_000_000, true)))
            .expect_err("over-cap span must fail");
        assert!(error.to_string().contains("buffer cap"), "{error}");
    }

    #[test]
    fn the_unsubmitted_byte_cap_is_enforced() {
        let (mut interleaver, _muxer) = with_audio();
        let big = vec![0x41u8; 1024 * 1024];
        for index in 0..8 {
            interleaver
                .push_video(
                    0,
                    Some(VideoAu {
                        pts_us: 0,
                        data: big.clone(),
                    }),
                )
                .unwrap_or_else(|error| panic!("frame {index}: {error}"));
        }
        let error = interleaver
            .push_video(
                0,
                Some(VideoAu {
                    pts_us: 0,
                    data: big,
                }),
            )
            .expect_err("over-cap bytes must fail");
        assert!(error.to_string().contains("buffer cap"), "{error}");
    }

    // ---- independent TS demux for the boundary regression -------------------

    /// Splits a segment into `(pid, payload_unit_start, payload)` packets,
    /// skipping the transport and adaptation headers.
    fn ts_payloads(segment: &[u8]) -> Vec<(u16, bool, Vec<u8>)> {
        const TS_PACKET: usize = 188;
        assert_eq!(segment.len() % TS_PACKET, 0, "not whole TS packets");
        let mut packets = Vec::new();
        for packet in segment.as_chunks::<TS_PACKET>().0 {
            assert_eq!(packet[0], 0x47, "TS sync byte missing");
            let pid = ((u16::from(packet[1] & 0x1F)) << 8) | u16::from(packet[2]);
            let pusi = packet[1] & 0x40 != 0;
            let afc = (packet[3] >> 4) & 0x03;
            let mut offset = 4usize;
            if afc & 0x02 != 0 {
                let adaptation_len = packet[4] as usize;
                assert!(5 + adaptation_len <= TS_PACKET, "adaptation overruns");
                offset = 5 + adaptation_len;
            }
            let payload = if afc & 0x01 != 0 {
                packet[offset..].to_vec()
            } else {
                Vec::new()
            };
            packets.push((pid, pusi, payload));
        }
        packets
    }

    /// Reassembles the first complete PSI section on `pid`.
    fn psi_section(packets: &[(u16, bool, Vec<u8>)], pid: u16) -> Vec<u8> {
        let mut section = Vec::new();
        for (packet_pid, pusi, payload) in packets {
            if *packet_pid != pid || payload.is_empty() {
                continue;
            }
            if *pusi {
                let pointer = payload[0] as usize;
                section.clear();
                if let Some(rest) = payload.get(1 + pointer..) {
                    section.extend_from_slice(rest);
                }
            } else if !section.is_empty() {
                section.extend_from_slice(payload);
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

    /// PTS (90 kHz, as written by the muxer) of every audio PES packet.
    fn audio_pes_pts_90k(segment: &[u8]) -> Vec<u64> {
        let packets = ts_payloads(segment);
        let pat = psi_section(&packets, 0x0000);
        assert!(!pat.is_empty(), "PAT missing");
        let pmt_pid = ((u16::from(pat[10] & 0x1F)) << 8) | u16::from(pat[11]);
        let pmt = psi_section(&packets, pmt_pid);
        assert!(pmt.len() >= 12, "PMT missing or truncated");
        let program_info_len = (((pmt[10] & 0x0F) as usize) << 8) | pmt[11] as usize;
        let mut index = 12 + program_info_len;
        let mut audio_pid = None;
        while index + 4 < pmt.len() {
            let stream_type = pmt[index];
            let pid = ((u16::from(pmt[index + 1] & 0x1F)) << 8) | u16::from(pmt[index + 2]);
            let es_info_len = (((pmt[index + 3] & 0x0F) as usize) << 8) | pmt[index + 4] as usize;
            if stream_type == 0x0F {
                audio_pid = Some(pid);
            }
            index += 5 + es_info_len;
        }
        let audio_pid = audio_pid.expect("PMT must declare an AAC (0x0F) stream");

        let mut pts = Vec::new();
        let mut pes = Vec::new();
        for (pid, pusi, payload) in &packets {
            if *pid != audio_pid {
                continue;
            }
            if *pusi && !pes.is_empty() {
                if let Some(pts_90k) = pes_pts_90k(&pes) {
                    pts.push(pts_90k);
                }
                pes.clear();
            }
            pes.extend_from_slice(payload);
        }
        if !pes.is_empty()
            && let Some(pts_90k) = pes_pts_90k(&pes)
        {
            pts.push(pts_90k);
        }
        pts
    }

    fn pes_pts_90k(pes: &[u8]) -> Option<u64> {
        if pes.len() < 14 || &pes[..3] != b"\x00\x00\x01" || pes[7] & 0x80 == 0 {
            return None;
        }
        let bytes = &pes[9..14];
        Some(
            (((bytes[0] as u64) >> 1 & 0x07) << 30)
                | ((bytes[1] as u64) << 22)
                | (((bytes[2] as u64) >> 1) << 15)
                | ((bytes[3] as u64) << 7)
                | ((bytes[4] as u64) >> 1),
        )
    }

    /// The muxer adds a one-second PES offset to every timestamp.
    fn expected_pts_90k(pts_us: u64) -> u64 {
        (((pts_us as u128 * 9 / 100) as u64) + 90_000) & ((1u64 << 33) - 1)
    }

    /// Regression demux: a frame at exactly the sealing IDR timestamp must land
    /// in the *new* segment, the frame at PTS 0 must survive the opening IDR,
    /// and every frame strictly before the boundary must be complete in the
    /// closing segment.
    #[test]
    fn boundary_audio_opens_the_new_segment_and_prior_audio_is_complete() {
        let (mut interleaver, mut muxer) = with_audio();
        for (pts, idr) in [
            (0u64, true),
            (100_000, false),
            (500_000, false),
            (900_000, false),
            (1_000_000, true),
            (1_500_000, false),
            (2_000_000, true),
        ] {
            interleaver
                .push_video(pts, Some(video(pts, idr)))
                .expect("queue video");
        }
        let audio: Vec<AacFrame> = (0..=200).map(|step| aac(step * 10_000)).collect();
        interleaver.push_audio(audio).expect("queue AAC");

        let sealed = interleaver.flush(&mut muxer).expect("flush");
        assert_eq!(sealed.len(), 2, "both 1 s IDRs must seal a segment");

        let closing = audio_pes_pts_90k(&sealed[0].data);
        let opening = audio_pes_pts_90k(&sealed[1].data);

        // Segment 0 carries every frame strictly before the boundary, from the
        // retained frame at PTS 0 through 990 ms, and nothing at/after 1 s.
        let expected_closing: Vec<u64> = (0..=99)
            .map(|step| expected_pts_90k(step * 10_000))
            .collect();
        assert_eq!(
            closing, expected_closing,
            "prior audio must close the segment complete"
        );
        assert!(!closing.contains(&expected_pts_90k(1_000_000)));

        // The boundary frame opens segment 1 and is followed in order.
        let expected_opening: Vec<u64> = (100..=199)
            .map(|step| expected_pts_90k(step * 10_000))
            .collect();
        assert_eq!(
            opening, expected_opening,
            "boundary audio must open the new segment"
        );
        assert_eq!(opening.first(), Some(&expected_pts_90k(1_000_000)));
    }
}
