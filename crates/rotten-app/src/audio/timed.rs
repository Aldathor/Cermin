//! Timed system-audio capture for the Cast path.
//!
//! This is the timestamped sibling of the AirPlay mirror capture in
//! `super::capture`. It never touches the endpoint mute state or volume, so
//! local audio keeps playing. Every WASAPI loopback packet is converted to
//! 44.1 kHz stereo S16 LE and tagged with the output sample frame of its first
//! sample, so downstream can anchor the stream to [`SessionClock`], trim
//! negative startup frames, and fill dropped packets from the absolute
//! timeline instead of compressing it. Contiguous device packets tile exactly:
//! [`PacketTimeline`] carries the converted length forward and only re-anchors
//! on a device discontinuity, so jittery QPC timestamps cannot insert or drop
//! samples.

use anyhow::anyhow;
use std::time::Instant;

#[cfg(target_os = "windows")]
use super::capture::{Resampler, decode_frame, validate_format};
#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "windows")]
use std::time::Duration;
#[cfg(target_os = "windows")]
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
#[cfg(target_os = "windows")]
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
#[cfg(target_os = "windows")]
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};

/// Capacity of the bounded converted-PCM queue handed to the consumer.
#[cfg(target_os = "windows")]
const QUEUE_CAPACITY: usize = 64;
/// Startup readiness must arrive quickly, otherwise the caller would hang.
#[cfg(target_os = "windows")]
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounded wait for the capture worker to unwind after a stop request.
#[cfg(target_os = "windows")]
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval while WASAPI has no completed packet.
#[cfg(target_os = "windows")]
const POLL_INTERVAL: Duration = Duration::from_millis(3);
/// Lowest mix rate accepted from WASAPI.
#[cfg(target_os = "windows")]
const MIN_INPUT_RATE: u32 = 8_000;
/// Highest mix rate accepted from WASAPI.
#[cfg(target_os = "windows")]
const MAX_INPUT_RATE: u32 = 384_000;
/// Upper bound on mix channels; conversion only reads the first two.
#[cfg(target_os = "windows")]
const MAX_INPUT_CHANNELS: u16 = 32;
/// Hard cap on native frames in one packet: 500 ms at the highest mix rate.
#[cfg(target_os = "windows")]
const MAX_NATIVE_PACKET_FRAMES: u32 = MAX_INPUT_RATE / 2;
/// Hard cap on converted 44.1 kHz frames in one packet: 500 ms plus two frames,
/// so a full 64-packet queue stays below 6 MiB.
#[cfg(target_os = "windows")]
const MAX_OUTPUT_PACKET_FRAMES: usize = 22_052;
/// Number of attempts used to find a tight Instant/QPC calibration bracket.
#[cfg(target_os = "windows")]
const CALIBRATION_ATTEMPTS: usize = 8;
/// A bracket at or below this span is good enough to stop retrying.
#[cfg(target_os = "windows")]
const CALIBRATION_GOOD_SPAN: Duration = Duration::from_millis(1);

/// One converted system-audio packet on a shared session timeline.
#[derive(Debug)]
pub struct TimedPcm {
    /// Output frame index of this packet's first sample, relative to the
    /// owning [`SessionClock`] origin. The index counts 44.1 kHz stereo frames;
    /// negative values are startup frames that downstream may trim. Packets of
    /// one continuous device stream are positioned by the carried converted
    /// length, so they tile exactly and a dropped packet leaves a true hole.
    pub start_frame: i64,
    /// Interleaved stereo S16 LE samples at 44.1 kHz.
    pub data: Vec<u8>,
}

/// Per-session origin for mapping WASAPI QPC timestamps to the 44.1 kHz output
/// timeline.
///
/// The clock itself is a cheap `Instant` origin, so it also works on platforms
/// without WASAPI. On Windows creation additionally calibrates the origin
/// against `QueryPerformanceCounter`/`QueryPerformanceFrequency`, which lets
/// the 100 ns QPC timestamps reported by WASAPI be converted into a signed
/// output sample frame. The `Instant` origin and the QPC origin are aligned by
/// bracketing the counter read with two `Instant` samples and taking their
/// midpoint; the residual alignment uncertainty is at most half of the
/// tightest bracket (microseconds in practice). Clones share the origin, so
/// several consumers can use one session timeline without a global clock.
#[derive(Clone)]
pub struct SessionClock {
    origin: Instant,
    qpc: Option<QpcAnchor>,
}

#[derive(Clone, Copy)]
struct QpcAnchor {
    /// `QueryPerformanceCounter` value at creation, in the 100 ns units WASAPI
    /// uses for its QPC timestamps.
    origin_100ns: i128,
}

impl SessionClock {
    /// Create a clock anchored at the current instant.
    ///
    /// On Windows the `Instant` origin is aligned with the QPC origin (see
    /// [`windows_qpc_anchor`]), so [`SessionClock::elapsed_us`] and
    /// [`SessionClock::sample_index_for_qpc`] describe the same timeline.
    pub fn new() -> anyhow::Result<Self> {
        #[cfg(target_os = "windows")]
        {
            let (origin, qpc) = windows_qpc_anchor()?;
            Ok(Self {
                origin,
                qpc: Some(qpc),
            })
        }
        #[cfg(not(target_os = "windows"))]
        {
            Ok(Self {
                origin: Instant::now(),
                qpc: None,
            })
        }
    }

    /// Microseconds elapsed since the session origin.
    pub fn elapsed_us(&self) -> u64 {
        self.origin.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
    }

    /// Map a WASAPI QPC timestamp (100 ns units, as reported by
    /// `IAudioCaptureClient::GetBuffer`) to the 44.1 kHz output sample frame
    /// that contains it, relative to this clock's origin.
    ///
    /// Frames before the origin are negative so callers can trim startup
    /// audio. The arithmetic is checked and never silently wraps.
    pub fn sample_index_for_qpc(&self, qpc_100ns: u64) -> anyhow::Result<i64> {
        let anchor = self.qpc.ok_or_else(|| {
            anyhow!("QPC timestamps can only be mapped by a Windows session clock")
        })?;
        qpc_100ns_to_sample_index(anchor.origin_100ns, qpc_100ns)
    }
}

/// Calibrate the session clock against `QueryPerformanceCounter`.
///
/// The frequency is stable for the machine's uptime, so it is read before the
/// timestamp bracket and cannot skew the alignment. The counter reading itself
/// is bracketed by two `Instant::now()` samples; the returned `Instant` origin
/// is the midpoint of the tightest bracket observed, so the Instant and QPC
/// origins describe the same moment even if the thread was preempted during
/// the counter call. The residual alignment uncertainty is at most half of that
/// bracket (retried up to [`CALIBRATION_ATTEMPTS`] times, typically
/// microseconds).
#[cfg(target_os = "windows")]
fn windows_qpc_anchor() -> anyhow::Result<(Instant, QpcAnchor)> {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    let mut frequency = 0i64;
    unsafe { QueryPerformanceFrequency(&mut frequency) }
        .map_err(|error| anyhow!("could not read the performance counter frequency: {error}"))?;
    if frequency <= 0 {
        anyhow::bail!("performance counter frequency is invalid: {frequency}");
    }

    let mut best: Option<(Instant, i64, Instant)> = None;
    for _ in 0..CALIBRATION_ATTEMPTS {
        let before = Instant::now();
        let mut counter = 0i64;
        unsafe { QueryPerformanceCounter(&mut counter) }
            .map_err(|error| anyhow!("could not read the performance counter: {error}"))?;
        if counter < 0 {
            anyhow::bail!("performance counter returned a negative value: {counter}");
        }
        let after = Instant::now();
        let span = after.saturating_duration_since(before);
        let tightest = best.as_ref().is_none_or(|(best_before, _, best_after)| {
            span < best_after.saturating_duration_since(*best_before)
        });
        if tightest {
            best = Some((before, counter, after));
        }
        if span <= CALIBRATION_GOOD_SPAN {
            break;
        }
    }
    let Some((before, counter, after)) = best else {
        anyhow::bail!("performance counter calibration did not run");
    };
    let (origin, origin_100ns) = calibration_from_bracket(before, counter, after, frequency)?;
    Ok((origin, QpcAnchor { origin_100ns }))
}

/// Convert a raw QPC counter reading into the 100 ns units WASAPI uses for its
/// QPC timestamps, with checked integer arithmetic.
#[cfg(any(target_os = "windows", test))]
fn qpc_counter_to_100ns(counter: i64, frequency: i64) -> anyhow::Result<i128> {
    if frequency <= 0 {
        anyhow::bail!("performance counter frequency is invalid: {frequency}");
    }
    if counter < 0 {
        anyhow::bail!("performance counter returned a negative value: {counter}");
    }
    Ok(i128::from(counter) * 10_000_000 / i128::from(frequency))
}

/// Pair a bracketed QPC reading with the `Instant` at the middle of its
/// bracket, so both origins refer to the same moment.
#[cfg(any(target_os = "windows", test))]
fn calibration_from_bracket(
    before: Instant,
    counter: i64,
    after: Instant,
    frequency: i64,
) -> anyhow::Result<(Instant, i128)> {
    let origin = before + after.saturating_duration_since(before) / 2;
    let origin_100ns = qpc_counter_to_100ns(counter, frequency)?;
    Ok((origin, origin_100ns))
}

/// Shared checked QPC -> 44.1 kHz sample-frame math, independent of the
/// platform-specific calibration in [`SessionClock::new`].
fn qpc_100ns_to_sample_index(origin_100ns: i128, qpc_100ns: u64) -> anyhow::Result<i64> {
    const HNS_PER_SECOND: i128 = 10_000_000;
    const OUTPUT_RATE: i128 = 44_100;
    let delta = i128::from(qpc_100ns)
        .checked_sub(origin_100ns)
        .ok_or_else(|| anyhow!("QPC timestamp is out of range"))?;
    let samples = delta
        .checked_mul(OUTPUT_RATE)
        .ok_or_else(|| anyhow!("QPC timestamp is out of range"))?
        .div_euclid(HNS_PER_SECOND);
    i64::try_from(samples).map_err(|_| anyhow!("QPC timestamp maps outside the sample timeline"))
}

/// Bounded-queue WASAPI loopback capture that never changes local audio.
///
/// The worker owns the COM apartment, device, client and buffer lifetimes; the
/// owner only holds the stop flag and the join handle, so dropping it requests
/// shutdown even while [`TimedLoopback::start`] is still awaiting readiness.
#[cfg(target_os = "windows")]
pub struct TimedLoopback {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
}

#[cfg(target_os = "windows")]
impl TimedLoopback {
    /// Start capturing the default render endpoint. The returned receiver
    /// yields 44.1 kHz stereo S16 LE packets tagged with their output-timeline
    /// position; dropping it stops the worker promptly.
    pub async fn start(
        clock: SessionClock,
    ) -> anyhow::Result<(Self, tokio::sync::mpsc::Receiver<TimedPcm>)> {
        let (tx, rx) = tokio::sync::mpsc::channel::<TimedPcm>(QUEUE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
        let thread = std::thread::Builder::new()
            .name("wasapi-timed-loopback".into())
            .spawn(move || {
                let mut ready_tx = Some(ready_tx);
                let result = run_timed_loopback(tx, worker_stop, clock, || {
                    if let Some(ready_tx) = ready_tx.take() {
                        let _ = ready_tx.send(Ok(()));
                    }
                });
                if let Err(error) = &result {
                    if let Some(ready_tx) = ready_tx.take() {
                        let _ = ready_tx.send(Err(anyhow!("{error:#}")));
                    }
                    tracing::warn!(%error, "timed WASAPI loopback capture ended");
                }
                result
            })
            .map_err(|error| anyhow!("could not spawn the audio capture worker: {error}"))?;
        // Create the owner before awaiting readiness so cancellation during
        // startup still signals the worker (and its COM cleanup) to stop.
        let timed = Self {
            stop,
            thread: Some(thread),
        };
        let ready = tokio::time::timeout(STARTUP_TIMEOUT, ready_rx)
            .await
            .map_err(|_| {
                anyhow!(
                    "timed audio capture did not start within {} s",
                    STARTUP_TIMEOUT.as_secs()
                )
            })?
            .map_err(|_| anyhow!("audio capture worker exited during startup"))?;
        ready?;
        Ok((timed, rx))
    }

    /// Stop the worker and report capture errors that happened after startup.
    ///
    /// The wait is bounded; a device driver that never returns from WASAPI
    /// produces a diagnostic instead of hanging the caller.
    pub async fn stop(mut self) -> anyhow::Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        tokio::time::timeout(
            STOP_TIMEOUT,
            tokio::task::spawn_blocking(move || thread.join()),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "audio capture worker did not stop within {} s; the audio device may be stuck",
                STOP_TIMEOUT.as_secs()
            )
        })?
        .map_err(|error| anyhow!("could not join the audio capture worker: {error}"))?
        .map_err(|_| anyhow!("audio capture worker panicked"))?
    }
}

#[cfg(target_os = "windows")]
impl Drop for TimedLoopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(target_os = "windows")]
struct ComApartment;

#[cfg(target_os = "windows")]
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

#[cfg(target_os = "windows")]
struct MixFormat(*mut WAVEFORMATEX);

#[cfg(target_os = "windows")]
impl Drop for MixFormat {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

#[cfg(target_os = "windows")]
struct StartedClient(IAudioClient);

#[cfg(target_os = "windows")]
impl Drop for StartedClient {
    fn drop(&mut self) {
        if let Err(error) = unsafe { self.0.Stop() } {
            tracing::warn!(%error, "could not stop WASAPI audio client");
        }
    }
}

/// Releases a WASAPI capture buffer on every path, including `?` and panics.
#[cfg(target_os = "windows")]
struct BufferGuard<'a> {
    capture: &'a IAudioCaptureClient,
    frames: u32,
    released: bool,
}

#[cfg(target_os = "windows")]
impl<'a> BufferGuard<'a> {
    fn new(capture: &'a IAudioCaptureClient, frames: u32) -> Self {
        Self {
            capture,
            frames,
            released: false,
        }
    }

    fn release(&mut self) -> anyhow::Result<()> {
        if self.released {
            return Ok(());
        }
        // Mark before the call so a failed release is never retried by Drop.
        self.released = true;
        unsafe { self.capture.ReleaseBuffer(self.frames) }
            .map_err(|error| anyhow!("could not release audio packet: {error}"))
    }
}

#[cfg(target_os = "windows")]
impl Drop for BufferGuard<'_> {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if let Err(error) = unsafe { self.capture.ReleaseBuffer(self.frames) } {
            tracing::warn!(%error, "could not release WASAPI audio packet");
        }
    }
}

/// QPC *measurement* of a packet's first sample: its timestamp converted to
/// the 44.1 kHz timeline plus the resampler phase carried in from the previous
/// packet.
///
/// This is not the per-packet position handed downstream: measurements jitter
/// with the QPC timestamps WASAPI reports, so [`PacketTimeline`] only uses the
/// first packet of a continuous stream as an anchor and carries the actual
/// converted length forward. The measurement is then checked against the
/// carried position as a clock watchdog. Callers must invoke this before
/// pushing the packet's frames, while the resampler phase still describes the
/// boundary.
#[cfg(target_os = "windows")]
fn packet_start_frame(
    clock: &SessionClock,
    resampler: &Resampler,
    input_rate: f64,
    qpc_position: u64,
) -> anyhow::Result<i64> {
    let phase_output_frames = resampler.next_output_offset_frames() * (44_100.0 / input_rate);
    clock
        .sample_index_for_qpc(qpc_position)?
        .checked_add(phase_output_frames.round() as i64)
        .ok_or_else(|| anyhow!("audio timestamp overflowed the sample timeline"))
}

/// Hard limit on `|QPC measurement - carried timeline position|` before the
/// capture is declared broken: 100 ms at 44.1 kHz.
///
/// WASAPI QPC timestamps are jittery (driver buffering moves them by tens of
/// microseconds), so the carried timeline is authoritative for packet starts
/// and the measurement only anchors and watches it. A driver clock that
/// genuinely runs at a different rate than the QPC still drifts past this
/// within a session, and then the capture fails loudly instead of letting the
/// A/V offset grow without bound. This patch deliberately does not estimate a
/// sample-rate offset or adapt the resampling ratio, so a bounded offset is
/// tolerated but a persistent mismatch ends the session.
#[cfg(target_os = "windows")]
const MAX_CLOCK_DRIFT_FRAMES: i64 = 4_410;

/// Stateful 44.1 kHz timeline for converted packets.
///
/// The first packet that emits converted frames is anchored by its QPC
/// measurement ([`packet_start_frame`]). Every following packet of the same
/// continuous device stream starts exactly one frame after the previous
/// packet's last converted sample, using the resampler's actual output length,
/// so jittery QPC timestamps cannot insert or drop samples. A bounded capture
/// queue may drop a packet without tearing the timeline: the carried position
/// advances regardless and the next packet starts after the true hole. Packets
/// that emit no converted frames leave the carried position untouched. The
/// timeline is reset only when the worker detects a device discontinuity,
/// together with the resampler, and then re-anchors at the new measurement.
#[cfg(target_os = "windows")]
struct PacketTimeline {
    /// Start frame of the next converted sample of the current continuous
    /// device stream, once anchoring has happened.
    carried: Option<i64>,
}

#[cfg(target_os = "windows")]
impl PacketTimeline {
    fn new() -> Self {
        Self { carried: None }
    }

    /// Position one converted packet on the timeline.
    ///
    /// `measured_start` is the packet's QPC measurement from
    /// [`packet_start_frame`] and `emitted_frames` the number of 44.1 kHz
    /// stereo frames the resampler actually produced for it. `discontinuity`
    /// must be true when the worker observed a WASAPI data discontinuity, a
    /// device-position mismatch, or any other resampler reset: the timeline
    /// then re-anchors at `measured_start`. Otherwise the carried position
    /// wins; a measurement more than [`MAX_CLOCK_DRIFT_FRAMES`] away fails
    /// explicitly with an audio-clock diagnostic instead of silently shifting
    /// samples.
    fn advance(
        &mut self,
        measured_start: i64,
        discontinuity: bool,
        emitted_frames: usize,
    ) -> anyhow::Result<i64> {
        if discontinuity {
            self.carried = None;
        }
        let start = match self.carried {
            Some(carried) => {
                let Some(drift) = measured_start.checked_sub(carried) else {
                    return Err(clock_drift_error(measured_start, carried, None));
                };
                if !(-MAX_CLOCK_DRIFT_FRAMES..=MAX_CLOCK_DRIFT_FRAMES).contains(&drift) {
                    return Err(clock_drift_error(measured_start, carried, Some(drift)));
                }
                carried
            }
            None => measured_start,
        };
        if emitted_frames > 0 {
            let emitted = i64::try_from(emitted_frames)
                .map_err(|_| anyhow!("converted audio packet frame count is out of range"))?;
            self.carried = Some(
                start
                    .checked_add(emitted)
                    .ok_or_else(|| anyhow!("audio packet start overflowed the sample timeline"))?,
            );
        }
        Ok(start)
    }
}

/// Failure for a QPC measurement that no longer matches the carried timeline.
/// The caller surfaces this as a session error so the UI can offer a reconnect;
/// the worker never compensates by inserting or dropping samples.
#[cfg(target_os = "windows")]
fn clock_drift_error(measured_start: i64, carried: i64, drift: Option<i64>) -> anyhow::Error {
    let drift = match drift {
        Some(drift) => format!("{drift} frames"),
        None => "an out-of-range number of frames".to_string(),
    };
    anyhow!(
        "audio clock mismatch: the QPC measurement (frame {measured_start}) is {drift} from the carried capture \
         timeline (frame {carried}), beyond the ±{MAX_CLOCK_DRIFT_FRAMES}-frame (100 ms) limit; reconnect the \
         session and check the audio clock"
    )
}

#[cfg(target_os = "windows")]
fn run_timed_loopback(
    tx: tokio::sync::mpsc::Sender<TimedPcm>,
    stop: Arc<AtomicBool>,
    clock: SessionClock,
    on_started: impl FnOnce(),
) -> anyhow::Result<()> {
    // KSDATAFORMAT_SUBTYPE_PCM from the Windows SDK (ksmedia.h).
    const PCM_SUBFORMAT: windows::core::GUID =
        windows::core::GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);

    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|error| anyhow!("could not initialize COM for audio capture: {error}"))?;
        let _apartment = ComApartment;

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|error| {
                anyhow!("could not create the audio device enumerator: {error}")
            })?;
        // Cast mirrors the default system output, never a microphone.
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|error| anyhow!("could not open the default audio output device: {error}"))?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|error| {
            anyhow!("could not activate WASAPI on the default audio output device: {error}")
        })?;
        let mix = client
            .GetMixFormat()
            .map_err(|error| anyhow!("could not read the WASAPI mix format: {error}"))?;
        if mix.is_null() {
            anyhow::bail!("WASAPI returned no mix format");
        }
        let _mix_format = MixFormat(mix);
        let fmt: WAVEFORMATEX = std::ptr::read_unaligned(mix);
        let format_tag = fmt.wFormatTag;
        let sample_rate = fmt.nSamplesPerSec;
        let channel_count = fmt.nChannels;
        let input_rate = f64::from(sample_rate);
        let channels = channel_count as usize;
        let block_align = fmt.nBlockAlign as usize;
        let bits = fmt.wBitsPerSample;
        let is_float = if format_tag == 3 {
            true
        } else if format_tag == 1 {
            false
        } else if format_tag == 0xFFFE {
            if fmt.cbSize < 22 {
                anyhow::bail!("WASAPI returned a truncated extensible mix format");
            }
            let ext: WAVEFORMATEXTENSIBLE =
                std::ptr::read_unaligned(mix as *const WAVEFORMATEXTENSIBLE);
            // Copy out of the packed struct before comparing: taking a
            // reference to a packed field is undefined behavior.
            let sub_format = ext.SubFormat;
            if sub_format != KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && sub_format != PCM_SUBFORMAT {
                anyhow::bail!("unsupported WASAPI extensible sample format");
            }
            sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            anyhow::bail!("unsupported WASAPI format tag: {format_tag}");
        };
        // Reject implausible mix formats before the capture loop and before
        // any packet-sized allocation, so the per-packet caps below stay
        // meaningful.
        if !(MIN_INPUT_RATE..=MAX_INPUT_RATE).contains(&sample_rate) {
            anyhow::bail!(
                "unsupported WASAPI mix rate: {sample_rate} Hz (supported: {MIN_INPUT_RATE}..={MAX_INPUT_RATE})"
            );
        }
        if channel_count > MAX_INPUT_CHANNELS {
            anyhow::bail!(
                "unsupported WASAPI channel count: {channel_count} (maximum {MAX_INPUT_CHANNELS})"
            );
        }
        validate_format(sample_rate, channel_count, bits, fmt.nBlockAlign, is_float)
            .map_err(anyhow::Error::msg)?;
        // Numeric format only: never log endpoint or stream names here.
        tracing::debug!(
            sample_rate,
            channels = channel_count,
            bits,
            float = is_float,
            "timed WASAPI loopback format"
        );

        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                2_000_000, // 200 ms buffer (100-ns units)
                0,
                mix,
                None,
            )
            .map_err(|error| anyhow!("could not initialize WASAPI loopback capture: {error}"))?;
        let max_frames = client
            .GetBufferSize()
            .map_err(|error| anyhow!("could not read the WASAPI buffer size: {error}"))?;
        if max_frames == 0 {
            anyhow::bail!("WASAPI reported an empty capture buffer");
        }
        let capture: IAudioCaptureClient = client
            .GetService()
            .map_err(|error| anyhow!("could not get the WASAPI capture client: {error}"))?;
        client
            .Start()
            .map_err(|error| anyhow!("could not start WASAPI loopback capture: {error}"))?;
        let _started_client = StartedClient(client);
        on_started();

        let mut resampler = Resampler::new(input_rate);
        let mut timeline = PacketTimeline::new();
        let mut out: Vec<u8> = Vec::new();
        let mut expected_device_position: Option<u64> = None;

        while !stop.load(Ordering::Relaxed) && !tx.is_closed() {
            let packet = capture
                .GetNextPacketSize()
                .map_err(|error| anyhow!("could not query audio packet: {error}"))?;
            if packet == 0 {
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }

            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut device_position = 0u64;
            let mut qpc_position = 0u64;
            capture
                .GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    Some(&mut device_position),
                    Some(&mut qpc_position),
                )
                .map_err(|error| anyhow!("could not acquire audio packet: {error}"))?;
            let mut buffer = BufferGuard::new(&capture, frames);

            // A broken timestamp must fail loudly: inventing one would shift
            // the receiver's whole audio timeline.
            if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0 {
                anyhow::bail!(
                    "WASAPI reported a timestamp error at device position {device_position}; refusing to guess an audio timeline"
                );
            }
            if frames == 0 {
                buffer.release()?;
                continue;
            }
            let max_native_frames = (sample_rate / 2).min(MAX_NATIVE_PACKET_FRAMES);
            if frames > max_frames || frames > max_native_frames {
                anyhow::bail!(
                    "WASAPI delivered {frames} frames, above the supported limits ({max_frames} device buffer, {max_native_frames} half-second cap)"
                );
            }
            let byte_len = (frames as usize)
                .checked_mul(block_align)
                .ok_or_else(|| anyhow!("audio packet length overflowed"))?;

            let discontinuity = flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0
                || expected_device_position.is_some_and(|expected| expected != device_position);
            if discontinuity {
                // Do not interpolate across a real gap in captured audio, and
                // do not carry the old timeline across it either.
                resampler = Resampler::new(input_rate);
                tracing::warn!(
                    device_position,
                    "WASAPI audio discontinuity; re-anchoring the resampler and audio timeline"
                );
            }
            expected_device_position = device_position.checked_add(u64::from(frames));

            // QPC measurement of this packet's first sample: timestamp plus the
            // resampler phase carried in from the previous packet. The timeline
            // uses it only to anchor a new continuous stream and to watch the
            // clock; contiguous packets are positioned by the carried converted
            // length, so QPC jitter or a queue drop cannot tear the stream.
            let measured_start = packet_start_frame(&clock, &resampler, input_rate, qpc_position)?;

            let converted_frames = f64::from(frames) * 44_100.0 / input_rate;
            if !converted_frames.is_finite() || converted_frames > MAX_OUTPUT_PACKET_FRAMES as f64 {
                anyhow::bail!(
                    "audio packet would convert to {converted_frames} 44.1 kHz frames, above the {MAX_OUTPUT_PACKET_FRAMES}-frame cap"
                );
            }
            out.clear();
            out.reserve((converted_frames.ceil() as usize + 2) * 4);

            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            if silent || data.is_null() {
                for _ in 0..frames {
                    resampler.push([0.0, 0.0], &mut out);
                }
            } else {
                let slice = std::slice::from_raw_parts(data, byte_len);
                for frame in 0..frames as usize {
                    let raw = &slice[frame * block_align..(frame + 1) * block_align];
                    let (left, right) = decode_frame(raw, channels, bits, is_float);
                    resampler.push([left, right], &mut out);
                }
            }
            buffer.release()?;

            // Position the packet only after conversion, so the timeline
            // advances by the frames actually emitted. This runs before the
            // bounded queue send below: a drop must leave a true hole, not
            // rewind or stretch the stream. Zero-output packets leave the
            // carried anchor untouched.
            let start_frame = timeline.advance(measured_start, discontinuity, out.len() / 4)?;
            if out.is_empty() {
                continue;
            }
            if out.len() > MAX_OUTPUT_PACKET_FRAMES * 4 {
                anyhow::bail!(
                    "resampled audio packet exceeded the {MAX_OUTPUT_PACKET_FRAMES}-frame cap"
                );
            }
            let pcm = TimedPcm {
                start_frame,
                data: std::mem::take(&mut out),
            };
            match tx.try_send(pcm) {
                Ok(()) => {}
                // Dropping a packet leaves a true hole: the timeline already
                // advanced by its converted frames, so later packets keep the
                // absolute start instead of compressing time.
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn synthetic_clock(origin_100ns: i128) -> SessionClock {
        SessionClock {
            origin: Instant::now(),
            qpc: Some(QpcAnchor { origin_100ns }),
        }
    }

    #[test]
    fn qpc_mapping_floors_to_the_containing_frame() {
        let clock = synthetic_clock(0);
        assert_eq!(clock.sample_index_for_qpc(0).unwrap(), 0);
        assert_eq!(clock.sample_index_for_qpc(1).unwrap(), 0);
        assert_eq!(clock.sample_index_for_qpc(226).unwrap(), 0);
        assert_eq!(clock.sample_index_for_qpc(227).unwrap(), 1);
        assert_eq!(clock.sample_index_for_qpc(10_000_000).unwrap(), 44_100);
        assert_eq!(clock.sample_index_for_qpc(20_000_000).unwrap(), 88_200);
    }

    #[test]
    fn qpc_mapping_allows_negative_startup_frames() {
        let clock = synthetic_clock(10_000_000);
        assert_eq!(clock.sample_index_for_qpc(10_000_001).unwrap(), 0);
        assert_eq!(clock.sample_index_for_qpc(10_000_000).unwrap(), 0);
        assert_eq!(clock.sample_index_for_qpc(9_999_999).unwrap(), -1);
        assert_eq!(clock.sample_index_for_qpc(9_999_774).unwrap(), -1);
        assert_eq!(clock.sample_index_for_qpc(9_999_773).unwrap(), -2);
        assert_eq!(clock.sample_index_for_qpc(9_000_000).unwrap(), -4_410);
    }

    #[test]
    fn qpc_mapping_reports_uncalibrated_and_out_of_range_timestamps() {
        let uncalibrated = SessionClock {
            origin: Instant::now(),
            qpc: None,
        };
        assert!(uncalibrated.sample_index_for_qpc(0).is_err());
        // A synthetic far-future origin must fail instead of wrapping i64.
        let clock = synthetic_clock(i128::from(u64::MAX) * 1_000_000);
        assert!(clock.sample_index_for_qpc(0).is_err());
    }

    #[test]
    fn elapsed_us_is_monotonic_and_shared_by_clones() {
        let clock = SessionClock::new().unwrap();
        let clone = clock.clone();
        let before = clock.elapsed_us();
        std::thread::sleep(Duration::from_millis(2));
        let after = clone.elapsed_us();
        assert!(after >= before + 1_000, "{before} -> {after}");
    }

    #[test]
    fn calibration_midpoint_aligns_instant_and_qpc_origins() {
        let before = Instant::now();
        let after = before + Duration::from_millis(10);
        let (origin, origin_100ns) =
            calibration_from_bracket(before, 1_000_000, after, 10_000_000).unwrap();
        assert_eq!(origin, before + Duration::from_millis(5));
        assert!(origin >= before && origin <= after);
        assert_eq!(origin_100ns, 1_000_000);
    }

    #[test]
    fn qpc_counter_conversion_uses_checked_integer_math() {
        assert_eq!(
            qpc_counter_to_100ns(1_000_000, 10_000_000).unwrap(),
            1_000_000
        );
        assert_eq!(
            qpc_counter_to_100ns(3_579_545, 3_579_545).unwrap(),
            10_000_000
        );
        assert_eq!(qpc_counter_to_100ns(1, 3).unwrap(), 3_333_333);
        assert!(qpc_counter_to_100ns(1, 0).is_err());
        assert!(qpc_counter_to_100ns(1, -1).is_err());
        assert!(qpc_counter_to_100ns(-1, 10_000_000).is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn clock_origin_matches_a_fresh_qpc_reading_within_a_few_ms() {
        use windows::Win32::System::Performance::{
            QueryPerformanceCounter, QueryPerformanceFrequency,
        };

        let clock = SessionClock::new().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let before_us = clock.elapsed_us();
        let mut counter = 0i64;
        unsafe { QueryPerformanceCounter(&mut counter) }.unwrap();
        let after_us = clock.elapsed_us();
        let mut frequency = 0i64;
        unsafe { QueryPerformanceFrequency(&mut frequency) }.unwrap();

        let qpc_100ns = qpc_counter_to_100ns(counter, frequency).unwrap();
        let frame = clock
            .sample_index_for_qpc(u64::try_from(qpc_100ns).unwrap())
            .unwrap();
        let mapped_us = u64::try_from(frame.max(0)).unwrap() * 1_000_000 / 44_100;

        // The QPC reading happened between the two `elapsed_us` samples; a few
        // milliseconds of slack cover frame quantization and scheduler noise.
        assert!(
            mapped_us >= 40_000,
            "50 ms sleep only mapped to {mapped_us} us"
        );
        assert!(
            mapped_us + 10_000 >= before_us,
            "{before_us} -> {mapped_us}"
        );
        assert!(mapped_us <= after_us + 10_000, "{after_us} -> {mapped_us}");
    }

    /// Run one simulated 44.1 kHz packet exactly like the worker: timestamp it
    /// with [`packet_start_frame`] before pushing, then convert `packet_frames`
    /// native frames.
    #[cfg(target_os = "windows")]
    fn push_timed_packet(
        clock: &SessionClock,
        resampler: &mut Resampler,
        input_rate: u32,
        native_frame: &mut usize,
        packet_frames: usize,
    ) -> (i64, i64) {
        let qpc = *native_frame as u64 * 10_000_000 / u64::from(input_rate);
        let start = packet_start_frame(clock, resampler, f64::from(input_rate), qpc).unwrap();
        let mut out = Vec::new();
        for _ in 0..packet_frames {
            resampler.push([0.0, 0.0], &mut out);
        }
        *native_frame += packet_frames;
        (start, (out.len() / 4) as i64)
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn packet_timestamps_stay_continuous_across_many_packets() {
        for input_rate in [48_000u32, 44_100] {
            let packet_frames = input_rate as usize / 100; // 10 ms of input
            let clock = synthetic_clock(0);
            let mut resampler = Resampler::new(f64::from(input_rate));
            let mut native_frame = 0usize;
            let mut first_start = 0i64;
            let mut emitted = 0i64;
            for packet in 0..600 {
                let (start, frames) = push_timed_packet(
                    &clock,
                    &mut resampler,
                    input_rate,
                    &mut native_frame,
                    packet_frames,
                );
                if packet == 0 {
                    first_start = start;
                }
                // No accumulating offset: the packet starts where the previous
                // packet's output frames ended, within one frame of rounding.
                let exact = first_start + emitted;
                assert!(
                    (start - exact).abs() <= 1,
                    "rate {input_rate} packet {packet}: start {start}, expected {exact}"
                );
                emitted += frames;
            }
            assert!(emitted > 5_000, "rate {input_rate}: emitted {emitted}");
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn resampler_reset_after_gap_does_not_accumulate_timestamp_offset() {
        const INPUT_RATE: u32 = 48_000;
        const PACKET_FRAMES: usize = 480;
        let clock = synthetic_clock(0);
        let mut resampler = Resampler::new(f64::from(INPUT_RATE));
        let mut native_frame = 0usize;
        for _ in 0..10 {
            let _ = push_timed_packet(
                &clock,
                &mut resampler,
                INPUT_RATE,
                &mut native_frame,
                PACKET_FRAMES,
            );
        }

        // A device gap makes the worker drop the carried phase: the next packet
        // starts from a fresh resampler while its QPC timestamp jumps forward
        // with the skipped native frames.
        native_frame += INPUT_RATE as usize / 2;
        resampler = Resampler::new(f64::from(INPUT_RATE));
        let qpc = native_frame as u64 * 10_000_000 / u64::from(INPUT_RATE);
        let expected_first = clock.sample_index_for_qpc(qpc).unwrap();

        let mut first_start: Option<i64> = None;
        let mut emitted = 0i64;
        for packet in 0..120 {
            let (start, frames) = push_timed_packet(
                &clock,
                &mut resampler,
                INPUT_RATE,
                &mut native_frame,
                PACKET_FRAMES,
            );
            let first = *first_start.get_or_insert(start);
            if packet == 0 {
                // A fresh resampler carries no phase, so the first post-gap
                // packet starts exactly at its QPC frame.
                assert_eq!(start, expected_first);
            }
            let exact = first + emitted;
            assert!(
                (start - exact).abs() <= 1,
                "post-gap packet {packet}: start {start}, expected {exact}"
            );
            emitted += frames;
        }
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    #[ignore = "requires a real Windows audio endpoint; does not mute it or record audio"]
    async fn real_loopback_timed_smoke() {
        let clock = SessionClock::new().unwrap();
        let (capture, mut rx) = TimedLoopback::start(clock).await.unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        capture.stop().await.unwrap();
        // A real endpoint may legitimately be silent, so only validate a packet
        // when one arrives; the endpoint's mute state is never touched.
        if let Ok(Some(TimedPcm { data, .. })) = packet {
            assert_eq!(data.len() % 4, 0);
        }
    }

    /// One simulated device packet, recorded after the worker-style timestamp
    /// and conversion steps.
    #[cfg(target_os = "windows")]
    #[derive(Debug, Clone, Copy)]
    struct SimulatedPacket {
        native_frame: u64,
        qpc: u64,
        qpc_frame: i64,
        phase_output_frames: f64,
        measured_start: i64,
        start_frame: i64,
        frames: usize,
    }

    /// Drives the worker's packet path without hardware or playback: contiguous
    /// native device frames go through the real [`Resampler`], each packet is
    /// timestamped with [`packet_start_frame`] before its frames are pushed, and
    /// the helper retains both the concatenated converted waveform (the ground
    /// truth for a lossless stitch) and every reported start frame.
    #[cfg(target_os = "windows")]
    struct SimulatedCapture {
        clock: SessionClock,
        resampler: Resampler,
        timeline: PacketTimeline,
        input_rate: u32,
        qpc_base_100ns: u64,
        next_native_frame: u64,
        reference: Vec<u8>,
        packets: Vec<SimulatedPacket>,
    }

    /// Nonzero, non-frame-aligned session anchor: 3_141_592 hundred-ns ticks
    /// after the QPC origin. The capture starts 500 ms + 57 ticks later, so the
    /// first packet maps to output frame 22_050.25137..., a nonzero fractional
    /// session anchor for the packet timeline (never a multiple of one frame).
    #[cfg(target_os = "windows")]
    const SIMULATED_ORIGIN_100NS: i128 = 3_141_592;
    #[cfg(target_os = "windows")]
    const SIMULATED_FIRST_PACKET_100NS: i128 = 5_000_057;
    /// Deterministic but irregular native packet lengths, like a real WASAPI
    /// loopback stream.
    #[cfg(target_os = "windows")]
    const SIMULATED_PACKET_LENGTHS_US: [u64; 8] =
        [10_000, 3_000, 20_000, 5_000, 7_500, 13_000, 4_000, 16_000];

    #[cfg(target_os = "windows")]
    fn simulated_packet_frames(input_rate: u32, packet: usize) -> usize {
        let micros = SIMULATED_PACKET_LENGTHS_US[packet % SIMULATED_PACKET_LENGTHS_US.len()];
        (u64::from(input_rate) * micros / 1_000_000).max(2) as usize
    }

    /// Deterministic stereo input; identities do not matter unless a start
    /// frame places the packet at the wrong offset in the reference waveform.
    #[cfg(target_os = "windows")]
    fn simulated_wave(native_frame: u64) -> [f32; 2] {
        let left = (native_frame % 500) as f32 / 499.0 - 0.5;
        let right = (native_frame % 251) as f32 / 250.0 - 0.5;
        [left, right]
    }

    #[cfg(target_os = "windows")]
    impl SimulatedCapture {
        fn new(input_rate: u32) -> Self {
            let qpc_base_100ns =
                u64::try_from(SIMULATED_ORIGIN_100NS + SIMULATED_FIRST_PACKET_100NS).unwrap();
            Self {
                clock: synthetic_clock(SIMULATED_ORIGIN_100NS),
                resampler: Resampler::new(f64::from(input_rate)),
                timeline: PacketTimeline::new(),
                input_rate,
                qpc_base_100ns,
                next_native_frame: 0,
                reference: Vec::new(),
                packets: Vec::new(),
            }
        }

        /// Convert `frames` contiguous native frames exactly like the worker:
        /// measure the packet from its 100 ns-quantized QPC while the resampler
        /// phase still describes the boundary, push its frames, then position
        /// it on the shared production [`PacketTimeline`] from the frames the
        /// resampler actually emitted.
        fn push_packet(&mut self, frames: usize, jitter_100ns: i64) {
            self.push_packet_with_discontinuity(frames, jitter_100ns, false);
        }

        /// `discontinuity` mirrors the worker's WASAPI data-discontinuity or
        /// device-position-mismatch detection: the resampler and the timeline
        /// both reset, so the packet re-anchors at its own measurement.
        fn push_packet_with_discontinuity(
            &mut self,
            frames: usize,
            jitter_100ns: i64,
            discontinuity: bool,
        ) {
            if discontinuity {
                self.resampler = Resampler::new(f64::from(self.input_rate));
            }
            let native_frame = self.next_native_frame;
            let qpc = u64::try_from(
                i128::from(self.qpc_base_100ns)
                    + i128::from(native_frame * 10_000_000 / u64::from(self.input_rate))
                    + i128::from(jitter_100ns),
            )
            .expect("simulated QPC must stay positive");
            let qpc_frame = self.clock.sample_index_for_qpc(qpc).unwrap();
            let phase_output_frames = self.resampler.next_output_offset_frames()
                * (44_100.0 / f64::from(self.input_rate));
            let measured_start = packet_start_frame(
                &self.clock,
                &self.resampler,
                f64::from(self.input_rate),
                qpc,
            )
            .unwrap();

            let mut out = Vec::new();
            for offset in 0..frames as u64 {
                self.resampler
                    .push(simulated_wave(native_frame + offset), &mut out);
            }
            let emitted = out.len() / 4;
            let start_frame = self
                .timeline
                .advance(measured_start, discontinuity, emitted)
                .expect("simulated timeline must accept the packet");
            self.packets.push(SimulatedPacket {
                native_frame,
                qpc,
                qpc_frame,
                phase_output_frames,
                measured_start,
                start_frame,
                frames: emitted,
            });
            self.reference.extend_from_slice(&out);
            self.next_native_frame += frames as u64;
        }

        /// Advance the device frame counter without capturing anything, as a
        /// real WASAPI hole does: the next packet's QPC jumps with the device
        /// clock while no converted frames exist for the skipped interval.
        fn skip_native_frames(&mut self, frames: u64) {
            self.next_native_frame += frames;
        }
    }

    /// Rebuilds the stream the way `PcmTimeline` consumes it: each packet is
    /// placed at its reported start frame relative to the first packet, missing
    /// regions become silence, and an overlapping prefix is trimmed (earlier
    /// packet wins). `dropped` simulates one packet the bounded capture queue
    /// rejected. A lossless result must equal the contiguous reference.
    #[cfg(target_os = "windows")]
    fn stitch_packets(capture: &SimulatedCapture, dropped: Option<usize>) -> Vec<u8> {
        let base = capture.packets[0].start_frame;
        let mut stitched: Vec<u8> = Vec::new();
        let mut reference_offset = 0usize;
        for (index, packet) in capture.packets.iter().enumerate() {
            let end = reference_offset + packet.frames * 4;
            let data = &capture.reference[reference_offset..end];
            reference_offset = end;
            if dropped == Some(index) {
                continue;
            }

            let start = usize::try_from(packet.start_frame - base).unwrap();
            let cursor = stitched.len() / 4;
            if start > cursor {
                stitched.resize(start * 4, 0);
                stitched.extend_from_slice(data);
            } else {
                let skip = cursor - start;
                if skip < packet.frames {
                    stitched.extend_from_slice(&data[skip * 4..]);
                }
            }
        }
        stitched
    }

    /// Strict continuity check: every packet must start exactly where the
    /// previous packet's converted samples end, otherwise the stitcher drops or
    /// zero-fills samples. Prints reproducible counts and the first failure
    /// (values only, no audio) under `--nocapture` and returns the summary for
    /// the caller's exact assertion.
    #[cfg(target_os = "windows")]
    fn check_packets_contiguous(
        input_rate: u32,
        capture: &SimulatedCapture,
        scenario: &str,
    ) -> Result<(), String> {
        let packet_count = capture.packets.len();
        assert!(
            packet_count >= 2,
            "need at least two packets to measure continuity"
        );

        let mut mismatches = 0usize;
        let mut first_failure: Option<(usize, i64)> = None;
        for index in 1..packet_count {
            let previous = capture.packets[index - 1];
            let previous_end = previous.start_frame + previous.frames as i64;
            let delta = capture.packets[index].start_frame - previous_end;
            if delta != 0 {
                mismatches += 1;
                if first_failure.is_none() {
                    first_failure = Some((index, delta));
                }
            }
        }

        let reference_frames = capture.reference.len() / 4;
        let converted_frames: usize = capture.packets.iter().map(|packet| packet.frames).sum();
        assert_eq!(
            reference_frames, converted_frames,
            "simulation bookkeeping must match the retained reference waveform"
        );
        println!(
            "rate {input_rate} ({scenario}): {packet_count} packets, {converted_frames} converted \
             frames, {mismatches}/{} boundaries discontinuous",
            packet_count - 1
        );
        let first_detail = match first_failure {
            Some((index, delta)) => {
                let previous = capture.packets[index - 1];
                let current = capture.packets[index];
                let previous_end = previous.start_frame + previous.frames as i64;
                println!(
                    "  first failure: packet {index} native_frame={} qpc={} qpc_frame={} \
                     start_frame={} previous_end={previous_end} delta={delta}",
                    current.native_frame, current.qpc, current.qpc_frame, current.start_frame
                );
                println!(
                    "  previous: start_frame={} frames={} phase_output_frames={:.6}; current \
                     phase_output_frames={:.6}",
                    previous.start_frame,
                    previous.frames,
                    previous.phase_output_frames,
                    current.phase_output_frames
                );
                format!(
                    "first failure at packet {index}: start_frame {} != previous end {previous_end} \
                     (delta {delta}, native_frame {}, qpc {}, qpc_frame {}, phase {:+.6})",
                    current.start_frame,
                    current.native_frame,
                    current.qpc,
                    current.qpc_frame,
                    current.phase_output_frames,
                )
            }
            None => "none".to_string(),
        };
        if mismatches > 0 {
            return Err(format!(
                "rate {input_rate} ({scenario}): {mismatches}/{} boundaries discontinuous; \
                 {first_detail}",
                packet_count - 1
            ));
        }

        assert_eq!(
            stitch_packets(capture, None),
            capture.reference,
            "rate {input_rate} ({scenario}): stitching packets at their reported start frames does \
             not reproduce the contiguous converted waveform"
        );
        Ok(())
    }

    /// Regression: WASAPI QPC timestamps are quantized to 100 ns. Flooring a
    /// packet's QPC to an output frame and rounding the carried resampler phase
    /// independently must not tear the converted stream even though the session
    /// anchor is fractional: adjacent packets have to start exactly where the
    /// previous packet's converted samples end.
    #[cfg(target_os = "windows")]
    #[test]
    fn converted_packets_stay_contiguous_across_variable_length_device_packets() {
        let mut failures = Vec::new();
        for input_rate in [44_100u32, 48_000, 96_000] {
            let mut capture = SimulatedCapture::new(input_rate);
            for packet in 0..256usize {
                capture.push_packet(simulated_packet_frames(input_rate, packet), 0);
            }
            if let Err(failure) =
                check_packets_contiguous(input_rate, &capture, "quantized QPC, no jitter")
            {
                failures.push(failure);
            }
        }
        assert!(
            failures.is_empty(),
            "contiguous device capture must tile converted samples exactly:\n{}",
            failures.join("\n")
        );
    }

    /// Regression: realistic bounded QPC jitter must not shift packet starts,
    /// because the device delivered contiguous native frames. Alternating
    /// +/-100 us is the worst case for a per-packet QPC mapping.
    #[cfg(target_os = "windows")]
    #[test]
    fn alternating_100us_qpc_jitter_keeps_converted_packets_contiguous() {
        const JITTER_100NS: i64 = 1_000; // 100 us
        let mut failures = Vec::new();
        for input_rate in [44_100u32, 48_000, 96_000] {
            let mut capture = SimulatedCapture::new(input_rate);
            for packet in 0..256usize {
                let jitter = if packet % 2 == 0 {
                    JITTER_100NS
                } else {
                    -JITTER_100NS
                };
                capture.push_packet(simulated_packet_frames(input_rate, packet), jitter);
            }
            if let Err(failure) =
                check_packets_contiguous(input_rate, &capture, "alternating +/-100us QPC jitter")
            {
                failures.push(failure);
            }
        }
        assert!(
            failures.is_empty(),
            "contiguous device capture must tile converted samples exactly:\n{}",
            failures.join("\n")
        );
    }

    /// The timeline anchors on the first measured packet, then carries the
    /// actual converted length: measurement jitter cannot move later starts and
    /// a packet that converts nothing does not consume the anchor.
    #[cfg(target_os = "windows")]
    #[test]
    fn packet_timeline_carries_converted_frames_and_preserves_zero_output_anchors() {
        let mut timeline = PacketTimeline::new();
        assert_eq!(timeline.advance(1_000, false, 441).unwrap(), 1_000);
        // A contiguous packet starts at the carried position although its QPC
        // measurement moved by a frame.
        assert_eq!(timeline.advance(1_442, false, 440).unwrap(), 1_441);
        // Zero converted frames leave the carried anchor untouched.
        assert_eq!(timeline.advance(1_881, false, 0).unwrap(), 1_881);
        assert_eq!(timeline.advance(1_883, false, 441).unwrap(), 1_881);
        assert_eq!(timeline.advance(2_322, false, 100).unwrap(), 2_322);

        // Before any anchor exists a zero-output packet establishes nothing:
        // the next packet still anchors at its own measurement.
        let mut fresh = PacketTimeline::new();
        assert_eq!(fresh.advance(1_000, false, 0).unwrap(), 1_000);
        assert_eq!(fresh.advance(7_000, false, 441).unwrap(), 7_000);
    }

    /// Startup audio keeps its signed frame indices.
    #[cfg(target_os = "windows")]
    #[test]
    fn packet_timeline_keeps_signed_startup_frames() {
        let mut timeline = PacketTimeline::new();
        assert_eq!(timeline.advance(-22_050, false, 441).unwrap(), -22_050);
        assert_eq!(timeline.advance(-21_610, false, 441).unwrap(), -21_609);
        assert_eq!(timeline.advance(-21_168, false, 0).unwrap(), -21_168);
    }

    /// Only a detected device discontinuity re-anchors; a large measurement
    /// jump without that signal is reported as a clock mismatch.
    #[cfg(target_os = "windows")]
    #[test]
    fn packet_timeline_reanchors_only_on_discontinuity() {
        let mut timeline = PacketTimeline::new();
        assert_eq!(timeline.advance(0, false, 441).unwrap(), 0);
        // 2 s of measurement jump without the WASAPI signal: refuse.
        assert!(timeline.advance(88_200, false, 441).is_err());
        // The worker saw the discontinuity/device-position mismatch, so the
        // timeline re-anchors and carries on from there.
        assert_eq!(timeline.advance(88_200, true, 441).unwrap(), 88_200);
        assert_eq!(timeline.advance(88_642, false, 441).unwrap(), 88_641);
    }

    /// Exactly 100 ms of measurement offset (4410 frames) is tolerated; one
    /// more frame fails with the reconnect/audio-clock diagnostic.
    #[cfg(target_os = "windows")]
    #[test]
    fn packet_timeline_tolerates_the_100ms_boundary_and_rejects_beyond_it() {
        let mut at_limit = PacketTimeline::new();
        // Anchor at 3_970 with 440 emitted frames: carried becomes 4_410.
        assert_eq!(at_limit.advance(3_970, false, 440).unwrap(), 3_970);
        assert_eq!(at_limit.advance(0, false, 441).unwrap(), 4_410);

        let mut over_limit = PacketTimeline::new();
        assert_eq!(over_limit.advance(3_970, false, 441).unwrap(), 3_970);
        let error = over_limit.advance(0, false, 441).unwrap_err().to_string();
        assert!(error.contains("clock mismatch"), "{error}");
        assert!(error.contains("reconnect"), "{error}");
    }

    /// Timeline and drift arithmetic stay checked instead of wrapping.
    #[cfg(target_os = "windows")]
    #[test]
    fn packet_timeline_rejects_timeline_and_drift_overflow() {
        let mut timeline = PacketTimeline::new();
        let near_max = i64::MAX - 1;
        assert_eq!(timeline.advance(near_max, false, 1).unwrap(), near_max);
        assert!(timeline.advance(i64::MAX, false, 1).is_err());

        let mut extreme = PacketTimeline::new();
        assert_eq!(
            extreme.advance(i64::MAX - 1, false, 1).unwrap(),
            i64::MAX - 1
        );
        assert!(extreme.advance(i64::MIN, false, 0).is_err());
    }

    /// A first packet that converts nothing must not consume the anchor: the
    /// next packet anchors at its own measurement and later packets tile.
    #[cfg(target_os = "windows")]
    #[test]
    fn zero_output_packet_does_not_shift_the_anchor() {
        const INPUT_RATE: u32 = 48_000;
        let mut capture = SimulatedCapture::new(INPUT_RATE);
        // The priming push emits no converted frames for a 1-frame packet.
        capture.push_packet(1, 0);
        assert_eq!(capture.packets[0].frames, 0);
        for packet in 1..64usize {
            capture.push_packet(simulated_packet_frames(INPUT_RATE, packet), 0);
        }
        check_packets_contiguous(INPUT_RATE, &capture, "zero-output anchor packet").unwrap();
    }

    /// A real device gap (WASAPI discontinuity or device-position jump)
    /// re-anchors at the new QPC measurement and keeps the missing interval as
    /// a hole instead of stretching the old timeline over it.
    #[cfg(target_os = "windows")]
    #[test]
    fn discontinuous_device_packet_reanchors_at_its_measurement_and_keeps_the_gap() {
        const INPUT_RATE: u32 = 48_000;
        const GAP_NATIVE_FRAMES: u64 = 24_000; // 500 ms
        let mut capture = SimulatedCapture::new(INPUT_RATE);
        for packet in 0..8usize {
            capture.push_packet(simulated_packet_frames(INPUT_RATE, packet), 0);
        }
        let last = *capture.packets.last().unwrap();
        let previous_end = last.start_frame + last.frames as i64;

        capture.skip_native_frames(GAP_NATIVE_FRAMES);
        capture.push_packet_with_discontinuity(simulated_packet_frames(INPUT_RATE, 8), 0, true);
        let reanchored = *capture.packets.last().unwrap();
        assert_eq!(reanchored.start_frame, reanchored.measured_start);
        // A reset resampler carries no phase, so the measurement is the QPC
        // frame itself.
        assert_eq!(reanchored.measured_start, reanchored.qpc_frame);
        let gap = reanchored.start_frame - previous_end;
        assert!(
            (gap - 22_050).abs() <= 2,
            "500 ms device gap mapped to {gap} output frames"
        );

        // After the re-anchor the new stream tiles exactly again.
        let mut previous = reanchored.start_frame + reanchored.frames as i64;
        for packet in 9..24usize {
            capture.push_packet(simulated_packet_frames(INPUT_RATE, packet), 0);
            let current = *capture.packets.last().unwrap();
            assert_eq!(current.start_frame, previous, "post-gap packet {packet}");
            previous += current.frames as i64;
        }

        // The stitch zero-fills the gap and keeps every captured sample after
        // it exactly where the contiguous reference has them.
        let base = capture.packets[0].start_frame;
        let captured_before_gap = (previous_end - base) as usize * 4;
        let gap_start = (reanchored.start_frame - base) as usize * 4;
        let stitched = stitch_packets(&capture, None);
        assert_eq!(
            &stitched[..captured_before_gap],
            &capture.reference[..captured_before_gap]
        );
        assert!(
            stitched[captured_before_gap..gap_start]
                .iter()
                .all(|&byte| byte == 0),
            "the device gap must stay silent"
        );
        assert_eq!(
            &stitched[gap_start..],
            &capture.reference[captured_before_gap..]
        );
    }

    /// The producer advances the timeline before the bounded queue send, so a
    /// rejected packet leaves exactly its converted frames as a hole and later
    /// packets do not shift earlier.
    #[cfg(target_os = "windows")]
    #[test]
    fn downstream_queue_drop_advances_the_timeline_and_leaves_a_true_hole() {
        const INPUT_RATE: u32 = 48_000;
        const DROPPED: usize = 6;
        let mut capture = SimulatedCapture::new(INPUT_RATE);
        for packet in 0..16usize {
            capture.push_packet(simulated_packet_frames(INPUT_RATE, packet), 0);
        }
        let dropped = capture.packets[DROPPED];
        let next = capture.packets[DROPPED + 1];
        // The timeline advanced by the dropped packet's converted frames even
        // though downstream never saw them.
        assert_eq!(
            next.start_frame,
            dropped.start_frame + dropped.frames as i64
        );
        assert!(dropped.frames > 0, "the dropped packet must emit frames");

        let base = capture.packets[0].start_frame;
        let hole_start = (dropped.start_frame - base) as usize * 4;
        let hole_end = hole_start + dropped.frames * 4;
        let mut expected = capture.reference.clone();
        expected[hole_start..hole_end].fill(0);
        assert_eq!(stitch_packets(&capture, Some(DROPPED)), expected);
    }
}
