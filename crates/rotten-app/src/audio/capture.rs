//! WASAPI loopback capture of the default render endpoint, converted to
//! 44.1 kHz stereo interleaved S16 PCM for the AirPlay audio RTP stream.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::Sender;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, IAudioCaptureClient, IAudioClient,
    IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};

// KSDATAFORMAT_SUBTYPE_PCM from the Windows SDK (ksmedia.h).
const PCM_SUBFORMAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);

struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct MixFormat(*mut WAVEFORMATEX);

impl Drop for MixFormat {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

struct StartedClient(IAudioClient);

impl Drop for StartedClient {
    fn drop(&mut self) {
        if let Err(error) = unsafe { self.0.Stop() } {
            tracing::warn!(%error, "could not stop WASAPI audio client");
        }
    }
}

/// Keep mute ownership separate from the endpoint API so failure and unwind
/// behavior can be tested without modifying the machine's audio settings.
struct MuteGuard<F: FnMut(bool) -> Result<(), String>> {
    originally_muted: bool,
    applied_mute: bool,
    set_mute: F,
}

impl<F: FnMut(bool) -> Result<(), String>> MuteGuard<F> {
    fn new(originally_muted: bool, set_mute: F) -> Self {
        Self {
            originally_muted,
            applied_mute: false,
            set_mute,
        }
    }

    fn apply(&mut self, requested: bool) -> Result<(), String> {
        if requested != self.applied_mute {
            (self.set_mute)(requested || self.originally_muted)?;
            self.applied_mute = requested;
        }
        Ok(())
    }
}

impl<F: FnMut(bool) -> Result<(), String>> Drop for MuteGuard<F> {
    fn drop(&mut self) {
        if let Err(error) = self.apply(false) {
            tracing::warn!(%error, "could not restore audio endpoint mute state");
        }
    }
}

pub(super) fn validate_format(
    rate: u32,
    channels: u16,
    bits: u16,
    block_align: u16,
    is_float: bool,
) -> Result<(), String> {
    let supported_bits = if is_float {
        bits == 32
    } else {
        matches!(bits, 16 | 24 | 32)
    };
    if rate == 0
        || channels == 0
        || !supported_bits
        || usize::from(block_align) < usize::from(channels) * usize::from(bits / 8)
    {
        return Err(format!(
            "unsupported WASAPI mix format: {rate} Hz, {channels} channels, {bits} bits, block alignment {block_align}"
        ));
    }
    Ok(())
}

pub fn run_loopback(
    tx: Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    mute_request: Arc<AtomicBool>,
    on_started: impl FnOnce(),
) -> Result<(), String> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|e| e.to_string())?;
        let _apartment = ComApartment;

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| e.to_string())?;
        let endpoint_volume = device
            .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
            .and_then(|volume| volume.GetMute().map(|muted| (volume, muted.as_bool())));
        let mut endpoint_mute = match endpoint_volume {
            Ok((volume, originally_muted)) => Some(MuteGuard::new(originally_muted, move |mute| {
                volume
                    .SetMute(mute, std::ptr::null())
                    .map_err(|e| e.to_string())
            })),
            Err(error) => {
                tracing::warn!(%error, "local audio mute control unavailable");
                None
            }
        };
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| e.to_string())?;
        let mix = client.GetMixFormat().map_err(|e| e.to_string())?;
        if mix.is_null() {
            return Err("WASAPI returned no mix format".into());
        }
        let _mix_format = MixFormat(mix);
        let fmt: WAVEFORMATEX = std::ptr::read_unaligned(mix);
        let n_samples = fmt.nSamplesPerSec;
        let n_channels = fmt.nChannels;
        let block_align = fmt.nBlockAlign as usize;
        let bits = fmt.wBitsPerSample;
        let format_tag = fmt.wFormatTag;

        let input_rate = f64::from(n_samples);
        let channels = n_channels as usize;
        let is_float = if format_tag == 3 {
            true
        } else if format_tag == 1 {
            false
        } else if format_tag == 0xFFFE {
            if fmt.cbSize < 22 {
                return Err("WASAPI returned a truncated extensible mix format".into());
            }
            let ext: WAVEFORMATEXTENSIBLE =
                std::ptr::read_unaligned(mix as *const WAVEFORMATEXTENSIBLE);
            let sub = ext.SubFormat;
            if sub != KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && sub != PCM_SUBFORMAT {
                return Err("unsupported WASAPI extensible sample format".into());
            }
            sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            return Err(format!("unsupported WASAPI format tag: {format_tag}"));
        };
        validate_format(n_samples, n_channels, bits, fmt.nBlockAlign, is_float)?;

        eprintln!(
            "[audio] WASAPI mix format: {} Hz, {} ch, {}-bit, {}",
            n_samples,
            n_channels,
            bits,
            if is_float { "float" } else { "pcm" }
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
            .map_err(|e| e.to_string())?;
        let capture: IAudioCaptureClient = client.GetService().map_err(|e| e.to_string())?;
        client.Start().map_err(|e| e.to_string())?;
        let _started_client = StartedClient(client);
        on_started();

        let mut resampler = Resampler::new(input_rate);
        let mut out: Vec<u8> = Vec::with_capacity(8192);
        let mut mute_zero_ms: f64 = 0.0;

        while !stop.load(Ordering::Relaxed) && !tx.is_closed() {
            if let Some(mute) = endpoint_mute.as_mut() {
                let want = mute_request.load(Ordering::Relaxed);
                mute.apply(want)?;
            }
            let packet = capture
                .GetNextPacketSize()
                .map_err(|e| format!("could not query audio packet: {e}"))?;
            if packet == 0 {
                std::thread::sleep(Duration::from_millis(3));
                continue;
            }

            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            capture
                .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                .map_err(|e| format!("could not acquire audio packet: {e}"))?;

            let silent = flags & 0x2 != 0; // AUDCLNT_BUFFERFLAGS_SILENT
            let mut sum_sq: f64 = 0.0;
            if silent || data.is_null() {
                for _ in 0..frames {
                    resampler.push([0.0, 0.0], &mut out);
                }
            } else {
                let slice = std::slice::from_raw_parts(data, frames as usize * block_align);
                for f in 0..frames as usize {
                    let frame = &slice[f * block_align..(f + 1) * block_align];
                    let (l, r) = decode_frame(frame, channels, bits, is_float);
                    sum_sq += f64::from(l) * f64::from(l) + f64::from(r) * f64::from(r);
                    resampler.push([l, r], &mut out);
                }
            }
            capture
                .ReleaseBuffer(frames)
                .map_err(|e| format!("could not release audio packet: {e}"))?;

            // Some drivers silence loopback capture when the endpoint is muted.
            // If that happens, the receiver would go quiet, so restore local
            // audio (echo returns, but the TV keeps its sound).
            if endpoint_mute.as_ref().is_some_and(|mute| mute.applied_mute) {
                if sum_sq == 0.0 {
                    mute_zero_ms += f64::from(frames) * 1000.0 / input_rate;
                } else {
                    mute_zero_ms = 0.0;
                }
                if mute_zero_ms >= 1000.0 {
                    if let Some(mute) = endpoint_mute.as_mut() {
                        mute.apply(false)?;
                    }
                    mute_request.store(false, Ordering::Relaxed);
                    eprintln!(
                        "[audio] this driver silences loopback capture when muted; local audio restored"
                    );
                }
            } else {
                mute_zero_ms = 0.0;
            }

            if out.len() >= 4096 {
                // Never block the capture thread on network backpressure: it
                // must keep servicing stop/mute requests even with a full queue.
                match tx.try_send(std::mem::take(&mut out)) {
                    Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                }
                out.reserve(8192);
            }
        }

        if let Some(mute) = endpoint_mute.as_mut() {
            mute.apply(false)?;
        }
        Ok(())
    }
}

pub(super) fn decode_frame(frame: &[u8], channels: usize, bits: u16, is_float: bool) -> (f32, f32) {
    let sample = |ch: usize| -> f32 {
        let off = ch * (bits as usize / 8);
        let bytes = &frame[off..];
        if is_float && bits == 32 {
            f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        } else if bits == 16 {
            i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32768.0
        } else if bits == 32 {
            i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32 / 2_147_483_648.0
        } else if bits == 24 {
            let raw =
                i32::from(bytes[0]) | (i32::from(bytes[1]) << 8) | (i32::from(bytes[2]) << 16);
            ((raw << 8) >> 8) as f32 / 8_388_608.0
        } else {
            0.0
        }
    };
    match channels {
        0 => (0.0, 0.0),
        1 => {
            let s = sample(0);
            (s, s)
        }
        _ => (sample(0), sample(1)),
    }
}

fn to_s16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// Streaming linear resampler from the mix rate to 44.1 kHz stereo.
pub(super) struct Resampler {
    ratio: f64,
    pos: f64,
    prev: [f32; 2],
    primed: bool,
}

impl Resampler {
    pub(super) fn new(input_rate: f64) -> Self {
        Self {
            ratio: input_rate / 44_100.0,
            pos: 0.0,
            prev: [0.0, 0.0],
            primed: false,
        }
    }

    pub(super) fn push(&mut self, curr: [f32; 2], out: &mut Vec<u8>) {
        if !self.primed {
            self.prev = curr;
            self.pos = 0.0;
            self.primed = true;
            return;
        }
        loop {
            if self.pos >= 1.0 {
                self.pos -= 1.0;
                self.prev = curr;
                break;
            }
            let t = self.pos as f32;
            let l = self.prev[0] + (curr[0] - self.prev[0]) * t;
            let r = self.prev[1] + (curr[1] - self.prev[1]) * t;
            out.extend_from_slice(&to_s16(l).to_le_bytes());
            out.extend_from_slice(&to_s16(r).to_le_bytes());
            self.pos += self.ratio;
        }
    }

    /// Offset, in input sample frames, of the first output sample the next
    /// `push` will emit relative to the next input packet's first sample.
    ///
    /// Returns `0.0` until the resampler is primed: the first pushed sample
    /// only seeds interpolation and the following push emits that sample's
    /// frame. Once primed, the carried phase `pos` (relative to the previous
    /// input sample) places the first output at `pos - 1.0` input frames.
    /// Callers scale by `44_100 / input_rate` to reach output-timeline frames.
    pub(super) fn next_output_offset_frames(&self) -> f64 {
        if self.primed { self.pos - 1.0 } else { 0.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a real Windows audio endpoint; does not mute it or save audio"]
    fn real_loopback_starts_and_stops_without_muting() {
        let (tx, _rx) = tokio::sync::mpsc::channel(2);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = run_loopback(
                tx,
                worker_stop,
                Arc::new(AtomicBool::new(false)),
                move || {
                    let _ = ready_tx.send(());
                },
            );
            let _ = done_tx.send(result);
        });
        let ready = ready_rx.recv_timeout(Duration::from_secs(5));
        if ready.is_ok() {
            // Leave the bounded PCM queue undrained to exercise shutdown even
            // when captured audio fills it; the endpoint stays untouched.
            std::thread::sleep(Duration::from_millis(150));
        }
        stop.store(true, Ordering::Relaxed);
        let result = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("audio capture must stop promptly");
        worker.join().unwrap();
        result.expect("WASAPI capture should succeed");
        ready.expect("WASAPI must signal readiness");
    }
    use std::cell::RefCell;

    #[test]
    fn mute_restores_original_state_on_unwind() {
        for original in [false, true] {
            let calls = RefCell::new(Vec::new());
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut mute = MuteGuard::new(original, |value| {
                    calls.borrow_mut().push(value);
                    Ok(())
                });
                mute.apply(true).unwrap();
                panic!("capture failure");
            }));
            assert!(result.is_err());
            assert_eq!(*calls.borrow(), vec![true, original]);
        }
    }

    #[test]
    fn failed_mute_restore_is_retried_on_drop() {
        let calls = RefCell::new(Vec::new());
        {
            let mut mute = MuteGuard::new(false, |value| {
                let mut calls = calls.borrow_mut();
                calls.push(value);
                if calls.len() == 2 {
                    Err("temporary failure".into())
                } else {
                    Ok(())
                }
            });
            mute.apply(true).unwrap();
            assert!(mute.apply(false).is_err());
            assert!(mute.applied_mute);
        }
        assert_eq!(*calls.borrow(), vec![true, false, false]);
    }

    #[test]
    fn untouched_endpoint_is_not_changed_on_drop() {
        let mute = MuteGuard::new(false, |_| {
            panic!("untouched endpoint should stay unchanged")
        });
        drop(mute);
    }

    #[test]
    fn invalid_formats_are_rejected_before_decoding() {
        assert!(validate_format(0, 2, 32, 8, true).is_err());
        assert!(validate_format(48_000, 0, 32, 8, true).is_err());
        assert!(validate_format(48_000, 2, 32, 4, true).is_err());
        assert!(validate_format(48_000, 2, 64, 16, true).is_err());
        assert!(validate_format(48_000, 2, 8, 2, false).is_err());
        assert!(validate_format(48_000, 2, 32, 8, true).is_ok());
        assert!(validate_format(44_100, 1, 24, 3, false).is_ok());
    }

    #[test]
    fn pcm_decoding_preserves_sign_and_duplicates_mono() {
        assert_eq!(decode_frame(&[0, 0, 128], 1, 24, false), (-1.0, -1.0));
        assert_eq!(decode_frame(&[0, 128, 0, 64], 2, 16, false), (-1.0, 0.5));
        assert_eq!(decode_frame(&[0, 0, 0, 128], 1, 32, false), (-1.0, -1.0));
    }

    #[test]
    fn resampler_outputs_one_second_at_common_input_rates() {
        for rate in [22_050, 44_100, 48_000, 96_000] {
            let mut resampler = Resampler::new(f64::from(rate));
            let mut out = Vec::new();
            for _ in 0..=rate {
                resampler.push([0.5, -0.5], &mut out);
            }
            assert!((out.len() / 4).abs_diff(44_100) <= 1, "rate {rate}");
            for frame in out.chunks_exact(4) {
                assert_eq!(frame, &[255, 63, 1, 192]);
            }
        }
    }

    #[test]
    fn next_output_offset_reports_priming_then_carried_phase() {
        let mut resampler = Resampler::new(44_100.0);
        assert_eq!(resampler.next_output_offset_frames(), 0.0);

        let mut out = Vec::new();
        resampler.push([0.0, 0.0], &mut out);
        assert!(out.is_empty(), "the priming push emits no output");
        assert!((resampler.next_output_offset_frames() + 1.0).abs() < 1e-9);

        resampler.push([0.0, 0.0], &mut out);
        assert_eq!(out.len(), 4, "44.1 kHz input emits exactly one frame");
        assert!((resampler.next_output_offset_frames() + 1.0).abs() < 1e-9);
    }

    #[test]
    fn next_output_offset_matches_emitted_frames_for_48k() {
        let input_rate = 48_000.0;
        let ratio = input_rate / 44_100.0;
        let mut resampler = Resampler::new(input_rate);
        let mut out = Vec::new();
        for pushed in 0..300usize {
            resampler.push([0.0, 0.0], &mut out);
            let emitted = (out.len() / 4) as f64;
            // Output `n` is emitted at input position `n * ratio`; the carried
            // phase is that position minus the last pushed input frame.
            let expected = emitted * ratio - (pushed as f64) - 1.0;
            assert!(
                (resampler.next_output_offset_frames() - expected).abs() < 1e-6,
                "pushed {pushed}: got {}, expected {expected}",
                resampler.next_output_offset_frames()
            );
        }
    }
}
