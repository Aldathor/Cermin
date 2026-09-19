//! WASAPI loopback capture of the default render endpoint, converted to
//! 44.1 kHz stereo interleaved S16 PCM for the AirPlay audio RTP stream.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::Sender;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

pub fn run_loopback(
    tx: Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    mute_request: Arc<AtomicBool>,
) -> Result<(), String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| e.to_string())?;
        let endpoint_volume: Option<IAudioEndpointVolume> =
            unsafe { device.Activate(CLSCTX_ALL, None) }.ok();
        let originally_muted = endpoint_volume
            .as_ref()
            .and_then(|v| unsafe { v.GetMute() }.ok())
            .map(|b| b.as_bool())
            .unwrap_or(false);
        let mut applied_mute = false;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;
        let mix = client.GetMixFormat().map_err(|e| e.to_string())?;
        if mix.is_null() {
            return Err("WASAPI returned no mix format".into());
        }
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
            let ext: WAVEFORMATEXTENSIBLE =
                std::ptr::read_unaligned(mix as *const WAVEFORMATEXTENSIBLE);
            let sub = ext.SubFormat;
            sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            false
        };

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

        let mut resampler = Resampler::new(input_rate);
        let mut out: Vec<u8> = Vec::with_capacity(8192);
        let mut mute_zero_ms: f64 = 0.0;

        while !stop.load(Ordering::Relaxed) {
            if let Some(vol) = endpoint_volume.as_ref() {
                let want = mute_request.load(Ordering::Relaxed);
                if want != applied_mute {
                    let _ = unsafe { vol.SetMute(want, std::ptr::null()) };
                    applied_mute = want;
                }
            }
            let packet = match capture.GetNextPacketSize() {
                Ok(n) => n,
                Err(_) => break,
            };
            if packet == 0 {
                std::thread::sleep(Duration::from_millis(3));
                continue;
            }

            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            if capture
                .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                .is_err()
            {
                break;
            }

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
            if capture.ReleaseBuffer(frames).is_err() {
                break;
            }

            // Some drivers silence loopback capture when the endpoint is muted.
            // If that happens, the receiver would go quiet, so restore local
            // audio (echo returns, but the TV keeps its sound).
            if applied_mute && !silent && !data.is_null() {
                if sum_sq == 0.0 {
                    mute_zero_ms += f64::from(frames) * 1000.0 / input_rate;
                } else {
                    mute_zero_ms = 0.0;
                }
                if mute_zero_ms >= 1000.0 {
                    if let Some(vol) = endpoint_volume.as_ref() {
                        let _ = unsafe { vol.SetMute(originally_muted, std::ptr::null()) };
                    }
                    applied_mute = false;
                    mute_request.store(false, Ordering::Relaxed);
                    eprintln!(
                        "[audio] this driver silences loopback capture when muted; local audio restored"
                    );
                }
            } else {
                mute_zero_ms = 0.0;
            }

            if out.len() >= 4096 {
                if tx.blocking_send(std::mem::take(&mut out)).is_err() {
                    break;
                }
                out.reserve(8192);
            }
        }

        if applied_mute {
            if let Some(vol) = endpoint_volume.as_ref() {
                let _ = unsafe { vol.SetMute(originally_muted, std::ptr::null()) };
            }
        }
        let _ = client.Stop();
        CoTaskMemFree(Some(mix as *const core::ffi::c_void));
        Ok(())
    }
}

fn decode_frame(frame: &[u8], channels: usize, bits: u16, is_float: bool) -> (f32, f32) {
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
struct Resampler {
    ratio: f64,
    pos: f64,
    prev: [f32; 2],
    primed: bool,
}

impl Resampler {
    fn new(input_rate: f64) -> Self {
        Self {
            ratio: input_rate / 44_100.0,
            pos: 0.0,
            prev: [0.0, 0.0],
            primed: false,
        }
    }

    fn push(&mut self, curr: [f32; 2], out: &mut Vec<u8>) {
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
}
