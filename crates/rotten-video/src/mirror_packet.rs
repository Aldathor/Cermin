use std::time::Duration;

const PACKET_HEADER_LEN: usize = 128;

/// NTP timestamp on the shared clock (no extra playout bias).
pub fn ntp_time_now() -> u64 {
    ntp_time_with_bias(Duration::ZERO)
}

/// NTP timestamp with forward playout bias on the shared boot-relative clock
/// (no NTP epoch — matches doubletake video frame headers).
pub fn ntp_time_with_bias(bias: Duration) -> u64 {
    let bias = bias.max(Duration::from_millis(5));
    rotten_core::ntp::ntp_boot_relative_with_bias(bias)
}

/// NTP timestamp for a frame captured at `elapsed_ns` on the session clock
/// (0 = stamp at send time). Capture-time stamps preserve the capture cadence.
pub fn ntp_time_from_elapsed_ns(elapsed_ns: u64, bias: Duration) -> u64 {
    let bias = bias.max(Duration::from_millis(5));
    if elapsed_ns == 0 {
        return rotten_core::ntp::ntp_boot_relative_with_bias(bias);
    }
    rotten_core::ntp::ntp_boot_relative_from_elapsed(Duration::from_nanos(elapsed_ns), bias)
}

/// Session playout bias from audio latency samples at 44.1 kHz.
pub fn bias_from_audio_latency_samples(samples: u32) -> Duration {
    Duration::from_nanos(u64::from(samples) * 1_000_000_000 / 44_100)
}

fn put_f32_le(buf: &mut [u8], off: usize, v: f32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Unencrypted SPS+PPS codec packet header (type 0x01).
/// Offsets 16/40 = encoded content size; 56/60 = presentation/display size (doubletake).
pub fn build_codec_header(
    content_width: u32,
    content_height: u32,
    display_width: u32,
    display_height: u32,
    payload_len: usize,
    ntp_timestamp: u64,
) -> [u8; PACKET_HEADER_LEN] {
    let mut header = [0u8; PACKET_HEADER_LEN];
    header[0..4].copy_from_slice(&(payload_len as u32).to_le_bytes());
    header[4] = 0x01;
    header[6] = 0x16;
    header[7] = 0x01;
    header[8..16].copy_from_slice(&ntp_timestamp.to_le_bytes());
    put_f32_le(&mut header, 16, content_width as f32);
    put_f32_le(&mut header, 20, content_height as f32);
    put_f32_le(&mut header, 40, content_width as f32);
    put_f32_le(&mut header, 44, content_height as f32);
    put_f32_le(&mut header, 56, display_width as f32);
    put_f32_le(&mut header, 60, display_height as f32);
    header
}

/// Encrypted VCL video packet header (type 0x00).
pub fn build_video_header(
    payload_len: usize,
    is_keyframe: bool,
    ntp_timestamp: u64,
) -> [u8; PACKET_HEADER_LEN] {
    let mut header = [0u8; PACKET_HEADER_LEN];
    header[0..4].copy_from_slice(&(payload_len as u32).to_le_bytes());
    header[4] = 0x00;
    header[5] = if is_keyframe { 0x10 } else { 0x00 };
    header[8..16].copy_from_slice(&ntp_timestamp.to_le_bytes());
    header
}

/// Data-channel keepalive (type 0x02, no payload).
pub fn build_heartbeat_header() -> [u8; PACKET_HEADER_LEN] {
    let mut header = [0u8; PACKET_HEADER_LEN];
    header[4] = 0x02;
    header[6] = 0x1e;
    header
}

/// Split Annex-B bitstream into raw NAL units (without start codes).
pub fn split_annex_b(data: &[u8]) -> Vec<Vec<u8>> {
    let mut nals = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let mut start = None;
        if i + 3 <= data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            start = Some(i + 3);
            i += 3;
        } else if i + 4 <= data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            start = Some(i + 4);
            i += 4;
        } else {
            i += 1;
            continue;
        }

        let nal_start = start.unwrap();
        let mut end = data.len();
        let mut j = nal_start;
        while j + 3 < data.len() {
            if data[j] == 0
                && data[j + 1] == 0
                && (data[j + 2] == 1
                    || (j + 3 < data.len() && data[j + 2] == 0 && data[j + 3] == 1))
            {
                end = j;
                break;
            }
            j += 1;
        }
        if nal_start < end {
            nals.push(data[nal_start..end].to_vec());
            i = end;
        } else {
            i += 1;
        }
    }
    nals
}

pub fn build_avcc_config(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let avcc_len = 6 + 2 + sps.len() + 1 + 2 + pps.len();
    let mut payload = vec![0u8; avcc_len + 4];
    payload[0] = 0x01;
    if sps.len() >= 4 {
        payload[1] = sps[1];
        payload[2] = sps[2];
        payload[3] = sps[3];
    }
    payload[4] = 0xff;
    payload[5] = 0xe1;
    payload[6..8].copy_from_slice(&(sps.len() as u16).to_be_bytes());
    payload[8..8 + sps.len()].copy_from_slice(sps);
    let off = 8 + sps.len();
    payload[off] = 0x01;
    payload[off + 1..off + 3].copy_from_slice(&(pps.len() as u16).to_be_bytes());
    payload[off + 3..off + 3 + pps.len()].copy_from_slice(pps);
    payload[avcc_len] = 0x02;
    payload
}

pub fn nals_to_avcc(nals: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

fn is_vcl_nal_type(nal_type: u8) -> bool {
    matches!(nal_type, 1 | 2 | 3 | 4 | 5)
}

/// Extract raw NAL payloads (no start codes) from Annex-B or length-prefixed bitstream.
pub fn extract_nals(data: &[u8]) -> Vec<Vec<u8>> {
    if has_annex_b_start(data) {
        return split_annex_b(data);
    }

    let mut nals = Vec::new();
    let mut off = 0;
    while off + 4 <= data.len() {
        let len = u32::from_be_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        if len == 0 || len > 16 * 1024 * 1024 || off + 4 + len > data.len() {
            break;
        }
        nals.push(data[off + 4..off + 4 + len].to_vec());
        off += 4 + len;
    }
    if nals.is_empty() && !data.is_empty() {
        // OpenH264 may emit raw NAL bytes without start codes or length prefixes.
        nals.push(data.to_vec());
    }
    nals
}

fn has_annex_b_start(data: &[u8]) -> bool {
    if data.len() >= 4 && data[0] == 0 && data[1] == 0 && data[2] == 0 && data[3] == 1 {
        return true;
    }
    data.len() >= 3 && data[0] == 0 && data[1] == 0 && data[2] == 1
}

pub fn partition_access_unit(data: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>, Vec<Vec<u8>>) {
    let nals = extract_nals(data);
    let mut sps = None;
    let mut pps = None;
    let mut vcl = Vec::new();
    for nal in nals {
        let nal_type = nal.first().copied().unwrap_or(0) & 0x1f;
        match nal_type {
            7 => sps = Some(nal),
            8 => pps = Some(nal),
            6 | 9 => {} // skip SEI and AUD
            _ if is_vcl_nal_type(nal_type) => vcl.push(nal),
            _ => {}
        }
    }
    (sps, pps, vcl)
}

/// NAL type summary for debug logging.
pub fn nal_type_summary(data: &[u8]) -> Vec<u8> {
    extract_nals(data)
        .iter()
        .map(|nal| nal.first().copied().unwrap_or(0) & 0x1f)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b_access_unit() {
        let au = [
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f, 0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, 0, 0, 0, 1,
            0x65, 0x88, 0x84,
        ];
        let (sps, pps, vcl) = partition_access_unit(&au);
        assert!(sps.is_some());
        assert!(pps.is_some());
        assert_eq!(vcl.len(), 1);
        assert_eq!(vcl[0][0] & 0x1f, 5);
    }
}
