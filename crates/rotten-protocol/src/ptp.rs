//! Minimal gPTP (IEEE 1588 / 802.1AS) master for AirPlay PTP sessions.
//!
//! Receivers that advertise PTP-only timing (feature bit 41 set, bit 45 clear —
//! e.g. the Samsung LSP7 projector) tear down the mirror video data channel
//! roughly a second after frames start arriving unless the sender acts as the
//! session's timing authority. This module sends unicast two-step
//! Sync/Follow_Up (125 ms), Announce (1 s) and replies to the receiver's
//! Delay_Req with Delay_Resp on UDP 319/320, exactly like the reference
//! senders (cliairplay ap2_ptp.c, NQPTP peers).
//!
//! All timestamps live on the shared session clock
//! (`rotten_core::ntp::session_elapsed`) so video frame headers and the PTP
//! timeline stay consistent.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use plist::{Dictionary, Value};
use rotten_core::debug_log::agent_log;
use rotten_core::error::{Result, RottenError};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

pub const PTP_EVENT_PORT: u16 = 319;
pub const PTP_GENERAL_PORT: u16 = 320;

/// Clock identity of the peer when the receiver is the elected PTP master.
/// Audio anchors must name the timeline's master clock, not our own identity.
static PEER_CLOCK_ID: AtomicU64 = AtomicU64::new(0);

pub fn peer_clock_id() -> u64 {
    PEER_CLOCK_ID.load(Ordering::Relaxed)
}

const HEADER_LEN: usize = 34;
const TWO_STEP_FLAG: u16 = 0x0200;

const MSG_SYNC: u8 = 0x00;
const MSG_DELAY_REQ: u8 = 0x01;
const MSG_FOLLOW_UP: u8 = 0x08;
const MSG_DELAY_RESP: u8 = 0x09;
const MSG_ANNOUNCE: u8 = 0x0b;
const MSG_SIGNALING: u8 = 0x0c;

/// A PTP peer as carried in `timingPeerInfo` / `timingPeerList`.
#[derive(Debug, Clone)]
pub struct PtpPeer {
    pub id: String,
    pub addresses: Vec<String>,
    pub clock_id: Option<u64>,
}

impl PtpPeer {
    pub fn to_plist(&self) -> Value {
        let mut d = Dictionary::new();
        d.insert(
            "ID".into(),
            Value::String(self.id.clone()),
        );
        d.insert(
            "Addresses".into(),
            Value::Array(
                self.addresses
                    .iter()
                    .map(|a| Value::String(a.clone()))
                    .collect(),
            ),
        );
        if let Some(clock_id) = self.clock_id {
            d.insert("DeviceType".into(), Value::Integer(0.into()));
            d.insert(
                "ClockID".into(),
                Value::Integer((clock_id as i64).into()),
            );
            d.insert(
                "SupportsClockPortMatchingOverride".into(),
                Value::Boolean(false),
            );
        } else {
            d.insert(
                "SupportsClockPortMatchingOverride".into(),
                Value::Boolean(true),
            );
        }
        Value::Dictionary(d)
    }

    pub fn from_plist(value: &Value) -> Option<Self> {
        let d = value.as_dictionary()?;
        let id = d.get("ID")?.as_string()?.to_string();
        let addresses = d
            .get("Addresses")?
            .as_array()?
            .iter()
            .filter_map(|v| v.as_string().map(str::to_string))
            .collect();
        let clock_id = d
            .get("ClockID")
            .and_then(Value::as_unsigned_integer)
            .map(|v| v as u64);
        Some(Self {
            id,
            addresses,
            clock_id,
        })
    }
}

/// Extract `timingPeerInfo` from a SETUP response plist.
pub fn peer_info_from_body(body: &[u8]) -> Option<PtpPeer> {
    let value: Value = plist::from_bytes(body).ok()?;
    let info = value.as_dictionary()?.get("timingPeerInfo")?;
    PtpPeer::from_plist(info)
}

/// Derive the 64-bit PTP clock identity from the AirPlay DACP identifier
/// (16 hex chars, e.g. `8e70f5df22738290`).
pub fn clock_id_from_identifier(identifier: &str) -> [u8; 8] {
    let hex: String = identifier
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    let mut out = [0u8; 8];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex.as_bytes().get(i * 2).copied().unwrap_or(b'0');
        let lo = hex.as_bytes().get(i * 2 + 1).copied().unwrap_or(b'0');
        let parse = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
        *byte = (parse(hi) << 4) | parse(lo);
    }
    out
}

fn now_ns() -> u64 {
    rotten_core::ntp::session_elapsed().as_nanos() as u64
}

fn encode_timestamp(ns: u64) -> [u8; 10] {
    let seconds = ns / 1_000_000_000;
    let nanos = (ns % 1_000_000_000) as u32;
    let mut out = [0u8; 10];
    out[..6].copy_from_slice(&(seconds & 0xFFFF_FFFF_FFFF).to_be_bytes()[2..]);
    out[6..].copy_from_slice(&nanos.to_be_bytes());
    out
}

fn build_header(
    msg_type: u8,
    control: u8,
    seq: u16,
    clock_id: &[u8; 8],
    message_length: u16,
    flags: u16,
    log_interval: i8,
) -> Vec<u8> {
    let mut h = Vec::with_capacity(HEADER_LEN);
    h.push(msg_type & 0x0f);
    h.push(0x02);
    h.extend_from_slice(&message_length.to_be_bytes());
    h.push(0);
    h.push(0);
    h.extend_from_slice(&flags.to_be_bytes());
    h.extend_from_slice(&0i64.to_be_bytes());
    h.extend_from_slice(&0u32.to_be_bytes());
    h.extend_from_slice(clock_id);
    h.extend_from_slice(&1u16.to_be_bytes());
    h.extend_from_slice(&seq.to_be_bytes());
    h.push(control);
    h.push(log_interval as u8);
    h
}

/// Two-step Sync + Follow_Up pair (Sync on event port, Follow_Up on general).
fn build_sync_follow_up(clock_id: &[u8; 8], seq: u16, t1_ns: u64) -> (Vec<u8>, Vec<u8>) {
    let mut sync = build_header(
        MSG_SYNC,
        0,
        seq,
        clock_id,
        (HEADER_LEN + 10) as u16,
        TWO_STEP_FLAG,
        -3,
    );
    sync.extend_from_slice(&[0u8; 10]);

    let mut tlv = Vec::with_capacity(32);
    tlv.extend_from_slice(&0x0003u16.to_be_bytes());
    tlv.extend_from_slice(&28u16.to_be_bytes());
    tlv.extend_from_slice(&[0x00, 0x80, 0xc2]);
    tlv.extend_from_slice(&[0x00, 0x00, 0x01]);
    tlv.extend_from_slice(&[0u8; 22]);

    let mut follow_up = build_header(
        MSG_FOLLOW_UP,
        2,
        seq,
        clock_id,
        (HEADER_LEN + 10 + tlv.len()) as u16,
        0,
        -3,
    );
    follow_up.extend_from_slice(&encode_timestamp(t1_ns));
    follow_up.extend_from_slice(&tlv);
    (sync, follow_up)
}

/// Announce with sender as grandmaster (priority 250, clock class 248).
fn build_announce(clock_id: &[u8; 8], seq: u16, t_ns: u64) -> Vec<u8> {
    let mut pkt = build_header(MSG_ANNOUNCE, 5, seq, clock_id, 64, 0, 0);
    pkt.extend_from_slice(&encode_timestamp(t_ns));
    pkt.extend_from_slice(&37i16.to_be_bytes());
    pkt.push(0);
    pkt.push(250);
    pkt.push(248);
    pkt.push(0xfe);
    pkt.extend_from_slice(&0xffffu16.to_be_bytes());
    pkt.push(128);
    pkt.extend_from_slice(clock_id);
    pkt.extend_from_slice(&0u16.to_be_bytes());
    pkt.push(0xa0);
    pkt
}

/// gPTP Signaling with message-interval request + Apple TLVs (macOS style).
fn build_signaling(clock_id: &[u8; 8], seq: u16) -> Vec<u8> {
    let sync_interval = (-3i8) as u8;
    let announce_interval = (-2i8) as u8;

    let mut tlv = Vec::new();
    tlv.extend_from_slice(&0x0003u16.to_be_bytes());
    let interval_payload = [sync_interval, sync_interval, announce_interval, 0x02];
    tlv.extend_from_slice(&((6 + interval_payload.len()) as u16).to_be_bytes());
    tlv.extend_from_slice(&[0x00, 0x80, 0xc2, 0x00, 0x00, 0x02]);
    tlv.extend_from_slice(&interval_payload);

    let mut apple1 = Vec::new();
    apple1.extend_from_slice(&0x0003u16.to_be_bytes());
    apple1.extend_from_slice(&((6 + 16) as u16).to_be_bytes());
    apple1.extend_from_slice(&[0x00, 0x0d, 0x93, 0x00, 0x00, 0x01]);
    apple1.extend_from_slice(&[
        0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x27, 0x10, 0x00, 0x00, 0x27, 0x10, 0x00, 0x00, 0x00,
        0x00,
    ]);

    let mut apple2 = Vec::new();
    apple2.extend_from_slice(&0x0003u16.to_be_bytes());
    apple2.extend_from_slice(&((6 + 26) as u16).to_be_bytes());
    apple2.extend_from_slice(&[0x00, 0x0d, 0x93, 0x00, 0x00, 0x05]);
    apple2.extend_from_slice(&[
        0x00, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x27, 0x10, 0x00, 0x00, 0x27, 0x10, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ]);

    let mut body = vec![0xffu8; 10];
    body.extend_from_slice(&tlv);
    body.extend_from_slice(&apple1);
    body.extend_from_slice(&apple2);

    let mut pkt = build_header(
        MSG_SIGNALING,
        5,
        seq,
        clock_id,
        (HEADER_LEN + body.len()) as u16,
        0,
        -2,
    );
    pkt.extend_from_slice(&body);
    pkt
}

/// Precise origin timestamp of a Follow_Up (message timestamp + correction).
fn follow_up_origin_ns(pkt: &[u8]) -> Option<i64> {
    if pkt.len() < 44 {
        return None;
    }
    let correction_raw = i64::from_be_bytes(pkt[8..16].try_into().ok()?);
    let correction_ns = correction_raw / 65_536;
    let sec = ((pkt[34] as u64) << 40)
        | ((pkt[35] as u64) << 32)
        | ((pkt[36] as u64) << 24)
        | ((pkt[37] as u64) << 16)
        | ((pkt[38] as u64) << 8)
        | (pkt[39] as u64);
    let nanos = u32::from_be_bytes([pkt[40], pkt[41], pkt[42], pkt[43]]);
    Some(sec as i64 * 1_000_000_000 + i64::from(nanos) + correction_ns)
}

/// Announce dataset fields we care about (offset 34 = body start).
fn announce_dataset(pkt: &[u8]) -> Option<(u8, u8)> {
    if pkt.len() < 64 {
        return None;
    }
    Some((pkt[47], pkt[48]))
}

/// Delay_Resp answering a Delay_Req: t4 + requestingPortIdentity.
fn build_delay_resp(clock_id: &[u8; 8], delay_req: &[u8], t4_ns: u64) -> Vec<u8> {
    let seq = if delay_req.len() >= 32 {
        u16::from_be_bytes([delay_req[30], delay_req[31]])
    } else {
        0
    };
    let mut pkt = build_header(MSG_DELAY_RESP, 3, seq, clock_id, 54, 0, 0);
    pkt.extend_from_slice(&encode_timestamp(t4_ns));
    if delay_req.len() >= 30 {
        pkt.extend_from_slice(&delay_req[20..30]);
    } else {
        pkt.extend_from_slice(&[0u8; 10]);
    }
    pkt
}

/// Live PTP master bound to UDP 319/320, unicasting to a single receiver.
pub struct PtpMaster {
    task: JoinHandle<()>,
    event: Arc<UdpSocket>,
    general: Arc<UdpSocket>,
    peer: IpAddr,
    clock_id: [u8; 8],
}

impl PtpMaster {
    /// Bind 319/320 and start the Sync/Announce/Delay_Resp engine.
    pub async fn start(peer: IpAddr, clock_id: [u8; 8]) -> Result<Self> {
        let event = UdpSocket::bind(("0.0.0.0", PTP_EVENT_PORT))
            .await
            .map_err(|e| RottenError::Protocol(format!("bind PTP UDP {PTP_EVENT_PORT}: {e}")))?;
        let general = UdpSocket::bind(("0.0.0.0", PTP_GENERAL_PORT))
            .await
            .map_err(|e| RottenError::Protocol(format!("bind PTP UDP {PTP_GENERAL_PORT}: {e}")))?;
        let event = Arc::new(event);
        let general = Arc::new(general);

        let event_task = event.clone();
        let general_task = general.clone();
        let task = tokio::spawn(async move {
            run(event_task, general_task, peer, clock_id).await;
        });

        // #region agent log
        agent_log(
            "ptp.rs:PtpMaster::start",
            "PTP master started",
            "H-PTP",
            serde_json::json!({
                "peer": peer.to_string(),
                "clockId": hex::encode(clock_id),
                "eventPort": PTP_EVENT_PORT,
                "generalPort": PTP_GENERAL_PORT,
            }),
        );
        // #endregion

        Ok(Self {
            task,
            event,
            general,
            peer,
            clock_id,
        })
    }

    pub fn local_event_port(&self) -> Option<SocketAddr> {
        self.event.local_addr().ok()
    }

    pub fn local_general_port(&self) -> Option<SocketAddr> {
        self.general.local_addr().ok()
    }

    /// Send an immediate Announce + Sync/Follow_Up burst so the receiver can
    /// measure our clock without waiting for the periodic intervals. Called
    /// right after SETPEERS, which is what makes a receiver follow us.
    pub async fn kick(&self) {
        let now = now_ns();
        let seq: u16 = 0xfff0;
        let (sync, follow_up) = build_sync_follow_up(&self.clock_id, seq, now);
        let _ = self
            .event
            .send_to(&sync, (self.peer, PTP_EVENT_PORT))
            .await;
        let _ = self
            .general
            .send_to(&follow_up, (self.peer, PTP_GENERAL_PORT))
            .await;
        let announce = build_announce(&self.clock_id, seq, now);
        let _ = self
            .general
            .send_to(&announce, (self.peer, PTP_GENERAL_PORT))
            .await;
        // #region agent log
        agent_log(
            "ptp.rs:PtpMaster::kick",
            "PTP timing kick sent",
            "H-PTP",
            serde_json::json!({ "peer": self.peer.to_string() }),
        );
        // #endregion
    }
}

impl Drop for PtpMaster {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(
    event: Arc<UdpSocket>,
    general: Arc<UdpSocket>,
    peer: IpAddr,
    clock_id: [u8; 8],
) {
    let mut sync_seq: u16 = 0;
    let mut announce_seq: u16 = 0;
    let event_addr = SocketAddr::new(peer, PTP_EVENT_PORT);
    let general_addr = SocketAddr::new(peer, PTP_GENERAL_PORT);
    // BMCA outcome: some receivers (e.g. the LSP7) assert themselves as the
    // PTP master and send Sync/Follow_Up to us. When that happens we slave to
    // their clock and express every timestamp on their timeline.
    let mut peer_is_master = false;
    let mut offset_ns: i64 = 0;
    let mut announce_logged = false;
    let mut offset_logged = false;
    let mut offset_log_counter: u32 = 0;
    let mut event_packets_logged: u32 = 0;

    let mut sync_tick = tokio::time::interval(Duration::from_millis(125));
    sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut announce_tick = tokio::time::interval(Duration::from_millis(1000));
    announce_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut event_buf = [0u8; 512];
    let mut general_buf = [0u8; 1024];

    loop {
        tokio::select! {
            _ = sync_tick.tick(), if !peer_is_master => {
                let (sync, follow_up) = build_sync_follow_up(&clock_id, sync_seq, now_ns());
                sync_seq = sync_seq.wrapping_add(1);
                if let Err(e) = event.send_to(&sync, event_addr).await {
                    agent_log(
                        "ptp.rs:run",
                        "PTP sync send failed",
                        "H-PTP",
                        serde_json::json!({ "error": e.to_string() }),
                    );
                }
                let _ = general.send_to(&follow_up, general_addr).await;
            }
            _ = announce_tick.tick(), if !peer_is_master => {
                let announce = build_announce(&clock_id, announce_seq, now_ns());
                let _ = general.send_to(&announce, general_addr).await;
                let signaling = build_signaling(&clock_id, announce_seq);
                let _ = general.send_to(&signaling, general_addr).await;
                announce_seq = announce_seq.wrapping_add(1);
            }
            result = event.recv_from(&mut event_buf) => {
                match result {
                    Ok((n, from)) => {
                        let msg_type = event_buf[0] & 0x0f;
                        if msg_type == MSG_DELAY_REQ {
                            let resp = build_delay_resp(&clock_id, &event_buf[..n], now_ns());
                            let _ = general.send_to(&resp, from).await;
                            agent_log(
                                "ptp.rs:run",
                                "PTP Delay_Req answered",
                                "H-PTP",
                                serde_json::json!({ "from": from.to_string(), "bytes": n }),
                            );
                        } else if event_packets_logged < 3 {
                            event_packets_logged += 1;
                            agent_log(
                                "ptp.rs:run",
                                "PTP event packet",
                                "H-PTP",
                                serde_json::json!({
                                    "from": from.to_string(),
                                    "bytes": n,
                                    "type": msg_type,
                                }),
                            );
                        }
                    }
                    Err(e) => {
                        agent_log(
                            "ptp.rs:run",
                            "PTP event recv error",
                            "H-PTP",
                            serde_json::json!({ "error": e.to_string() }),
                        );
                    }
                }
            }
            result = general.recv_from(&mut general_buf) => {
                match result {
                    Ok((n, _from)) => {
                        let msg_type = general_buf[0] & 0x0f;
                        match msg_type {
                            MSG_FOLLOW_UP => {
                                if n >= 28 {
                                    let source_id =
                                        u64::from_be_bytes(general_buf[20..28].try_into().unwrap());
                                    PEER_CLOCK_ID.store(source_id, Ordering::Relaxed);
                                }
                                if let Some(master_ns) = follow_up_origin_ns(&general_buf[..n]) {
                                    let raw_offset = master_ns - now_ns() as i64;
                                    if !peer_is_master {
                                        offset_ns = raw_offset;
                                        peer_is_master = true;
                                    } else {
                                        offset_ns += (raw_offset - offset_ns) / 4;
                                    }
                                    rotten_core::ntp::set_session_offset_ns(offset_ns);
                                    if !offset_logged || offset_log_counter % 40 == 0 {
                                        offset_logged = true;
                                        // #region agent log
                                        agent_log(
                                            "ptp.rs:run",
                                            "PTP slaved to receiver clock",
                                            "H-PTP",
                                            serde_json::json!({
                                                "masterTsSec": master_ns / 1_000_000_000,
                                                "offsetNs": offset_ns,
                                                "offsetMs": offset_ns / 1_000_000,
                                            }),
                                        );
                                        // #endregion
                                    }
                                    offset_log_counter = offset_log_counter.wrapping_add(1);
                                }
                            }
                            MSG_ANNOUNCE => {
                                if !announce_logged {
                                    announce_logged = true;
                                    // #region agent log
                                    agent_log(
                                        "ptp.rs:run",
                                        "PTP receiver announce dataset",
                                        "H-PTP",
                                        serde_json::json!({
                                            "priority1": announce_dataset(&general_buf[..n]).map(|d| d.0),
                                            "clockClass": announce_dataset(&general_buf[..n]).map(|d| d.1),
                                        }),
                                    );
                                    // #endregion
                                }
                            }
                            _ => {
                                // #region agent log
                                agent_log(
                                    "ptp.rs:run",
                                    "PTP general packet",
                                    "H-PTP",
                                    serde_json::json!({ "bytes": n, "type": msg_type }),
                                );
                                // #endregion
                            }
                        }
                    }
                    Err(e) => {
                        agent_log(
                            "ptp.rs:run",
                            "PTP general recv error",
                            "H-PTP",
                            serde_json::json!({ "error": e.to_string() }),
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_id_from_hex_identifier() {
        let id = clock_id_from_identifier("8e70f5df22738290");
        assert_eq!(hex::encode(id), "8e70f5df22738290");
    }

    #[test]
    fn sync_and_follow_up_lengths() {
        let clock = [1u8; 8];
        let (sync, follow) = build_sync_follow_up(&clock, 7, 1_500_000_000);
        assert_eq!(sync.len(), 44);
        assert_eq!(follow.len(), 76);
        assert_eq!(follow[34..44], encode_timestamp(1_500_000_000));
    }

    #[test]
    fn announce_length() {
        let announce = build_announce(&[2u8; 8], 0, 0);
        assert_eq!(announce.len(), 64);
    }

    #[test]
    fn delay_resp_echoes_sequence_and_identity() {
        let mut req = build_header(MSG_DELAY_REQ, 1, 42, &[3u8; 8], 44, 0, 0);
        req.extend_from_slice(&[0u8; 10]);
        let resp = build_delay_resp(&[4u8; 8], &req, 123);
        assert_eq!(resp.len(), 54);
        assert_eq!(resp[0] & 0x0f, MSG_DELAY_RESP);
        assert_eq!(u16::from_be_bytes([resp[30], resp[31]]), 42);
        assert_eq!(&resp[44..54], &req[20..30]);
    }
}
