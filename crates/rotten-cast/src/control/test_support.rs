//! Shared mock receiver for `control` unit tests.
//!
//! The mock speaks the same framed Cast protocol over a `tokio::io::duplex`
//! loopback stream: no TLS, no network and no hardware. It is only compiled for
//! tests.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};

use super::proto::{self, CastMessage};
use super::session::Timeouts;

pub(crate) const TEST_URL: &str = "http://127.0.0.1:18080/live.m3u8";
pub(crate) const SESSION_ID: &str = "session-1";
pub(crate) const MEDIA_SESSION_ID: u64 = 41;
pub(crate) const TEST_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn test_timeouts() -> Timeouts {
    Timeouts {
        connect: Duration::from_secs(2),
        handshake: Duration::from_secs(5),
        write: Duration::from_secs(1),
        request: Duration::from_secs(2),
        playing: Duration::from_secs(2),
        heartbeat: Duration::from_millis(100),
        liveness: Duration::from_secs(5),
        poll: Duration::from_millis(10),
        stop: Duration::from_secs(2),
    }
}

/// Loopback transport that only accepts a byte budget per test phase and turns
/// into backpressure (`Poll::Pending`) afterwards. Tests use it to model a slow
/// socket whose write future is cancelled or times out mid-frame.
pub(crate) struct LimitedWriter {
    io: DuplexStream,
    budget: Arc<AtomicUsize>,
    chunk: usize,
}

impl LimitedWriter {
    pub(crate) fn new(io: DuplexStream, budget: Arc<AtomicUsize>, chunk: usize) -> Self {
        Self { io, budget, chunk }
    }
}

impl AsyncRead for LimitedWriter {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for LimitedWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let budget = self.budget.load(Ordering::Relaxed);
        let len = buf.len().min(self.chunk).min(budget);
        if len == 0 {
            // Backpressure with no waker: the caller's write deadline drives it.
            return Poll::Pending;
        }
        let written = match Pin::new(&mut self.io).poll_write(cx, &buf[..len]) {
            Poll::Ready(Ok(written)) => written,
            other => return other,
        };
        self.budget.fetch_sub(written, Ordering::Relaxed);
        Poll::Ready(Ok(written))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// Loopback transport that accepts exactly one byte per `delay`, modelling a
/// socket that trickles slowly. Used to prove that a single write deadline bounds
/// a whole frame instead of each partial write.
pub(crate) struct TrickleWriter {
    io: DuplexStream,
    delay: Duration,
    waiting: bool,
}

impl TrickleWriter {
    pub(crate) fn new(io: DuplexStream, delay: Duration) -> Self {
        Self {
            io,
            delay,
            waiting: false,
        }
    }
}

impl AsyncRead for TrickleWriter {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for TrickleWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if !self.waiting {
            // Schedule a wake after `delay` and report backpressure meanwhile.
            self.waiting = true;
            let waker = cx.waker().clone();
            let delay = self.delay;
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                waker.wake();
            });
            return Poll::Pending;
        }
        self.waiting = false;
        Pin::new(&mut self.io).poll_write(cx, &buf[..1])
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

pub(crate) struct MockReceiver {
    io: DuplexStream,
}

impl MockReceiver {
    pub(crate) fn new(io: DuplexStream) -> Self {
        Self { io }
    }

    pub(crate) async fn recv(&mut self) -> CastMessage {
        let mut header = [0u8; 4];
        self.io
            .read_exact(&mut header)
            .await
            .expect("mock: frame header");
        let length = u32::from_be_bytes(header) as usize;
        assert!(
            length > 0 && length <= proto::MAX_FRAME_BYTES,
            "mock: invalid frame length {length}"
        );
        let mut body = vec![0u8; length];
        self.io
            .read_exact(&mut body)
            .await
            .expect("mock: frame body");
        proto::decode_cast_message(&body).expect("mock: frame decodes")
    }

    pub(crate) async fn write_raw(&mut self, bytes: &[u8]) {
        self.io.write_all(bytes).await.expect("mock: write raw");
    }

    pub(crate) async fn send(
        &mut self,
        source: &str,
        destination: &str,
        namespace: &str,
        payload: Value,
    ) {
        let message = CastMessage::json(source, destination, namespace, &payload);
        self.write_raw(&message.encode_frame()).await;
    }

    pub(crate) async fn reply_pong(&mut self, ping: &CastMessage) {
        self.send(
            proto::RECEIVER_ID,
            &ping.source,
            proto::NS_HEARTBEAT,
            json!({"type": "PONG"}),
        )
        .await;
    }

    /// Receive the next non-heartbeat message, answering client PINGs so the
    /// session's liveness timer stays satisfied.
    pub(crate) async fn expect_message(&mut self, namespace: &str, kind: &str) -> CastMessage {
        loop {
            let message = self.recv().await;
            if is_type(&message, proto::NS_HEARTBEAT, "PING") {
                self.reply_pong(&message).await;
                continue;
            }
            assert_eq!(
                message.namespace, namespace,
                "unexpected namespace (kind {kind})"
            );
            assert!(
                is_type(&message, namespace, kind),
                "unexpected message kind, wanted {kind}"
            );
            return message;
        }
    }
}

pub(crate) fn payload_of(message: &CastMessage) -> Value {
    message.payload_json().expect("message payload is JSON")
}

pub(crate) fn request_id_of(message: &CastMessage) -> u64 {
    payload_of(message)
        .get("requestId")
        .and_then(Value::as_u64)
        .expect("message carries a requestId")
}

pub(crate) fn is_type(message: &CastMessage, namespace: &str, kind: &str) -> bool {
    message.namespace == namespace
        && payload_of(message)
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|value| value == kind)
}

pub(crate) fn app_entry(app_id: &str, session_id: &str, display_name: &str, media: bool) -> Value {
    app_entry_with_transport(app_id, session_id, session_id, display_name, media)
}

pub(crate) fn app_entry_with_transport(
    app_id: &str,
    session_id: &str,
    transport_id: &str,
    display_name: &str,
    media: bool,
) -> Value {
    let namespaces = if media {
        json!([{"name": proto::NS_MEDIA}])
    } else {
        json!([])
    };
    json!({
        "appId": app_id,
        "sessionId": session_id,
        "transportId": transport_id,
        "displayName": display_name,
        "namespaces": namespaces,
    })
}

pub(crate) fn receiver_status(request_id: Option<u64>, applications: Vec<Value>) -> Value {
    let mut payload = json!({"type": "RECEIVER_STATUS", "status": {"applications": applications}});
    if let Some(request_id) = request_id {
        payload["requestId"] = json!(request_id);
    }
    payload
}

pub(crate) fn media_status(
    request_id: Option<u64>,
    player_state: &str,
    media_session_id: u64,
) -> Value {
    let mut payload = json!({
        "type": "MEDIA_STATUS",
        "status": [{"mediaSessionId": media_session_id, "playerState": player_state}],
    });
    if let Some(request_id) = request_id {
        payload["requestId"] = json!(request_id);
    }
    payload
}

pub(crate) async fn complete_handshake(server: &mut MockReceiver) {
    let connect = server.recv().await;
    assert!(
        is_type(&connect, proto::NS_CONNECTION, "CONNECT"),
        "expected CONNECT, got {}",
        connect.namespace
    );
    assert_eq!(connect.destination, proto::RECEIVER_ID);
    let ping = server.recv().await;
    assert!(is_type(&ping, proto::NS_HEARTBEAT, "PING"));
    server.reply_pong(&ping).await;
}

pub(crate) async fn expect_get_status_and_reply(
    server: &mut MockReceiver,
    applications: Vec<Value>,
) {
    let get = loop {
        let message = server.recv().await;
        if is_type(&message, proto::NS_HEARTBEAT, "PING") {
            server.reply_pong(&message).await;
            continue;
        }
        if is_type(&message, proto::NS_MEDIA, "GET_STATUS") {
            // A steady-state poll already in flight when playback stopped. A
            // real receiver answers it; cleanup correlation ignores the answer
            // because it carries a different request id.
            server
                .send(
                    SESSION_ID,
                    "*",
                    proto::NS_MEDIA,
                    media_status(Some(request_id_of(&message)), "PLAYING", MEDIA_SESSION_ID),
                )
                .await;
            continue;
        }
        assert_eq!(
            message.namespace,
            proto::NS_RECEIVER,
            "unexpected namespace (kind GET_STATUS)"
        );
        assert!(
            is_type(&message, proto::NS_RECEIVER, "GET_STATUS"),
            "unexpected message kind, wanted GET_STATUS"
        );
        break message;
    };
    // Real receivers broadcast status updates with a `*` destination.
    server
        .send(
            proto::RECEIVER_ID,
            "*",
            proto::NS_RECEIVER,
            receiver_status(Some(request_id_of(&get)), applications),
        )
        .await;
}

/// Answer LAUNCH with a fresh Default Media Receiver session.
pub(crate) async fn script_launch(server: &mut MockReceiver) {
    let launch = server.expect_message(proto::NS_RECEIVER, "LAUNCH").await;
    assert_eq!(
        payload_of(&launch).get("appId").and_then(Value::as_str),
        Some(proto::DMR_APP_ID)
    );
    server
        .send(
            proto::RECEIVER_ID,
            "*",
            proto::NS_RECEIVER,
            receiver_status(
                Some(request_id_of(&launch)),
                vec![app_entry(
                    proto::DMR_APP_ID,
                    SESSION_ID,
                    "Default Media Receiver",
                    true,
                )],
            ),
        )
        .await;
}

/// Expect the transport CONNECT and the live HLS LOAD, validating its shape.
pub(crate) async fn script_connect_and_load(server: &mut MockReceiver) -> CastMessage {
    let connect = server.expect_message(proto::NS_CONNECTION, "CONNECT").await;
    assert_eq!(connect.destination, SESSION_ID);

    let load = server.expect_message(proto::NS_MEDIA, "LOAD").await;
    assert_eq!(load.destination, SESSION_ID);
    let payload = payload_of(&load);
    let media = &payload["media"];
    assert_eq!(
        media.get("contentId").and_then(Value::as_str),
        Some(TEST_URL)
    );
    assert_eq!(
        media.get("streamType").and_then(Value::as_str),
        Some("LIVE")
    );
    assert_eq!(
        media.get("contentType").and_then(Value::as_str),
        Some("application/x-mpegurl")
    );
    assert!(
        media.get("currentTime").is_none(),
        "LOAD must omit currentTime"
    );
    assert_eq!(
        payload.get("autoplay").and_then(Value::as_bool),
        Some(true),
        "LOAD must request autoplay at the root"
    );
    assert!(
        media.get("autoplay").is_none(),
        "autoplay must not be nested under media"
    );
    load
}

pub(crate) async fn script_launch_and_play(server: &mut MockReceiver) {
    script_launch(server).await;
    let load = script_connect_and_load(server).await;
    server
        .send(
            SESSION_ID,
            "*",
            proto::NS_MEDIA,
            media_status(Some(request_id_of(&load)), "PLAYING", MEDIA_SESSION_ID),
        )
        .await;
}

/// Handshake, reject-busy status check and playback startup.
pub(crate) async fn script_to_playing(server: &mut MockReceiver) {
    complete_handshake(server).await;
    expect_get_status_and_reply(server, Vec::new()).await;
    script_launch_and_play(server).await;
}

/// Verify the owned session still exists, then expect the STOP sequence.
pub(crate) async fn script_cleanup_after_playing(server: &mut MockReceiver) {
    expect_get_status_and_reply(
        server,
        vec![app_entry(
            proto::DMR_APP_ID,
            SESSION_ID,
            "Default Media Receiver",
            true,
        )],
    )
    .await;
    let media_get = server.expect_message(proto::NS_MEDIA, "GET_STATUS").await;
    assert_eq!(media_get.destination, SESSION_ID);
    server
        .send(
            SESSION_ID,
            "*",
            proto::NS_MEDIA,
            media_status(Some(request_id_of(&media_get)), "PLAYING", MEDIA_SESSION_ID),
        )
        .await;
    let media_stop = server.expect_message(proto::NS_MEDIA, "STOP").await;
    assert_eq!(
        payload_of(&media_stop)
            .get("mediaSessionId")
            .and_then(Value::as_u64),
        Some(MEDIA_SESSION_ID)
    );
    let app_stop = server.expect_message(proto::NS_RECEIVER, "STOP").await;
    assert_eq!(
        payload_of(&app_stop)
            .get("sessionId")
            .and_then(Value::as_str),
        Some(SESSION_ID)
    );
    let close = server.expect_message(proto::NS_CONNECTION, "CLOSE").await;
    assert_eq!(close.destination, SESSION_ID);
}

/// Collect frames until `window` of silence passes, answering client PINGs.
pub(crate) async fn drain_messages(
    server: &mut MockReceiver,
    window: Duration,
) -> Vec<CastMessage> {
    let mut seen = Vec::new();
    loop {
        match tokio::time::timeout(window, server.recv()).await {
            Ok(message) => {
                if is_type(&message, proto::NS_HEARTBEAT, "PING") {
                    server.reply_pong(&message).await;
                }
                seen.push(message);
            }
            Err(_) => return seen,
        }
    }
}
