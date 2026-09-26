//! Bounded Google Cast V2 receiver/media session driver.
//!
//! The driver owns the transport; it never spawns detached tasks. Frame reads are
//! incremental and keep partial-frame state in [`proto::FrameReader`], so a cancelled
//! `stream` future can be followed by [`Session::cleanup`] on the same connection
//! without losing framing state.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::time::{Instant, sleep_until, timeout_at};

use super::proto::{self, CastMessage, CodecError, FrameError, FrameReader};

/// A transport that can be boxed behind [`BoxedTransport`].
pub(crate) trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> Transport for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

/// Type-erased transport used by [`crate::CastClient`].
pub(crate) type BoxedTransport = Box<dyn Transport + Send>;

/// Deadlines and cadences used by the session driver.
///
/// The defaults follow the Cast V2 keep-alive convention (heartbeat every five
/// seconds, fifteen-second liveness window); tests override them with short values.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timeouts {
    /// Overall budget for [`crate::CastClient::connect`].
    pub(crate) connect: Duration,
    /// Budget for TCP/TLS plus the protocol CONNECT/PING exchange.
    pub(crate) handshake: Duration,
    /// Budget for a single frame write.
    pub(crate) write: Duration,
    /// Budget for a request/response exchange.
    pub(crate) request: Duration,
    /// Budget for the LOAD round trip to reach `PLAYING`.
    pub(crate) playing: Duration,
    /// Interval between outbound heartbeat PINGs; also paces the steady-state
    /// media status poll.
    pub(crate) heartbeat: Duration,
    /// Maximum quiet time before the receiver is considered dead.
    pub(crate) liveness: Duration,
    /// Cancellation polling cadence; the stream never sleeps longer than this.
    pub(crate) poll: Duration,
    /// Budget for best-effort STOP cleanup.
    pub(crate) stop: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(15),
            handshake: Duration::from_secs(10),
            write: Duration::from_secs(5),
            request: Duration::from_secs(10),
            playing: Duration::from_secs(15),
            heartbeat: Duration::from_secs(5),
            liveness: Duration::from_secs(15),
            poll: Duration::from_millis(100),
            stop: Duration::from_secs(3),
        }
    }
}

/// Session-level failures. No variant carries media URLs or payload bodies.
#[derive(Debug)]
pub(crate) enum SessionError {
    /// The transport failed.
    Io(std::io::Error),
    /// A frame could not be read.
    Frame(FrameError),
    /// A Cast message could not be decoded.
    Codec(CodecError),
    /// A bounded wait expired.
    Timeout(&'static str),
    /// The receiver stopped sending traffic within the liveness window.
    Liveness,
    /// The caller requested cancellation.
    Stopped,
    /// The receiver closed the transport.
    Closed,
    /// The receiver already runs a media application.
    Busy(String),
    /// The receiver rejected `LAUNCH`.
    LaunchFailed(String),
    /// The receiver rejected `LOAD`.
    LoadFailed(&'static str),
    /// The media session left `PLAYING` for `IDLE`.
    MediaIdle,
    /// The application or transport was replaced by someone else.
    Replaced,
    /// Cleanup could not verify the media session and left everything running.
    CleanupUnverified,
    /// The received protocol state was impossible.
    Protocol(&'static str),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "cast transport error: {error}"),
            Self::Frame(error) => write!(f, "{error}"),
            Self::Codec(error) => write!(f, "cast protocol error: {error}"),
            Self::Timeout(what) => write!(f, "timed out waiting for {what} from the receiver"),
            Self::Liveness => {
                write!(
                    f,
                    "receiver stopped responding (no Cast traffic within the liveness window)"
                )
            }
            Self::Stopped => write!(f, "cast stream cancelled"),
            Self::Closed => write!(f, "receiver closed the Cast connection"),
            Self::Busy(app) => write!(f, "receiver is busy with application {app}"),
            Self::LaunchFailed(code) => write!(f, "receiver rejected LAUNCH ({code})"),
            Self::LoadFailed(kind) => write!(f, "receiver failed to load media ({kind})"),
            Self::MediaIdle => write!(f, "receiver media session went idle"),
            Self::Replaced => write!(f, "Cast application or transport was replaced"),
            Self::CleanupUnverified => write!(
                f,
                "could not verify the Cast media session before stopping; left it running"
            ),
            Self::Protocol(what) => write!(f, "cast protocol violation: {what}"),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Frame(error) => Some(error),
            Self::Codec(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SessionError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<FrameError> for SessionError {
    fn from(error: FrameError) -> Self {
        Self::Frame(error)
    }
}

impl From<CodecError> for SessionError {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

/// A receiver application session that this client launched and still owns.
#[derive(Debug, Clone)]
pub(crate) struct OwnedSession {
    pub(crate) session_id: String,
    pub(crate) transport_id: String,
    pub(crate) media_session_id: Option<u64>,
    pub(crate) playing: bool,
    /// Whether the launched application advertised the media namespace.
    pub(crate) media_namespace: bool,
}

/// What a bounded wait is expecting next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitGoal {
    /// The receiver's reply to the initial heartbeat PING.
    Pong,
    /// A correlated RECEIVER_STATUS.
    ReceiverStatus,
    /// The new application session from LAUNCH.
    Launch,
    /// `PLAYING` media status from LOAD.
    Playing,
    /// A correlated MEDIA_STATUS answering a cleanup query.
    MediaStatus,
    /// Steady-state playback until cancellation or failure.
    Running,
}

impl WaitGoal {
    fn label(self) -> &'static str {
        match self {
            Self::Pong => "Cast handshake PONG",
            Self::ReceiverStatus => "receiver status",
            Self::Launch => "application launch",
            Self::Playing => "media playback start",
            Self::MediaStatus => "media status",
            Self::Running => "media session",
        }
    }
}

/// Successful results of a bounded wait.
#[derive(Debug)]
enum WaitOutcome {
    Pong,
    Status(Value),
    LaunchReady,
    Playing,
    StopRequested,
}

/// The request a bounded wait is correlating against.
#[derive(Debug)]
struct Expect {
    request_id: u64,
    namespace: &'static str,
    destination: String,
    /// While true, any RECEIVER_STATUS showing a non-backdrop media application
    /// aborts with [`SessionError::Busy`] instead of being ignored.
    busy_guard: bool,
}

/// One Cast V2 control connection.
pub(crate) struct Session<S> {
    io: S,
    reader: FrameReader,
    sender: String,
    next_request_id: u64,
    timeouts: Timeouts,
    owned: Option<OwnedSession>,
    /// Request id of an in-flight LAUNCH that has not produced an owned session.
    pending_launch: Option<u64>,
    /// Outbound frame that has not been fully written yet, plus how many bytes
    /// the receiver already accepted. Retained across cancellation and timeouts
    /// so a resumed write never starts a second frame mid-way.
    outbound: Vec<u8>,
    outbound_offset: usize,
    last_rx: Instant,
    next_ping: Instant,
    /// When the steady-state media status poll (heartbeat cadence) is due.
    next_media_status_poll: Instant,
    poisoned: bool,
}

impl<S> Session<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub(crate) fn new(io: S, timeouts: Timeouts) -> Self {
        let now = Instant::now();
        Self {
            io,
            reader: FrameReader::new(),
            sender: "sender-0".to_owned(),
            next_request_id: 0,
            timeouts,
            owned: None,
            pending_launch: None,
            outbound: Vec::new(),
            outbound_offset: 0,
            last_rx: now,
            next_ping: now + timeouts.heartbeat,
            next_media_status_poll: now + timeouts.heartbeat,
            poisoned: false,
        }
    }

    /// Open the virtual connection to `receiver-0` and prove liveness with a PING.
    pub(crate) async fn handshake(
        &mut self,
        stop: Option<&AtomicBool>,
    ) -> Result<(), SessionError> {
        let deadline = Instant::now() + self.timeouts.handshake;
        self.send_json(
            proto::NS_CONNECTION,
            proto::RECEIVER_ID,
            &json!({"type": "CONNECT"}),
            Some(deadline),
        )
        .await?;
        self.send_json(
            proto::NS_HEARTBEAT,
            proto::RECEIVER_ID,
            &json!({"type": "PING"}),
            Some(deadline),
        )
        .await?;
        self.next_ping = Instant::now() + self.timeouts.heartbeat;

        match self
            .pump_once(stop, WaitGoal::Pong, None, Some(deadline))
            .await?
        {
            WaitOutcome::Pong => Ok(()),
            WaitOutcome::StopRequested => Err(SessionError::Stopped),
            _ => Err(SessionError::Protocol("unexpected handshake outcome")),
        }
    }

    /// Send GET_STATUS and return the correlated RECEIVER_STATUS payload.
    ///
    /// No application is launched.
    pub(crate) async fn receiver_status(
        &mut self,
        stop: Option<&AtomicBool>,
    ) -> Result<Value, SessionError> {
        self.get_status(stop, false).await
    }

    /// Launch the Default Media Receiver and stream `url` until `stop` is set.
    ///
    /// The callback runs exactly once after the receiver reports `PLAYING` and
    /// before the steady-state loop starts. On every exit path after an owned
    /// application session exists, a bounded STOP is sent for that session only.
    /// Cleanup failures are surfaced when the stream itself ended successfully,
    /// otherwise the original stream error is preserved.
    pub(crate) async fn stream(
        &mut self,
        url: &str,
        stop: &AtomicBool,
        on_playing: Option<Box<dyn FnOnce() + Send + 'static>>,
    ) -> Result<(), SessionError> {
        let result = self.run_stream(url, stop, on_playing).await;
        match (result, self.cleanup().await) {
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cleanup_error)) => {
                tracing::warn!(error = %cleanup_error, "Cast session cleanup failed");
                Err(error)
            }
        }
    }

    /// Bounded STOP for the application session this client launched.
    ///
    /// Intended for orchestrators whose `stream` future was dropped before it could
    /// clean up. The first half of the stop budget verifies ownership with fresh
    /// receiver and media status queries (issued sequentially, so an early media
    /// reply can never be consumed by the receiver wait); the rest is reserved for
    /// STOP writes.
    /// A replaced application or media session is never stopped. When a media
    /// session was loaded but cannot be verified, nothing is stopped and
    /// [`SessionError::CleanupUnverified`] is returned so the caller can log it
    /// rather than disrupt someone else's playback. The application STOP is scoped
    /// to the exact owned `sessionId`; without a known media session it is safe even
    /// when the receiver status was unverified, because it cannot affect another
    /// sender's media. Write failures are reported and the owned session is kept so
    /// a later `stop()` can retry.
    pub(crate) async fn cleanup(&mut self) -> Result<(), SessionError> {
        let started = Instant::now();
        let stop_budget = self.timeouts.stop;
        let settle_deadline = started + stop_budget / 4;
        let status_deadline = started + stop_budget / 2;
        let write_deadline = started + stop_budget;

        // A frame left half-written by a cancelled stream (possibly the LAUNCH
        // itself) is finished first so the receiver can answer it.
        if !self.poisoned
            && !self.outbound.is_empty()
            && let Err(error) = self.flush_outbound(settle_deadline).await
        {
            tracing::debug!(error = %error, "failed to flush a pending Cast frame during cleanup");
        }

        // A LAUNCH that never produced its correlated response may still have
        // started an application. Drain for that response before deciding
        // anything; an unsolicited DMR from someone else is not evidence.
        if self.owned.is_none() && self.pending_launch.is_some() {
            self.settle_pending_launch(settle_deadline).await;
        }
        self.pending_launch = None;

        let Some(owned) = self.owned.clone() else {
            return Ok(());
        };

        // Ask the receiver first. The media query is issued only after the
        // receiver answer has been processed; otherwise an early correlated
        // MEDIA_STATUS would be consumed by the receiver wait and the media wait
        // would never see it.
        let receiver_request_id = self.next_request_id();
        let receiver_sent = self
            .send_json(
                proto::NS_RECEIVER,
                proto::RECEIVER_ID,
                &json!({"type": "GET_STATUS", "requestId": receiver_request_id}),
                Some(status_deadline),
            )
            .await
            .is_ok();

        let mut ownership = CleanupOwnership::Unknown;
        if receiver_sent {
            let expect = Expect {
                request_id: receiver_request_id,
                namespace: proto::NS_RECEIVER,
                destination: proto::RECEIVER_ID.to_owned(),
                busy_guard: false,
            };
            match self
                .pump_once(
                    None,
                    WaitGoal::ReceiverStatus,
                    Some(&expect),
                    Some(status_deadline),
                )
                .await
            {
                Ok(WaitOutcome::Status(status)) => {
                    ownership = match receiver_app_by_session(&status, &owned.session_id) {
                        Some(app) => match app.transport_id.as_deref() {
                            Some(transport_id) if transport_id != owned.transport_id => {
                                CleanupOwnership::Replaced
                            }
                            _ => CleanupOwnership::Ours,
                        },
                        None => CleanupOwnership::Replaced,
                    };
                }
                Err(SessionError::Replaced) => ownership = CleanupOwnership::Replaced,
                // Unverified: a media-less application STOP is still exact-session safe.
                _ => {}
            }
        }
        if ownership == CleanupOwnership::Replaced {
            self.owned = None;
            return Ok(());
        }

        // Now that the receiver answer was processed, ask the media namespace.
        let media_request_id = if owned.media_session_id.is_some() {
            let request_id = self.next_request_id();
            self.send_json(
                proto::NS_MEDIA,
                &owned.transport_id,
                &json!({"type": "GET_STATUS", "requestId": request_id}),
                Some(status_deadline),
            )
            .await
            .is_ok()
            .then_some(request_id)
        } else {
            None
        };

        // Verify the media session as well: a same-application takeover swaps the
        // media session id without changing the application session id.
        let mut media_ownership = if owned.media_session_id.is_none() {
            CleanupOwnership::Ours
        } else {
            CleanupOwnership::Unknown
        };
        if let (Some(media_request_id), Some(known_media_id)) =
            (media_request_id, owned.media_session_id)
        {
            let expect = Expect {
                request_id: media_request_id,
                namespace: proto::NS_MEDIA,
                destination: owned.transport_id.clone(),
                busy_guard: false,
            };
            match self
                .pump_once(
                    None,
                    WaitGoal::MediaStatus,
                    Some(&expect),
                    Some(status_deadline),
                )
                .await
            {
                Ok(WaitOutcome::Status(status)) => {
                    media_ownership = match media_session_id_of(&status) {
                        Some(media_session_id) if media_session_id == known_media_id => {
                            CleanupOwnership::Ours
                        }
                        Some(_) => CleanupOwnership::Replaced,
                        None => CleanupOwnership::Unknown,
                    };
                }
                Err(SessionError::Replaced) => media_ownership = CleanupOwnership::Replaced,
                _ => {}
            }
        }
        match media_ownership {
            CleanupOwnership::Replaced => {
                self.owned = None;
                return Ok(());
            }
            CleanupOwnership::Unknown => {
                // Stopping the application could disrupt media that is no longer
                // ours, so leave everything running and report the inability.
                tracing::warn!(
                    "Cast cleanup could not verify the media session; leaving it running"
                );
                return Err(SessionError::CleanupUnverified);
            }
            CleanupOwnership::Ours => {}
        }

        let mut write_error: Option<SessionError> = None;
        if let Some(media_session_id) = owned.media_session_id {
            let payload = json!({
                "type": "STOP",
                "requestId": self.next_request_id(),
                "mediaSessionId": media_session_id,
            });
            if let Err(error) = self
                .send_json(
                    proto::NS_MEDIA,
                    &owned.transport_id,
                    &payload,
                    Some(write_deadline),
                )
                .await
            {
                tracing::debug!(error = %error, "failed to send media STOP to receiver");
                write_error = Some(error);
            }
        }

        let payload = json!({
            "type": "STOP",
            "requestId": self.next_request_id(),
            "sessionId": owned.session_id,
        });
        if let Err(error) = self
            .send_json(
                proto::NS_RECEIVER,
                proto::RECEIVER_ID,
                &payload,
                Some(write_deadline),
            )
            .await
        {
            tracing::debug!(error = %error, "failed to send application STOP to receiver");
            if write_error.is_none() {
                write_error = Some(error);
            }
        }

        if let Err(error) = self
            .send_json(
                proto::NS_CONNECTION,
                &owned.transport_id,
                &json!({"type": "CLOSE"}),
                Some(write_deadline),
            )
            .await
        {
            // The application STOP already went out; a failed CLOSE is harmless.
            tracing::debug!(error = %error, "failed to close Cast transport connection");
        }

        match write_error {
            Some(error) => Err(error),
            None => {
                self.owned = None;
                Ok(())
            }
        }
    }

    /// Boundedly recover a correlated LAUNCH response after cancellation.
    async fn settle_pending_launch(&mut self, deadline: Instant) {
        let Some(request_id) = self.pending_launch else {
            return;
        };
        let expect = Expect {
            request_id,
            namespace: proto::NS_RECEIVER,
            destination: proto::RECEIVER_ID.to_owned(),
            busy_guard: false,
        };
        if let Ok(WaitOutcome::LaunchReady) = self
            .pump_once(None, WaitGoal::Launch, Some(&expect), Some(deadline))
            .await
        {
            tracing::info!("recovered the Cast launch response during cleanup");
        }
        // Bounded: without correlated evidence nothing is stopped; the
        // application might belong to someone else.
    }

    async fn run_stream(
        &mut self,
        url: &str,
        stop: &AtomicBool,
        on_playing: Option<Box<dyn FnOnce() + Send + 'static>>,
    ) -> Result<(), SessionError> {
        if self.owned.is_some() || self.pending_launch.is_some() {
            return Err(SessionError::Protocol(
                "a Cast session is already active; call stop() first",
            ));
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Refuse to take over a receiver that is already playing media.
        match self.get_status(Some(stop), true).await {
            Ok(_) => {}
            Err(SessionError::Stopped) => return Ok(()),
            Err(error) => return Err(error),
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        let request_id = self.next_request_id();
        self.pending_launch = Some(request_id);
        self.send_json(
            proto::NS_RECEIVER,
            proto::RECEIVER_ID,
            &json!({
                "type": "LAUNCH",
                "appId": proto::DMR_APP_ID,
                "requestId": request_id,
            }),
            None,
        )
        .await?;
        let expect = Expect {
            request_id,
            namespace: proto::NS_RECEIVER,
            destination: proto::RECEIVER_ID.to_owned(),
            busy_guard: false,
        };
        let deadline = Instant::now() + self.timeouts.request;
        match self
            .pump_once(Some(stop), WaitGoal::Launch, Some(&expect), Some(deadline))
            .await
        {
            Ok(WaitOutcome::LaunchReady) => {}
            // The in-flight launch is kept for cleanup; it may still have started
            // an application.
            Ok(WaitOutcome::StopRequested) => return Ok(()),
            Ok(_) => return Err(SessionError::Protocol("unexpected LAUNCH outcome")),
            Err(error) => return Err(error),
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        let Some(owned) = self.owned.as_ref() else {
            return Err(SessionError::Protocol(
                "LAUNCH succeeded without an application session",
            ));
        };
        if !owned.media_namespace {
            return Err(SessionError::Protocol(
                "launched application does not advertise the media namespace",
            ));
        }
        let transport_id = owned.transport_id.clone();
        self.send_json(
            proto::NS_CONNECTION,
            &transport_id,
            &json!({"type": "CONNECT"}),
            None,
        )
        .await?;

        let request_id = self.next_request_id();
        let load = json!({
            "type": "LOAD",
            "requestId": request_id,
            "autoplay": true,
            "media": {
                "contentId": url,
                "streamType": "LIVE",
                "contentType": "application/x-mpegurl",
            },
        });
        self.send_json(proto::NS_MEDIA, &transport_id, &load, None)
            .await?;
        let expect = Expect {
            request_id,
            namespace: proto::NS_MEDIA,
            destination: transport_id,
            busy_guard: false,
        };
        let deadline = Instant::now() + self.timeouts.playing;
        match self
            .pump_once(Some(stop), WaitGoal::Playing, Some(&expect), Some(deadline))
            .await
        {
            Ok(WaitOutcome::Playing) => {}
            Ok(WaitOutcome::StopRequested) => return Ok(()),
            Ok(_) => return Err(SessionError::Protocol("unexpected LOAD outcome")),
            Err(error) => return Err(error),
        }

        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Some(callback) = on_playing {
            callback();
        }

        // The first steady-state poll is one heartbeat after playback starts, so
        // startup and cleaned-up sessions never race an extra status request.
        self.next_media_status_poll = Instant::now() + self.timeouts.heartbeat;
        match self
            .pump_once(Some(stop), WaitGoal::Running, None, None)
            .await?
        {
            WaitOutcome::StopRequested => Ok(()),
            _ => Err(SessionError::Protocol("unexpected steady-state outcome")),
        }
    }

    async fn get_status(
        &mut self,
        stop: Option<&AtomicBool>,
        busy_guard: bool,
    ) -> Result<Value, SessionError> {
        let request_id = self.next_request_id();
        self.send_json(
            proto::NS_RECEIVER,
            proto::RECEIVER_ID,
            &json!({"type": "GET_STATUS", "requestId": request_id}),
            None,
        )
        .await?;
        let expect = Expect {
            request_id,
            namespace: proto::NS_RECEIVER,
            destination: proto::RECEIVER_ID.to_owned(),
            busy_guard,
        };
        let deadline = Instant::now() + self.timeouts.request;
        match self
            .pump_once(
                stop,
                WaitGoal::ReceiverStatus,
                Some(&expect),
                Some(deadline),
            )
            .await?
        {
            WaitOutcome::Status(status) => Ok(status),
            WaitOutcome::StopRequested => Err(SessionError::Stopped),
            _ => Err(SessionError::Protocol("unexpected GET_STATUS outcome")),
        }
    }

    /// Wait for one of the events the `goal` cares about, servicing heartbeats,
    /// cancellation polls and liveness checks in the meantime.
    async fn pump_once(
        &mut self,
        stop: Option<&AtomicBool>,
        goal: WaitGoal,
        expect: Option<&Expect>,
        deadline: Option<Instant>,
    ) -> Result<WaitOutcome, SessionError> {
        loop {
            let now = Instant::now();
            if deadline.is_some_and(|deadline| now >= deadline) {
                return Err(SessionError::Timeout(goal.label()));
            }
            if stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
                return Ok(WaitOutcome::StopRequested);
            }
            if now.saturating_duration_since(self.last_rx) >= self.timeouts.liveness {
                return Err(SessionError::Liveness);
            }
            if now >= self.next_ping {
                self.send_json(
                    proto::NS_HEARTBEAT,
                    proto::RECEIVER_ID,
                    &json!({"type": "PING"}),
                    None,
                )
                .await?;
                self.next_ping = now + self.timeouts.heartbeat;
            }

            // Steady-state telemetry only: one fire-and-forget GET_STATUS to the
            // owned media transport at the heartbeat cadence. The reply is
            // classified by the regular media path (or ignored when unrelated),
            // so it can never take over another session or race a cleanup query,
            // whose correlation is bound to its own request id.
            if goal == WaitGoal::Running && now >= self.next_media_status_poll {
                let transport_id = self.owned.as_ref().map(|owned| owned.transport_id.clone());
                if let Some(transport_id) = transport_id {
                    let request_id = self.next_request_id();
                    self.send_json(
                        proto::NS_MEDIA,
                        &transport_id,
                        &json!({"type": "GET_STATUS", "requestId": request_id}),
                        None,
                    )
                    .await?;
                }
                self.next_media_status_poll = now + self.timeouts.heartbeat;
            }

            let mut wake = now + self.timeouts.poll;
            if goal == WaitGoal::Running {
                wake = wake.min(self.next_media_status_poll);
            }
            if let Some(deadline) = deadline {
                wake = wake.min(deadline);
            }
            wake = wake
                .min(self.next_ping)
                .min(self.last_rx + self.timeouts.liveness);

            let incoming = tokio::select! {
                result = self.reader.next_frame(&mut self.io) => Some(result),
                _ = sleep_until(wake) => None,
            };
            if let Some(result) = incoming {
                let body = match result.map_err(SessionError::Frame)? {
                    Some(body) => body,
                    None => return Err(SessionError::Closed),
                };
                let message = proto::decode_cast_message(&body).map_err(SessionError::Codec)?;
                self.last_rx = Instant::now();
                if let Some(outcome) = self.classify(goal, expect, &message).await? {
                    return Ok(outcome);
                }
            }
        }
    }

    /// Classify one inbound message, updating owned-session state as needed.
    async fn classify(
        &mut self,
        goal: WaitGoal,
        expect: Option<&Expect>,
        message: &CastMessage,
    ) -> Result<Option<WaitOutcome>, SessionError> {
        // Real receivers broadcast statuses with a `*` destination; correlated
        // replies and requests are addressed to our sender.
        let addressed_to_us = message.destination == self.sender || message.destination == "*";
        if !addressed_to_us {
            tracing::debug!(namespace = %message.namespace, "ignoring Cast message for another destination");
            return Ok(None);
        }
        if message.payload_type != 0 {
            tracing::debug!(namespace = %message.namespace, "ignoring binary Cast payload");
            return Ok(None);
        }
        let payload = message.payload_json().map_err(SessionError::Codec)?;
        let Some(kind) = payload.get("type").and_then(Value::as_str) else {
            tracing::debug!(namespace = %message.namespace, "ignoring Cast message without a type");
            return Ok(None);
        };

        if message.namespace == proto::NS_HEARTBEAT {
            // Heartbeats are only trusted from the platform receiver.
            if message.source != proto::RECEIVER_ID {
                tracing::debug!(source = %message.source, "ignoring heartbeat from a non-receiver source");
                return Ok(None);
            }
            match kind {
                "PING" => {
                    let reply = CastMessage::json(
                        &self.sender,
                        &message.source,
                        proto::NS_HEARTBEAT,
                        &json!({"type": "PONG"}),
                    );
                    self.write_message(&reply, None).await?;
                    return Ok(None);
                }
                "PONG" => {
                    return Ok((goal == WaitGoal::Pong).then_some(WaitOutcome::Pong));
                }
                _ => return Ok(None),
            }
        }

        let response_matches = self.response_matches(expect, message, &payload);
        let error_matches = self.error_responsible(expect, message, &payload);

        if kind == "INVALID_REQUEST" {
            if error_matches {
                self.pending_launch = None;
                return Err(SessionError::Protocol(
                    "receiver rejected a request as invalid",
                ));
            }
            tracing::debug!(namespace = %message.namespace, "ignoring unrelated INVALID_REQUEST");
            return Ok(None);
        }
        if kind == "LAUNCH_ERROR" {
            if self.pending_launch.is_some()
                && message.namespace == proto::NS_RECEIVER
                && message.source == proto::RECEIVER_ID
                && error_matches
            {
                self.pending_launch = None;
                let code = payload
                    .get("errorCode")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                return Err(SessionError::LaunchFailed(code.to_owned()));
            }
            tracing::debug!(namespace = %message.namespace, "ignoring unrelated LAUNCH_ERROR");
            return Ok(None);
        }

        match message.namespace.as_str() {
            proto::NS_RECEIVER if kind == "RECEIVER_STATUS" => {
                // Ownership is only ever changed by the platform receiver.
                if message.source != proto::RECEIVER_ID {
                    tracing::debug!(source = %message.source, "ignoring receiver status from a non-receiver source");
                    return Ok(None);
                }
                if expect.is_some_and(|expect| expect.busy_guard)
                    && self.pending_launch.is_none()
                    && let Some(app) = busy_media_app(&payload)
                {
                    return Err(SessionError::Busy(app));
                }
                if let Some(owned) = self.owned.as_ref() {
                    match receiver_app_by_session(&payload, &owned.session_id) {
                        Some(app) => {
                            let transport_changed = app
                                .transport_id
                                .as_deref()
                                .is_some_and(|transport_id| transport_id != owned.transport_id);
                            if transport_changed {
                                self.owned = None;
                                return Err(SessionError::Replaced);
                            }
                        }
                        None => {
                            self.owned = None;
                            return Err(SessionError::Replaced);
                        }
                    }
                } else if self.pending_launch.is_some() && response_matches {
                    // Only a correlated LAUNCH response proves the application is ours.
                    if let Some(app) = receiver_app_by_id(&payload, proto::DMR_APP_ID) {
                        let Some(transport_id) = app.transport_id.clone() else {
                            self.pending_launch = None;
                            return Err(SessionError::Protocol(
                                "LAUNCH response is missing transportId",
                            ));
                        };
                        tracing::info!(session = %app.session_id, "default media receiver launched");
                        self.owned = Some(OwnedSession {
                            session_id: app.session_id,
                            transport_id,
                            media_session_id: None,
                            playing: false,
                            media_namespace: app.has_media_namespace,
                        });
                        self.pending_launch = None;
                        if goal == WaitGoal::Launch {
                            return Ok(Some(WaitOutcome::LaunchReady));
                        }
                    }
                    // Correlated status without the DMR: wait for a later status.
                }
                if goal == WaitGoal::ReceiverStatus && response_matches {
                    return Ok(Some(WaitOutcome::Status(payload)));
                }
                Ok(None)
            }
            proto::NS_MEDIA => {
                self.classify_media(goal, expect, message, kind, &payload)
                    .await
            }
            proto::NS_CONNECTION if kind == "CLOSE" => {
                let expected = self.owned.as_ref().map(|owned| owned.transport_id.as_str());
                if expected == Some(message.source.as_str()) || message.source == proto::RECEIVER_ID
                {
                    Err(SessionError::Closed)
                } else {
                    tracing::debug!(source = %message.source, "ignoring CLOSE for another transport");
                    Ok(None)
                }
            }
            _ => {
                tracing::debug!(namespace = %message.namespace, kind, "ignoring Cast message");
                Ok(None)
            }
        }
    }

    async fn classify_media(
        &mut self,
        goal: WaitGoal,
        expect: Option<&Expect>,
        message: &CastMessage,
        kind: &str,
        payload: &Value,
    ) -> Result<Option<WaitOutcome>, SessionError> {
        if message.namespace != proto::NS_MEDIA {
            tracing::debug!(namespace = %message.namespace, "ignoring non-media message");
            return Ok(None);
        }
        let Some(mut owned) = self.owned.clone() else {
            tracing::debug!(kind, "ignoring media message without an owned session");
            return Ok(None);
        };
        if message.source != owned.transport_id {
            tracing::debug!(kind, "ignoring media message for another transport");
            return Ok(None);
        }

        if goal == WaitGoal::MediaStatus && kind == "MEDIA_STATUS" {
            // Cleanup query: return the correlated status without mutating
            // ownership; the caller compares the media session itself.
            if self.response_matches(expect, message, payload) {
                return Ok(Some(WaitOutcome::Status(payload.clone())));
            }
            tracing::debug!(kind, "ignoring unrelated media status");
            return Ok(None);
        }

        match kind {
            "MEDIA_STATUS" => {
                let Some(statuses) = payload.get("status").and_then(Value::as_array) else {
                    return Ok(None);
                };
                for status in statuses {
                    let Some(media_session_id) =
                        status.get("mediaSessionId").and_then(Value::as_u64)
                    else {
                        continue;
                    };
                    let player_state = status
                        .get("playerState")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    // Only known state labels and numeric fields may reach the
                    // log: an untrusted state string could contain a media URL.
                    let logged_state = match player_state {
                        "PLAYING" => "PLAYING",
                        "BUFFERING" => "BUFFERING",
                        "PAUSED" => "PAUSED",
                        "IDLE" => "IDLE",
                        _ => "UNKNOWN",
                    };
                    let current_time = status.get("currentTime").and_then(Value::as_f64);
                    let live_edge_lag = live_edge_lag_seconds(status);
                    tracing::info!(
                        target: "cermin",
                        player_state = logged_state,
                        current_time = ?current_time,
                        live_edge_lag_seconds = ?live_edge_lag,
                        media_session_id,
                        "cast media status received"
                    );
                    let seen_before = owned.media_session_id.is_some();
                    match owned.media_session_id {
                        Some(known) if known != media_session_id => {
                            // A different media session on our transport means the
                            // application was taken over: relinquish ownership now so
                            // cleanup can never stop someone else's media.
                            self.owned = None;
                            return Err(SessionError::Replaced);
                        }
                        None => owned.media_session_id = Some(media_session_id),
                        _ => {}
                    }
                    match player_state {
                        "PLAYING" => {
                            owned.playing = true;
                            self.owned = Some(owned);
                            if goal == WaitGoal::Playing {
                                return Ok(Some(WaitOutcome::Playing));
                            }
                            return Ok(None);
                        }
                        // A first IDLE right after LOAD can be transient; a later
                        // one, or any IDLE once playing, means the session died.
                        "IDLE" if owned.playing || (goal == WaitGoal::Playing && seen_before) => {
                            self.owned = Some(owned);
                            return Err(SessionError::MediaIdle);
                        }
                        _ => {}
                    }
                }
                self.owned = Some(owned);
                Ok(None)
            }
            "LOAD_FAILED" => {
                if self.error_responsible(expect, message, payload) {
                    Err(SessionError::LoadFailed("LOAD_FAILED"))
                } else {
                    tracing::debug!(kind, "ignoring unrelated LOAD_FAILED");
                    Ok(None)
                }
            }
            "LOAD_CANCELLED" => {
                if self.error_responsible(expect, message, payload) {
                    Err(SessionError::LoadFailed("LOAD_CANCELLED"))
                } else {
                    tracing::debug!(kind, "ignoring unrelated LOAD_CANCELLED");
                    Ok(None)
                }
            }
            _ => {
                tracing::debug!(kind, "ignoring media message");
                Ok(None)
            }
        }
    }

    /// Success payloads correlate on namespace, source and request id.
    fn response_matches(
        &self,
        expect: Option<&Expect>,
        message: &CastMessage,
        payload: &Value,
    ) -> bool {
        let Some(expect) = expect else {
            return false;
        };
        let addressed = message.destination == self.sender || message.destination == "*";
        addressed
            && message.namespace == expect.namespace
            && message.source == expect.destination
            && payload.get("requestId").and_then(Value::as_u64) == Some(expect.request_id)
    }

    /// Error frames correlate on namespace and source, plus request id when the
    /// receiver included one.
    fn error_responsible(
        &self,
        expect: Option<&Expect>,
        message: &CastMessage,
        payload: &Value,
    ) -> bool {
        let Some(expect) = expect else {
            return false;
        };
        let addressed = message.destination == self.sender || message.destination == "*";
        if !(addressed
            && message.namespace == expect.namespace
            && message.source == expect.destination)
        {
            return false;
        }
        match payload.get("requestId").and_then(Value::as_u64) {
            Some(request_id) => request_id == expect.request_id,
            None => true,
        }
    }

    fn next_request_id(&mut self) -> u64 {
        self.next_request_id = self.next_request_id.wrapping_add(1);
        if self.next_request_id == 0 {
            self.next_request_id = 1;
        }
        self.next_request_id
    }

    async fn send_json(
        &mut self,
        namespace: &str,
        destination: &str,
        payload: &Value,
        deadline: Option<Instant>,
    ) -> Result<(), SessionError> {
        let message = CastMessage::json(&self.sender, destination, namespace, payload);
        self.write_message(&message, deadline).await
    }

    async fn write_message(
        &mut self,
        message: &CastMessage,
        deadline: Option<Instant>,
    ) -> Result<(), SessionError> {
        if self.poisoned {
            return Err(SessionError::Protocol(
                "connection unusable after an earlier write failure",
            ));
        }
        // A single absolute deadline covers finishing any pending frame and
        // sending this one, so a trickle of partial writes cannot extend the
        // budget indefinitely.
        let call_deadline = match deadline {
            Some(deadline) => deadline.min(Instant::now() + self.timeouts.write),
            None => Instant::now() + self.timeouts.write,
        };
        self.flush_outbound(call_deadline).await?;

        let bytes = message.encode_frame();
        if bytes.len() > proto::MAX_FRAME_BYTES + 4 {
            return Err(SessionError::Protocol(
                "outbound Cast frame exceeds the 64 KiB limit",
            ));
        }
        self.outbound = bytes;
        self.outbound_offset = 0;
        self.flush_outbound(call_deadline).await
    }

    /// Write the pending outbound frame with cancellation-safe `write` calls.
    ///
    /// A single `write` either consumes bytes or writes nothing, so dropping the
    /// future or timing out only leaves [`Session::outbound_offset`] where it was:
    /// the next call resumes the same frame instead of starting a new one. The
    /// absolute `deadline` bounds the whole frame, not each partial write.
    async fn flush_outbound(&mut self, deadline: Instant) -> Result<(), SessionError> {
        while self.outbound_offset < self.outbound.len() {
            if Instant::now() >= deadline {
                return Err(SessionError::Timeout("write"));
            }
            let start = self.outbound_offset;
            match timeout_at(deadline, self.io.write(&self.outbound[start..])).await {
                Ok(Ok(0)) => {
                    self.poisoned = true;
                    return Err(SessionError::Io(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "cast write returned zero bytes",
                    )));
                }
                Ok(Ok(written)) => self.outbound_offset += written,
                Ok(Err(error)) => {
                    // A hard I/O failure leaves the stream unusable; further writes
                    // (including cleanup STOP) would only fail again.
                    self.poisoned = true;
                    return Err(SessionError::Io(error));
                }
                // The partially written frame stays pending for a later attempt.
                Err(_) => return Err(SessionError::Timeout("write")),
            }
        }
        self.outbound.clear();
        self.outbound_offset = 0;
        Ok(())
    }
}

/// How fresh receiver evidence classifies the owned application before STOP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupOwnership {
    Ours,
    Replaced,
    Unknown,
}

#[derive(Debug)]
struct AppEntry {
    app_id: String,
    session_id: String,
    /// Required to message the application transport; never inferred from the
    /// session id.
    transport_id: Option<String>,
    display_name: String,
    has_media_namespace: bool,
}

/// Applications shown by a RECEIVER_STATUS payload.
///
/// The protocol nests applications under `status.applications`; a flat
/// `applications` array is accepted too for robustness.
fn receiver_apps(status: &Value) -> Vec<AppEntry> {
    let apps = status
        .get("status")
        .and_then(|status| status.get("applications"))
        .or_else(|| status.get("applications"))
        .and_then(Value::as_array);
    let Some(apps) = apps else {
        return Vec::new();
    };
    apps.iter()
        .filter_map(|app| {
            let app_id = app.get("appId").and_then(Value::as_str)?.to_owned();
            let session_id = app.get("sessionId").and_then(Value::as_str)?.to_owned();
            let transport_id = app
                .get("transportId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let display_name = app
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let has_media_namespace =
                app.get("namespaces")
                    .and_then(Value::as_array)
                    .is_some_and(|namespaces| {
                        namespaces.iter().any(|namespace| {
                            namespace.get("name").and_then(Value::as_str) == Some(proto::NS_MEDIA)
                        })
                    });
            Some(AppEntry {
                app_id,
                session_id,
                transport_id,
                display_name,
                has_media_namespace,
            })
        })
        .collect()
}

fn is_backdrop(app: &AppEntry) -> bool {
    app.app_id == proto::BACKDROP_APP_ID || app.display_name.eq_ignore_ascii_case("backdrop")
}

fn busy_media_app(status: &Value) -> Option<String> {
    receiver_apps(status)
        .into_iter()
        .find(|app| {
            !is_backdrop(app) && (app.app_id == proto::DMR_APP_ID || app.has_media_namespace)
        })
        .map(|app| app.app_id)
}

fn receiver_app_by_id(status: &Value, app_id: &str) -> Option<AppEntry> {
    receiver_apps(status)
        .into_iter()
        .find(|app| app.app_id == app_id)
}

fn receiver_app_by_session(status: &Value, session_id: &str) -> Option<AppEntry> {
    receiver_apps(status)
        .into_iter()
        .find(|app| app.session_id == session_id)
}

/// First `mediaSessionId` reported by a MEDIA_STATUS payload.
fn media_session_id_of(status: &Value) -> Option<u64> {
    status
        .get("status")
        .and_then(Value::as_array)
        .and_then(|statuses| {
            statuses
                .iter()
                .find_map(|entry| entry.get("mediaSessionId").and_then(Value::as_u64))
        })
}

/// Receiver-timeline distance from playback to its reported seekable live end.
/// This is not a measurement of capture-to-panel latency. Never infer it from
/// the sender's clock: the receiver may use a different timeline origin.
fn live_edge_lag_seconds(status: &Value) -> Option<f64> {
    let current = status.get("currentTime")?.as_f64()?;
    let end = status.get("liveSeekableRange")?.get("end")?.as_f64()?;
    (current.is_finite() && end.is_finite() && current >= 0.0 && end >= current)
        .then_some(end - current)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::sync::oneshot;
    use tokio::time::{Instant, timeout};

    use super::*;

    #[test]
    fn live_edge_distance_requires_receiver_timeline_evidence() {
        assert_eq!(
            live_edge_lag_seconds(&json!({
                "currentTime": 100.25,
                "liveSeekableRange": {"start": 95.0, "end": 103.5}
            })),
            Some(3.25)
        );
        for status in [
            json!({"currentTime": 1.0}),
            json!({"liveSeekableRange": {"end": 10.0}}),
            json!({"currentTime": -1.0, "liveSeekableRange": {"end": 10.0}}),
            json!({"currentTime": 11.0, "liveSeekableRange": {"end": 10.0}}),
            json!({"currentTime": "invalid", "liveSeekableRange": {"end": 10.0}}),
        ] {
            assert_eq!(live_edge_lag_seconds(&status), None);
        }
    }
    use crate::control::test_support::{
        LimitedWriter, MEDIA_SESSION_ID, MockReceiver, SESSION_ID, TEST_TIMEOUT, TEST_URL,
        TrickleWriter, app_entry, app_entry_with_transport, complete_handshake, drain_messages,
        expect_get_status_and_reply, is_type, media_status, payload_of, receiver_status,
        request_id_of, script_cleanup_after_playing, script_connect_and_load, script_launch,
        script_launch_and_play, script_to_playing, test_timeouts,
    };

    fn stop_flag() -> (Arc<AtomicBool>, Arc<AtomicBool>) {
        let shared = Arc::new(AtomicBool::new(false));
        (Arc::clone(&shared), shared)
    }

    #[tokio::test]
    async fn handshake_and_receiver_status_round_trip() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let status = session.receiver_status(None).await;
                (session, status)
            });

            complete_handshake(&mut server).await;
            let get = server
                .expect_message(proto::NS_RECEIVER, "GET_STATUS")
                .await;
            let expected = receiver_status(
                Some(request_id_of(&get)),
                vec![app_entry(
                    proto::BACKDROP_APP_ID,
                    "backdrop",
                    "Backdrop",
                    true,
                )],
            );
            server
                .send(
                    proto::RECEIVER_ID,
                    &get.source,
                    proto::NS_RECEIVER,
                    expected.clone(),
                )
                .await;

            let (_session, status) = client.await.unwrap();
            assert_eq!(status.unwrap(), expected);
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn receiver_status_ignores_uncorrelated_status() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let status = session.receiver_status(None).await;
                (session, status)
            });

            complete_handshake(&mut server).await;
            let get = server
                .expect_message(proto::NS_RECEIVER, "GET_STATUS")
                .await;
            let request_id = request_id_of(&get);

            // Async status (no requestId) showing a busy receiver: must not be
            // consumed as the GET_STATUS response.
            server
                .send(
                    proto::RECEIVER_ID,
                    &get.source,
                    proto::NS_RECEIVER,
                    receiver_status(
                        None,
                        vec![app_entry(proto::DMR_APP_ID, "decoy-1", "Decoy", true)],
                    ),
                )
                .await;
            // Matching requestId from the wrong sender: must be ignored too.
            server
                .send(
                    "not-receiver-0",
                    &get.source,
                    proto::NS_RECEIVER,
                    receiver_status(
                        Some(request_id),
                        vec![app_entry(proto::DMR_APP_ID, "decoy-2", "Decoy", true)],
                    ),
                )
                .await;

            let expected = receiver_status(Some(request_id), Vec::new());
            server
                .send(
                    proto::RECEIVER_ID,
                    &get.source,
                    proto::NS_RECEIVER,
                    expected.clone(),
                )
                .await;

            let (_session, status) = client.await.unwrap();
            assert_eq!(status.unwrap(), expected);
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_loads_plays_and_stops_own_session() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (callback_stop, stop) = stop_flag();
            let on_playing: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
                callback_stop.store(true, Ordering::Relaxed);
            });

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, Some(on_playing)).await;
                (session, result)
            });

            script_to_playing(&mut server).await;
            script_cleanup_after_playing(&mut server).await;

            let (_session, result) = client.await.unwrap();
            assert!(result.is_ok(), "stream failed: {result:?}");
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_rejects_busy_receiver_without_launching() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (_callback_stop, stop) = stop_flag();

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, None).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            expect_get_status_and_reply(
                &mut server,
                vec![app_entry(
                    proto::DMR_APP_ID,
                    "someone-else",
                    "Default Media Receiver",
                    true,
                )],
            )
            .await;

            let (_session, result) = client.await.unwrap();
            assert!(
                matches!(&result, Err(SessionError::Busy(app)) if app == proto::DMR_APP_ID),
                "unexpected result: {result:?}"
            );

            let seen = drain_messages(&mut server, Duration::from_millis(150)).await;
            assert!(
                !seen
                    .iter()
                    .any(|m| is_type(m, proto::NS_RECEIVER, "LAUNCH"))
            );
            assert!(!seen.iter().any(|m| is_type(m, proto::NS_RECEIVER, "STOP")));
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_tolerates_backdrop_and_launches() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (callback_stop, stop) = stop_flag();
            let on_playing: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
                callback_stop.store(true, Ordering::Relaxed);
            });

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, Some(on_playing)).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            expect_get_status_and_reply(
                &mut server,
                vec![
                    app_entry(proto::BACKDROP_APP_ID, "backdrop", "Backdrop", true),
                    app_entry("A1B2C3D4", "menu-app", "Setup", false),
                ],
            )
            .await;
            script_launch_and_play(&mut server).await;
            script_cleanup_after_playing(&mut server).await;

            let (_session, result) = client.await.unwrap();
            assert!(result.is_ok(), "stream failed: {result:?}");
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_reports_launch_error_without_stop() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (_callback_stop, stop) = stop_flag();

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, None).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            expect_get_status_and_reply(&mut server, Vec::new()).await;
            let launch = server.expect_message(proto::NS_RECEIVER, "LAUNCH").await;
            server
                .send(
                    proto::RECEIVER_ID,
                    &launch.source,
                    proto::NS_RECEIVER,
                    json!({
                        "type": "LAUNCH_ERROR",
                        "requestId": request_id_of(&launch),
                        "appId": proto::DMR_APP_ID,
                        "errorCode": "app_unavailable",
                    }),
                )
                .await;

            let (_session, result) = client.await.unwrap();
            assert!(
                matches!(&result, Err(SessionError::LaunchFailed(code)) if code == "app_unavailable"),
                "unexpected result: {result:?}"
            );

            let seen = drain_messages(&mut server, Duration::from_millis(150)).await;
            assert!(!seen.iter().any(|m| is_type(m, proto::NS_RECEIVER, "STOP")));
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_rejects_launch_without_transport_id() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (_callback_stop, stop) = stop_flag();

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, None).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            expect_get_status_and_reply(&mut server, Vec::new()).await;
            let launch = server.expect_message(proto::NS_RECEIVER, "LAUNCH").await;
            // A status without transportId cannot be messaged; do not fall back
            // to the session id.
            server
                .send(
                    proto::RECEIVER_ID,
                    "*",
                    proto::NS_RECEIVER,
                    json!({
                        "type": "RECEIVER_STATUS",
                        "requestId": request_id_of(&launch),
                        "status": {"applications": [{
                            "appId": proto::DMR_APP_ID,
                            "sessionId": SESSION_ID,
                            "displayName": "Default Media Receiver",
                            "namespaces": [{"name": proto::NS_MEDIA}],
                        }]},
                    }),
                )
                .await;

            let (_session, result) = client.await.unwrap();
            assert!(
                matches!(&result, Err(SessionError::Protocol(_))),
                "unexpected result: {result:?}"
            );
            let seen = drain_messages(&mut server, Duration::from_millis(150)).await;
            assert!(!seen.iter().any(|m| is_type(m, proto::NS_RECEIVER, "STOP")));
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_rejects_launch_without_media_namespace() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (_callback_stop, stop) = stop_flag();

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, None).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            expect_get_status_and_reply(&mut server, Vec::new()).await;
            let launch = server.expect_message(proto::NS_RECEIVER, "LAUNCH").await;
            // The app is ours but does not advertise the media namespace, so a
            // LOAD cannot be delivered to it.
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
                            false,
                        )],
                    ),
                )
                .await;

            // Cleanup still owns the launched application and stops it.
            let server_task = tokio::spawn(async move {
                expect_get_status_and_reply(
                    &mut server,
                    vec![app_entry(
                        proto::DMR_APP_ID,
                        SESSION_ID,
                        "Default Media Receiver",
                        false,
                    )],
                )
                .await;
                let app_stop = server.expect_message(proto::NS_RECEIVER, "STOP").await;
                assert_eq!(
                    payload_of(&app_stop)
                        .get("sessionId")
                        .and_then(Value::as_str),
                    Some(SESSION_ID)
                );
                let _close = server.expect_message(proto::NS_CONNECTION, "CLOSE").await;
            });

            let (_session, result) = client.await.unwrap();
            assert!(
                matches!(&result, Err(SessionError::Protocol(_))),
                "unexpected result: {result:?}"
            );
            server_task.await.unwrap();
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_load_failure_stops_owned_session() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (_callback_stop, stop) = stop_flag();

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, None).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            expect_get_status_and_reply(&mut server, Vec::new()).await;
            script_launch(&mut server).await;
            let load = script_connect_and_load(&mut server).await;
            server
                .send(
                    SESSION_ID,
                    &load.source,
                    proto::NS_MEDIA,
                    json!({"type": "LOAD_FAILED", "requestId": request_id_of(&load)}),
                )
                .await;

            // Cleanup: still ours -> application STOP and CLOSE, no media STOP
            // because no mediaSessionId was ever reported.
            let server_task = tokio::spawn(async move {
                expect_get_status_and_reply(
                    &mut server,
                    vec![app_entry(
                        proto::DMR_APP_ID,
                        SESSION_ID,
                        "Default Media Receiver",
                        true,
                    )],
                )
                .await;
                let app_stop = server.expect_message(proto::NS_RECEIVER, "STOP").await;
                assert_eq!(
                    payload_of(&app_stop)
                        .get("sessionId")
                        .and_then(Value::as_str),
                    Some(SESSION_ID)
                );
                let _close = server.expect_message(proto::NS_CONNECTION, "CLOSE").await;
            });

            let (_session, result) = client.await.unwrap();
            assert!(
                matches!(&result, Err(SessionError::LoadFailed("LOAD_FAILED"))),
                "unexpected result: {result:?}"
            );
            server_task.await.unwrap();
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_load_cancelled_stops_owned_session() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (_callback_stop, stop) = stop_flag();

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, None).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            expect_get_status_and_reply(&mut server, Vec::new()).await;
            script_launch(&mut server).await;
            let load = script_connect_and_load(&mut server).await;
            server
                .send(
                    SESSION_ID,
                    &load.source,
                    proto::NS_MEDIA,
                    json!({"type": "LOAD_CANCELLED", "requestId": request_id_of(&load)}),
                )
                .await;

            let server_task = tokio::spawn(async move {
                expect_get_status_and_reply(
                    &mut server,
                    vec![app_entry(
                        proto::DMR_APP_ID,
                        SESSION_ID,
                        "Default Media Receiver",
                        true,
                    )],
                )
                .await;
                let _app_stop = server.expect_message(proto::NS_RECEIVER, "STOP").await;
                let _close = server.expect_message(proto::NS_CONNECTION, "CLOSE").await;
            });

            let (_session, result) = client.await.unwrap();
            assert!(
                matches!(&result, Err(SessionError::LoadFailed("LOAD_CANCELLED"))),
                "unexpected result: {result:?}"
            );
            server_task.await.unwrap();
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_reports_invalid_request_without_stop() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (_callback_stop, stop) = stop_flag();

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, None).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            let get = server
                .expect_message(proto::NS_RECEIVER, "GET_STATUS")
                .await;
            server
                .send(
                    proto::RECEIVER_ID,
                    &get.source,
                    proto::NS_RECEIVER,
                    json!({"type": "INVALID_REQUEST", "requestId": request_id_of(&get)}),
                )
                .await;

            let (_session, result) = client.await.unwrap();
            assert!(
                matches!(&result, Err(SessionError::Protocol(_))),
                "unexpected result: {result:?}"
            );

            let seen = drain_messages(&mut server, Duration::from_millis(150)).await;
            assert!(!seen.iter().any(|m| is_type(m, proto::NS_RECEIVER, "STOP")));
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_request_timeout() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (_callback_stop, stop) = stop_flag();
            let mut timeouts = test_timeouts();
            timeouts.request = Duration::from_millis(150);

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, timeouts);
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, None).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            let _get = server
                .expect_message(proto::NS_RECEIVER, "GET_STATUS")
                .await;
            // Never answer the status request.

            let (_session, result) = client.await.unwrap();
            assert!(
                matches!(&result, Err(SessionError::Timeout("receiver status"))),
                "unexpected result: {result:?}"
            );
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_fails_when_liveness_lapses() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (_callback_stop, stop) = stop_flag();
            let mut timeouts = test_timeouts();
            timeouts.request = Duration::from_secs(5);
            timeouts.handshake = Duration::from_secs(1);
            timeouts.liveness = Duration::from_millis(250);
            timeouts.heartbeat = Duration::from_millis(50);

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, timeouts);
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, None).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            let _get = server
                .expect_message(proto::NS_RECEIVER, "GET_STATUS")
                .await;
            // Go silent: the client's PINGs are never answered.
            let (_session, result) = client.await.unwrap();
            assert!(
                matches!(&result, Err(SessionError::Liveness)),
                "unexpected result: {result:?}"
            );
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_heartbeats_and_answers_ping_while_waiting() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (callback_stop, stop) = stop_flag();
            let on_playing: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
                callback_stop.store(true, Ordering::Relaxed);
            });

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, Some(on_playing)).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            expect_get_status_and_reply(&mut server, Vec::new()).await;
            let launch = server.expect_message(proto::NS_RECEIVER, "LAUNCH").await;

            // The client must heartbeat while the LAUNCH reply is withheld.
            let ping = server.recv().await;
            assert!(is_type(&ping, proto::NS_HEARTBEAT, "PING"), "wanted PING");
            server.reply_pong(&ping).await;

            // Receiver-initiated PING must be answered with PONG.
            server
                .send(
                    proto::RECEIVER_ID,
                    &ping.source,
                    proto::NS_HEARTBEAT,
                    json!({"type": "PING"}),
                )
                .await;
            let pong = server.expect_message(proto::NS_HEARTBEAT, "PONG").await;
            assert_eq!(pong.destination, proto::RECEIVER_ID);

            server
                .send(
                    proto::RECEIVER_ID,
                    &launch.source,
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
            let load = script_connect_and_load(&mut server).await;
            server
                .send(
                    SESSION_ID,
                    &load.source,
                    proto::NS_MEDIA,
                    media_status(Some(request_id_of(&load)), "PLAYING", MEDIA_SESSION_ID),
                )
                .await;
            script_cleanup_after_playing(&mut server).await;

            let (_session, result) = client.await.unwrap();
            assert!(result.is_ok(), "stream failed: {result:?}");
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_running_polls_owned_media_status_and_keeps_heartbeating() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (stop, stop_for_task) = stop_flag();

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop_for_task, None).await;
                (session, result)
            });

            script_to_playing(&mut server).await;

            // Steady state: the periodic poll is a media GET_STATUS addressed to
            // the owned transport and carries a request-counter id.
            let poll = loop {
                let message = server.recv().await;
                if is_type(&message, proto::NS_HEARTBEAT, "PING") {
                    server.reply_pong(&message).await;
                    continue;
                }
                assert!(
                    is_type(&message, proto::NS_MEDIA, "GET_STATUS"),
                    "unexpected steady-state message: {message:?}"
                );
                break message;
            };
            assert_eq!(poll.destination, SESSION_ID);
            let poll_request_id = request_id_of(&poll);
            assert!(poll_request_id > 0, "the poll must use the request counter");

            // The reply is fire-and-forget: the existing media path classifies
            // it and ownership must stay intact.
            server
                .send(
                    SESSION_ID,
                    "*",
                    proto::NS_MEDIA,
                    media_status(Some(poll_request_id), "PLAYING", MEDIA_SESSION_ID),
                )
                .await;

            // Heartbeats continue while running; any further poll is answered
            // like the real receiver would so cleanup stays verifiable.
            let ping = loop {
                let message = server.recv().await;
                if is_type(&message, proto::NS_HEARTBEAT, "PING") {
                    break message;
                }
                assert!(
                    is_type(&message, proto::NS_MEDIA, "GET_STATUS"),
                    "unexpected message while running: {message:?}"
                );
                server
                    .send(
                        SESSION_ID,
                        "*",
                        proto::NS_MEDIA,
                        media_status(Some(request_id_of(&message)), "PLAYING", MEDIA_SESSION_ID),
                    )
                    .await;
            };
            assert_eq!(ping.destination, proto::RECEIVER_ID);
            server.reply_pong(&ping).await;

            // Stop; the cleanup script tolerates a poll that was in flight.
            stop.store(true, Ordering::Relaxed);
            let server_task = tokio::spawn(async move {
                let (mut media_stops, mut app_stops, mut closed) = (0usize, 0usize, false);
                loop {
                    match tokio::time::timeout(Duration::from_millis(600), server.recv()).await {
                        Err(_) => break,
                        Ok(message) => {
                            if is_type(&message, proto::NS_HEARTBEAT, "PING") {
                                server.reply_pong(&message).await;
                            } else if is_type(&message, proto::NS_MEDIA, "GET_STATUS") {
                                server
                                    .send(
                                        SESSION_ID,
                                        "*",
                                        proto::NS_MEDIA,
                                        media_status(
                                            Some(request_id_of(&message)),
                                            "PLAYING",
                                            MEDIA_SESSION_ID,
                                        ),
                                    )
                                    .await;
                            } else if is_type(&message, proto::NS_RECEIVER, "GET_STATUS") {
                                server
                                    .send(
                                        proto::RECEIVER_ID,
                                        "*",
                                        proto::NS_RECEIVER,
                                        receiver_status(
                                            Some(request_id_of(&message)),
                                            vec![app_entry(
                                                proto::DMR_APP_ID,
                                                SESSION_ID,
                                                "Default Media Receiver",
                                                true,
                                            )],
                                        ),
                                    )
                                    .await;
                            } else if is_type(&message, proto::NS_MEDIA, "STOP") {
                                media_stops += 1;
                            } else if is_type(&message, proto::NS_RECEIVER, "STOP") {
                                app_stops += 1;
                            } else if is_type(&message, proto::NS_CONNECTION, "CLOSE") {
                                closed = true;
                                break;
                            }
                        }
                    }
                }
                (media_stops, app_stops, closed)
            });

            let (_session, result) = client.await.unwrap();
            assert!(result.is_ok(), "stream failed: {result:?}");
            let (media_stops, app_stops, closed) = server_task.await.unwrap();
            assert!(closed, "cleanup must close the transport");
            assert!(media_stops >= 1, "expected a media STOP");
            assert!(app_stops >= 1, "expected an application STOP");
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_ignores_launch_decoys_and_accepts_correlated_broadcast() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (callback_stop, stop) = stop_flag();
            let on_playing: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
                callback_stop.store(true, Ordering::Relaxed);
            });

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, Some(on_playing)).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            expect_get_status_and_reply(&mut server, Vec::new()).await;
            let launch = server.expect_message(proto::NS_RECEIVER, "LAUNCH").await;

            // Uncorrelated broadcast DMR: someone else, must not be adopted.
            server
                .send(
                    proto::RECEIVER_ID,
                    "*",
                    proto::NS_RECEIVER,
                    receiver_status(
                        None,
                        vec![app_entry(
                            proto::DMR_APP_ID,
                            "decoy-1",
                            "Default Media Receiver",
                            true,
                        )],
                    ),
                )
                .await;
            // Correlated request id but the wrong source: must not be adopted
            // or clear/replace ownership.
            server
                .send(
                    "not-receiver-0",
                    "sender-0",
                    proto::NS_RECEIVER,
                    receiver_status(
                        Some(request_id_of(&launch)),
                        vec![app_entry(
                            proto::DMR_APP_ID,
                            "decoy-2",
                            "Default Media Receiver",
                            true,
                        )],
                    ),
                )
                .await;
            // The real correlated launch response arrives as a broadcast.
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

            let load = script_connect_and_load(&mut server).await;
            server
                .send(
                    SESSION_ID,
                    "*",
                    proto::NS_MEDIA,
                    media_status(Some(request_id_of(&load)), "PLAYING", MEDIA_SESSION_ID),
                )
                .await;
            script_cleanup_after_playing(&mut server).await;

            let (_session, result) = client.await.unwrap();
            assert!(result.is_ok(), "stream failed: {result:?}");
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_ignores_unrelated_load_error() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (callback_stop, stop) = stop_flag();
            let on_playing: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
                callback_stop.store(true, Ordering::Relaxed);
            });

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, Some(on_playing)).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            expect_get_status_and_reply(&mut server, Vec::new()).await;
            script_launch(&mut server).await;
            let load = script_connect_and_load(&mut server).await;
            let load_request_id = request_id_of(&load);

            // LOAD_FAILED from another transport: not ours, ignore.
            server
                .send(
                    "other-transport",
                    &load.source,
                    proto::NS_MEDIA,
                    json!({"type": "LOAD_FAILED", "requestId": load_request_id}),
                )
                .await;
            // INVALID_REQUEST naming the LOAD request id but in the wrong
            // namespace: not this request's answer, ignore.
            server
                .send(
                    proto::RECEIVER_ID,
                    "*",
                    proto::NS_RECEIVER,
                    json!({"type": "INVALID_REQUEST", "requestId": load_request_id}),
                )
                .await;
            server
                .send(
                    SESSION_ID,
                    "*",
                    proto::NS_MEDIA,
                    media_status(Some(load_request_id), "PLAYING", MEDIA_SESSION_ID),
                )
                .await;
            script_cleanup_after_playing(&mut server).await;

            let (_session, result) = client.await.unwrap();
            assert!(result.is_ok(), "stream failed: {result:?}");
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_media_idle_fails_and_stops() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (_callback_stop, stop) = stop_flag();

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, None).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            expect_get_status_and_reply(&mut server, Vec::new()).await;
            script_launch(&mut server).await;
            let load = script_connect_and_load(&mut server).await;
            server
                .send(
                    SESSION_ID,
                    &load.source,
                    proto::NS_MEDIA,
                    media_status(Some(request_id_of(&load)), "BUFFERING", MEDIA_SESSION_ID),
                )
                .await;
            server
                .send(
                    SESSION_ID,
                    &load.source,
                    proto::NS_MEDIA,
                    media_status(None, "IDLE", MEDIA_SESSION_ID),
                )
                .await;

            let server_task =
                tokio::spawn(async move { script_cleanup_after_playing(&mut server).await });
            let (_session, result) = client.await.unwrap();
            assert!(
                matches!(&result, Err(SessionError::MediaIdle)),
                "unexpected result: {result:?}"
            );
            server_task.await.unwrap();
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_tolerates_transient_initial_idle() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (callback_stop, stop) = stop_flag();
            let on_playing: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
                callback_stop.store(true, Ordering::Relaxed);
            });

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, Some(on_playing)).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            expect_get_status_and_reply(&mut server, Vec::new()).await;
            script_launch(&mut server).await;
            let load = script_connect_and_load(&mut server).await;
            // Some receivers report IDLE briefly before the live session starts.
            server
                .send(
                    SESSION_ID,
                    &load.source,
                    proto::NS_MEDIA,
                    media_status(Some(request_id_of(&load)), "IDLE", MEDIA_SESSION_ID),
                )
                .await;
            server
                .send(
                    SESSION_ID,
                    &load.source,
                    proto::NS_MEDIA,
                    media_status(None, "PLAYING", MEDIA_SESSION_ID),
                )
                .await;
            script_cleanup_after_playing(&mut server).await;

            let (_session, result) = client.await.unwrap();
            assert!(result.is_ok(), "stream failed: {result:?}");
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_replaced_application_is_not_stopped() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (_callback_stop, stop) = stop_flag();

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, None).await;
                (session, result)
            });

            script_to_playing(&mut server).await;
            server
                .send(
                    proto::RECEIVER_ID,
                    "sender-0",
                    proto::NS_RECEIVER,
                    receiver_status(
                        None,
                        vec![app_entry(
                            proto::DMR_APP_ID,
                            "session-2",
                            "Default Media Receiver",
                            true,
                        )],
                    ),
                )
                .await;

            let (_session, result) = client.await.unwrap();
            assert!(
                matches!(&result, Err(SessionError::Replaced)),
                "unexpected result: {result:?}"
            );

            let seen = drain_messages(&mut server, Duration::from_millis(150)).await;
            assert!(
                !seen
                    .iter()
                    .any(|m| is_type(m, proto::NS_RECEIVER, "GET_STATUS"))
            );
            assert!(!seen.iter().any(|m| is_type(m, proto::NS_RECEIVER, "STOP")));
            assert!(!seen.iter().any(|m| is_type(m, proto::NS_MEDIA, "STOP")));
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_media_replacement_is_not_stopped() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (playing_tx, playing_rx) = oneshot::channel::<()>();
            let stop = Arc::new(AtomicBool::new(false));
            let on_playing: Box<dyn FnOnce() + Send + 'static> =
                Box::new(move || playing_tx.send(()).unwrap());

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, Some(on_playing)).await;
                (session, result)
            });

            script_to_playing(&mut server).await;
            // The callback proves PLAYING was processed before the replacement.
            playing_rx.await.unwrap();
            // Same application session, different media session: the media was
            // taken over within our DMR. Ownership must be relinquished.
            server
                .send(
                    SESSION_ID,
                    "*",
                    proto::NS_MEDIA,
                    media_status(None, "PLAYING", MEDIA_SESSION_ID + 1),
                )
                .await;

            let (_session, result) = client.await.unwrap();
            assert!(
                matches!(&result, Err(SessionError::Replaced)),
                "unexpected result: {result:?}"
            );
            let seen = drain_messages(&mut server, Duration::from_millis(150)).await;
            assert!(
                !seen
                    .iter()
                    .any(|m| is_type(m, proto::NS_RECEIVER, "GET_STATUS"))
            );
            assert!(!seen.iter().any(|m| is_type(m, proto::NS_MEDIA, "STOP")));
            assert!(!seen.iter().any(|m| is_type(m, proto::NS_RECEIVER, "STOP")));
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_transport_swap_is_not_stopped() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (playing_tx, playing_rx) = oneshot::channel::<()>();
            let stop = Arc::new(AtomicBool::new(false));
            let on_playing: Box<dyn FnOnce() + Send + 'static> =
                Box::new(move || playing_tx.send(()).unwrap());

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, Some(on_playing)).await;
                (session, result)
            });

            script_to_playing(&mut server).await;
            playing_rx.await.unwrap();
            // Same session id but a new transport: the application was replaced.
            server
                .send(
                    proto::RECEIVER_ID,
                    "*",
                    proto::NS_RECEIVER,
                    receiver_status(
                        None,
                        vec![app_entry_with_transport(
                            proto::DMR_APP_ID,
                            SESSION_ID,
                            "session-1b",
                            "Default Media Receiver",
                            true,
                        )],
                    ),
                )
                .await;

            let (_session, result) = client.await.unwrap();
            assert!(
                matches!(&result, Err(SessionError::Replaced)),
                "unexpected result: {result:?}"
            );
            let seen = drain_messages(&mut server, Duration::from_millis(150)).await;
            assert!(
                !seen
                    .iter()
                    .any(|m| is_type(m, proto::NS_RECEIVER, "GET_STATUS"))
            );
            assert!(!seen.iter().any(|m| is_type(m, proto::NS_MEDIA, "STOP")));
            assert!(!seen.iter().any(|m| is_type(m, proto::NS_RECEIVER, "STOP")));
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_ignores_wrong_source_replacement_status() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (playing_tx, playing_rx) = oneshot::channel::<()>();
            let (stop, stop_for_task) = stop_flag();
            let on_playing: Box<dyn FnOnce() + Send + 'static> =
                Box::new(move || playing_tx.send(()).unwrap());

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session
                    .stream(TEST_URL, &stop_for_task, Some(on_playing))
                    .await;
                (session, result)
            });

            script_to_playing(&mut server).await;
            playing_rx.await.unwrap();
            // A wrong-source status claiming a replacement must neither clear nor
            // replace ownership.
            server
                .send(
                    "not-receiver-0",
                    "*",
                    proto::NS_RECEIVER,
                    receiver_status(
                        None,
                        vec![app_entry(
                            proto::DMR_APP_ID,
                            "session-2",
                            "Default Media Receiver",
                            true,
                        )],
                    ),
                )
                .await;
            // A heartbeat proves the stream is still alive after the decoy.
            let ping = server.recv().await;
            assert!(is_type(&ping, proto::NS_HEARTBEAT, "PING"));
            server.reply_pong(&ping).await;

            stop.store(true, Ordering::Relaxed);
            let server_task =
                tokio::spawn(async move { script_cleanup_after_playing(&mut server).await });
            let (_session, result) = client.await.unwrap();
            assert!(result.is_ok(), "stream failed: {result:?}");
            server_task.await.unwrap();
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_close_event_fails_and_stops() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (_callback_stop, stop) = stop_flag();

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop, None).await;
                (session, result)
            });

            script_to_playing(&mut server).await;
            server
                .send(
                    SESSION_ID,
                    "sender-0",
                    proto::NS_CONNECTION,
                    json!({"type": "CLOSE"}),
                )
                .await;

            let server_task =
                tokio::spawn(async move { script_cleanup_after_playing(&mut server).await });
            let (_session, result) = client.await.unwrap();
            assert!(
                matches!(&result, Err(SessionError::Closed)),
                "unexpected result: {result:?}"
            );
            server_task.await.unwrap();
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stop_after_cancelled_stream_resumes_partial_frame() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let mut session = Session::new(client_io, test_timeouts());

            // `connect` normally performs this handshake before `stream`; answer the
            // PING up front because the mock script starts after `stream` is polled.
            server
                .send(
                    proto::RECEIVER_ID,
                    "sender-0",
                    proto::NS_HEARTBEAT,
                    json!({"type": "PONG"}),
                )
                .await;
            session.handshake(None).await.unwrap();

            let (partial_tx, partial_rx) = oneshot::channel::<()>();
            let (finish_tx, finish_rx) = oneshot::channel::<()>();

            let server_task = tokio::spawn(async move {
                script_to_playing(&mut server).await;

                // Send a heartbeat PING minus its last byte so the client parks
                // mid-frame, then complete it only after the stream future is gone.
                let frame = CastMessage::json(
                    proto::RECEIVER_ID,
                    "sender-0",
                    proto::NS_HEARTBEAT,
                    &json!({"type": "PING"}),
                )
                .encode_frame();
                server.write_raw(&frame[..frame.len() - 1]).await;
                partial_tx.send(()).unwrap();
                finish_rx.await.unwrap();
                server.write_raw(&frame[frame.len() - 1..]).await;

                // cleanup runs: receiver and media GET_STATUS, resumed PONG, STOPs.
                let get = server
                    .expect_message(proto::NS_RECEIVER, "GET_STATUS")
                    .await;
                server
                    .send(
                        proto::RECEIVER_ID,
                        &get.source,
                        proto::NS_RECEIVER,
                        receiver_status(
                            Some(request_id_of(&get)),
                            vec![app_entry(
                                proto::DMR_APP_ID,
                                SESSION_ID,
                                "Default Media Receiver",
                                true,
                            )],
                        ),
                    )
                    .await;
                // The resumed PING is answered while the receiver query is in
                // flight, so the PONG arrives before the media query.
                let pong = server.expect_message(proto::NS_HEARTBEAT, "PONG").await;
                assert_eq!(pong.destination, proto::RECEIVER_ID);
                let media_get = server.expect_message(proto::NS_MEDIA, "GET_STATUS").await;
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
                let _close = server.expect_message(proto::NS_CONNECTION, "CLOSE").await;
            });

            let stop = Arc::new(AtomicBool::new(false));
            let mut stream_future = Box::pin(session.stream(TEST_URL, &stop, None));
            tokio::select! {
                result = partial_rx => result.unwrap(),
                result = &mut stream_future => {
                    panic!("stream ended before cancellation: {result:?}")
                }
            }
            timeout(Duration::from_millis(50), &mut stream_future)
                .await
                .expect_err("stream should be parked mid-frame");
            drop(stream_future);

            finish_tx.send(()).unwrap();
            session.cleanup().await.unwrap();
            server_task.await.unwrap();
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stop_after_cancellation_does_not_stop_replaced_session() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let mut session = Session::new(client_io, test_timeouts());

            // `connect` normally performs this handshake before `stream`.
            server
                .send(
                    proto::RECEIVER_ID,
                    "sender-0",
                    proto::NS_HEARTBEAT,
                    json!({"type": "PONG"}),
                )
                .await;
            session.handshake(None).await.unwrap();

            let (playing_tx, playing_rx) = oneshot::channel::<()>();
            let (cancel_tx, cancel_rx) = oneshot::channel::<()>();

            let server_task = tokio::spawn(async move {
                script_to_playing(&mut server).await;
                playing_tx.send(()).unwrap();
                cancel_rx.await.unwrap();

                // Someone else replaced the DMR before the orchestrator stops.
                server
                    .send(
                        proto::RECEIVER_ID,
                        "sender-0",
                        proto::NS_RECEIVER,
                        receiver_status(
                            None,
                            vec![app_entry(
                                proto::DMR_APP_ID,
                                "session-2",
                                "Default Media Receiver",
                                true,
                            )],
                        ),
                    )
                    .await;
                expect_get_status_and_reply(
                    &mut server,
                    vec![app_entry(
                        proto::DMR_APP_ID,
                        "session-2",
                        "Default Media Receiver",
                        true,
                    )],
                )
                .await;

                let seen = drain_messages(&mut server, Duration::from_millis(150)).await;
                assert!(
                    !seen.iter().any(|m| is_type(m, proto::NS_RECEIVER, "STOP")),
                    "must not stop a replaced application"
                );
                assert!(!seen.iter().any(|m| is_type(m, proto::NS_MEDIA, "STOP")));
            });

            let stop = Arc::new(AtomicBool::new(false));
            let mut stream_future = Box::pin(session.stream(TEST_URL, &stop, None));
            tokio::select! {
                result = playing_rx => result.unwrap(),
                result = &mut stream_future => {
                    panic!("stream ended before cancellation: {result:?}")
                }
            }
            timeout(Duration::from_millis(50), &mut stream_future)
                .await
                .expect_err("stream should still be running");
            drop(stream_future);

            cancel_tx.send(()).unwrap();
            session.cleanup().await.unwrap();
            server_task.await.unwrap();
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stop_settles_pending_launch_before_stop() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let mut session = Session::new(client_io, test_timeouts());

            server
                .send(
                    proto::RECEIVER_ID,
                    "sender-0",
                    proto::NS_HEARTBEAT,
                    json!({"type": "PONG"}),
                )
                .await;
            session.handshake(None).await.unwrap();

            let (launch_tx, launch_rx) = oneshot::channel::<()>();
            let (respond_tx, respond_rx) = oneshot::channel::<()>();

            let server_task = tokio::spawn(async move {
                complete_handshake(&mut server).await;
                expect_get_status_and_reply(&mut server, Vec::new()).await;
                let launch = server.expect_message(proto::NS_RECEIVER, "LAUNCH").await;
                launch_tx.send(()).unwrap();
                respond_rx.await.unwrap();
                // The correlated launch response only arrives after the stream
                // future was cancelled; cleanup must still find it and stop the app.
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
                let get = server
                    .expect_message(proto::NS_RECEIVER, "GET_STATUS")
                    .await;
                server
                    .send(
                        proto::RECEIVER_ID,
                        "*",
                        proto::NS_RECEIVER,
                        receiver_status(
                            Some(request_id_of(&get)),
                            vec![app_entry(
                                proto::DMR_APP_ID,
                                SESSION_ID,
                                "Default Media Receiver",
                                true,
                            )],
                        ),
                    )
                    .await;
                let app_stop = server.expect_message(proto::NS_RECEIVER, "STOP").await;
                assert_eq!(
                    payload_of(&app_stop)
                        .get("sessionId")
                        .and_then(Value::as_str),
                    Some(SESSION_ID)
                );
                let close = server.expect_message(proto::NS_CONNECTION, "CLOSE").await;
                assert_eq!(close.destination, SESSION_ID);
            });

            let stop = Arc::new(AtomicBool::new(false));
            let mut stream_future = Box::pin(session.stream(TEST_URL, &stop, None));
            tokio::select! {
                result = launch_rx => result.unwrap(),
                result = &mut stream_future => {
                    panic!("stream ended before cancellation: {result:?}")
                }
            }
            // Drop the stream while the LAUNCH response is still missing.
            timeout(Duration::from_millis(50), &mut stream_future)
                .await
                .expect_err("stream should be waiting for LAUNCH");
            drop(stream_future);

            respond_tx.send(()).unwrap();
            session.cleanup().await.unwrap();
            assert!(session.owned.is_none());
            server_task.await.unwrap();
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stop_without_launch_evidence_does_not_stop() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let mut timeouts = test_timeouts();
            timeouts.stop = Duration::from_millis(200);
            let mut session = Session::new(client_io, timeouts);

            server
                .send(
                    proto::RECEIVER_ID,
                    "sender-0",
                    proto::NS_HEARTBEAT,
                    json!({"type": "PONG"}),
                )
                .await;
            session.handshake(None).await.unwrap();

            let (launch_tx, launch_rx) = oneshot::channel::<()>();
            let (respond_tx, respond_rx) = oneshot::channel::<()>();

            let server_task = tokio::spawn(async move {
                complete_handshake(&mut server).await;
                expect_get_status_and_reply(&mut server, Vec::new()).await;
                let _launch = server.expect_message(proto::NS_RECEIVER, "LAUNCH").await;
                launch_tx.send(()).unwrap();
                respond_rx.await.unwrap();
                // An unsolicited DMR from someone else is not launch evidence.
                server
                    .send(
                        proto::RECEIVER_ID,
                        "*",
                        proto::NS_RECEIVER,
                        receiver_status(
                            None,
                            vec![app_entry(
                                proto::DMR_APP_ID,
                                "someone-else",
                                "Default Media Receiver",
                                true,
                            )],
                        ),
                    )
                    .await;

                let seen = drain_messages(&mut server, Duration::from_millis(200)).await;
                assert!(!seen.iter().any(|m| is_type(m, proto::NS_RECEIVER, "STOP")));
                assert!(
                    !seen
                        .iter()
                        .any(|m| is_type(m, proto::NS_RECEIVER, "GET_STATUS"))
                );
                assert!(!seen.iter().any(|m| is_type(m, proto::NS_MEDIA, "STOP")));
            });

            let stop = Arc::new(AtomicBool::new(false));
            let mut stream_future = Box::pin(session.stream(TEST_URL, &stop, None));
            tokio::select! {
                result = launch_rx => result.unwrap(),
                result = &mut stream_future => {
                    panic!("stream ended before cancellation: {result:?}")
                }
            }
            timeout(Duration::from_millis(50), &mut stream_future)
                .await
                .expect_err("stream should be waiting for LAUNCH");
            drop(stream_future);

            respond_tx.send(()).unwrap();
            session.cleanup().await.unwrap();
            assert!(session.owned.is_none());
            server_task.await.unwrap();
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stream_observes_stop_during_setup() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let (stop, stop_for_task) = stop_flag();

            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.stream(TEST_URL, &stop_for_task, None).await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            let _get = server
                .expect_message(proto::NS_RECEIVER, "GET_STATUS")
                .await;
            // The status request is never answered; the stop flag must win.
            stop.store(true, Ordering::Relaxed);

            let (_session, result) = client.await.unwrap();
            assert!(result.is_ok(), "unexpected result: {result:?}");

            let seen = drain_messages(&mut server, Duration::from_millis(150)).await;
            assert!(
                !seen
                    .iter()
                    .any(|m| is_type(m, proto::NS_RECEIVER, "LAUNCH"))
            );
            assert!(!seen.iter().any(|m| is_type(m, proto::NS_RECEIVER, "STOP")));
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn stop_without_session_is_a_noop() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let client = tokio::spawn(async move {
                let mut session = Session::new(client_io, test_timeouts());
                session.handshake(None).await.unwrap();
                let result = session.cleanup().await;
                (session, result)
            });

            complete_handshake(&mut server).await;
            let (_session, result) = client.await.unwrap();
            assert!(result.is_ok());

            let seen = drain_messages(&mut server, Duration::from_millis(100)).await;
            assert!(
                seen.iter().all(|m| is_type(m, proto::NS_HEARTBEAT, "PING")),
                "cleanup sent unexpected frames"
            );
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn write_resumes_pending_frame_after_timeout() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let budget = Arc::new(AtomicUsize::new(10));
            let mut timeouts = test_timeouts();
            timeouts.write = Duration::from_millis(40);
            let writer = LimitedWriter::new(client_io, Arc::clone(&budget), 10);
            let mut session = Session::new(writer, timeouts);

            let first = CastMessage::json(
                "sender-0",
                proto::RECEIVER_ID,
                proto::NS_HEARTBEAT,
                &json!({"type": "PING"}),
            );
            let second = CastMessage::json(
                "sender-0",
                proto::RECEIVER_ID,
                proto::NS_HEARTBEAT,
                &json!({"type": "PONG"}),
            );

            let error = session.write_message(&first, None).await.unwrap_err();
            assert!(
                matches!(error, SessionError::Timeout("write")),
                "unexpected error: {error:?}"
            );

            // The first frame must be finished (not concatenated with the second).
            budget.store(usize::MAX, Ordering::Relaxed);
            session.write_message(&second, None).await.unwrap();

            let mut server = MockReceiver::new(server_io);
            assert_eq!(server.recv().await, first);
            assert_eq!(server.recv().await, second);
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn write_survives_cancelled_write() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let budget = Arc::new(AtomicUsize::new(10));
            let mut timeouts = test_timeouts();
            timeouts.write = Duration::from_secs(5);
            let writer = LimitedWriter::new(client_io, Arc::clone(&budget), 10);
            let mut session = Session::new(writer, timeouts);

            let first = CastMessage::json(
                "sender-0",
                proto::RECEIVER_ID,
                proto::NS_HEARTBEAT,
                &json!({"type": "PING"}),
            );
            let second = CastMessage::json(
                "sender-0",
                proto::RECEIVER_ID,
                proto::NS_HEARTBEAT,
                &json!({"type": "PONG"}),
            );

            // Cancel the write future mid-frame, as a dropped `stream` would.
            timeout(
                Duration::from_millis(20),
                session.write_message(&first, None),
            )
            .await
            .expect_err("write should be blocked by backpressure");

            budget.store(usize::MAX, Ordering::Relaxed);
            session.write_message(&second, None).await.unwrap();

            let mut server = MockReceiver::new(server_io);
            assert_eq!(server.recv().await, first);
            assert_eq!(server.recv().await, second);
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn cleanup_write_failure_is_reported_and_retryable() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let budget = Arc::new(AtomicUsize::new(0));
            let mut timeouts = test_timeouts();
            timeouts.write = Duration::from_millis(30);
            timeouts.stop = Duration::from_millis(400);
            let writer = LimitedWriter::new(client_io, Arc::clone(&budget), 10);
            let mut session = Session::new(writer, timeouts);
            session.owned = Some(OwnedSession {
                session_id: SESSION_ID.to_owned(),
                transport_id: SESSION_ID.to_owned(),
                media_session_id: Some(MEDIA_SESSION_ID),
                playing: true,
                media_namespace: true,
            });

            let first = session.cleanup().await;
            assert!(first.is_err(), "cleanup must report write failures");
            assert!(session.owned.is_some(), "owned session kept for retry");

            budget.store(usize::MAX, Ordering::Relaxed);
            let server_task = tokio::spawn(async move {
                let mut server = MockReceiver::new(server_io);
                let (mut media_stops, mut app_stops) = (0, 0);
                loop {
                    match tokio::time::timeout(Duration::from_millis(600), server.recv()).await {
                        Err(_) => break,
                        Ok(message) => {
                            if is_type(&message, proto::NS_HEARTBEAT, "PING") {
                                server.reply_pong(&message).await;
                            } else if is_type(&message, proto::NS_RECEIVER, "GET_STATUS") {
                                server
                                    .send(
                                        proto::RECEIVER_ID,
                                        "*",
                                        proto::NS_RECEIVER,
                                        receiver_status(
                                            Some(request_id_of(&message)),
                                            vec![app_entry(
                                                proto::DMR_APP_ID,
                                                SESSION_ID,
                                                "Default Media Receiver",
                                                true,
                                            )],
                                        ),
                                    )
                                    .await;
                            } else if is_type(&message, proto::NS_MEDIA, "GET_STATUS") {
                                server
                                    .send(
                                        SESSION_ID,
                                        "*",
                                        proto::NS_MEDIA,
                                        media_status(
                                            Some(request_id_of(&message)),
                                            "PLAYING",
                                            MEDIA_SESSION_ID,
                                        ),
                                    )
                                    .await;
                            } else if is_type(&message, proto::NS_MEDIA, "STOP") {
                                media_stops += 1;
                            } else if is_type(&message, proto::NS_RECEIVER, "STOP") {
                                app_stops += 1;
                            } else if is_type(&message, proto::NS_CONNECTION, "CLOSE") {
                                break;
                            }
                        }
                    }
                }
                (media_stops, app_stops)
            });

            session.cleanup().await.unwrap();
            assert!(session.owned.is_none());
            let (media_stops, app_stops) = server_task.await.unwrap();
            assert!(media_stops >= 1, "expected a media STOP");
            assert!(app_stops >= 1, "expected an application STOP");
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn cleanup_resumes_partial_stop_frame_after_timeout() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let budget = Arc::new(AtomicUsize::new(usize::MAX));
            let mut timeouts = test_timeouts();
            timeouts.write = Duration::from_millis(30);
            timeouts.stop = Duration::from_millis(500);
            let writer = LimitedWriter::new(client_io, Arc::clone(&budget), 10);
            let mut session = Session::new(writer, timeouts);
            session.owned = Some(OwnedSession {
                session_id: SESSION_ID.to_owned(),
                transport_id: SESSION_ID.to_owned(),
                media_session_id: Some(MEDIA_SESSION_ID),
                playing: true,
                media_namespace: true,
            });

            let (phase2_tx, phase2_rx) = oneshot::channel::<()>();
            let budget_for_server = Arc::clone(&budget);
            let server_task = tokio::spawn(async move {
                let mut server = MockReceiver::new(server_io);
                // First cleanup: both status verifications succeed, then the media
                // STOP is cut off after five bytes.
                let get = server
                    .expect_message(proto::NS_RECEIVER, "GET_STATUS")
                    .await;
                server
                    .send(
                        proto::RECEIVER_ID,
                        "*",
                        proto::NS_RECEIVER,
                        receiver_status(
                            Some(request_id_of(&get)),
                            vec![app_entry(
                                proto::DMR_APP_ID,
                                SESSION_ID,
                                "Default Media Receiver",
                                true,
                            )],
                        ),
                    )
                    .await;
                let media_get = server.expect_message(proto::NS_MEDIA, "GET_STATUS").await;
                budget_for_server.store(5, Ordering::Relaxed);
                server
                    .send(
                        SESSION_ID,
                        "*",
                        proto::NS_MEDIA,
                        media_status(Some(request_id_of(&media_get)), "PLAYING", MEDIA_SESSION_ID),
                    )
                    .await;

                phase2_rx.await.unwrap();
                let (mut media_stops, mut app_stops) = (0, 0);
                loop {
                    match tokio::time::timeout(Duration::from_millis(600), server.recv()).await {
                        Err(_) => break,
                        Ok(message) => {
                            if is_type(&message, proto::NS_HEARTBEAT, "PING") {
                                server.reply_pong(&message).await;
                            } else if is_type(&message, proto::NS_RECEIVER, "GET_STATUS") {
                                server
                                    .send(
                                        proto::RECEIVER_ID,
                                        "*",
                                        proto::NS_RECEIVER,
                                        receiver_status(
                                            Some(request_id_of(&message)),
                                            vec![app_entry(
                                                proto::DMR_APP_ID,
                                                SESSION_ID,
                                                "Default Media Receiver",
                                                true,
                                            )],
                                        ),
                                    )
                                    .await;
                            } else if is_type(&message, proto::NS_MEDIA, "GET_STATUS") {
                                server
                                    .send(
                                        SESSION_ID,
                                        "*",
                                        proto::NS_MEDIA,
                                        media_status(
                                            Some(request_id_of(&message)),
                                            "PLAYING",
                                            MEDIA_SESSION_ID,
                                        ),
                                    )
                                    .await;
                            } else if is_type(&message, proto::NS_MEDIA, "STOP") {
                                media_stops += 1;
                            } else if is_type(&message, proto::NS_RECEIVER, "STOP") {
                                app_stops += 1;
                            } else if is_type(&message, proto::NS_CONNECTION, "CLOSE") {
                                break;
                            }
                        }
                    }
                }
                (media_stops, app_stops)
            });

            let first = session.cleanup().await;
            assert!(first.is_err(), "partial write must fail cleanup");
            assert!(session.owned.is_some());

            // Resume: the pending STOP frame completes before any new frame.
            budget.store(usize::MAX, Ordering::Relaxed);
            phase2_tx.send(()).unwrap();
            session.cleanup().await.unwrap();
            assert!(session.owned.is_none());

            let (media_stops, app_stops) = server_task.await.unwrap();
            assert!(
                media_stops >= 2,
                "expected the resumed and a fresh media STOP"
            );
            assert!(app_stops >= 1, "expected an application STOP");
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn cleanup_abandons_same_app_media_takeover() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let mut session = Session::new(client_io, test_timeouts());
            session.owned = Some(OwnedSession {
                session_id: SESSION_ID.to_owned(),
                transport_id: SESSION_ID.to_owned(),
                media_session_id: Some(MEDIA_SESSION_ID),
                playing: true,
                media_namespace: true,
            });

            let server_task = tokio::spawn(async move {
                let get = server
                    .expect_message(proto::NS_RECEIVER, "GET_STATUS")
                    .await;
                server
                    .send(
                        proto::RECEIVER_ID,
                        "*",
                        proto::NS_RECEIVER,
                        receiver_status(
                            Some(request_id_of(&get)),
                            vec![app_entry(
                                proto::DMR_APP_ID,
                                SESSION_ID,
                                "Default Media Receiver",
                                true,
                            )],
                        ),
                    )
                    .await;
                let media_get = server.expect_message(proto::NS_MEDIA, "GET_STATUS").await;
                // The media session was taken over within the same DMR between
                // disconnect and cleanup.
                server
                    .send(
                        SESSION_ID,
                        "*",
                        proto::NS_MEDIA,
                        media_status(
                            Some(request_id_of(&media_get)),
                            "PLAYING",
                            MEDIA_SESSION_ID + 1,
                        ),
                    )
                    .await;

                let seen = drain_messages(&mut server, Duration::from_millis(150)).await;
                assert!(!seen.iter().any(|m| is_type(m, proto::NS_MEDIA, "STOP")));
                assert!(!seen.iter().any(|m| is_type(m, proto::NS_RECEIVER, "STOP")));
            });

            let result = session.cleanup().await;
            assert!(result.is_ok(), "takeover should skip quietly: {result:?}");
            assert!(session.owned.is_none());
            server_task.await.unwrap();
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn cleanup_media_query_is_sequential_and_correlated() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let mut session = Session::new(client_io, test_timeouts());
            session.owned = Some(OwnedSession {
                session_id: SESSION_ID.to_owned(),
                transport_id: SESSION_ID.to_owned(),
                media_session_id: Some(MEDIA_SESSION_ID),
                playing: true,
                media_namespace: true,
            });

            let server_task = tokio::spawn(async move {
                let get = server
                    .expect_message(proto::NS_RECEIVER, "GET_STATUS")
                    .await;
                // The media query must not be issued before the receiver answer is
                // processed, or an early media reply would be swallowed by the
                // receiver wait.
                let early = tokio::time::timeout(Duration::from_millis(50), async {
                    loop {
                        let message = server.recv().await;
                        assert!(
                            !is_type(&message, proto::NS_MEDIA, "GET_STATUS"),
                            "media query issued before receiver verification"
                        );
                        if is_type(&message, proto::NS_HEARTBEAT, "PING") {
                            server.reply_pong(&message).await;
                        }
                    }
                })
                .await;
                assert!(
                    early.is_err(),
                    "unexpected frames before the receiver reply"
                );
                server
                    .send(
                        proto::RECEIVER_ID,
                        "*",
                        proto::NS_RECEIVER,
                        receiver_status(
                            Some(request_id_of(&get)),
                            vec![app_entry(
                                proto::DMR_APP_ID,
                                SESSION_ID,
                                "Default Media Receiver",
                                true,
                            )],
                        ),
                    )
                    .await;

                let media_get = server.expect_message(proto::NS_MEDIA, "GET_STATUS").await;
                // An unrelated broadcast MEDIA_STATUS must not be consumed as the
                // answer to the media query.
                server
                    .send(
                        SESSION_ID,
                        "*",
                        proto::NS_MEDIA,
                        media_status(None, "PLAYING", MEDIA_SESSION_ID),
                    )
                    .await;
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
                let _app_stop = server.expect_message(proto::NS_RECEIVER, "STOP").await;
                let _close = server.expect_message(proto::NS_CONNECTION, "CLOSE").await;
            });

            let result = session.cleanup().await;
            assert!(result.is_ok(), "cleanup failed: {result:?}");
            assert!(session.owned.is_none());
            server_task.await.unwrap();
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn cleanup_unverified_media_status_leaves_session_running() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut server = MockReceiver::new(server_io);
            let mut timeouts = test_timeouts();
            timeouts.stop = Duration::from_millis(300);
            let mut session = Session::new(client_io, timeouts);
            session.owned = Some(OwnedSession {
                session_id: SESSION_ID.to_owned(),
                transport_id: SESSION_ID.to_owned(),
                media_session_id: Some(MEDIA_SESSION_ID),
                playing: true,
                media_namespace: true,
            });

            let server_task = tokio::spawn(async move {
                let get = server
                    .expect_message(proto::NS_RECEIVER, "GET_STATUS")
                    .await;
                server
                    .send(
                        proto::RECEIVER_ID,
                        "*",
                        proto::NS_RECEIVER,
                        receiver_status(
                            Some(request_id_of(&get)),
                            vec![app_entry(
                                proto::DMR_APP_ID,
                                SESSION_ID,
                                "Default Media Receiver",
                                true,
                            )],
                        ),
                    )
                    .await;
                // Read the media query but never answer it.
                let _media_get = server.expect_message(proto::NS_MEDIA, "GET_STATUS").await;

                let seen = drain_messages(&mut server, Duration::from_millis(200)).await;
                assert!(!seen.iter().any(|m| is_type(m, proto::NS_MEDIA, "STOP")));
                assert!(!seen.iter().any(|m| is_type(m, proto::NS_RECEIVER, "STOP")));
            });

            let result = session.cleanup().await;
            assert!(
                matches!(&result, Err(SessionError::CleanupUnverified)),
                "unexpected result: {result:?}"
            );
            assert!(session.owned.is_some(), "session kept for a later retry");
            server_task.await.unwrap();
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    async fn write_deadline_bounds_trickle_and_remains_resumable() {
        timeout(TEST_TIMEOUT, async {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let mut timeouts = test_timeouts();
            timeouts.write = Duration::from_millis(30);
            let writer = TrickleWriter::new(client_io, Duration::from_millis(10));
            let mut session = Session::new(writer, timeouts);

            let first = CastMessage::json(
                "sender-0",
                proto::RECEIVER_ID,
                proto::NS_HEARTBEAT,
                &json!({"type": "PING"}),
            );
            let second = CastMessage::json(
                "sender-0",
                proto::RECEIVER_ID,
                proto::NS_HEARTBEAT,
                &json!({"type": "PONG"}),
            );

            let started = Instant::now();
            let error = session.write_message(&first, None).await.unwrap_err();
            assert!(
                matches!(error, SessionError::Timeout("write")),
                "unexpected error: {error:?}"
            );
            assert!(
                started.elapsed() < Duration::from_millis(500),
                "a trickling socket must not extend the per-call deadline"
            );

            // The partially written frame is still resumable, and the second
            // frame is not concatenated into it.
            session.timeouts.write = Duration::from_secs(5);
            session.write_message(&second, None).await.unwrap();

            let mut server = MockReceiver::new(server_io);
            assert_eq!(server.recv().await, first);
            assert_eq!(server.recv().await, second);
        })
        .await
        .expect("test timed out");
    }
}
