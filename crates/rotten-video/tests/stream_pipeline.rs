#![cfg(feature = "software-encode-source")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use rotten_core::config::HwAccel;
use rotten_crypto::MirrorVideoCrypto;
use rotten_video::{MirrorStreamer, SyntheticSource, frame_channel};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

async fn read_packet(socket: &mut TcpStream) -> ([u8; 128], Vec<u8>) {
    let mut header = [0; 128];
    socket.read_exact(&mut header).await.unwrap();
    let payload_len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    assert!(payload_len < 1024 * 1024, "unexpectedly large packet");
    let mut payload = vec![0; payload_len];
    socket.read_exact(&mut payload).await.unwrap();
    (header, payload)
}

fn append_nal(annex_b: &mut Vec<u8>, nal: &[u8]) {
    annex_b.extend_from_slice(&[0, 0, 0, 1]);
    annex_b.extend_from_slice(nal);
}

/// Exercises capture, latest-frame replacement, source encoding, TCP framing,
/// first-frame readiness, and sender-driven shutdown without an Apple TV or DLL.
#[tokio::test]
#[cfg_attr(
    feature = "software-encode-dll",
    ignore = "requires the official OpenH264 DLL"
)]
async fn synthetic_frame_is_decodable_over_tcp_and_sender_close_ends_stream() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let streamer = MirrorStreamer::new(
            "127.0.0.1".into(),
            listener.local_addr().unwrap().port(),
            MirrorVideoCrypto::None,
        );
        let stats = streamer.stats();
        let (frame_tx, frame_rx) = frame_channel();
        let (first_tx, mut first_rx) = oneshot::channel();
        assert_eq!(
            first_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );

        let mut source = SyntheticSource::new(64, 48);
        for capture_ns in [1_000_000, 2_000_000] {
            let (rgba, width, height) = source.next_frame().unwrap();
            frame_tx.send((rgba, width, height, capture_ns));
        }
        assert_eq!(frame_tx.dropped_frames(), 1);

        let send = streamer.stream_from_channel(
            64,
            48,
            64,
            48,
            64,
            48,
            0,
            30,
            2000,
            HwAccel::None,
            frame_rx,
            Some(first_tx),
            Arc::new(AtomicBool::new(false)),
        );
        let receive = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (codec_header, codec) = read_packet(&mut socket).await;
            assert_eq!(codec_header[4], 1, "codec must precede video");
            for (offset, expected) in [(16, 64.0), (20, 48.0), (56, 64.0), (60, 48.0)] {
                assert_eq!(
                    f32::from_le_bytes(codec_header[offset..offset + 4].try_into().unwrap()),
                    expected
                );
            }

            // Parse the transported avcC rather than using the packet builder.
            assert_eq!(codec[0], 1);
            assert_eq!(codec[4] & 3, 3, "four-byte NAL lengths");
            assert_eq!(codec[5] & 31, 1, "one SPS");
            let sps_len = u16::from_be_bytes(codec[6..8].try_into().unwrap()) as usize;
            let sps = &codec[8..8 + sps_len];
            assert_eq!(sps[0] & 31, 7);
            let pps_offset = 8 + sps_len;
            assert_eq!(codec[pps_offset], 1, "one PPS");
            let pps_len =
                u16::from_be_bytes(codec[pps_offset + 1..pps_offset + 3].try_into().unwrap())
                    as usize;
            let pps = &codec[pps_offset + 3..pps_offset + 3 + pps_len];
            assert_eq!(pps[0] & 31, 8);
            let mut annex_b = Vec::new();
            append_nal(&mut annex_b, sps);
            append_nal(&mut annex_b, pps);

            let (video_header, video) = read_packet(&mut socket).await;
            assert_eq!(video_header[4], 0);
            assert_eq!(video_header[5], 0x10, "first frame is a keyframe");
            assert_eq!(&codec_header[8..16], &video_header[8..16]);
            assert_ne!(
                u64::from_le_bytes(video_header[8..16].try_into().unwrap()),
                0
            );
            let mut remaining = video.as_slice();
            let mut has_idr = false;
            while !remaining.is_empty() {
                assert!(remaining.len() >= 4);
                let len = u32::from_be_bytes(remaining[..4].try_into().unwrap()) as usize;
                remaining = &remaining[4..];
                assert!(len > 0 && len <= remaining.len());
                let nal = &remaining[..len];
                assert!(matches!(nal[0] & 31, 1..=5), "video contains only VCL");
                has_idr |= nal[0] & 31 == 5;
                append_nal(&mut annex_b, nal);
                remaining = &remaining[len..];
            }
            assert!(has_idr);

            let mut decoder = Decoder::new().unwrap();
            let decoded = decoder.decode(&annex_b).unwrap().expect("decoded frame");
            assert_eq!(decoded.dimensions(), (64, 48));
            assert!(decoded.y().iter().any(|&y| y.abs_diff(decoded.y()[0]) > 20));

            first_rx.await.expect("first-frame readiness signal");
            assert_eq!(stats.frames_sent.load(Ordering::Relaxed), 1);
            assert_eq!(
                stats.bytes_sent.load(Ordering::Relaxed),
                (128 + video.len()) as u64
            );
            drop(frame_tx);
            let mut tail = Vec::new();
            socket.read_to_end(&mut tail).await.unwrap();
            // A slow test host may allow a heartbeat before closure is handled.
            assert_eq!(tail.len() % 128, 0);
            for heartbeat in tail.chunks_exact(128) {
                assert_eq!(&heartbeat[..4], &[0; 4]);
                assert_eq!(heartbeat[4], 2);
            }
        };
        let (result, ()) = tokio::join!(send, receive);
        result.unwrap();
        assert_eq!(stats.dropped_frames.load(Ordering::Relaxed), 1);
    })
    .await
    .expect("stream must finish after the frame sender closes");
}
