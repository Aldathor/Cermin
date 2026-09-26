//! AAC encoding for the Cast audio path.
//!
//! This module wraps the built-in Windows Media Foundation AAC encoder MFT
//! (`CLSID_AACMFTEncoder`, exposed as
//! `windows::Win32::Media::MediaFoundation::AACMFTEncoder`). The encoder is a
//! software component that ships with Windows: no third-party codec, FFmpeg or
//! extra DLL is involved, and no audio is captured or played back here.
//!
//! Input is always 44.1 kHz stereo signed 16-bit little-endian PCM, delivered
//! as contiguous blocks of exactly [`PCM_BLOCK_BYTES`] bytes (1024 PCM frames
//! per block). One call to [`AacEncoder::encode`] corresponds to one such block
//! and `start_frame` counts 44.1 kHz sample frames from the session origin.
//! Output frames are ADTS-wrapped AAC-LC at 128 kbit/s. When the platform
//! encoder accepts `MF_MT_AAC_PAYLOAD_TYPE = 1` the ADTS framing comes from
//! Media Foundation; otherwise the raw AAC frames are wrapped by this module.
//!
//! Timestamps: input sample times/durations are derived with 128-bit integer
//! arithmetic from the absolute 44.1 kHz frame position, so the
//! 10 MHz (100 ns) Media Foundation timebase does not drift across a session.
//! Output PTS values come from `IMFSample::GetSampleTime` (never from the wall
//! clock); when one output sample carries several ADTS frames, each following
//! frame advances by exactly 1024 frames at 44.1 kHz.
//!
//! Threading: Media Foundation and COM are initialized on the thread that
//! creates [`AacEncoder`], and the transform is destroyed before
//! `MFShutdown`/`CoUninitialize` run on that same thread. The encoder is
//! intentionally neither `Send` nor `Sync`.

#[cfg(not(target_os = "windows"))]
use anyhow::Result;

/// One encoded AAC frame.
///
/// `data` is one complete ADTS frame (a 7-byte ADTS header followed by the raw
/// AAC payload), so callers can hand it to a receiver or decoder as-is.
/// `pts_us` is the presentation timestamp in microseconds relative to the
/// session origin used by [`AacEncoder::encode`]'s `start_frame`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AacFrame {
    pub data: Vec<u8>,
    pub pts_us: u64,
}

/// PCM sample rate accepted by [`AacEncoder`].
pub const SAMPLE_RATE: u32 = 44_100;
/// PCM channel count accepted by [`AacEncoder`].
pub const CHANNELS: u16 = 2;
/// PCM frames (per-channel samples) in one AAC frame and one `encode` block.
pub const FRAMES_PER_BLOCK: usize = 1024;
/// Exact input block size in bytes: 1024 frames * 2 channels * 2 bytes.
pub const PCM_BLOCK_BYTES: usize = FRAMES_PER_BLOCK * CHANNELS as usize * 2;
/// Requested AAC bitrate in bits per second.
pub const AAC_BITRATE_BPS: u32 = 128_000;
/// `MF_MT_AUDIO_AVG_BYTES_PER_SECOND` value for [`AAC_BITRATE_BPS`].
pub const AAC_BYTES_PER_SECOND: u32 = AAC_BITRATE_BPS / 8;
/// Duration of one encoded AAC frame in 100-ns units (truncated).
pub const AAC_FRAME_DURATION_HNS: i64 =
    (FRAMES_PER_BLOCK as u64 * 10_000_000 / SAMPLE_RATE as u64) as i64;

#[cfg(target_os = "windows")]
pub use win::AacEncoder;

#[cfg(target_os = "windows")]
mod win {
    use super::{
        AAC_BYTES_PER_SECOND, AAC_FRAME_DURATION_HNS, AacFrame, CHANNELS, FRAMES_PER_BLOCK,
        PCM_BLOCK_BYTES, SAMPLE_RATE,
    };
    use anyhow::{Result, anyhow, bail};
    use std::marker::PhantomData;
    use std::mem::ManuallyDrop;
    use std::rc::Rc;
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::Media::MediaFoundation::{
        AACMFTEncoder, IMFCollection, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform,
        MF_E_BUFFERTOOSMALL, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
        MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION,
        MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
        MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
        MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_VERSION, MFAudioFormat_AAC, MFAudioFormat_PCM,
        MFCreateAlignedMemoryBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
        MFMediaType_Audio, MFSTARTUP_FULL, MFShutdown, MFStartup, MFT_INPUT_STREAM_INFO,
        MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
        MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
        MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE,
        MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE, MFT_OUTPUT_DATA_BUFFER_STREAM_END,
        MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };

    /// The AAC encoder always has a single input and a single output stream,
    /// both with stream identifier 0.
    const INPUT_STREAM_ID: u32 = 0;
    const OUTPUT_STREAM_ID: u32 = 0;

    /// Media Foundation uses 100-ns units for sample times and durations.
    const HNS_PER_SECOND: u128 = 10_000_000;
    /// One AAC frame is [`FRAMES_PER_BLOCK`] PCM frames.
    const HNS_PER_FRAME: u128 = AAC_FRAME_DURATION_HNS as u128;
    /// `MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION` for AAC-LC (profile L2).
    const AAC_PROFILE_LEVEL_LC: u32 = 0x29;
    /// ADTS `sampling_frequency_index` for 44.1 kHz.
    const ADTS_SAMPLING_FREQUENCY_INDEX: u8 = 4;
    const ADTS_HEADER_LEN: usize = 7;
    /// Largest raw AAC payload an ADTS `frame_length` field can describe.
    const MAX_ADTS_PAYLOAD: usize = 0x1FFF - ADTS_HEADER_LEN;

    const MIN_OUTPUT_BUFFER: u32 = 8 * 1024;
    const MAX_OUTPUT_BUFFER: u32 = 1 << 20;
    /// A single MFT output sample may never exceed the output buffer cap, even
    /// when the MFT allocates it itself.
    const MAX_OUTPUT_SAMPLE_BYTES: usize = MAX_OUTPUT_BUFFER as usize;
    const MAX_STREAM_CHANGES: usize = 2;
    const MAX_NOTACCEPTING_RETRIES: usize = 4;
    const MAX_OUTPUT_CALLS_PER_DRAIN: usize = 32;
    const MAX_OUTPUT_BYTES_PER_DRAIN: usize = 1 << 20;

    /// Error used whenever the platform AAC encoder (or Media Foundation
    /// itself) is not available, with the usual remediation path.
    fn aac_unavailable(detail: impl std::fmt::Display) -> anyhow::Error {
        anyhow!(
            "Windows Media Foundation AAC encoder is unavailable: {detail}. \
             Install the Windows Media Feature Pack (N editions) or use a Windows \
             installation that includes Media Foundation audio codecs; a video-only \
             or codec-stripped image cannot encode AAC audio."
        )
    }

    /// Balances `CoInitializeEx` on the owning thread.
    struct ComGuard(bool);

    impl ComGuard {
        fn initialize() -> Result<Self> {
            match unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
                Ok(_) => Ok(Self(true)),
                // Another apartment is already active on this thread. Media
                // Foundation does not require us to own it, and calling
                // CoUninitialize here would unbalance the existing apartment.
                Err(e) if e.code() == RPC_E_CHANGED_MODE => Ok(Self(false)),
                Err(e) => Err(aac_unavailable(format!(
                    "COM could not be initialized on the encoding thread: {e}"
                ))),
            }
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    /// Owns Media Foundation (and COM) for the thread that creates an encoder.
    /// The transform is a sibling field declared before this guard, so it is
    /// always released before `MFShutdown`/`CoUninitialize` run.
    pub(super) struct MfSession {
        _com: ComGuard,
    }

    impl MfSession {
        pub(super) fn start() -> Result<Self> {
            let com = ComGuard::initialize()?;
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
                .map_err(|e| aac_unavailable(format!("Media Foundation could not start: {e}")))?;
            Ok(Self { _com: com })
        }
    }

    impl Drop for MfSession {
        fn drop(&mut self) {
            unsafe {
                let _ = MFShutdown();
            }
        }
    }

    /// RAII wrapper around `IMFMediaBuffer::Lock`/`Unlock`.
    pub(super) struct BufferLock<'a> {
        buffer: &'a IMFMediaBuffer,
        ptr: *mut u8,
        max_len: u32,
        current_len: u32,
    }

    impl<'a> BufferLock<'a> {
        pub(super) fn new(buffer: &'a IMFMediaBuffer) -> Result<Self> {
            let mut ptr = std::ptr::null_mut();
            let mut max_len = 0u32;
            let mut current_len = 0u32;
            unsafe { buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut current_len)) }
                .map_err(|e| anyhow!("could not lock a Media Foundation media buffer: {e}"))?;
            let lock = Self {
                buffer,
                ptr,
                max_len,
                current_len,
            };
            if lock.ptr.is_null() {
                bail!("Media Foundation returned a null media buffer pointer");
            }
            if lock.current_len > lock.max_len {
                bail!(
                    "Media Foundation reported a current buffer length of {} bytes, above its {} \
                     byte capacity",
                    lock.current_len,
                    lock.max_len
                );
            }
            Ok(lock)
        }

        /// The currently valid contents of the buffer.
        pub(super) fn as_slice(&self) -> &[u8] {
            // SAFETY: `Lock` returned a pointer valid for `current_len` bytes
            // and the buffer cannot be resized while locked.
            unsafe { std::slice::from_raw_parts(self.ptr, self.current_len as usize) }
        }

        /// The writable capacity of the buffer.
        pub(super) fn as_mut_slice(&mut self) -> &mut [u8] {
            // SAFETY: `Lock` returned a pointer valid for `max_len` bytes and
            // the borrow of `self` prevents aliasing.
            unsafe { std::slice::from_raw_parts_mut(self.ptr, self.max_len as usize) }
        }
    }

    impl Drop for BufferLock<'_> {
        fn drop(&mut self) {
            unsafe {
                let _ = self.buffer.Unlock();
            }
        }
    }

    /// RAII owner for `MFT_OUTPUT_DATA_BUFFER`. `pSample` and `pEvents` are
    /// `ManuallyDrop<Option<..>>`, so every path must explicitly release them;
    /// `Drop` guarantees that for success and error paths alike.
    struct OutputData {
        raw: MFT_OUTPUT_DATA_BUFFER,
    }

    impl OutputData {
        fn new(sample: Option<IMFSample>) -> Self {
            Self {
                raw: MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: OUTPUT_STREAM_ID,
                    pSample: ManuallyDrop::new(sample),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                },
            }
        }

        /// Takes ownership of the sample the MFT produced or echoed back.
        fn take_sample(&mut self) -> Option<IMFSample> {
            // SAFETY: the field is always initialized; it is re-armed with
            // `None` immediately so it can never be taken twice.
            let sample = unsafe { ManuallyDrop::take(&mut self.raw.pSample) };
            self.raw.pSample = ManuallyDrop::new(None);
            sample
        }

        /// Takes ownership of any event collection the MFT produced.
        fn take_events(&mut self) -> Option<IMFCollection> {
            // SAFETY: see `take_sample`.
            let events = unsafe { ManuallyDrop::take(&mut self.raw.pEvents) };
            self.raw.pEvents = ManuallyDrop::new(None);
            events
        }
    }

    impl Drop for OutputData {
        fn drop(&mut self) {
            let _ = self.take_sample();
            let _ = self.take_events();
        }
    }

    /// Successful output of one `ProcessOutput` call.
    pub(super) struct RawOutput {
        pub(super) data: Vec<u8>,
        pub(super) time_hns: Option<i64>,
        pub(super) duration_hns: Option<i64>,
        pub(super) status: u32,
    }

    /// One `ProcessOutput` step.
    pub(super) enum Pulled {
        Sample(RawOutput),
        NeedMoreInput,
        StreamChange,
    }

    /// The low `0x300` bits of `MFT_OUTPUT_DATA_BUFFER::dwStatus` form an
    /// enumeration (`FORMAT_CHANGE = 0x100`, `STREAM_END = 0x200`,
    /// `NO_SAMPLE = 0x300`); they are not bit flags and must never be tested
    /// with a bare `& flag != 0`. `MFT_OUTPUT_DATA_BUFFER_INCOMPLETE`
    /// (0x01000000) is the only true bit and may accompany any state.
    const MFT_OUTPUT_DATA_BUFFER_STATUS_MASK: u32 = 0x0000_0300;

    /// True when the per-stream `MFT_OUTPUT_DATA_BUFFER::dwStatus` reports an
    /// output format change. `MFT_OUTPUT_DATA_BUFFER_*` states are an
    /// enumeration, not the `ProcessOutput` out-param
    /// (`MFT_PROCESS_OUTPUT_STATUS`, whose `NEW_STREAMS` value is also 0x100).
    pub(super) fn status_signals_format_change(status: u32) -> bool {
        status & MFT_OUTPUT_DATA_BUFFER_STATUS_MASK == MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE.0 as u32
    }

    /// True when the per-stream `dwStatus` reports the end of the output
    /// stream.
    pub(super) fn status_signals_stream_end(status: u32) -> bool {
        status & MFT_OUTPUT_DATA_BUFFER_STATUS_MASK == MFT_OUTPUT_DATA_BUFFER_STREAM_END.0 as u32
    }

    /// True when the per-stream `dwStatus` reports no sample for this stream.
    pub(super) fn status_signals_no_sample(status: u32) -> bool {
        status & MFT_OUTPUT_DATA_BUFFER_STATUS_MASK == MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE.0 as u32
    }

    /// AAC output frames permitted for `submitted_pcm_frames` submitted PCM
    /// sample frames: one AAC frame per [`FRAMES_PER_BLOCK`] PCM frames. AAC
    /// frame counts and PCM sample frame counts must never be compared
    /// directly.
    pub(super) fn output_frames_allowed(submitted_pcm_frames: u64) -> u64 {
        submitted_pcm_frames / FRAMES_PER_BLOCK as u64
    }

    /// Pure timestamp policy: output timestamps must never be negative and
    /// must never regress. Extracted so the rejection paths are unit tested
    /// instead of only existing in an MF callback.
    pub(super) fn checked_output_time(previous: Option<i64>, pts_hns: i64) -> Result<()> {
        if pts_hns < 0 {
            bail!(
                "the AAC encoder returned a negative output timestamp ({pts_hns} hns); \
                 refusing to emit a flattened PTS"
            );
        }
        if let Some(previous) = previous
            && pts_hns < previous
        {
            bail!(
                "the AAC encoder output timestamp regressed from {previous} hns to {pts_hns} hns"
            );
        }
        Ok(())
    }

    /// Encodes 44.1 kHz stereo s16le PCM with the Windows AAC encoder.
    ///
    /// Not `Send`/`Sync`: the underlying Media Foundation transform and its
    /// apartment are owned by the creating thread.
    pub struct AacEncoder {
        transform: IMFTransform,
        /// True when the platform emits ADTS; otherwise this module wraps raw
        /// AAC frames in its own ADTS headers.
        adts: bool,
        started: bool,
        finished: bool,
        /// Next expected `start_frame`; blocks must be contiguous. Also equals
        /// the number of PCM frames submitted so far, which bounds the number
        /// of AAC frames the encoder may emit.
        next_input_frame: u64,
        /// Number of AAC frames emitted so far, used only to derive PTS if the
        /// platform ever returns an output sample without a timestamp.
        output_frames_seen: u64,
        /// Last (highest) output timestamp in 100-ns units; output timestamps
        /// must never regress or go negative.
        last_output_hns: Option<i64>,
        /// Current guess for the output media buffer size in bytes.
        buffer_hint: u32,
        stream_changes: usize,
        missing_timestamp_warned: bool,
        input_alignment: u32,
        _not_send_or_sync: PhantomData<Rc<()>>,
        /// Declared last so the transform above drops first.
        _session: MfSession,
    }

    impl AacEncoder {
        /// Creates and configures a Media Foundation AAC-LC encoder.
        ///
        /// COM and Media Foundation are initialized on the calling thread and
        /// torn down when the returned encoder is dropped. This function must
        /// therefore be called on the thread that will own the encoder.
        #[allow(clippy::new_without_default)]
        pub fn new() -> Result<Self> {
            let session = MfSession::start()?;
            let transform: IMFTransform = unsafe {
                CoCreateInstance(&AACMFTEncoder, None, CLSCTX_ALL)
            }
            .map_err(|e| aac_unavailable(format!("AACMFTEncoder could not be created ({e})")))?;
            let input_type = create_input_type()?;
            configure_types(&transform, &input_type)?;
            let adts = read_payload_type(&transform)?;
            let mut input_info = MFT_INPUT_STREAM_INFO::default();
            let input_alignment =
                unsafe { transform.GetInputStreamInfo(INPUT_STREAM_ID, &mut input_info) }
                    .map(|()| input_info.cbAlignment)
                    .unwrap_or(0);
            Ok(Self {
                transform,
                adts,
                started: false,
                finished: false,
                next_input_frame: 0,
                output_frames_seen: 0,
                last_output_hns: None,
                buffer_hint: MIN_OUTPUT_BUFFER,
                stream_changes: 0,
                missing_timestamp_warned: false,
                input_alignment,
                _not_send_or_sync: PhantomData,
                _session: session,
            })
        }

        /// Encodes exactly one 1024-frame PCM block.
        ///
        /// `pcm` must be exactly [`PCM_BLOCK_BYTES`] bytes. `start_frame` is
        /// the 44.1 kHz sample-frame position of the block relative to the
        /// session origin; the first call must use `0` and every following
        /// call must continue contiguously. Returns whatever AAC frames the
        /// encoder completed, which may be empty while it fills its internal
        /// lookahead.
        pub fn encode(&mut self, pcm: &[u8], start_frame: u64) -> Result<Vec<AacFrame>> {
            if self.finished {
                bail!(
                    "the AAC encoder was already finished; create a new encoder for a new session"
                );
            }
            if pcm.len() != PCM_BLOCK_BYTES {
                bail!(
                    "AAC input must be exactly {PCM_BLOCK_BYTES} bytes of 44.1 kHz stereo s16le PCM \
                     (1024 frames), got {}",
                    pcm.len()
                );
            }
            if start_frame != self.next_input_frame {
                bail!(
                    "AAC input blocks must be contiguous: expected start frame {}, got {} \
                     (time gaps and reordering are not supported)",
                    self.next_input_frame,
                    start_frame
                );
            }
            if !self.started {
                unsafe {
                    self.transform
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
                    self.transform
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
                }
                self.started = true;
            }

            // Follow the exact fractional grid instead of a fixed truncated
            // duration: some blocks last 232199 hns and some 232200 hns.
            let time_hns = frames_to_hns(start_frame)?;
            let end_hns = frames_to_hns(start_frame + FRAMES_PER_BLOCK as u64)?;
            let sample =
                create_media_sample(pcm, time_hns, end_hns - time_hns, self.input_alignment)?;

            let mut frames = Vec::new();
            let mut attempts = 0;
            loop {
                match unsafe { self.transform.ProcessInput(INPUT_STREAM_ID, &sample, 0) } {
                    Ok(()) => break,
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {
                        // The encoder is still holding output: pull everything
                        // it can produce and retry the same sample. Bounded so
                        // a misbehaving MFT cannot spin forever.
                        attempts += 1;
                        if attempts > MAX_NOTACCEPTING_RETRIES {
                            bail!(
                                "the AAC encoder kept refusing input after \
                                 {MAX_NOTACCEPTING_RETRIES} output drains"
                            );
                        }
                        self.collect_output(&mut frames)?;
                    }
                    Err(e) => {
                        return Err(anyhow!(
                            "the AAC encoder rejected input at frame {start_frame}: {e}"
                        ));
                    }
                }
            }
            self.next_input_frame = start_frame + FRAMES_PER_BLOCK as u64;
            self.collect_output(&mut frames)?;
            Ok(frames)
        }

        /// Signals end of stream, drains the encoder and returns the remaining
        /// AAC frames. Idempotent; calling it again returns no frames.
        pub fn finish(&mut self) -> Result<Vec<AacFrame>> {
            if self.finished {
                return Ok(Vec::new());
            }
            let mut frames = Vec::new();
            if self.started {
                unsafe {
                    self.transform
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
                    self.transform
                        .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
                }
                self.collect_output(&mut frames)?;
            }
            self.finished = true;
            Ok(frames)
        }

        /// Pulls output samples until the MFT needs more input, converting
        /// each into one or more ADTS frames.
        fn collect_output(&mut self, frames: &mut Vec<AacFrame>) -> Result<()> {
            let mut calls = 0usize;
            let mut bytes = 0usize;
            loop {
                if calls >= MAX_OUTPUT_CALLS_PER_DRAIN {
                    bail!(
                        "the AAC encoder produced output for {MAX_OUTPUT_CALLS_PER_DRAIN} \
                         ProcessOutput calls without needing input"
                    );
                }
                calls += 1;
                match pull_output_sample(&self.transform, &mut self.buffer_hint)? {
                    Pulled::NeedMoreInput => return Ok(()),
                    Pulled::StreamChange => {
                        self.stream_changes += 1;
                        if self.stream_changes > MAX_STREAM_CHANGES {
                            bail!("the AAC encoder requested repeated output stream changes");
                        }
                        self.renegotiate_output()?;
                    }
                    Pulled::Sample(raw) => {
                        if raw.duration_hns.is_some_and(|duration| duration <= 0) {
                            bail!(
                                "Media Foundation AAC output sample reported a non-positive \
                                 duration"
                            );
                        }
                        bytes = bytes.saturating_add(raw.data.len());
                        if bytes > MAX_OUTPUT_BYTES_PER_DRAIN {
                            bail!(
                                "the AAC encoder produced more than \
                                 {MAX_OUTPUT_BYTES_PER_DRAIN} bytes in one drain"
                            );
                        }
                        if !raw.data.is_empty() {
                            let base_hns = match raw.time_hns {
                                Some(time_hns) => time_hns,
                                None => {
                                    if !self.missing_timestamp_warned {
                                        tracing::warn!(
                                            "Media Foundation AAC output has no timestamp; \
                                             deriving PTS from the 1024-frame output grid"
                                        );
                                        self.missing_timestamp_warned = true;
                                    }
                                    derived_time_hns(self.output_frames_seen)?
                                }
                            };
                            frames.extend(self.parse_chunk(&raw.data, base_hns)?);
                        }
                        if status_signals_format_change(raw.status) {
                            self.renegotiate_output()?;
                        }
                        if status_signals_stream_end(raw.status) {
                            return Ok(());
                        }
                    }
                }
            }
        }

        /// Converts one output buffer into `AacFrame`s.
        fn parse_chunk(&mut self, data: &[u8], base_hns: i64) -> Result<Vec<AacFrame>> {
            let mut frames = Vec::new();
            if self.adts {
                let parts = split_adts_frames(data)?;
                if parts.is_empty() {
                    bail!("the AAC encoder produced output without a complete ADTS frame");
                }
                for (index, part) in parts.iter().enumerate() {
                    let pts_hns = base_hns.saturating_add(frame_offset_hns(index));
                    self.record_output_time(pts_hns)?;
                    frames.push(AacFrame {
                        data: part.to_vec(),
                        pts_us: hns_to_us(pts_hns),
                    });
                }
            } else {
                if data.len() > MAX_ADTS_PAYLOAD {
                    bail!(
                        "raw AAC frame of {} bytes is too large for an ADTS header",
                        data.len()
                    );
                }
                self.record_output_time(base_hns)?;
                let header = adts_header(data.len())?;
                let mut framed = Vec::with_capacity(header.len() + data.len());
                framed.extend_from_slice(&header);
                framed.extend_from_slice(data);
                frames.push(AacFrame {
                    data: framed,
                    pts_us: hns_to_us(base_hns),
                });
            }
            // The encoder may not invent output frames out of nowhere: never
            // emit more AAC frames than PCM frames submitted, and never
            // silently compress the output timeline. `emitted` counts AAC
            // frames while `next_input_frame` counts PCM sample frames, so
            // the submitted total must be divided by the AAC frame size.
            let emitted = self.output_frames_seen.saturating_add(frames.len() as u64);
            let allowed = output_frames_allowed(self.next_input_frame);
            if emitted > allowed {
                bail!(
                    "the AAC encoder produced {emitted} AAC frames for {} submitted PCM frames \
                     ({allowed} AAC frames); refusing a compressed output timeline",
                    self.next_input_frame
                );
            }
            self.output_frames_seen = emitted;
            Ok(frames)
        }

        /// Validates one output frame timestamp: never negative, never
        /// regressing. Missing timestamps use the exact 1024-frame grid in
        /// [`Self::collect_output`] and pass through the same checks.
        fn record_output_time(&mut self, pts_hns: i64) -> Result<()> {
            checked_output_time(self.last_output_hns, pts_hns)?;
            self.last_output_hns = Some(pts_hns);
            Ok(())
        }

        /// Re-establishes the AAC output type after a stream change.
        fn renegotiate_output(&mut self) -> Result<()> {
            let mut last_error = None;
            let mut negotiated = false;
            for adts in [true, false] {
                let output = create_output_type(adts)?;
                match unsafe { self.transform.SetOutputType(OUTPUT_STREAM_ID, &output, 0) } {
                    Ok(()) => {
                        negotiated = true;
                        break;
                    }
                    Err(e) => last_error = Some(e.to_string()),
                }
            }
            if !negotiated {
                return Err(anyhow!(
                    "could not renegotiate the AAC encoder output type{}",
                    last_error.map(|e| format!(": {e}")).unwrap_or_default()
                ));
            }
            self.adts = read_payload_type(&self.transform)?;
            Ok(())
        }
    }

    /// Performs one `ProcessOutput` call, retrying with a larger buffer when
    /// the MFT reports `MF_E_BUFFERTOOSMALL`.
    ///
    /// The samples handed back by the MFT (either the one supplied by the
    /// caller, or one the MFT allocated) are always released through
    /// [`OutputData`].
    pub(super) fn pull_output_sample(
        transform: &IMFTransform,
        capacity_hint: &mut u32,
    ) -> Result<Pulled> {
        loop {
            let info = unsafe { transform.GetOutputStreamInfo(OUTPUT_STREAM_ID) }
                .map_err(|e| anyhow!("could not query Media Foundation output stream info: {e}"))?;
            let mft_allocates = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            if info.cbSize > *capacity_hint {
                // The MFT reports a minimum buffer size. Sizes above the cap
                // are rejected outright instead of being silently clamped, so
                // no arbitrary allocation can hide a broken MFT.
                if info.cbSize > MAX_OUTPUT_BUFFER {
                    bail!(
                        "Media Foundation reports a {}-byte output buffer, over the \
                         {MAX_OUTPUT_BUFFER}-byte cap",
                        info.cbSize
                    );
                }
                *capacity_hint = info.cbSize;
            }
            let sample = if mft_allocates {
                None
            } else {
                Some(create_output_sample(*capacity_hint, info.cbAlignment)?)
            };
            let mut output = OutputData::new(sample);
            let mut process_status = 0u32;
            let result = unsafe {
                transform.ProcessOutput(
                    0,
                    std::slice::from_mut(&mut output.raw),
                    &mut process_status,
                )
            };
            match result {
                Ok(()) => {
                    if status_signals_no_sample(output.raw.dwStatus) {
                        // No output was produced for this stream; the caller
                        // should provide more input.
                        return Ok(Pulled::NeedMoreInput);
                    }
                    let sample = output.take_sample().ok_or_else(|| {
                        anyhow!("Media Foundation reported output without a sample")
                    })?;
                    let time_hns = unsafe { sample.GetSampleTime() }.ok();
                    let duration_hns = unsafe { sample.GetSampleDuration() }.ok();
                    let data = copy_sample_bytes(&sample)?;
                    // `process_status` is only the global
                    // `MFT_PROCESS_OUTPUT_STATUS`; the per-stream
                    // `MFT_OUTPUT_DATA_BUFFER_*` flags live in `dwStatus`.
                    let stream_status = output.raw.dwStatus;
                    let _ = output.take_events();
                    return Ok(Pulled::Sample(RawOutput {
                        data,
                        time_hns,
                        duration_hns,
                        status: stream_status,
                    }));
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    return Ok(Pulled::NeedMoreInput);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    return Ok(Pulled::StreamChange);
                }
                Err(e) if e.code() == MF_E_BUFFERTOOSMALL && !mft_allocates => {
                    let grown = capacity_hint.saturating_mul(2).max(MIN_OUTPUT_BUFFER);
                    if grown <= *capacity_hint || grown > MAX_OUTPUT_BUFFER {
                        return Err(anyhow!(
                            "Media Foundation output still did not fit {MAX_OUTPUT_BUFFER} bytes"
                        ));
                    }
                    *capacity_hint = grown;
                }
                Err(e) => {
                    return Err(anyhow!("Media Foundation ProcessOutput failed: {e}"));
                }
            }
        }
    }

    /// Configures both media types. The AAC encoder accepts either order; the
    /// ADTS output type (payload type 1) is preferred, then raw AAC (payload
    /// type 0), then input-first for encoder builds that require it.
    fn configure_types(transform: &IMFTransform, input: &IMFMediaType) -> Result<()> {
        let mut output_accepted = false;
        let mut last_error = None;
        for adts in [true, false] {
            let output = create_output_type(adts)?;
            match unsafe { transform.SetOutputType(OUTPUT_STREAM_ID, &output, 0) } {
                Ok(()) => {
                    output_accepted = true;
                    if unsafe { transform.SetInputType(INPUT_STREAM_ID, input, 0) }.is_ok() {
                        return Ok(());
                    }
                }
                Err(e) => last_error = Some(e.to_string()),
            }
        }
        if output_accepted {
            bail!(
                "the AAC encoder rejected the 44.1 kHz stereo s16le input type after accepting \
                 an output type"
            );
        }
        // Output-first was rejected outright, so this build wants input first.
        unsafe { transform.SetInputType(INPUT_STREAM_ID, input, 0) }
            .map_err(|e| anyhow!("could not set the AAC encoder input media type: {e}"))?;
        for adts in [true, false] {
            let output = create_output_type(adts)?;
            if unsafe { transform.SetOutputType(OUTPUT_STREAM_ID, &output, 0) }.is_ok() {
                return Ok(());
            }
        }
        Err(anyhow!(
            "the AAC encoder rejected the 44.1 kHz stereo AAC-LC output type{}",
            last_error.map(|e| format!(": {e}")).unwrap_or_default()
        ))
    }

    /// Returns `true` when the negotiated output type uses ADTS framing.
    fn read_payload_type(transform: &IMFTransform) -> Result<bool> {
        let current = unsafe { transform.GetOutputCurrentType(OUTPUT_STREAM_ID) }
            .map_err(|e| anyhow!("could not read the negotiated AAC encoder output type: {e}"))?;
        Ok(unsafe { current.GetUINT32(&MF_MT_AAC_PAYLOAD_TYPE) }
            .map(|payload| payload == 1)
            .unwrap_or(false))
    }

    fn create_input_type() -> Result<IMFMediaType> {
        unsafe {
            let media_type = MFCreateMediaType()?;
            media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            media_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
            media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
            Ok(media_type)
        }
    }

    fn create_output_type(adts: bool) -> Result<IMFMediaType> {
        unsafe {
            let media_type = MFCreateMediaType()?;
            media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            media_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
            media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
            media_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SECOND)?;
            media_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 1)?;
            media_type.SetUINT32(
                &MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION,
                AAC_PROFILE_LEVEL_LC,
            )?;
            if adts {
                media_type.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 1)?;
            }
            Ok(media_type)
        }
    }

    /// Creates an input sample holding `data`; used for PCM blocks and, in
    /// tests, for compressed AAC frames fed to the decoder.
    pub(super) fn create_media_sample(
        data: &[u8],
        time_hns: i64,
        duration_hns: i64,
        alignment: u32,
    ) -> Result<IMFSample> {
        let len = u32::try_from(data.len())
            .map_err(|_| anyhow!("media sample of {} bytes is too large", data.len()))?;
        unsafe {
            let buffer = if alignment > 1 {
                MFCreateAlignedMemoryBuffer(len, alignment)?
            } else {
                MFCreateMemoryBuffer(len)?
            };
            {
                let mut lock = BufferLock::new(&buffer)?;
                let dest = lock.as_mut_slice();
                if dest.len() < data.len() {
                    bail!(
                        "Media Foundation media buffer of {} bytes is smaller than the {} bytes \
                         that must be written",
                        dest.len(),
                        data.len()
                    );
                }
                dest[..data.len()].copy_from_slice(data);
            }
            buffer.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time_hns)?;
            sample.SetSampleDuration(duration_hns)?;
            Ok(sample)
        }
    }

    fn create_output_sample(capacity: u32, alignment: u32) -> Result<IMFSample> {
        unsafe {
            let buffer = if alignment > 1 {
                MFCreateAlignedMemoryBuffer(capacity, alignment)?
            } else {
                MFCreateMemoryBuffer(capacity)?
            };
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            Ok(sample)
        }
    }

    fn copy_sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
        unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| anyhow!("could not access a Media Foundation output buffer: {e}"))?;
            let lock = BufferLock::new(&buffer)?;
            let data = lock.as_slice();
            if data.len() > MAX_OUTPUT_SAMPLE_BYTES {
                bail!(
                    "Media Foundation output sample of {} bytes is over the {} byte cap",
                    data.len(),
                    MAX_OUTPUT_SAMPLE_BYTES
                );
            }
            Ok(data.to_vec())
        }
    }

    /// Converts an absolute 44.1 kHz frame position to 100-ns units exactly.
    fn frames_to_hns(frames: u64) -> Result<i64> {
        let hns = (frames as u128) * HNS_PER_SECOND / SAMPLE_RATE as u128;
        i64::try_from(hns)
            .map_err(|_| anyhow!("AAC start frame {frames} is outside the supported session range"))
    }

    /// Exact 100-ns offset of the `index`-th AAC frame inside one output
    /// sample. Uses 128-bit arithmetic so repeated frames never accumulate a
    /// rounding error.
    fn frame_offset_hns(index: usize) -> i64 {
        let hns = index as u128 * HNS_PER_FRAME;
        i64::try_from(hns).unwrap_or(i64::MAX)
    }

    fn derived_time_hns(frames_seen: u64) -> Result<i64> {
        frames_to_hns(frames_seen.saturating_mul(FRAMES_PER_BLOCK as u64))
    }

    fn hns_to_us(hns: i64) -> u64 {
        debug_assert!(hns >= 0, "negative output timestamps are rejected earlier");
        (hns / 10) as u64
    }

    /// Splits a buffer of concatenated ADTS frames into individual frames
    /// (including their headers). Validates the syncword, the header length
    /// implied by `protection_absent`, and every `frame_length` field.
    pub(super) fn split_adts_frames(data: &[u8]) -> Result<Vec<&[u8]>> {
        let mut frames = Vec::new();
        let mut pos = 0usize;
        while pos < data.len() {
            if data.len() - pos < ADTS_HEADER_LEN {
                bail!("truncated ADTS header at byte {pos}");
            }
            let header = &data[pos..pos + ADTS_HEADER_LEN];
            if header[0] != 0xFF || header[1] & 0xF0 != 0xF0 {
                bail!("bad ADTS syncword at byte {pos}");
            }
            let header_len = if header[1] & 0x01 != 0 {
                ADTS_HEADER_LEN
            } else {
                ADTS_HEADER_LEN + 2
            };
            if data.len() - pos < header_len {
                bail!("truncated ADTS CRC header at byte {pos}");
            }
            let frame_len = ((header[3] as usize & 0x03) << 11)
                | ((header[4] as usize) << 3)
                | (header[5] as usize >> 5);
            if frame_len < header_len {
                bail!("ADTS frame at byte {pos} claims a length shorter than its header");
            }
            let end = pos
                .checked_add(frame_len)
                .ok_or_else(|| anyhow!("ADTS frame length overflow at byte {pos}"))?;
            if end > data.len() {
                bail!("ADTS frame at byte {pos} runs past the end of the output buffer");
            }
            frames.push(&data[pos..end]);
            pos = end;
        }
        Ok(frames)
    }

    /// Builds a 7-byte ADTS header for a raw AAC-LC payload at 44.1 kHz
    /// stereo (used when the platform only emits raw AAC frames).
    fn adts_header(payload_len: usize) -> Result<[u8; ADTS_HEADER_LEN]> {
        let frame_len = payload_len + ADTS_HEADER_LEN;
        if frame_len > 0x1FFF {
            bail!("AAC payload of {payload_len} bytes does not fit an ADTS frame");
        }
        let channel_config = CHANNELS as u8;
        let mut header = [0u8; ADTS_HEADER_LEN];
        header[0] = 0xFF;
        // MPEG-4, layer 0, no CRC.
        header[1] = 0xF1;
        // AAC-LC profile (1), 44.1 kHz index (4), private bit 0, channel
        // configuration high bit.
        header[2] =
            (1 << 6) | (ADTS_SAMPLING_FREQUENCY_INDEX << 2) | ((channel_config >> 2) & 0x01);
        // Channel configuration low bits and frame length bits 12..11.
        header[3] = ((channel_config & 0x03) << 6) | ((frame_len >> 11) as u8 & 0x03);
        // Frame length bits 10..3.
        header[4] = ((frame_len >> 3) & 0xFF) as u8;
        // Frame length bits 2..0 followed by the top 5 bits of the VBR buffer
        // fullness (0x7FF).
        header[5] = (((frame_len & 0x07) << 5) as u8) | 0x1F;
        // Remaining buffer fullness bits and 0 raw data blocks.
        header[6] = 0xFC;
        Ok(header)
    }
}

/// Stub encoder for non-Windows targets so the module still compiles and the
/// API surface is identical everywhere.
#[cfg(not(target_os = "windows"))]
pub struct AacEncoder {
    _unavailable: (),
}

#[cfg(not(target_os = "windows"))]
impl AacEncoder {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Result<Self> {
        Err(unsupported_platform())
    }

    pub fn encode(&mut self, _pcm: &[u8], _start_frame: u64) -> Result<Vec<AacFrame>> {
        Err(unsupported_platform())
    }

    pub fn finish(&mut self) -> Result<Vec<AacFrame>> {
        Err(unsupported_platform())
    }
}

#[cfg(not(target_os = "windows"))]
fn unsupported_platform() -> anyhow::Error {
    anyhow::anyhow!(
        "AAC encoding is only implemented on Windows (Media Foundation); this build has no AAC \
         encoder"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_constants_match_the_airplay_audio_format() {
        assert_eq!(SAMPLE_RATE, 44_100);
        assert_eq!(CHANNELS, 2);
        assert_eq!(FRAMES_PER_BLOCK, 1024);
        assert_eq!(PCM_BLOCK_BYTES, 4096);
        assert_eq!(AAC_BITRATE_BPS, 128_000);
        assert_eq!(AAC_BYTES_PER_SECOND, 16_000);
        assert_eq!(AAC_FRAME_DURATION_HNS, 232_199);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn stub_encoder_reports_the_unsupported_platform() {
        let error = match AacEncoder::new() {
            Ok(_) => panic!("AAC encoding must be unsupported on non-Windows targets"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("Windows"), "{error}");
        assert!(error.contains("Media Foundation"), "{error}");
    }

    #[cfg(target_os = "windows")]
    mod platform {
        use super::*;
        use crate::cast_audio::win;
        use windows::Win32::Media::MediaFoundation::{
            CLSID_MSAACDecMFT, IMFMediaType, IMFTransform, MF_E_NOTACCEPTING,
            MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE,
            MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
            MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_USER_DATA,
            MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateMediaType, MFMediaType_Audio,
            MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
            MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
            MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE, MFT_OUTPUT_DATA_BUFFER_INCOMPLETE,
            MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE, MFT_OUTPUT_DATA_BUFFER_STREAM_END,
            MFT_PROCESS_OUTPUT_STATUS_NEW_STREAMS,
        };
        use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
        use windows::core::GUID;

        /// 44 blocks of 1024 frames: slightly more than one second, enough to
        /// observe the encoder's lookahead and drain behaviour.
        const TEST_BLOCKS: u64 = 44;
        /// The decoder may consume a frame of priming but must return almost
        /// the whole input as PCM.
        const MIN_DECODED_FRAMES: usize = 40_000;
        /// 440 Hz at a low but clearly audible level (-20 dBFS).
        const TONE_AMPLITUDE: f64 = 3_000.0;

        fn silence_block(_start_frame: u64) -> Vec<u8> {
            vec![0u8; PCM_BLOCK_BYTES]
        }

        fn tone_block(start_frame: u64) -> Vec<u8> {
            let mut pcm = Vec::with_capacity(PCM_BLOCK_BYTES);
            for index in 0..FRAMES_PER_BLOCK {
                let frame = start_frame + index as u64;
                let phase = std::f64::consts::TAU * 440.0 * frame as f64 / SAMPLE_RATE as f64;
                let sample = (phase.sin() * TONE_AMPLITUDE) as i16;
                pcm.extend_from_slice(&sample.to_le_bytes());
                pcm.extend_from_slice(&sample.to_le_bytes());
            }
            pcm
        }

        /// Encodes `blocks` PCM blocks and drains, returning `(frames emitted
        /// by encode, frames emitted by finish)`.
        fn encode_session(
            blocks: u64,
            block: fn(u64) -> Vec<u8>,
        ) -> (Vec<AacFrame>, Vec<AacFrame>) {
            let mut encoder = AacEncoder::new().expect("Media Foundation AAC encoder");
            let mut frames = Vec::new();
            for index in 0..blocks {
                let start = index * FRAMES_PER_BLOCK as u64;
                frames.extend(
                    encoder
                        .encode(&block(start), start)
                        .expect("encode a 1024-frame PCM block"),
                );
            }
            let drained = encoder.finish().expect("drain the AAC encoder");
            (frames, drained)
        }

        /// Every `AacFrame` must be exactly one well-formed ADTS AAC-LC frame
        /// for 44.1 kHz stereo.
        fn assert_adts_stream(label: &str, frames: &[AacFrame]) {
            assert!(
                !frames.is_empty(),
                "{label}: encoder produced no AAC frames"
            );
            for (index, frame) in frames.iter().enumerate() {
                let parts = win::split_adts_frames(&frame.data)
                    .unwrap_or_else(|e| panic!("{label} frame {index} is not valid ADTS: {e}"));
                assert_eq!(
                    parts.len(),
                    1,
                    "{label} frame {index} must hold exactly one ADTS frame"
                );
                let part = parts[0];
                assert_eq!(part.len(), frame.data.len());
                assert_eq!(
                    part[1] & 0xF6,
                    0xF0,
                    "{label} frame {index}: not MPEG-4 layer 0"
                );
                let profile = part[2] >> 6;
                let sample_rate_index = (part[2] >> 2) & 0x0F;
                let channel_config = ((part[2] & 0x01) << 2) | (part[3] >> 6);
                assert_eq!(profile, 1, "{label} frame {index}: expected AAC-LC profile");
                assert_eq!(
                    sample_rate_index, 4,
                    "{label} frame {index}: expected 44.1 kHz"
                );
                assert_eq!(channel_config, 2, "{label} frame {index}: expected stereo");
            }
        }

        /// PTS must follow the 1024-frame output grid monotonically. A drift
        /// or priming offset larger than 100 us fails.
        fn assert_pts_grid(label: &str, frames: &[AacFrame]) {
            for (index, frame) in frames.iter().enumerate() {
                let expected_us =
                    index as u64 * FRAMES_PER_BLOCK as u64 * 1_000_000 / SAMPLE_RATE as u64;
                assert!(
                    frame.pts_us.abs_diff(expected_us) <= 100,
                    "{label} frame {index}: PTS {} us is not on the 1024-frame grid (~{expected_us} us)",
                    frame.pts_us
                );
                if index > 0 {
                    let step = frame.pts_us - frames[index - 1].pts_us;
                    assert!(
                        (23_000..=23_400).contains(&step),
                        "{label} frame {index}: step of {step} us is not one AAC frame"
                    );
                }
            }
        }

        struct DecodedPcm {
            samples: Vec<i16>,
            sample_rate: u32,
            channels: u32,
        }

        fn set_type_attributes(media_type: &IMFMediaType, subtype: &GUID) {
            unsafe {
                media_type
                    .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                    .unwrap();
                media_type.SetGUID(&MF_MT_SUBTYPE, subtype).unwrap();
                media_type
                    .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                    .unwrap();
                media_type
                    .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)
                    .unwrap();
                media_type
                    .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)
                    .unwrap();
            }
        }

        fn adts_payload(frame: &AacFrame) -> Vec<u8> {
            let parts = win::split_adts_frames(&frame.data).expect("ADTS frame");
            assert_eq!(parts.len(), 1);
            let header_len = if parts[0][1] & 0x01 != 0 { 7 } else { 9 };
            parts[0][header_len..].to_vec()
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        struct DecoderFormat {
            subtype: GUID,
            bits: u32,
            rate: u32,
            channels: u32,
            block_align: u32,
        }

        fn decoder_output_format(decoder: &IMFTransform) -> DecoderFormat {
            let media_type =
                unsafe { decoder.GetOutputCurrentType(0) }.expect("decoder output type");
            DecoderFormat {
                subtype: unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.expect("output subtype"),
                bits: unsafe { media_type.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE) }
                    .expect("output bits"),
                rate: unsafe { media_type.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND) }
                    .expect("output rate"),
                channels: unsafe { media_type.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS) }
                    .expect("output channels"),
                block_align: unsafe { media_type.GetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT) }
                    .expect("output block alignment"),
            }
        }

        /// Checked before every decoded output sample: a format change or a
        /// non-16-bit PCM rate must never be silently reinterpreted as s16le.
        fn assert_decoder_format(format: DecoderFormat) {
            assert_eq!(
                format,
                DecoderFormat {
                    subtype: MFAudioFormat_PCM,
                    bits: 16,
                    rate: SAMPLE_RATE,
                    channels: CHANNELS as u32,
                    block_align: CHANNELS as u32 * 2,
                },
                "AAC decoder output format changed"
            );
        }

        fn drain_decoder(decoder: &IMFTransform, samples: &mut Vec<i16>, capacity: &mut u32) {
            let mut calls = 0usize;
            loop {
                calls += 1;
                assert!(calls < 64, "AAC decoder output drain did not finish");
                match win::pull_output_sample(decoder, capacity).expect("decoder ProcessOutput") {
                    win::Pulled::NeedMoreInput => return,
                    win::Pulled::StreamChange => {
                        panic!("AAC decoder unexpectedly announced a stream change")
                    }
                    win::Pulled::Sample(raw) => {
                        let format = decoder_output_format(decoder);
                        assert_decoder_format(format);
                        let frame_bytes = format.block_align as usize;
                        assert!(
                            raw.data.len().is_multiple_of(frame_bytes),
                            "decoder output of {} bytes is not whole {frame_bytes}-byte frames",
                            raw.data.len()
                        );
                        if let Some(duration_hns) = raw.duration_hns {
                            let frames =
                                (duration_hns as u128 * SAMPLE_RATE as u128 / 10_000_000) as usize;
                            let expected_bytes = frames * frame_bytes;
                            assert!(
                                raw.data.len().abs_diff(expected_bytes) <= frame_bytes,
                                "decoder output of {} bytes does not match its {duration_hns} hns \
                                 duration (~{expected_bytes} bytes)",
                                raw.data.len()
                            );
                        }
                        for bytes in raw.data.as_chunks::<2>().0 {
                            samples.push(i16::from_le_bytes(*bytes));
                        }
                    }
                }
            }
        }

        /// Independent regression: decodes the produced ADTS frames with the
        /// Windows Media Foundation AAC decoder and returns PCM.
        fn decode_frames(frames: &[AacFrame]) -> DecodedPcm {
            let _session = win::MfSession::start().expect("Media Foundation session");
            let decoder: IMFTransform =
                unsafe { CoCreateInstance(&CLSID_MSAACDecMFT, None, CLSCTX_ALL) }
                    .expect("Media Foundation AAC decoder");
            unsafe {
                let _ = decoder.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
                let _ = decoder.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            }

            let input = unsafe { MFCreateMediaType() }.expect("decoder input media type");
            set_type_attributes(&input, &MFAudioFormat_AAC);
            unsafe {
                input.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 1).unwrap();
                input
                    .SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)
                    .unwrap();
            }
            // Some decoder builds only accept raw AAC plus an
            // AudioSpecificConfig in MF_MT_USER_DATA.
            let adts_input = if unsafe { decoder.SetInputType(0, &input, 0) }.is_ok() {
                true
            } else {
                let mut user_data = vec![0u8; 12];
                user_data[2..4].copy_from_slice(&0x29u16.to_le_bytes());
                user_data.extend_from_slice(&[0x12, 0x10]); // AAC-LC, 44.1 kHz, stereo
                unsafe {
                    input.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).unwrap();
                    input.SetBlob(&MF_MT_USER_DATA, &user_data).unwrap();
                }
                unsafe { decoder.SetInputType(0, &input, 0) }.expect("decoder raw AAC input type");
                false
            };

            let output = unsafe { MFCreateMediaType() }.expect("decoder output media type");
            set_type_attributes(&output, &MFAudioFormat_PCM);
            unsafe {
                output.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4).unwrap();
            }
            unsafe { decoder.SetOutputType(0, &output, 0) }.expect("decoder PCM output type");

            let mut samples = Vec::new();
            let mut capacity = 8 * 1024u32;
            for frame in frames {
                let payload = if adts_input {
                    frame.data.clone()
                } else {
                    adts_payload(frame)
                };
                let sample = win::create_media_sample(
                    &payload,
                    i64::try_from(frame.pts_us).unwrap_or(i64::MAX) * 10,
                    AAC_FRAME_DURATION_HNS,
                    0,
                )
                .expect("decoder input sample");
                let mut attempts = 0;
                loop {
                    match unsafe { decoder.ProcessInput(0, &sample, 0) } {
                        Ok(()) => break,
                        Err(e) if e.code() == MF_E_NOTACCEPTING => {
                            attempts += 1;
                            assert!(attempts < 8, "AAC decoder kept refusing input");
                            drain_decoder(&decoder, &mut samples, &mut capacity);
                        }
                        Err(e) => panic!("AAC decoder rejected input: {e}"),
                    }
                }
                drain_decoder(&decoder, &mut samples, &mut capacity);
            }
            unsafe {
                decoder
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                    .expect("decoder end of stream");
                decoder
                    .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                    .expect("decoder drain");
            }
            drain_decoder(&decoder, &mut samples, &mut capacity);

            let current = unsafe { decoder.GetOutputCurrentType(0) }.expect("decoder output type");
            let sample_rate =
                unsafe { current.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND) }.expect("rate");
            let channels =
                unsafe { current.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS) }.expect("channels");
            DecodedPcm {
                samples,
                sample_rate,
                channels,
            }
        }

        #[test]
        fn encodes_silence_and_tone_with_expected_frames_and_timestamps() {
            for (label, block) in [
                ("silence", silence_block as fn(u64) -> Vec<u8>),
                ("tone", tone_block),
            ] {
                let (frames, drained) = encode_session(TEST_BLOCKS, block);
                assert!(
                    !drained.is_empty(),
                    "{label}: finish() must drain the encoder's remaining lookahead"
                );
                let mut all = frames;
                all.extend(drained);
                assert_eq!(all.len(), TEST_BLOCKS as usize, "{label}: total AAC frames");
                assert_adts_stream(label, &all);
                assert_pts_grid(label, &all);
            }
        }

        #[test]
        fn mf_aac_decoder_recovers_silence_and_audible_tone() {
            let (mut frames, drained) = encode_session(TEST_BLOCKS, silence_block);
            frames.extend(drained);
            let decoded_silence = decode_frames(&frames);
            assert_eq!(decoded_silence.sample_rate, SAMPLE_RATE);
            assert_eq!(decoded_silence.channels, CHANNELS as u32);
            let silence_frames = decoded_silence.samples.len() / CHANNELS as usize;
            assert!(
                silence_frames >= MIN_DECODED_FRAMES,
                "silence decoded to only {silence_frames} stereo frames"
            );
            let silence_peak = decoded_silence
                .samples
                .iter()
                .map(|sample| sample.unsigned_abs())
                .max()
                .unwrap_or(0);
            assert!(
                silence_peak < 100,
                "silence decoded with peak {silence_peak}"
            );

            let (mut frames, drained) = encode_session(TEST_BLOCKS, tone_block);
            frames.extend(drained);
            let decoded_tone = decode_frames(&frames);
            assert_eq!(decoded_tone.sample_rate, SAMPLE_RATE);
            assert_eq!(decoded_tone.channels, CHANNELS as u32);
            let tone_frames = decoded_tone.samples.len() / CHANNELS as usize;
            assert!(
                tone_frames >= MIN_DECODED_FRAMES,
                "tone decoded to only {tone_frames} stereo frames"
            );
            let audible = decoded_tone
                .samples
                .iter()
                .filter(|sample| sample.unsigned_abs() > 500)
                .count();
            assert!(
                audible > 2_000,
                "only {audible} audible samples decoded from the 440 Hz tone"
            );
            let mean_square = decoded_tone
                .samples
                .iter()
                .map(|sample| f64::from(*sample) * f64::from(*sample))
                .sum::<f64>()
                / decoded_tone.samples.len() as f64;
            assert!(
                mean_square > 100_000.0,
                "decoded tone is too quiet (mean square {mean_square})"
            );
        }

        #[test]
        fn com_and_mf_teardown_allow_new_encoders() {
            let first = AacEncoder::new().expect("first AAC encoder");
            let second = AacEncoder::new().expect("second concurrent AAC encoder");
            drop(first);
            let third = AacEncoder::new().expect("AAC encoder after a drop");
            drop(second);
            drop(third);
        }

        #[test]
        fn rejects_short_oversized_and_non_contiguous_blocks() {
            let mut encoder = AacEncoder::new().expect("AAC encoder");
            let valid = vec![0u8; PCM_BLOCK_BYTES];
            assert!(encoder.encode(&valid[..PCM_BLOCK_BYTES - 1], 0).is_err());
            assert!(encoder.encode(&vec![0u8; PCM_BLOCK_BYTES + 1], 0).is_err());
            // The first block must start the session at frame 0.
            assert!(encoder.encode(&valid, 1).is_err());
            encoder.encode(&valid, 0).expect("first block at frame 0");
            // Gaps, reordering and overlap are rejected.
            assert!(encoder.encode(&valid, FRAMES_PER_BLOCK as u64 * 3).is_err());
            encoder
                .encode(&valid, FRAMES_PER_BLOCK as u64)
                .expect("contiguous second block");
            // Repeat/overlap and forward gaps are both rejected.
            assert!(encoder.encode(&valid, FRAMES_PER_BLOCK as u64).is_err());
            assert!(encoder.encode(&valid, FRAMES_PER_BLOCK as u64 * 4).is_err());
        }

        #[test]
        fn per_stream_status_flags_are_read_from_dw_status() {
            // `MFT_OUTPUT_DATA_BUFFER_*` values are an enumeration, not bit
            // flags: compare the status field, never a bare bitwise AND.
            let incomplete = MFT_OUTPUT_DATA_BUFFER_INCOMPLETE.0 as u32;
            let format_change = MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE.0 as u32;
            let stream_end = MFT_OUTPUT_DATA_BUFFER_STREAM_END.0 as u32;
            let no_sample = MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE.0 as u32;
            assert!(!win::status_signals_format_change(0));
            assert!(!win::status_signals_format_change(incomplete));
            assert!(win::status_signals_format_change(format_change));
            assert!(win::status_signals_format_change(
                format_change | incomplete
            ));
            assert!(!win::status_signals_format_change(stream_end));
            assert!(!win::status_signals_format_change(no_sample));
            assert!(!win::status_signals_stream_end(format_change));
            assert!(win::status_signals_stream_end(stream_end));
            assert!(win::status_signals_stream_end(stream_end | incomplete));
            assert!(!win::status_signals_stream_end(incomplete));
            assert!(!win::status_signals_stream_end(no_sample));
            assert!(win::status_signals_no_sample(no_sample));
            assert!(!win::status_signals_no_sample(stream_end));
            // The global `MFT_PROCESS_OUTPUT_STATUS` out-param shares values
            // with this enumeration (`NEW_STREAMS == FORMAT_CHANGE == 0x100`),
            // which is why only `MFT_OUTPUT_DATA_BUFFER::dwStatus` may be fed
            // to these helpers.
            let new_streams = MFT_PROCESS_OUTPUT_STATUS_NEW_STREAMS.0 as u32;
            assert_eq!(new_streams, format_change);
            assert!(win::status_signals_format_change(new_streams));
        }

        #[test]
        fn output_frame_bound_counts_aac_frames_not_pcm_frames() {
            // One 1024-frame PCM block permits exactly one AAC frame; the old
            // direct comparison against `next_input_frame` wrongly allowed 1024.
            assert_eq!(win::output_frames_allowed(0), 0);
            assert_eq!(win::output_frames_allowed(1023), 0);
            assert_eq!(win::output_frames_allowed(FRAMES_PER_BLOCK as u64), 1);
            assert_eq!(win::output_frames_allowed(FRAMES_PER_BLOCK as u64 + 1), 1);
            assert_eq!(win::output_frames_allowed(2 * FRAMES_PER_BLOCK as u64), 2);
            assert_eq!(win::output_frames_allowed(4095), 3);
            assert_eq!(win::output_frames_allowed(4096), 4);
            assert!(2 > win::output_frames_allowed(FRAMES_PER_BLOCK as u64));
        }

        #[test]
        fn output_timestamp_policy_rejects_negative_and_regressing_times() {
            assert!(win::checked_output_time(None, 0).is_ok());
            assert!(win::checked_output_time(Some(100), 100).is_ok());
            assert!(win::checked_output_time(Some(100), 101).is_ok());
            let negative = win::checked_output_time(None, -1).unwrap_err().to_string();
            assert!(negative.contains("negative"), "{negative}");
            let regression = win::checked_output_time(Some(100), 99)
                .unwrap_err()
                .to_string();
            assert!(regression.contains("regressed"), "{regression}");
        }

        #[test]
        fn finish_without_input_is_empty_and_idempotent() {
            let mut encoder = AacEncoder::new().expect("AAC encoder");
            assert!(encoder.finish().expect("finish").is_empty());
            assert!(encoder.finish().expect("second finish").is_empty());
            assert!(encoder.encode(&vec![0u8; PCM_BLOCK_BYTES], 0).is_err());
        }

        /// Multiple native codec instances in one process must keep exact
        /// decoded sample counts; a doubled or truncated count must fail.
        #[test]
        fn parallel_mf_instances_keep_exact_decoded_counts() {
            use std::sync::{Arc, Barrier};
            const THREADS: usize = 4;
            const ROUNDS: usize = 3;
            let barrier = Arc::new(Barrier::new(THREADS));
            let mut handles = Vec::new();
            for _ in 0..THREADS {
                let barrier = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    for round in 0..ROUNDS {
                        let (mut frames, drained) = encode_session(TEST_BLOCKS, tone_block);
                        frames.extend(drained);
                        assert_eq!(
                            frames.len(),
                            TEST_BLOCKS as usize,
                            "round {round} frame count"
                        );
                        let decoded = decode_frames(&frames);
                        assert_eq!(
                            decoded.samples.len(),
                            frames.len() * FRAMES_PER_BLOCK * CHANNELS as usize,
                            "round {round} decoded sample count"
                        );
                    }
                }));
            }
            for handle in handles {
                handle.join().expect("parallel MF test thread");
            }
        }

        // ------------------------------------------------------------------
        // Waveform fidelity regression
        //
        // The tests above only prove that something audible was decoded. This
        // section verifies the decoded waveform itself: one global decoder
        // priming offset is found once and applied to the whole stream, then
        // per-channel correlation/SNR, every 1024-frame window, gain,
        // clipping and channel independence must all hold. Negative controls
        // below prove the same metric rejects swapped/duplicated channels,
        // zeroed/dropped/repeated blocks and static noise.
        // ------------------------------------------------------------------

        /// Two seconds of stereo probe signal (88 * 1024 frames = 2.04 s).
        const PROBE_BLOCKS: u64 = 88;
        /// Frames in the whole probe; the chirp sweep spans exactly this long.
        const PROBE_FRAMES: u64 = PROBE_BLOCKS * FRAMES_PER_BLOCK as u64;
        /// Probe amplitude (peak 2500, RMS 1768 = -25.4 dBFS). Moderate level
        /// with plenty of headroom, and no clipping risk after AAC overshoot.
        const PROBE_AMPLITUDE: f64 = 2_500.0;
        /// Left sweeps up, right sweeps down: a linear chirp has a single
        /// sharp autocorrelation peak, so the global lag is unambiguous
        /// (a sum of tones has near-period lags that alias the search), and
        /// opposite sweeps keep the two channels independent.
        const LEFT_CHIRP_HZ: [f64; 2] = [200.0, 3_400.0];
        const RIGHT_CHIRP_HZ: [f64; 2] = [3_400.0, 200.0];
        /// Largest single global decoder priming/lookahead offset accepted.
        const MAX_DECODER_LAG_FRAMES: usize = 4_096;
        /// Encoder windowing and decoder priming edge effects are excluded.
        const EDGE_SKIP_FRAMES: usize = 2 * FRAMES_PER_BLOCK;
        /// Floors for "reasonable AAC-LC at 128 kbit/s" quality. The current
        /// round trip measures correlation >= 0.99995, SNR ~41 dB and minimum
        /// 1024-frame-block SNR ~31 dB, so these floors leave roughly a 20 dB
        /// regression margin while still rejecting the corrupted streams the
        /// negative controls inject. `mf_aac_decoder_preserves_stereo_
        /// waveform_fidelity` prints the measured values.
        const MIN_CHANNEL_CORRELATION: f64 = 0.98;
        const MIN_CHANNEL_SNR_DB: f64 = 20.0;
        const MIN_BLOCK_SNR_DB: f64 = 15.0;
        const MIN_CHANNEL_GAIN: f64 = 0.9;
        const MAX_CHANNEL_GAIN: f64 = 1.1;
        const MAX_CHANNEL_GAIN_MISMATCH: f64 = 0.1;
        const MAX_CROSS_CHANNEL_CORRELATION: f64 = 0.5;
        /// Uniform deterministic static for the negative control.
        const STATIC_NOISE_AMPLITUDE: i32 = 6_000;

        /// Instantaneous phase of a linear chirp over the whole probe; derived
        /// from the absolute frame so block boundaries stay seamless.
        fn chirp_sample(frame: u64, range: [f64; 2]) -> f64 {
            let seconds = frame as f64 / SAMPLE_RATE as f64;
            let duration = PROBE_FRAMES as f64 / SAMPLE_RATE as f64;
            let sweep = (range[1] - range[0]) / duration;
            let phase =
                std::f64::consts::TAU * (range[0] * seconds + 0.5 * sweep * seconds * seconds);
            phase.sin() * PROBE_AMPLITUDE
        }

        /// Deterministic stereo probe: a rising 200-3400 Hz chirp on the left
        /// and a falling 3400-200 Hz chirp on the right.
        fn probe_sample(frame: u64) -> (i16, i16) {
            (
                chirp_sample(frame, LEFT_CHIRP_HZ) as i16,
                chirp_sample(frame, RIGHT_CHIRP_HZ) as i16,
            )
        }

        fn stereo_probe_block(start_frame: u64) -> Vec<u8> {
            let mut pcm = Vec::with_capacity(PCM_BLOCK_BYTES);
            for index in 0..FRAMES_PER_BLOCK {
                let (left, right) = probe_sample(start_frame + index as u64);
                pcm.extend_from_slice(&left.to_le_bytes());
                pcm.extend_from_slice(&right.to_le_bytes());
            }
            pcm
        }

        fn stereo_probe_reference(blocks: u64) -> Vec<i16> {
            let mut reference =
                Vec::with_capacity(blocks as usize * FRAMES_PER_BLOCK * CHANNELS as usize);
            for frame in 0..blocks * FRAMES_PER_BLOCK as u64 {
                let (left, right) = probe_sample(frame);
                reference.push(left);
                reference.push(right);
            }
            reference
        }

        fn deinterleave(samples: &[i16]) -> (Vec<f64>, Vec<f64>) {
            let frames = samples.len() / CHANNELS as usize;
            let mut left = Vec::with_capacity(frames);
            let mut right = Vec::with_capacity(frames);
            for pair in samples.as_chunks::<2>().0 {
                left.push(f64::from(pair[0]));
                right.push(f64::from(pair[1]));
            }
            (left, right)
        }

        /// Pearson correlation with the means removed, so a constant priming
        /// bias cannot inflate the score. Flat signals score 0.
        fn normalized_correlation(left: &[f64], right: &[f64]) -> f64 {
            assert_eq!(left.len(), right.len(), "correlation inputs");
            let count = left.len() as f64;
            let left_mean = left.iter().sum::<f64>() / count;
            let right_mean = right.iter().sum::<f64>() / count;
            let (mut dot, mut left_energy, mut right_energy) = (0.0, 0.0, 0.0);
            for (a, b) in left.iter().zip(right) {
                let a = a - left_mean;
                let b = b - right_mean;
                dot += a * b;
                left_energy += a * a;
                right_energy += b * b;
            }
            let denominator = (left_energy * right_energy).sqrt();
            if denominator <= f64::MIN_POSITIVE {
                0.0
            } else {
                dot / denominator
            }
        }

        fn downsample_mean(samples: &[f64], factor: usize) -> Vec<f64> {
            samples
                .chunks_exact(factor)
                .map(|chunk| chunk.iter().sum::<f64>() / factor as f64)
                .collect()
        }

        /// Finds the one global decoder priming/lookahead offset. The bounded
        /// coarse pass runs on 4x mean-downsampled left-channel data (the
        /// 3.4 kHz chirp stays below the 5.5 kHz decimated Nyquist), then a
        /// full-rate fit refines the winner. The result is applied to both
        /// channels and to every block for the whole stream: this is
        /// deliberately not a per-block realignment.
        fn find_decoder_lag(reference: &[i16], decoded: &[i16]) -> usize {
            const FACTOR: usize = 4;
            let (reference_left, _) = deinterleave(reference);
            let (decoded_left, _) = deinterleave(decoded);
            let reference_ds = downsample_mean(&reference_left, FACTOR);
            let decoded_ds = downsample_mean(&decoded_left, FACTOR);

            let search_frames = (SAMPLE_RATE as usize / 2).min(reference_left.len());
            let reference_ds = &reference_ds[..search_frames / FACTOR];
            let max_lag_ds = MAX_DECODER_LAG_FRAMES / FACTOR;
            let mut coarse_lag_ds = 0usize;
            let mut coarse_correlation = f64::NEG_INFINITY;
            for lag_ds in 0..=max_lag_ds {
                let length = reference_ds
                    .len()
                    .min(decoded_ds.len().saturating_sub(lag_ds));
                if length < 32 {
                    break;
                }
                let correlation = normalized_correlation(
                    &reference_ds[..length],
                    &decoded_ds[lag_ds..lag_ds + length],
                );
                if correlation > coarse_correlation {
                    coarse_correlation = correlation;
                    coarse_lag_ds = lag_ds;
                }
            }

            let coarse = coarse_lag_ds * FACTOR;
            let first = coarse.saturating_sub(FACTOR * 2);
            let last = (coarse + FACTOR * 2).min(MAX_DECODER_LAG_FRAMES);
            let length = search_frames.min(decoded_left.len().saturating_sub(last));
            let mut lag = coarse;
            let mut correlation = f64::NEG_INFINITY;
            for candidate in first..=last {
                let candidate_correlation = normalized_correlation(
                    &reference_left[..length],
                    &decoded_left[candidate..candidate + length],
                );
                if candidate_correlation > correlation {
                    correlation = candidate_correlation;
                    lag = candidate;
                }
            }
            lag
        }

        #[derive(Debug)]
        struct ChannelScore {
            correlation: f64,
            gain: f64,
            snr_db: f64,
            min_block_snr_db: f64,
            worst_block: usize,
            peak: i32,
            pinned: usize,
        }

        #[derive(Debug)]
        struct FidelityScore {
            lag: usize,
            left: ChannelScore,
            right: ChannelScore,
            left_reference_to_right_decoded: f64,
            right_reference_to_left_decoded: f64,
        }

        fn score_channel(reference: &[f64], decoded: &[f64], full_decoded: &[f64]) -> ChannelScore {
            assert_eq!(reference.len(), decoded.len(), "aligned channel lengths");
            let correlation = normalized_correlation(reference, decoded);
            let reference_energy: f64 = reference.iter().map(|sample| sample * sample).sum();
            let dot: f64 = reference
                .iter()
                .zip(decoded)
                .map(|(reference, decoded)| reference * decoded)
                .sum();
            let gain = if reference_energy > 0.0 {
                dot / reference_energy
            } else {
                1.0
            };
            let residual_energy: f64 = reference
                .iter()
                .zip(decoded)
                .map(|(reference, decoded)| {
                    let residual = decoded - gain * reference;
                    residual * residual
                })
                .sum();
            let snr_db = 10.0 * (reference_energy / residual_energy.max(f64::MIN_POSITIVE)).log10();

            let mut min_block_snr_db = f64::INFINITY;
            let mut worst_block = 0usize;
            for (block, (reference_block, decoded_block)) in reference
                .chunks(FRAMES_PER_BLOCK)
                .zip(decoded.chunks(FRAMES_PER_BLOCK))
                .enumerate()
            {
                let block_reference_energy: f64 =
                    reference_block.iter().map(|sample| sample * sample).sum();
                if block_reference_energy <= 1.0 {
                    continue; // only evaluate windows that carry the probe
                }
                let block_residual_energy: f64 = reference_block
                    .iter()
                    .zip(decoded_block)
                    .map(|(reference, decoded)| {
                        let residual = decoded - gain * reference;
                        residual * residual
                    })
                    .sum();
                let block_snr_db = 10.0
                    * (block_reference_energy / block_residual_energy.max(f64::MIN_POSITIVE))
                        .log10();
                if block_snr_db < min_block_snr_db {
                    min_block_snr_db = block_snr_db;
                    worst_block = block;
                }
            }

            let peak = full_decoded
                .iter()
                .map(|sample| sample.abs() as i32)
                .fold(0i32, i32::max);
            let pinned = full_decoded
                .iter()
                .filter(|sample| sample.abs() >= 32_767.0)
                .count();
            ChannelScore {
                correlation,
                gain,
                snr_db,
                min_block_snr_db,
                worst_block,
                peak,
                pinned,
            }
        }

        /// Scores `decoded` against `reference` after applying the single
        /// `lag`, skipping the first/last [`EDGE_SKIP_FRAMES`] of the overlap.
        fn score_fidelity(reference: &[i16], decoded: &[i16], lag: usize) -> FidelityScore {
            let channels = CHANNELS as usize;
            let reference_frames = reference.len() / channels;
            let decoded_frames = decoded.len() / channels;
            assert!(
                decoded_frames > lag,
                "decoded {decoded_frames} frames do not cover the {lag}-frame priming offset"
            );
            let overlap_frames = reference_frames.min(decoded_frames - lag);
            assert!(
                overlap_frames > 2 * EDGE_SKIP_FRAMES + FRAMES_PER_BLOCK,
                "only {overlap_frames} aligned frames to evaluate"
            );
            let start = EDGE_SKIP_FRAMES;
            let end = overlap_frames - EDGE_SKIP_FRAMES;

            let (reference_left, reference_right) = deinterleave(reference);
            let (decoded_left, decoded_right) = deinterleave(decoded);

            let left = score_channel(
                &reference_left[start..end],
                &decoded_left[start + lag..end + lag],
                &decoded_left,
            );
            let right = score_channel(
                &reference_right[start..end],
                &decoded_right[start + lag..end + lag],
                &decoded_right,
            );
            FidelityScore {
                lag,
                left,
                right,
                left_reference_to_right_decoded: normalized_correlation(
                    &reference_left[start..end],
                    &decoded_right[start + lag..end + lag],
                ),
                right_reference_to_left_decoded: normalized_correlation(
                    &reference_right[start..end],
                    &decoded_left[start + lag..end + lag],
                ),
            }
        }

        fn channel_failures(label: &str, score: &ChannelScore) -> Vec<String> {
            let mut failures = Vec::new();
            if score.correlation < MIN_CHANNEL_CORRELATION {
                failures.push(format!(
                    "{label} channel correlation {:.4} is below {MIN_CHANNEL_CORRELATION}",
                    score.correlation
                ));
            }
            if score.snr_db < MIN_CHANNEL_SNR_DB {
                failures.push(format!(
                    "{label} channel SNR {:.2} dB is below {MIN_CHANNEL_SNR_DB} dB",
                    score.snr_db
                ));
            }
            if score.min_block_snr_db < MIN_BLOCK_SNR_DB {
                failures.push(format!(
                    "{label} channel block {} SNR {:.2} dB is below {MIN_BLOCK_SNR_DB} dB",
                    score.worst_block, score.min_block_snr_db
                ));
            }
            if !(MIN_CHANNEL_GAIN..=MAX_CHANNEL_GAIN).contains(&score.gain) {
                failures.push(format!(
                    "{label} channel gain {:.4} is outside [{MIN_CHANNEL_GAIN}, {MAX_CHANNEL_GAIN}]",
                    score.gain
                ));
            }
            if score.pinned > 0 {
                failures.push(format!(
                    "{label} channel has {} samples pinned at the int16 limit (clipping, peak {})",
                    score.pinned, score.peak
                ));
            }
            failures
        }

        impl FidelityScore {
            fn failures(&self) -> Vec<String> {
                let mut failures = channel_failures("left", &self.left);
                failures.extend(channel_failures("right", &self.right));
                let mismatch = (self.left.gain - self.right.gain).abs();
                if mismatch > MAX_CHANNEL_GAIN_MISMATCH {
                    failures.push(format!(
                        "channel gain mismatch {mismatch:.4} exceeds {MAX_CHANNEL_GAIN_MISMATCH}"
                    ));
                }
                if self.left_reference_to_right_decoded > MAX_CROSS_CHANNEL_CORRELATION {
                    failures.push(format!(
                        "cross-channel independence failed: left reference to right decoded \
                         correlation {:.4} is above {MAX_CROSS_CHANNEL_CORRELATION}",
                        self.left_reference_to_right_decoded
                    ));
                }
                if self.right_reference_to_left_decoded > MAX_CROSS_CHANNEL_CORRELATION {
                    failures.push(format!(
                        "cross-channel independence failed: right reference to left decoded \
                         correlation {:.4} is above {MAX_CROSS_CHANNEL_CORRELATION}",
                        self.right_reference_to_left_decoded
                    ));
                }
                failures
            }
        }

        /// Encodes the full stereo probe, decodes it with the MF decoder, and
        /// returns `(reference, decoded, global lag)`.
        fn decode_probe() -> (Vec<i16>, Vec<i16>, usize) {
            let reference = stereo_probe_reference(PROBE_BLOCKS);
            let (mut frames, drained) = encode_session(PROBE_BLOCKS, stereo_probe_block);
            assert_eq!(
                frames.len() + drained.len(),
                PROBE_BLOCKS as usize,
                "probe encoder frame count"
            );
            frames.extend(drained);
            let decoded = decode_frames(&frames);
            assert_eq!(decoded.sample_rate, SAMPLE_RATE);
            assert_eq!(decoded.channels, CHANNELS as u32);
            let lag = find_decoder_lag(&reference, &decoded.samples);
            assert!(lag <= MAX_DECODER_LAG_FRAMES, "decoder lag {lag}");
            (reference, decoded.samples, lag)
        }

        /// First decoded frame of evaluation block `block` (block 0 is the
        /// first full block after [`EDGE_SKIP_FRAMES`], shifted by the global
        /// decoder lag). Used only to place the corruption in negative
        /// controls.
        fn decoded_eval_block_start(block: usize, lag: usize) -> usize {
            EDGE_SKIP_FRAMES + block * FRAMES_PER_BLOCK + lag
        }

        fn swap_channels(samples: &[i16]) -> Vec<i16> {
            samples
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|pair| [pair[1], pair[0]])
                .collect()
        }

        fn duplicate_left_into_right(samples: &[i16]) -> Vec<i16> {
            samples
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|pair| [pair[0], pair[0]])
                .collect()
        }

        fn zero_block(samples: &[i16], at: usize) -> Vec<i16> {
            let mut corrupted = samples.to_vec();
            let start = at * CHANNELS as usize;
            let end = start + FRAMES_PER_BLOCK * CHANNELS as usize;
            corrupted[start..end].fill(0);
            corrupted
        }

        fn drop_block(samples: &[i16], at: usize) -> Vec<i16> {
            let mut corrupted = samples.to_vec();
            let start = at * CHANNELS as usize;
            let end = start + FRAMES_PER_BLOCK * CHANNELS as usize;
            corrupted.drain(start..end);
            corrupted
        }

        fn repeat_block(samples: &[i16], at: usize) -> Vec<i16> {
            let mut corrupted = samples.to_vec();
            let start = at * CHANNELS as usize;
            let end = start + FRAMES_PER_BLOCK * CHANNELS as usize;
            let block = corrupted[start..end].to_vec();
            corrupted.splice(start..start, block);
            corrupted
        }

        fn add_static_noise(samples: &[i16]) -> Vec<i16> {
            let mut state = 0x1234_5678u32;
            samples
                .iter()
                .map(|sample| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let noise = (state % 12_001) as i32 - STATIC_NOISE_AMPLITUDE;
                    sample.saturating_add(noise as i16)
                })
                .collect()
        }

        fn assert_metric_rejects(label: &str, failures: &[String], needles: &[&str]) {
            assert!(
                !failures.is_empty(),
                "{label}: the corrupted stream passed every fidelity check"
            );
            for needle in needles {
                assert!(
                    failures.iter().any(|failure| failure.contains(needle)),
                    "{label}: expected a {needle:?} failure, got {failures:#?}"
                );
            }
            eprintln!(
                "{label}: rejected with {} failure(s); first: {}",
                failures.len(),
                failures[0]
            );
        }

        /// Real MF AAC encode/decode round trip of a deterministic chirp
        /// stereo probe. After one global decoder lag the waveform must match
        /// the reference: correlation and SNR of reasonable 128 kbit/s AAC
        /// quality, gain bounds, no clipping, channel independence, and every
        /// 1024-frame window must clear [`MIN_BLOCK_SNR_DB`].
        #[test]
        fn mf_aac_decoder_preserves_stereo_waveform_fidelity() {
            let (reference, decoded, lag) = decode_probe();
            let score = score_fidelity(&reference, &decoded, lag);
            eprintln!(
                "stereo fidelity: lag {} frames, left {:?}, right {:?}, \
                 cross L->R {:.5}, cross R->L {:.5}",
                score.lag,
                score.left,
                score.right,
                score.left_reference_to_right_decoded,
                score.right_reference_to_left_decoded
            );
            let failures = score.failures();
            assert!(
                failures.is_empty(),
                "AAC waveform fidelity regressed: {failures:#?}"
            );
        }

        /// Negative control: swapping or duplicating channels must fail the
        /// same fidelity metric (channel correlation and independence).
        #[test]
        fn waveform_fidelity_metric_rejects_swapped_and_duplicated_channels() {
            let (reference, decoded, lag) = decode_probe();

            let swapped = swap_channels(&decoded);
            let failures = score_fidelity(&reference, &swapped, lag).failures();
            assert_metric_rejects("swapped channels", &failures, &["independence"]);

            let duplicated = duplicate_left_into_right(&decoded);
            let failures = score_fidelity(&reference, &duplicated, lag).failures();
            assert_metric_rejects("duplicated left channel", &failures, &["independence"]);
        }

        /// Negative control: packet seams (zeroed, dropped or repeated
        /// 1024-frame blocks) and static noise must fail the same fidelity
        /// metric. The zeroed block hides from the global average, so only the
        /// per-block SNR check catches it.
        #[test]
        fn waveform_fidelity_metric_rejects_seams_and_static_noise() {
            let (reference, decoded, lag) = decode_probe();
            let seam = decoded_eval_block_start(20, lag);

            let zeroed = zero_block(&decoded, seam);
            let failures = score_fidelity(&reference, &zeroed, lag).failures();
            assert_metric_rejects("zeroed block", &failures, &["block"]);

            let dropped = drop_block(&decoded, seam);
            let failures = score_fidelity(&reference, &dropped, lag).failures();
            assert_metric_rejects("dropped block", &failures, &["correlation"]);

            let repeated = repeat_block(&decoded, seam);
            let failures = score_fidelity(&reference, &repeated, lag).failures();
            assert_metric_rejects("repeated block", &failures, &["correlation"]);

            let noisy = add_static_noise(&decoded);
            let failures = score_fidelity(&reference, &noisy, lag).failures();
            assert_metric_rejects("static noise", &failures, &["SNR"]);
        }

        /// Real CPU-only A/V transport regression: synthetic H.264 through the
        /// real Cast HLS muxer plus real MF AAC pulses, then an independent
        /// TS/PES demux, ADTS/PTS comparison and MF decode of the extracted
        /// audio. HTTP serving is covered by the rotten-video pipeline test.
        #[cfg(feature = "encode-source")]
        mod transport {
            use super::*;
            use rotten_cast::hls::{HlsMuxer, HlsStore, Segment};
            use rotten_video::{Encoder as _, SoftwareEncoder, SyntheticSource};
            use std::time::{Duration, Instant};

            const WIDTH: u32 = 64;
            const HEIGHT: u32 = 48;
            const FPS: u32 = 30;
            const BITRATE_KBPS: u32 = 2000;
            const IDR_PERIOD: usize = 30;
            /// 241 frames = eight one-second segments plus the sealing IDR.
            const VIDEO_FRAMES: usize = 241;
            /// ceil(8 s * 44_100 / 1024) + 1 = 346 AAC blocks, so audio
            /// outlasts the eighth video seal and the unsealed tail stays
            /// non-empty.
            const AUDIO_BLOCKS: u64 = 346;
            /// 200 ms loud / 200 ms quiet, so pulses survive any codec delay.
            const PULSE_FRAMES: u64 = SAMPLE_RATE as u64 / 5;
            const TS_PACKET: usize = 188;
            const PID_AUDIO: u16 = 0x0102;
            const PES_PTS_OFFSET_90K: u64 = 90_000;
            const PTS33_MASK: u64 = (1 << 33) - 1;

            enum AvEvent {
                Video {
                    data: Vec<u8>,
                    pts_us: u64,
                    key: bool,
                },
                Audio(AacFrame),
            }

            impl AvEvent {
                fn pts_us(&self) -> u64 {
                    match self {
                        Self::Video { pts_us, .. } => *pts_us,
                        Self::Audio(frame) => frame.pts_us,
                    }
                }
            }

            fn pulse_block(start_frame: u64) -> Vec<u8> {
                let mut pcm = Vec::with_capacity(PCM_BLOCK_BYTES);
                for index in 0..FRAMES_PER_BLOCK {
                    let frame = start_frame + index as u64;
                    let sample = if (frame / PULSE_FRAMES).is_multiple_of(2) {
                        let phase =
                            std::f64::consts::TAU * 440.0 * frame as f64 / SAMPLE_RATE as f64;
                        (phase.sin() * TONE_AMPLITUDE) as i16
                    } else {
                        0
                    };
                    pcm.extend_from_slice(&sample.to_le_bytes());
                    pcm.extend_from_slice(&sample.to_le_bytes());
                }
                pcm
            }

            struct TransportPes {
                pts_90k: u64,
                adts: Vec<u8>,
            }

            /// Reassembles the audio PES packets directly from TS packets.
            fn audio_pes_packets(segment: &Segment) -> Vec<TransportPes> {
                let (packets, remainder) = segment.data.as_chunks::<TS_PACKET>();
                assert!(
                    remainder.is_empty(),
                    "segment {} is whole TS packets",
                    segment.sequence
                );
                let mut pes = Vec::new();
                let mut current: Option<Vec<u8>> = None;
                for packet in packets {
                    assert_eq!(packet[0], 0x47, "TS sync byte");
                    let pid = (((packet[1] & 0x1F) as u16) << 8) | packet[2] as u16;
                    if pid != PID_AUDIO {
                        continue;
                    }
                    let afc = (packet[3] >> 4) & 0x03;
                    let mut offset = 4;
                    if afc & 0x02 != 0 {
                        let length = packet[4] as usize;
                        assert!(5 + length <= TS_PACKET, "adaptation field overruns");
                        offset = 5 + length;
                    }
                    if afc & 0x01 == 0 {
                        continue; // PCR-only refresh
                    }
                    if packet[1] & 0x40 != 0 {
                        if let Some(bytes) = current.take() {
                            pes.push(parse_audio_pes(&bytes));
                        }
                        current = Some(packet[offset..].to_vec());
                    } else if let Some(bytes) = current.as_mut() {
                        bytes.extend_from_slice(&packet[offset..]);
                    } else {
                        panic!("audio payload before the first PES start");
                    }
                }
                if let Some(bytes) = current {
                    pes.push(parse_audio_pes(&bytes));
                }
                pes
            }

            fn parse_audio_pes(data: &[u8]) -> TransportPes {
                assert!(data.len() >= 14, "audio PES too short");
                assert_eq!(&data[..3], &[0x00, 0x00, 0x01], "PES start code");
                assert_eq!(data[3], 0xC0, "audio PES stream id");
                let declared = u16::from_be_bytes([data[4], data[5]]) as usize;
                assert_eq!(declared, data.len() - 6, "bounded audio PES length");
                assert_eq!(data[7], 0x80, "PES PTS flag");
                assert_eq!(data[8], 5, "PTS-only PES header");
                let bytes = &data[9..14];
                let pts = (((bytes[0] as u64 >> 1) & 0x07) << 30)
                    | ((bytes[1] as u64) << 22)
                    | ((bytes[2] as u64 >> 1) << 15)
                    | ((bytes[3] as u64) << 7)
                    | (bytes[4] as u64 >> 1);
                TransportPes {
                    pts_90k: pts,
                    adts: data[14..].to_vec(),
                }
            }

            #[test]
            #[cfg_attr(feature = "encode-dll", ignore = "requires the OpenH264 DLL")]
            fn aac_survives_hls_ts_demux_and_mf_decode() {
                // Real MF AAC over a pulse pattern.
                let mut encoder = AacEncoder::new().expect("MF AAC encoder");
                let mut audio = Vec::new();
                for block in 0..AUDIO_BLOCKS {
                    let start = block * FRAMES_PER_BLOCK as u64;
                    audio.extend(
                        encoder
                            .encode(&pulse_block(start), start)
                            .expect("AAC block"),
                    );
                }
                audio.extend(encoder.finish().expect("drain AAC"));
                assert_eq!(audio.len(), AUDIO_BLOCKS as usize);

                // Real 64x48 H.264 with an IDR every second.
                let mut source = SyntheticSource::new(WIDTH, HEIGHT);
                let mut video = SoftwareEncoder::new(WIDTH, HEIGHT, BITRATE_KBPS, FPS)
                    .expect("OpenH264 source encoder");
                let mut events = Vec::new();
                for index in 0..VIDEO_FRAMES {
                    if index % IDR_PERIOD == 0 {
                        video.force_keyframe();
                    }
                    let (rgba, width, height) = source.next_frame().expect("synthetic frame");
                    let pts_us = index as u64 * 1_000_000 / u64::from(FPS);
                    let frame = video
                        .encode(&rgba, width, height, pts_us)
                        .expect("encode video")
                        .expect("video bitstream");
                    events.push(AvEvent::Video {
                        data: frame.data,
                        pts_us,
                        key: frame.is_keyframe,
                    });
                }
                for frame in audio {
                    events.push(AvEvent::Audio(frame));
                }
                // Stable sort keeps video before audio on equal timestamps.
                events.sort_by_key(|event| event.pts_us());

                // Mux in globally ascending PTS order and remember which audio
                // frames the muxer assigned to each sealed segment. The store's
                // advertised snapshot only advances once per wall second, so
                // offline publishing must drive a fake monotonic clock by the
                // end of every sealed segment instead of sleeping.
                let mut muxer = HlsMuxer::with_aac();
                let mut store = HlsStore::new();
                let mut segments = Vec::new();
                let mut assigned: Vec<Vec<AacFrame>> = Vec::new();
                let mut open_audio = Vec::new();
                let mut open = false;
                let clock_epoch = Instant::now();
                let mut clock_elapsed = Duration::ZERO;
                for event in events {
                    match event {
                        AvEvent::Video { data, pts_us, key } => {
                            if key {
                                open = true;
                            }
                            if let Some(segment) = muxer.push(&data, pts_us).expect("mux video") {
                                assigned.push(std::mem::take(&mut open_audio));
                                clock_elapsed += Duration::from_secs_f64(segment.duration);
                                store
                                    .publish_at(
                                        segment.clone(),
                                        muxer.codec().expect("codec"),
                                        clock_epoch + clock_elapsed,
                                    )
                                    .expect("publish segment");
                                segments.push(segment);
                            }
                        }
                        AvEvent::Audio(frame) => {
                            muxer
                                .push_audio(&frame.data, frame.pts_us)
                                .expect("mux audio");
                            if open {
                                open_audio.push(frame);
                            }
                        }
                    }
                }
                let unsealed_tail = open_audio;
                assert!(segments.len() >= 8, "sealed {} segments", segments.len());
                let advertised: f64 = segments.iter().map(|segment| segment.duration).sum();
                assert!(
                    advertised >= 8.0,
                    "the sealed segments must cover at least eight seconds, got {advertised}"
                );
                assert!(
                    store.ready(),
                    "store must advertise the initial eight-second A/V buffer"
                );
                assert!(muxer.codec().expect("codec").contains("mp4a.40.2"));
                assert_eq!(
                    assigned.iter().map(Vec::len).sum::<usize>() + unsealed_tail.len(),
                    AUDIO_BLOCKS as usize,
                    "every audio frame is either transported or in the unsealed tail"
                );
                assert!(
                    !unsealed_tail.is_empty(),
                    "the sealing IDR leaves an unsealed tail"
                );

                // Compare demuxed PES payloads and PTS to the encoded frames.
                let mut transported = Vec::new();
                let mut previous_pts = None;
                for (segment, expected) in segments.iter().zip(&assigned) {
                    let pes = audio_pes_packets(segment);
                    assert_eq!(
                        pes.len(),
                        expected.len(),
                        "segment {} audio frames",
                        segment.sequence
                    );
                    for (got, want) in pes.iter().zip(expected) {
                        assert_eq!(
                            got.adts, want.data,
                            "segment {} ADTS bytes",
                            segment.sequence
                        );
                        let expected_pts = ((want.pts_us as u128 * 9 / 100) as u64
                            + PES_PTS_OFFSET_90K)
                            & PTS33_MASK;
                        assert_eq!(
                            got.pts_90k, expected_pts,
                            "segment {} PTS",
                            segment.sequence
                        );
                        if let Some(previous) = previous_pts {
                            assert!(got.pts_90k > previous, "audio PTS must strictly increase");
                        }
                        previous_pts = Some(got.pts_90k);
                        transported.push(AacFrame {
                            data: got.adts.clone(),
                            pts_us: want.pts_us,
                        });
                    }
                }
                assert_eq!(
                    transported.len(),
                    AUDIO_BLOCKS as usize - unsealed_tail.len()
                );

                // The reconstructed stream must decode audibly with the MF
                // decoder, remain gapless, and keep pulse/quiet regions.
                let decoded = decode_frames(&transported);
                assert_eq!(decoded.sample_rate, SAMPLE_RATE);
                assert_eq!(decoded.channels, CHANNELS as u32);
                assert_eq!(
                    decoded.samples.len(),
                    transported.len() * FRAMES_PER_BLOCK * CHANNELS as usize,
                    "decoded sample count must exactly match the transported AAC frames"
                );
                let window = SAMPLE_RATE as usize / 5;
                let mut pulse_sq = 0.0;
                let mut pulse_n = 0usize;
                let mut quiet_sq = 0.0;
                let mut quiet_n = 0usize;
                for (index, stereo) in decoded.samples.as_chunks::<2>().0.iter().enumerate() {
                    let square = f64::from(stereo[0]) * f64::from(stereo[0]);
                    if (index / window).is_multiple_of(2) {
                        pulse_sq += square;
                        pulse_n += 1;
                    } else {
                        quiet_sq += square;
                        quiet_n += 1;
                    }
                }
                let pulse_rms = (pulse_sq / pulse_n as f64).sqrt();
                let quiet_rms = (quiet_sq / quiet_n as f64).sqrt();
                assert!(pulse_rms > 500.0, "pulse RMS {pulse_rms} is not audible");
                assert!(quiet_rms < 50.0, "quiet RMS {quiet_rms} is not silent");
                assert!(
                    pulse_rms > quiet_rms * 20.0,
                    "pulses ({pulse_rms}) and quiet ({quiet_rms}) are not separated"
                );
            }
        }
    }
}
