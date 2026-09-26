//! Fixed-grid PCM timeline for the Cast system-audio path.
//!
//! WASAPI delivers timestamped 44.1 kHz stereo S16 LE packets whose first
//! sample can start before the session origin, overlap a previous packet, or
//! leave a hole when the capture queue dropped one. The AAC encoder needs a
//! gapless stream of exactly 1024-sample frames in sample order instead. This
//! module turns timestamped packets into that stream: it trims data that is
//! already late, rejects malformed, oversized and far-future packets, fills
//! missing regions with silence on the absolute sample grid, and never resets
//! the counter to skip a gap (which would compress time and desync A/V).

use std::collections::VecDeque;

use anyhow::{Result, anyhow, bail};

use crate::audio::timed::TimedPcm;

/// Output sample rate of the Cast audio stream.
pub(crate) const SAMPLE_RATE: u64 = 44_100;
/// Interleaved stereo signed 16-bit little-endian.
pub(crate) const BYTES_PER_FRAME: usize = 4;
/// AAC-LC frame length the encoder pipeline consumes.
pub(crate) const AAC_FRAME_FRAMES: usize = 1024;
/// Bytes of one AAC input block (exactly 1024 sample frames).
pub(crate) const AAC_FRAME_BYTES: usize = AAC_FRAME_FRAMES * BYTES_PER_FRAME;
/// Emit this far behind the shared clock so capture packets usually arrive
/// before their deadline instead of being replaced by silence.
pub(crate) const HOLD_BACK_FRAMES: u64 = SAMPLE_RATE / 10;
/// Hard cap on queued but unemitted PCM; beyond this the oldest data would
/// only ever play after a long catch-up.
pub(crate) const QUEUE_CAP_FRAMES: u64 = SAMPLE_RATE * 2;
/// A clock lag beyond this is reported instead of chasing it forever.
pub(crate) const LAG_LIMIT_FRAMES: u64 = SAMPLE_RATE * 2;
/// Upper bound on AAC blocks emitted per poll so a backlog can never spin the
/// loop. 32 blocks are ~743 ms of audio, enough headroom for a video iteration
/// that takes 200-300 ms (slow encode) without the audio grid falling behind.
pub(crate) const MAX_BLOCKS_PER_POLL: usize = 32;

/// Far-future packets beyond this horizon are a capture bug, not a gap.
const HORIZON_FRAMES: u64 = SAMPLE_RATE * 2;

/// Converts microseconds on the shared session clock to absolute 44.1 kHz
/// frames. Exact rational conversion: 44_100 / 1_000_000 = 441 / 10_000.
pub(crate) fn frames_from_us(us: u64) -> u64 {
    let frames = u128::from(us) * 441 / 10_000;
    frames.min(u128::from(u64::MAX)) as u64
}

/// Converts an absolute frame index back to microseconds on the same rational
/// grid (floored). Kept next to the forward conversion so the tests pin both
/// directions of the grid.
#[cfg(test)]
pub(crate) fn us_from_frames(frames: u64) -> u64 {
    let us = u128::from(frames) * 10_000 / 441;
    us.min(u128::from(u64::MAX)) as u64
}

/// True during the first 100 ms of each clock second.
pub(crate) fn pulse_active(frame: u64) -> bool {
    frame % SAMPLE_RATE < SAMPLE_RATE / 10
}

/// Deterministic low-amplitude 440 Hz pulse used by `--test`: 100 ms of tone
/// followed by 900 ms of silence, phase-locked to whole seconds on the
/// absolute sample grid. No audio device is touched.
pub(crate) fn synthetic_pcm(start_frame: u64, frames: usize) -> Vec<u8> {
    let mut out = vec![0u8; frames * BYTES_PER_FRAME];
    for index in 0..frames {
        let frame = start_frame + index as u64;
        if !pulse_active(frame) {
            continue;
        }
        let seconds = frame as f64 / SAMPLE_RATE as f64;
        let sample = ((seconds * 440.0 * std::f64::consts::TAU).sin() * 0.1 * 32_767.0) as i16;
        let bytes = sample.to_le_bytes();
        let at = index * BYTES_PER_FRAME;
        out[at..at + 2].copy_from_slice(&bytes);
        out[at + 2..at + 4].copy_from_slice(&bytes);
    }
    out
}

/// One captured packet, trimmed so an earlier chunk always wins an overlap.
struct Chunk {
    start: i64,
    data: Vec<u8>,
}

impl Chunk {
    fn frames(&self) -> usize {
        self.data.len() / BYTES_PER_FRAME
    }

    fn end(&self) -> i64 {
        self.start + self.frames() as i64
    }

    /// Drops everything before `keep_from` (frames are 4 bytes wide).
    fn trim_to(&mut self, keep_from: i64) {
        let skip = keep_from.saturating_sub(self.start).max(0) as usize;
        let skip = skip.min(self.frames());
        self.data.drain(..skip * BYTES_PER_FRAME);
        self.start = keep_from;
    }
}

/// Turns timestamped PCM packets into continuous 1024-frame AAC blocks.
///
/// The grid is anchored at absolute sample 0 of the shared session clock (the
/// AAC encoder requires contiguous blocks starting at frame 0); a slow capture
/// open is covered by silence, and a gap is never skipped.
pub(crate) struct PcmTimeline {
    /// Start frame of the next AAC block.
    cursor: u64,
    /// Samples of the block being assembled (always < [`AAC_FRAME_BYTES`]).
    block: Vec<u8>,
    /// Captured, not-yet-consumed chunks sorted and kept disjoint by start.
    pending: VecDeque<Chunk>,
    emitted_frames: u64,
    dropped_late_frames: u64,
    dropped_overflow_frames: u64,
}

impl PcmTimeline {
    pub(crate) fn new() -> Self {
        Self {
            cursor: 0,
            block: Vec::with_capacity(AAC_FRAME_BYTES),
            pending: VecDeque::new(),
            emitted_frames: 0,
            dropped_late_frames: 0,
            dropped_overflow_frames: 0,
        }
    }

    /// Frames emitted to the encoder so far.
    pub(crate) fn emitted_frames(&self) -> u64 {
        self.emitted_frames
    }

    /// Captured frames dropped because they arrived after their grid position.
    #[cfg(test)]
    fn dropped_late_frames(&self) -> u64 {
        self.dropped_late_frames
    }

    /// Captured frames dropped because the queue cap was exceeded.
    #[cfg(test)]
    fn dropped_overflow_frames(&self) -> u64 {
        self.dropped_overflow_frames
    }

    /// Adds one captured packet. Returns an error for malformed or far-future
    /// input so a broken capture can never grow memory without bound.
    pub(crate) fn push(&mut self, packet: TimedPcm) -> Result<()> {
        let data_len = packet.data.len();
        if !data_len.is_multiple_of(BYTES_PER_FRAME) {
            bail!(
                "system audio packet has {data_len} bytes, not a whole number of 44.1 kHz stereo S16 frames"
            );
        }
        let frames = data_len / BYTES_PER_FRAME;
        if frames == 0 {
            return Ok(());
        }
        if frames as u64 > HORIZON_FRAMES {
            bail!(
                "system audio packet spans {frames} frames (over {} s); refusing to buffer it",
                HORIZON_FRAMES / SAMPLE_RATE
            );
        }
        let end = packet
            .start_frame
            .checked_add(i64::try_from(frames).unwrap_or(i64::MAX))
            .ok_or_else(|| {
                anyhow!("system audio packet timestamp overflowed the sample timeline")
            })?;

        let cursor = i64::try_from(self.cursor).unwrap_or(i64::MAX);
        if end <= cursor {
            self.dropped_late_frames += frames as u64;
            return Ok(());
        }
        if packet.start_frame.saturating_sub(cursor) > HORIZON_FRAMES as i64 {
            bail!(
                "system audio packet starts {} frames past the session clock; refusing to wait for it",
                packet.start_frame.saturating_sub(cursor)
            );
        }

        let mut chunk = Chunk {
            start: packet.start_frame,
            data: packet.data,
        };
        if chunk.start < cursor {
            chunk.trim_to(cursor);
        }

        let index = self
            .pending
            .iter()
            .rposition(|pending| pending.start <= chunk.start)
            .map_or(0, |position| position + 1);
        self.pending.insert(index, chunk);
        self.normalize(index);
        self.enforce_queue_cap();
        Ok(())
    }

    /// Emits every complete AAC block whose deadline (`block end + hold-back`)
    /// has passed, filling holes with silence. At most `max_blocks` blocks are
    /// emitted per call; the rest wait for the next producer poll. `should_stop`
    /// is checked before every block so a long catch-up aborts promptly.
    ///
    /// A sink error (or any other error) is fatal for the timeline: the caller
    /// must drop it instead of reusing it, because the failed block is not
    /// counted. The cursor advances after every successful block.
    pub(crate) fn emit_ready<F>(
        &mut self,
        deadline_frame: u64,
        max_blocks: usize,
        mut should_stop: impl FnMut() -> bool,
        mut sink: F,
    ) -> Result<usize>
    where
        F: FnMut(&[u8], u64) -> Result<()>,
    {
        let mut next = self.cursor;
        if deadline_frame.saturating_sub(next) > LAG_LIMIT_FRAMES {
            bail!(
                "system audio is {:.1} s behind the session clock; capture or AAC encoding stalled, stopping instead of an unbounded catch-up",
                deadline_frame.saturating_sub(next) as f64 / SAMPLE_RATE as f64
            );
        }
        let mut emitted = 0;
        while emitted < max_blocks && next + AAC_FRAME_FRAMES as u64 <= deadline_frame {
            if should_stop() {
                break;
            }
            self.fill_to(next, next + AAC_FRAME_FRAMES as u64);
            debug_assert_eq!(self.block.len(), AAC_FRAME_BYTES);
            sink(&self.block, next)?;
            self.block.clear();
            next += AAC_FRAME_FRAMES as u64;
            self.emitted_frames += AAC_FRAME_FRAMES as u64;
            self.cursor = next;
            emitted += 1;
        }
        Ok(emitted)
    }

    /// Fills `block` with samples `[cursor, target)` from pending chunks,
    /// using silence for anything not captured in time.
    fn fill_to(&mut self, mut cursor: u64, target: u64) {
        while cursor < target {
            let cursor_i = i64::try_from(cursor).unwrap_or(i64::MAX);
            while self
                .pending
                .front()
                .is_some_and(|front| front.end() <= cursor_i)
            {
                self.pending.pop_front();
            }
            match self.pending.front_mut() {
                Some(front) if front.start <= cursor_i => {
                    let skip = (cursor_i - front.start) as usize;
                    let available = front.frames() - skip;
                    let take = usize::try_from(target - cursor)
                        .unwrap_or(usize::MAX)
                        .min(available);
                    let from = skip * BYTES_PER_FRAME;
                    self.block
                        .extend_from_slice(&front.data[from..from + take * BYTES_PER_FRAME]);
                    cursor += take as u64;
                    if take == available {
                        self.pending.pop_front();
                    } else {
                        let resume = i64::try_from(cursor).unwrap_or(i64::MAX);
                        front.trim_to(resume);
                    }
                }
                _ => {
                    let next_start = self
                        .pending
                        .front()
                        .map(|chunk| u64::try_from(chunk.start).unwrap_or(u64::MAX))
                        .unwrap_or(target);
                    let hole_end = next_start.min(target).max(cursor);
                    let hole_frames = (hole_end - cursor) as usize;
                    self.block
                        .resize(self.block.len() + hole_frames * BYTES_PER_FRAME, 0);
                    cursor += hole_frames as u64;
                }
            }
        }
    }

    /// Earlier chunks win overlaps; a chunk fully covered by its predecessor
    /// is discarded.
    fn normalize(&mut self, index: usize) {
        if index > 0 {
            let previous_end = self.pending[index - 1].end();
            if self.pending[index].end() <= previous_end {
                let removed = self.pending.remove(index).expect("index checked");
                self.dropped_late_frames += removed.frames() as u64;
                return;
            }
            if self.pending[index].start < previous_end {
                self.pending[index].trim_to(previous_end);
            }
        }
        while index + 1 < self.pending.len() {
            let end = self.pending[index].end();
            if self.pending[index + 1].start >= end {
                break;
            }
            if self.pending[index + 1].end() <= end {
                let removed = self.pending.remove(index + 1).expect("index checked");
                self.dropped_late_frames += removed.frames() as u64;
            } else {
                self.pending[index + 1].trim_to(end);
                break;
            }
        }
    }

    /// Bounds queued memory by dropping the newest chunks; the gap stays a gap
    /// (silence later) while already captured chunks keep their timestamps.
    fn enforce_queue_cap(&mut self) {
        if self.buffered_frames() <= QUEUE_CAP_FRAMES {
            return;
        }
        let mut dropped = 0u64;
        while self.buffered_frames() > QUEUE_CAP_FRAMES {
            let Some(chunk) = self.pending.pop_back() else {
                break;
            };
            dropped += chunk.frames() as u64;
        }
        self.dropped_overflow_frames += dropped;
        tracing::warn!(
            dropped_frames = dropped,
            "system audio backlog exceeded the {} s queue cap; the newest packet was dropped",
            QUEUE_CAP_FRAMES / SAMPLE_RATE
        );
    }

    fn buffered_frames(&self) -> u64 {
        self.pending.iter().map(|chunk| chunk.frames() as u64).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(start: i64, frames: usize, seed: i16) -> TimedPcm {
        let mut data = Vec::with_capacity(frames * BYTES_PER_FRAME);
        for index in 0..frames {
            let sample = seed.wrapping_add(index as i16);
            data.extend_from_slice(&sample.to_le_bytes());
            data.extend_from_slice(&sample.to_le_bytes());
        }
        TimedPcm {
            start_frame: start,
            data,
        }
    }

    fn sample_at(data: &[u8], frame: usize) -> i16 {
        i16::from_le_bytes([data[frame * 4], data[frame * 4 + 1]])
    }

    /// Emits up to `cap` blocks up to `deadline` from the sample-0 grid.
    fn emit(timeline: &mut PcmTimeline, deadline: u64, cap: usize) -> Vec<(u64, Vec<u8>)> {
        let mut blocks = Vec::new();
        timeline
            .emit_ready(
                deadline,
                cap,
                || false,
                |data, start| {
                    blocks.push((start, data.to_vec()));
                    Ok(())
                },
            )
            .expect("emit blocks");
        blocks
    }

    #[test]
    fn microseconds_and_frames_share_one_rational_grid() {
        assert_eq!(frames_from_us(0), 0);
        assert_eq!(frames_from_us(1_000_000), 44_100);
        assert_eq!(us_from_frames(0), 0);
        assert_eq!(us_from_frames(44_100), 1_000_000);
        // 1024 frames are ~23.22 ms; the rational floor is stable.
        assert_eq!(us_from_frames(AAC_FRAME_FRAMES as u64), 23_219);
        assert_eq!(frames_from_us(23_219), 1023);
        let mut previous = 0;
        for frame in (0..200_000u64).step_by(97) {
            let us = us_from_frames(frame);
            assert!(us >= previous, "us must not go backwards at frame {frame}");
            previous = us;
        }
    }

    #[test]
    fn an_empty_timeline_emits_timestamped_silence() {
        let mut timeline = PcmTimeline::new();
        assert!(emit(&mut timeline, 0, MAX_BLOCKS_PER_POLL).is_empty());
        let blocks = emit(
            &mut timeline,
            3 * AAC_FRAME_FRAMES as u64,
            MAX_BLOCKS_PER_POLL,
        );
        assert_eq!(blocks.len(), 3);
        for (index, (start, data)) in blocks.iter().enumerate() {
            assert_eq!(*start, index as u64 * AAC_FRAME_FRAMES as u64);
            assert_eq!(data.len(), AAC_FRAME_BYTES);
            assert!(
                data.iter().all(|&byte| byte == 0),
                "block {index} is not silent"
            );
        }
        assert_eq!(timeline.emitted_frames(), 3 * AAC_FRAME_FRAMES as u64);
    }

    #[test]
    fn a_gap_is_filled_with_silence_without_compressing_time() {
        let mut timeline = PcmTimeline::new();
        emit(&mut timeline, 0, MAX_BLOCKS_PER_POLL);
        timeline.push(pcm(0, 100, 1000)).unwrap();
        timeline.push(pcm(300, 100, 2000)).unwrap();
        let blocks = emit(
            &mut timeline,
            2 * AAC_FRAME_FRAMES as u64,
            MAX_BLOCKS_PER_POLL,
        );
        assert_eq!(blocks.len(), 2);
        let first = &blocks[0].1;
        for frame in 0..100 {
            assert_eq!(sample_at(first, frame), 1000 + frame as i16);
        }
        for frame in 100..300 {
            assert_eq!(sample_at(first, frame), 0, "hole frame {frame}");
        }
        for frame in 300..400 {
            assert_eq!(sample_at(first, frame), 2000 + (frame - 300) as i16);
        }
        assert_eq!(blocks[1].0, AAC_FRAME_FRAMES as u64);
        assert_eq!(timeline.dropped_late_frames(), 0);
    }

    #[test]
    fn overlapping_packets_keep_the_earliest_samples() {
        let mut timeline = PcmTimeline::new();
        emit(&mut timeline, 0, MAX_BLOCKS_PER_POLL);
        timeline.push(pcm(0, 100, 1000)).unwrap();
        timeline.push(pcm(50, 100, 2000)).unwrap();
        let blocks = emit(&mut timeline, AAC_FRAME_FRAMES as u64, MAX_BLOCKS_PER_POLL);
        let first = &blocks[0].1;
        for frame in 0..100 {
            assert_eq!(sample_at(first, frame), 1000 + frame as i16);
        }
        for frame in 100..150 {
            assert_eq!(sample_at(first, frame), 2000 + (frame - 50) as i16);
        }
        for frame in 150..200 {
            assert_eq!(sample_at(first, frame), 0);
        }
        // A packet fully inside the already emitted range is dropped.
        timeline.push(pcm(10, 50, 3000)).unwrap();
        assert_eq!(timeline.dropped_late_frames(), 50);
    }

    #[test]
    fn negative_startup_frames_are_trimmed() {
        let mut timeline = PcmTimeline::new();
        emit(&mut timeline, 0, MAX_BLOCKS_PER_POLL);
        // One second before the origin, plus 100 frames of real content.
        timeline.push(pcm(-44_100, 44_200, 0)).unwrap();
        let blocks = emit(&mut timeline, AAC_FRAME_FRAMES as u64, MAX_BLOCKS_PER_POLL);
        let first = &blocks[0].1;
        for frame in 0..100 {
            assert_eq!(
                sample_at(first, frame),
                (44_100 + frame) as i16,
                "frame {frame} must carry the packet sample at its absolute position"
            );
        }
        assert_eq!(sample_at(first, 100), 0);
        // Fully late data is dropped, never replayed.
        timeline.push(pcm(-10_000, 100, 1)).unwrap();
        assert_eq!(timeline.dropped_late_frames(), 100);
    }

    #[test]
    fn malformed_oversized_and_far_future_packets_fail_instead_of_growing() {
        let mut timeline = PcmTimeline::new();
        emit(&mut timeline, 0, MAX_BLOCKS_PER_POLL);
        assert!(timeline.push(pcm(0, 0, 0)).is_ok());
        let error = timeline
            .push(TimedPcm {
                start_frame: 0,
                data: vec![0; 5],
            })
            .unwrap_err();
        assert!(error.to_string().contains("whole number"), "{error}");
        let error = timeline
            .push(pcm(0, HORIZON_FRAMES as usize + 1, 0))
            .unwrap_err();
        assert!(error.to_string().contains("refusing to buffer"), "{error}");
        let error = timeline
            .push(pcm(HORIZON_FRAMES as i64 + 1, 10, 0))
            .unwrap_err();
        assert!(
            error.to_string().contains("past the session clock"),
            "{error}"
        );
    }

    #[test]
    fn the_queue_cap_drops_the_newest_packet_and_keeps_older_chunks() {
        let mut timeline = PcmTimeline::new();
        emit(&mut timeline, 0, MAX_BLOCKS_PER_POLL);
        timeline.push(pcm(0, 60_000, 1)).unwrap();
        timeline.push(pcm(60_000, 60_000, 2)).unwrap();
        assert!(timeline.buffered_frames() <= QUEUE_CAP_FRAMES);
        assert!(timeline.dropped_overflow_frames() > 0);
        assert_eq!(timeline.pending.front().unwrap().start, 0);
    }

    #[test]
    fn a_lagging_timeline_fails_instead_of_unbounded_catch_up() {
        let mut timeline = PcmTimeline::new();
        emit(&mut timeline, 0, MAX_BLOCKS_PER_POLL);
        let error = timeline
            .emit_ready(
                LAG_LIMIT_FRAMES + AAC_FRAME_FRAMES as u64,
                MAX_BLOCKS_PER_POLL,
                || false,
                |_, _| Ok(()),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("behind the session clock"),
            "{error}"
        );
    }

    #[test]
    fn a_stop_during_a_block_drain_ends_the_poll_promptly() {
        let mut timeline = PcmTimeline::new();
        let mut checks = 0usize;
        let emitted = timeline
            .emit_ready(
                64 * AAC_FRAME_FRAMES as u64,
                MAX_BLOCKS_PER_POLL,
                || {
                    checks += 1;
                    checks > 3
                },
                |_, _| Ok(()),
            )
            .expect("a stop is a clean early return");
        assert_eq!(emitted, 3, "blocks before the stop are kept");
        assert_eq!(timeline.emitted_frames(), 3 * AAC_FRAME_FRAMES as u64);
    }

    /// Models a producer iteration that takes 250 ms (slow capture/encode)
    /// over 30 s: the 32-block budget must keep the audio grid contiguous and
    /// the backlog below one block after every poll.
    #[test]
    fn a_slow_video_loop_keeps_the_audio_grid_contiguous_and_bounded() {
        const ITERATION_FRAMES: u64 = SAMPLE_RATE / 4; // 250 ms
        const ITERATIONS: u64 = 120; // 30 s
        let mut timeline = PcmTimeline::new();
        let mut next_start = 0u64;
        let mut blocks = 0usize;
        for iteration in 1..=ITERATIONS {
            let deadline = iteration * ITERATION_FRAMES;
            timeline
                .emit_ready(
                    deadline,
                    MAX_BLOCKS_PER_POLL,
                    || false,
                    |_, start| {
                        assert_eq!(start, next_start, "blocks must stay contiguous");
                        next_start += AAC_FRAME_FRAMES as u64;
                        blocks += 1;
                        Ok(())
                    },
                )
                .expect("slow iterations stay within the lag cap");
            assert!(
                deadline - next_start < AAC_FRAME_FRAMES as u64,
                "backlog after iteration {iteration} must stay below one block"
            );
        }
        assert_eq!(
            blocks,
            (ITERATIONS * ITERATION_FRAMES / AAC_FRAME_FRAMES as u64) as usize
        );
        assert_eq!(next_start, blocks as u64 * AAC_FRAME_FRAMES as u64);
    }

    #[test]
    fn emission_is_bounded_per_poll_and_resumes_later() {
        let mut timeline = PcmTimeline::new();
        emit(&mut timeline, 0, MAX_BLOCKS_PER_POLL);
        assert_eq!(emit(&mut timeline, 10 * 1024, 2).len(), 2);
        assert_eq!(emit(&mut timeline, 10 * 1024, MAX_BLOCKS_PER_POLL).len(), 8);
        assert_eq!(emit(&mut timeline, 12 * 1024, MAX_BLOCKS_PER_POLL).len(), 2);
    }

    #[test]
    fn the_first_block_starts_at_sample_zero_even_after_a_slow_start() {
        let mut timeline = PcmTimeline::new();
        // Capture only starts 500 ms after the shared clock origin; the AAC
        // encoder requires the first block to be frame 0, so the lead-in must
        // be emitted as silence, never skipped.
        timeline.push(pcm(22_050, 4_410, 7)).unwrap();
        let mut blocks = Vec::new();
        loop {
            let more = emit(&mut timeline, 50_000, MAX_BLOCKS_PER_POLL);
            if more.is_empty() {
                break;
            }
            blocks.extend(more);
        }
        assert_eq!(blocks.len(), 48, "every complete block up to the deadline");
        for (index, (start, _)) in blocks.iter().enumerate() {
            assert_eq!(*start, index as u64 * AAC_FRAME_FRAMES as u64);
        }
        let block = &blocks[22_050 / AAC_FRAME_FRAMES].1;
        let offset = 22_050 % AAC_FRAME_FRAMES;
        assert_eq!(sample_at(block, offset), 7);
        assert_eq!(sample_at(block, offset - 1), 0, "hole before the capture");
    }

    #[test]
    fn an_incomplete_block_waits_for_its_deadline() {
        let mut timeline = PcmTimeline::new();
        emit(&mut timeline, 0, MAX_BLOCKS_PER_POLL);
        assert!(
            emit(
                &mut timeline,
                AAC_FRAME_FRAMES as u64 - 1,
                MAX_BLOCKS_PER_POLL
            )
            .is_empty()
        );
        assert_eq!(
            emit(&mut timeline, AAC_FRAME_FRAMES as u64, MAX_BLOCKS_PER_POLL).len(),
            1
        );
    }

    #[test]
    fn the_synthetic_pulse_is_phase_aligned_and_deterministic() {
        assert!(pulse_active(0));
        assert!(pulse_active(SAMPLE_RATE / 10 - 1));
        assert!(!pulse_active(SAMPLE_RATE / 10));
        assert!(pulse_active(SAMPLE_RATE));
        let active = synthetic_pcm(0, SAMPLE_RATE as usize / 10);
        assert!(
            active
                .as_chunks::<2>()
                .0
                .iter()
                .any(|sample| *sample != [0, 0])
        );
        let silent = synthetic_pcm(SAMPLE_RATE / 10, 1_000);
        assert!(silent.iter().all(|&byte| byte == 0));
        assert_eq!(synthetic_pcm(1_234, 64), synthetic_pcm(1_234, 64));
        for sample in active.as_chunks::<2>().0 {
            let value = i16::from_le_bytes([sample[0], sample[1]]);
            assert!(
                value.unsigned_abs() < 4_000,
                "pulse must stay low amplitude"
            );
        }
    }
}
