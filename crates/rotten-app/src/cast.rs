//! Experimental built-in Google Cast (Default Media Receiver) sender.
//!
//! Captures the desktop, encodes H.264 with OpenH264, optionally captures
//! system audio (WASAPI loopback on Windows, encoded as AAC), muxes both
//! tracks into a live HLS playlist, serves that over the LAN, and asks the
//! selected Google Cast receiver to play it. This path never changes the local
//! mute state: system sound keeps playing on the PC speakers.
//!
//! # Trust model
//!
//! The Cast control channel does not authenticate the receiver identity (see
//! `rotten_cast::control`) and the HLS media itself is served over plain
//! HTTP. Use this only on a trusted LAN.

mod interleave;
mod metrics;
mod pcm;
mod settings;

pub use metrics::{PipelineSummary, StageStats};
pub use settings::{CastLatency, CastQuality, CastSettings};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use rotten_capture::{CaptureBackend, create_capture_backend};
use rotten_cast::CastClient;
use rotten_cast::hls::{HlsMuxer, HlsStore};
use rotten_cast::http::HttpServer;
use rotten_core::device::CastDevice;
use rotten_video::{Encoder, SoftwareEncoder, SyntheticSource, downscale_rgba};

use crate::audio::timed::{SessionClock, TimedPcm};
use crate::cast::interleave::{AudioVideoInterleaver, VideoAu};
use crate::cast::metrics::PipelineMetrics;
use crate::cast::pcm::{
    HOLD_BACK_FRAMES, LAG_LIMIT_FRAMES, MAX_BLOCKS_PER_POLL, PcmTimeline, frames_from_us,
    pulse_active, synthetic_pcm,
};
use crate::cast_audio::AacEncoder;

/// Balanced output envelope: never above 720p on either side.
const MAX_WIDTH: u32 = 1280;
const MAX_HEIGHT: u32 = 720;
/// High output envelope: a true full-HD visible size on the even-pixel grid.
const HIGH_MAX_WIDTH: u32 = 1920;
const HIGH_MAX_HEIGHT: u32 = 1080;
/// Smallest visible side the exact High encoder accepts; below this the native
/// OpenH264 path rejects the frame, so the fit must fail instead of silently
/// producing an unencodable size.
const HIGH_MIN_SIDE: u32 = 16;
/// Fixed live-stream pacing for the Cast path; every quality preset runs at
/// 30 fps.
const TARGET_FPS: u32 = 30;
/// Bound on the loopback start so a hung endpoint cannot hold the session.
const AUDIO_START_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounded packet drain per producer iteration; excess stays in the channel.
const MAX_PCM_CHUNKS_PER_POLL: usize = 64;
/// Bound the wait for the store's selected readiness watermark (8 s stable,
/// 4 s responsive) before LOAD; the budget also covers native
/// capture/encoder initialization and is not the latency target.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const READY_POLL: Duration = Duration::from_millis(50);
/// Cast liveness is a 15 s window with a 5 s heartbeat; the pre-LOAD
/// readiness wait keeps the control connection warm at that cadence.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// How often the streaming select checks that the HLS server is still alive.
const SERVER_POLL: Duration = Duration::from_millis(250);
/// Capture workers can be stuck in native calls; never wait forever.
const PRODUCER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Give up after this many consecutive failed grabs (about 3 s at one 100 ms
/// retry backoff) instead of spinning forever on a dead capture backend.
const CAPTURE_ERROR_LIMIT: u32 = 30;
/// Hard cap for one `cast-benchmark` run.
const MAX_BENCHMARK_DURATION: Duration = Duration::from_secs(60);

/// Configuration for [`run_cast`].
#[derive(Debug, Clone, Copy)]
pub struct CastConfig {
    /// Capture this display; `None` selects the default display.
    pub display_index: Option<u32>,
    /// Send the synthetic test pattern instead of capturing a display.
    pub test_mode: bool,
    /// Fixed LAN HTTP port for the HLS server; `0` picks an ephemeral port.
    pub http_port: u16,
    /// Capture and stream system audio as AAC alongside the video. Defaults to
    /// `cfg!(target_os = "windows")`; a test-mode session uses the synthetic
    /// pulse instead of touching an endpoint.
    pub audio: bool,
    /// Video quality preset; defaults to Balanced (720p, 4 Mbps).
    pub quality: CastQuality,
    /// HLS startup-latency preset; defaults to Stable (about 8 s buffer).
    pub latency: CastLatency,
}

impl Default for CastConfig {
    fn default() -> Self {
        Self {
            display_index: None,
            test_mode: false,
            http_port: 0,
            audio: cfg!(target_os = "windows"),
            quality: CastQuality::default(),
            latency: CastLatency::default(),
        }
    }
}

/// Stream the desktop to a Google Cast receiver until `stop` is set.
///
/// `on_playing` runs once when the receiver reports `PLAYING`. The caller's
/// `stop` flag is only ever read: cancellation of this future or of the
/// capture worker uses private tokens, so stopping one session can never end
/// the next one.
pub async fn run_cast(
    device: CastDevice,
    config: CastConfig,
    stop: Arc<AtomicBool>,
    on_playing: Option<Box<dyn FnOnce() + Send + 'static>>,
) -> anyhow::Result<()> {
    warn_experimental(&device, config.audio);
    if stop.load(Ordering::Relaxed) {
        tracing::info!("Cast cancelled before any capture started");
        return Ok(());
    }
    // Fail before connecting: a caller that explicitly asked for audio must not
    // silently get a video-only session, and this build cannot encode AAC.
    #[cfg(not(target_os = "windows"))]
    if config.audio {
        anyhow::bail!(
            "system audio capture for Cast is only implemented on Windows (WASAPI loopback + \
             Media Foundation AAC); pass --no-audio or turn off \"System audio\" for a \
             video-only Cast session"
        );
    }

    let Some(mut client) =
        crate::mirror::until_stopped(CastClient::connect(&device.host, device.port), stop.clone())
            .await
            .with_context(|| {
                format!(
                    "connecting to the Cast receiver {}:{}",
                    device.host, device.port
                )
            })?
    else {
        tracing::info!("Cast cancelled before the control connection was established");
        return Ok(());
    };

    run_session(&mut client, &device, &config, &stop, on_playing).await
}

fn warn_experimental(device: &CastDevice, audio: bool) {
    tracing::info!(
        receiver = %device.name,
        host = %device.host,
        port = device.port,
        model = ?device.model,
        audio,
        "starting an experimental Google Cast (Default Media Receiver) session"
    );
    if audio {
        tracing::warn!(
            "Cast streams H.264 video with AAC system audio, adds several seconds of buffering \
             latency, and never mutes the local speakers; the HLS media is unencrypted over the \
             LAN while the control TLS identity stays unauthenticated — use it only on a trusted \
             network and allow the app through the Windows firewall on the private network"
        );
    } else {
        tracing::warn!(
            "Cast is video only (system audio disabled), adds several seconds of buffering \
             latency, and never mutes the local speakers; the HLS media is unencrypted over the \
             LAN while the control TLS identity stays unauthenticated — use it only on a trusted \
             network and allow the app through the Windows firewall on the private network"
        );
    }
}

/// [`run_capture_benchmark_with_presets`] with the default Balanced quality and
/// Stable latency presets. Kept so existing callers keep the exact previous
/// behavior and measurements.
pub async fn run_capture_benchmark(
    display_index: Option<u32>,
    test_mode: bool,
    duration: Duration,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<PipelineSummary> {
    run_capture_benchmark_with_presets(
        display_index,
        test_mode,
        duration,
        stop,
        CastQuality::default(),
        CastLatency::default(),
    )
    .await
}

/// Measures the exact producer pipeline used by [`run_cast`] without any
/// receiver or audio side effects: local capture -> scale -> H.264 -> HLS mux
/// into a bounded in-memory [`HlsStore`] created with the selected profile. No
/// `CastClient`, HTTP server, audio endpoint, filesystem output or full media
/// URL is involved.
///
/// `test_mode` uses the preset's own synthetic source (1280x720 balanced,
/// 1920x1080 high), so a local run measures the real producer rather than a
/// smaller stand-in.
///
/// Runs for `duration` (1..=60 s), or until `stop` is set, whichever comes
/// first. `stop` is only ever read: a private token ends the blocking worker,
/// which is then awaited for at most [`PRODUCER_SHUTDOWN_TIMEOUT`]. A producer
/// error is preserved even when the run was cancelled. Cancellation before the
/// capture backend opens returns an empty summary without touching hardware.
pub async fn run_capture_benchmark_with_presets(
    display_index: Option<u32>,
    test_mode: bool,
    duration: Duration,
    stop: Arc<AtomicBool>,
    quality: CastQuality,
    latency: CastLatency,
) -> anyhow::Result<PipelineSummary> {
    if duration < Duration::from_secs(1) || duration > MAX_BENCHMARK_DURATION {
        anyhow::bail!(
            "benchmark duration must be between 1 and {} seconds, got {:.3}s",
            MAX_BENCHMARK_DURATION.as_secs(),
            duration.as_secs_f64()
        );
    }
    if stop.load(Ordering::Relaxed) {
        tracing::info!("Cast benchmark cancelled before any capture started");
        return Ok(PipelineSummary::empty_for(quality, latency));
    }

    let clock = SessionClock::new().context("creating the Cast benchmark session clock")?;
    // Same bounded store the live path uses, created with the same profile;
    // nothing serves it anywhere.
    let store = Arc::new(Mutex::new(
        HlsStore::with_profile(latency.hls_profile(), quality.bandwidth_bps())
            .context("creating the Cast benchmark HLS store")?,
    ));
    let producer_stop = Arc::new(AtomicBool::new(false));
    // The caller's shared flag is never written; the guard stops the worker on
    // every return path, including cancellation.
    let _producer_guard = ProducerStopOnDrop(producer_stop.clone());
    // `--test` uses the preset's real encode size, so the measurements describe
    // the real producer and not a smaller stand-in.
    let config = ProducerConfig {
        test_mode,
        display_index,
        audio: false,
        quality,
        latency,
    };
    // One owned blocking worker running the same loop as the live Cast path.
    // No second task is spawned for the deadline; it is raced inline below.
    let mut worker = tokio::task::spawn_blocking({
        let store = store.clone();
        let producer_stop = producer_stop.clone();
        let external_stop = stop.clone();
        move || run_capture_loop(config, clock, None, &producer_stop, &external_stop, &store)
    });

    let joined = tokio::select! {
        joined = &mut worker => Some(joined),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(Instant::now() + duration)) => None,
        _ = wait_for_stop(&stop) => None,
    };
    let Some(joined) = joined else {
        // Deadline or user cancellation: set the private token promptly, then
        // bound the join so a native capture call cannot hold the command open.
        producer_stop.store(true, Ordering::Relaxed);
        return match tokio::time::timeout(PRODUCER_SHUTDOWN_TIMEOUT, worker).await {
            Ok(joined) => join_benchmark_worker(joined),
            Err(_) => {
                tracing::warn!(
                    timeout_secs = PRODUCER_SHUTDOWN_TIMEOUT.as_secs(),
                    "Cast benchmark capture worker did not stop in time; detaching after requesting stop"
                );
                anyhow::bail!(
                    "the Cast benchmark capture worker did not stop within {}s after cancellation",
                    PRODUCER_SHUTDOWN_TIMEOUT.as_secs()
                );
            }
        };
    };
    join_benchmark_worker(joined)
}

fn join_benchmark_worker(
    joined: std::result::Result<anyhow::Result<PipelineSummary>, tokio::task::JoinError>,
) -> anyhow::Result<PipelineSummary> {
    match joined {
        Ok(result) => result,
        Err(join_error) => Err(anyhow!(
            "the Cast benchmark capture worker panicked: {join_error}"
        )),
    }
}

async fn run_session(
    client: &mut CastClient,
    device: &CastDevice,
    config: &CastConfig,
    stop: &Arc<AtomicBool>,
    on_playing: Option<Box<dyn FnOnce() + Send + 'static>>,
) -> anyhow::Result<()> {
    // One clock is shared by the video timestamps, the 44.1 kHz audio grid and
    // the synthetic test pattern, so A/V timing is anchored to one origin.
    let clock = SessionClock::new().context("creating the shared Cast session clock")?;

    // The quality and latency presets are locked for this session: the store,
    // muxer and IDR cadence all use the selected profile, and a later selection
    // change can never alter a running stream. Created before any capture side
    // effect because it is the one fallible constructor here.
    let store = Arc::new(Mutex::new(
        HlsStore::with_profile(config.latency.hls_profile(), config.quality.bandwidth_bps())
            .context("creating the Cast HLS store")?,
    ));

    // System audio capture starts before the native capture does. Test mode
    // uses the deterministic synthetic pulse and never opens an endpoint.
    let mut audio_handle: Option<AudioCapture> = None;
    let mut pcm_rx: Option<tokio::sync::mpsc::Receiver<TimedPcm>> = None;
    if config.audio && !config.test_mode {
        #[cfg(target_os = "windows")]
        {
            match start_audio_capture(&clock, stop).await? {
                Some((handle, receiver)) => {
                    tracing::info!("Cast system audio capture started (WASAPI loopback)");
                    audio_handle = Some(AudioCapture::new(handle));
                    pcm_rx = Some(receiver);
                }
                None => {
                    tracing::info!("Cast cancelled while starting system audio capture");
                    return Ok(());
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            anyhow::bail!(
                "system audio capture for Cast is only implemented on Windows (WASAPI loopback \
                 + Media Foundation AAC); pass --no-audio or turn off \"System audio\" for a \
                 video-only Cast session"
            );
        }
    }

    // Bind the interface the Cast control connection uses, preserving an IPv6
    // zone id: rebuilding with `SocketAddr::new` would drop the scope and make
    // a link-local bind fail. The random URL token stays out of logs.
    let mut bind_addr = client.local_addr();
    bind_addr.set_port(config.http_port);
    if matches!(bind_addr.ip(), std::net::IpAddr::V6(ip) if ip.is_unicast_link_local()) {
        stop_audio_logged(&mut audio_handle).await;
        anyhow::bail!(
            "the Cast control connection uses link-local IPv6 ({bind_addr}), whose zone id cannot \
             be embedded in the HLS URL; reconnect using the receiver's IPv4 address"
        );
    }
    let allowed_peer = client.peer_addr().ip();
    let mut server = match HttpServer::start(bind_addr, allowed_peer, store.clone()).await {
        Ok(server) => server,
        Err(error) => {
            stop_audio_logged(&mut audio_handle).await;
            return Err(error)
                .with_context(|| format!("starting the Cast HLS server on {bind_addr}"));
        }
    };
    tracing::info!(
        %bind_addr,
        allowed_peer = %allowed_peer,
        "Cast HLS server ready (media URL withheld: it carries an access token)"
    );

    let producer_stop = Arc::new(AtomicBool::new(false));
    // Drop guard: every return/cancel/panic path stops the blocking worker
    // without ever writing to the caller's shared stop flag.
    let _producer_guard = ProducerStopOnDrop(producer_stop.clone());
    let (producer_done_tx, mut producer_done_rx) =
        tokio::sync::oneshot::channel::<anyhow::Result<()>>();
    let producer_handle = tokio::task::spawn_blocking({
        let store = store.clone();
        let producer_stop = producer_stop.clone();
        let external_stop = stop.clone();
        let producer_config = ProducerConfig {
            test_mode: config.test_mode,
            display_index: config.display_index,
            audio: config.audio,
            quality: config.quality,
            latency: config.latency,
        };
        move || {
            let result = run_capture_loop(
                producer_config,
                clock,
                pcm_rx,
                &producer_stop,
                &external_stop,
                &store,
            );
            if let Err(error) = &result {
                tracing::warn!(%error, "Cast capture/encode producer stopped with an error");
            }
            // The live session only needs success/failure; the counters were
            // already traced periodically and in the loop's final summary.
            let _ = producer_done_tx.send(result.map(|_| ()));
        }
    });

    // Hold the receiver at the control layer until enough HLS data exists.
    let mut ready = false;
    let mut session_error: Option<anyhow::Error> = None;
    match wait_until_hls_ready(
        &store,
        &mut producer_done_rx,
        &server,
        stop,
        ReadyPolicy {
            timeout: READY_TIMEOUT,
            status_interval: HEARTBEAT_INTERVAL,
            initial_buffer_secs: config.latency.initial_buffer_secs(),
        },
        client,
    )
    .await
    {
        ReadyWait::Ready => ready = true,
        ReadyWait::Stopped => {}
        ReadyWait::Failed(error) => session_error = Some(error),
    }

    let mut stream_result: Option<anyhow::Result<()>> = None;
    if ready {
        let url = server.url().to_owned();
        tracing::info!(receiver = %device.name, "asking the receiver to play the live HLS stream");
        tokio::select! {
            result = client.stream(&url, stop.clone(), on_playing) => {
                stream_result = Some(result);
            }
            result = &mut producer_done_rx => {
                let cancelled = stop.load(Ordering::Relaxed);
                match result {
                    Ok(_) if cancelled => {
                        tracing::info!("Cast capture stopped after cancellation");
                    }
                    Ok(Ok(())) => {
                        session_error = Some(anyhow!(
                            "desktop capture stopped unexpectedly while casting"
                        ));
                    }
                    Ok(Err(error)) => {
                        session_error =
                            Some(error.context("desktop capture/encode failed while casting"));
                    }
                    Err(_) => {
                        session_error =
                            Some(anyhow!("the Cast capture worker panicked while casting"));
                    }
                }
            }
            _ = server_failed(&server) => {
                session_error = Some(anyhow!("the Cast HLS server stopped while casting"));
            }
        }
    }

    // Stop publishing first, then release the receiver session. `stream` sends
    // its own bounded STOP when it returns, but this future cancelled it when
    // the producer failed, so an explicit best-effort STOP is required here.
    producer_stop.store(true, Ordering::Relaxed);
    let stream_failed = stream_result.as_ref().is_some_and(|result| result.is_err());
    match client.stop().await {
        Ok(()) => {}
        Err(error) => {
            tracing::warn!(%error, "explicit Cast STOP failed");
            if session_error.is_none() && !stream_failed {
                session_error = Some(error.context("stopping the Cast receiver session"));
            }
        }
    }

    if let Err(error) = server.shutdown().await {
        tracing::warn!(%error, "Cast HLS server shutdown failed");
        if session_error.is_none() && !stream_failed {
            session_error = Some(error.context("shutting down the Cast HLS server"));
        }
    }

    // Explicitly await the audio worker's bounded stop alongside the other
    // cleanup; dropping the handle (via `stop_audio`) would only signal it.
    // A real stop/join failure is surfaced even after a user stop, so the
    // session never reports a false clean shutdown; the first error wins.
    match stop_audio(&mut audio_handle).await {
        Ok(()) => {}
        Err(error) => {
            tracing::warn!(%error, "stopping the Cast system audio capture failed");
            if session_error.is_none() && !stream_failed {
                session_error = Some(error.context("stopping Cast system audio capture"));
            }
        }
    }
    let producer_result = finish_producer(producer_handle, &mut producer_done_rx).await;

    if let Some(error) = session_error {
        return Err(error);
    }
    if let Some(result) = stream_result {
        result?;
    }
    // A producer failure observed only after an explicit Stop is not a session
    // failure: the user asked to end the cast, and the worker was cancelled.
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    producer_result
}

/// Owns the timed loopback capture for one Cast session. Dropping it signals
/// the capture worker whatever the exit path; [`Self::stop`] awaits the bounded
/// join and reports capture errors that happened after startup.
struct AudioCapture {
    #[cfg(target_os = "windows")]
    loopback: crate::audio::timed::TimedLoopback,
}

impl AudioCapture {
    #[cfg(target_os = "windows")]
    fn new(loopback: crate::audio::timed::TimedLoopback) -> Self {
        Self { loopback }
    }

    async fn stop(self) -> anyhow::Result<()> {
        #[cfg(target_os = "windows")]
        {
            self.loopback.stop().await
        }
        #[cfg(not(target_os = "windows"))]
        {
            Ok(())
        }
    }
}

/// Starts the timed loopback with a bounded, cancellable start. `--test` never
/// reaches this function, so a test session performs no endpoint work.
#[cfg(target_os = "windows")]
async fn start_audio_capture(
    clock: &SessionClock,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<
    Option<(
        crate::audio::timed::TimedLoopback,
        tokio::sync::mpsc::Receiver<TimedPcm>,
    )>,
> {
    let started = bounded_start(
        crate::audio::timed::TimedLoopback::start(clock.clone()),
        stop,
        AUDIO_START_TIMEOUT,
    )
    .await
    .context(
        "starting Cast system audio capture (WASAPI loopback); pass --no-audio or turn off \
         \"System audio\" for a video-only cast",
    )?;
    Ok(started)
}

/// Resolves to `Ok(Some(_))` when `start` finishes, `Ok(None)` when the shared
/// stop flag is set first, and an error after `timeout`. Dropping the start
/// future on either alternative signals a partially created capture worker.
#[cfg(any(target_os = "windows", test))]
async fn bounded_start<F, T>(
    start: F,
    stop: &AtomicBool,
    timeout: Duration,
) -> anyhow::Result<Option<T>>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    tokio::select! {
        result = start => result.map(Some),
        _ = wait_for_stop(stop) => Ok(None),
        _ = tokio::time::sleep(timeout) => {
            anyhow::bail!(
                "timed out after {}s starting the audio capture; the audio device may be stuck",
                timeout.as_secs()
            )
        }
    }
}

/// Stops and joins the audio capture; the handle is consumed either way.
async fn stop_audio(audio: &mut Option<AudioCapture>) -> anyhow::Result<()> {
    match audio.take() {
        Some(handle) => handle.stop().await,
        None => Ok(()),
    }
}

/// Best-effort audio stop for early error paths that already have an error.
async fn stop_audio_logged(audio: &mut Option<AudioCapture>) {
    if let Err(error) = stop_audio(audio).await {
        tracing::warn!(%error, "stopping the Cast system audio capture failed");
    }
}

/// Result of waiting for the HLS store readiness watermark.
#[derive(Debug)]
enum ReadyWait {
    Ready,
    Stopped,
    Failed(anyhow::Error),
}

/// Periodic liveness probe used while waiting for HLS readiness.
///
/// Implemented for [`CastClient`]; tests inject deterministic fakes.
trait ReadinessProbe {
    async fn probe(&mut self) -> anyhow::Result<()>;
}

impl ReadinessProbe for CastClient {
    async fn probe(&mut self) -> anyhow::Result<()> {
        self.receiver_status().await?;
        Ok(())
    }
}

/// Producer completion result plus the join-channel receive error.
type ProducerJoin = std::result::Result<anyhow::Result<()>, tokio::sync::oneshot::error::RecvError>;

/// Timing policy for the pre-LOAD readiness wait.
///
/// `timeout` is the fixed 30 s startup budget, deliberately independent of the
/// selected latency preset: it covers native capture/encoder initialization on
/// slow machines, not the media buffer. `initial_buffer_secs` only shapes the
/// user-facing timeout text and must stay in sync with the store's readiness
/// watermark.
#[derive(Debug, Clone, Copy)]
struct ReadyPolicy {
    timeout: Duration,
    status_interval: Duration,
    initial_buffer_secs: u32,
}

/// Wait until the store advertises its selected initial buffer (8 s stable,
/// 4 s responsive), the producer fails, the server dies, the user stops, or
/// the readiness deadline expires.
///
/// The receiver-status probe (Cast liveness is a 5 s heartbeat inside a 15 s
/// window) is always raced against cancellation, producer completion, server
/// health and the deadline, so a slow status request can never overrun the
/// readiness budget. Dropping the probe mid-request is safe: the control
/// session ignores an uncorrelated late response.
async fn wait_until_hls_ready(
    store: &Mutex<HlsStore>,
    producer_done: &mut tokio::sync::oneshot::Receiver<anyhow::Result<()>>,
    server: &HttpServer,
    stop: &AtomicBool,
    policy: ReadyPolicy,
    probe: &mut impl ReadinessProbe,
) -> ReadyWait {
    let ReadyPolicy {
        timeout,
        status_interval,
        initial_buffer_secs,
    } = policy;
    let deadline = Instant::now() + timeout;
    let mut next_status = Instant::now() + status_interval;
    loop {
        if lock_store(store).ready() {
            return ReadyWait::Ready;
        }
        if stop.load(Ordering::Relaxed) {
            return ReadyWait::Stopped;
        }
        if server.check_health().is_err() {
            return ReadyWait::Failed(server_stopped_error());
        }
        match producer_done.try_recv() {
            Ok(result) => return producer_ready_outcome(result, stop),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                return producer_ready_outcome(
                    Err(anyhow!("the Cast capture worker panicked")),
                    stop,
                );
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
        }
        let now = Instant::now();
        if now >= deadline {
            return ReadyWait::Failed(readiness_timeout_error(timeout, initial_buffer_secs));
        }

        if now >= next_status {
            next_status = now + status_interval;
            let mut status = std::pin::pin!(probe.probe());
            let event = tokio::select! {
                result = &mut status => ReadyEvent::Status(result),
                _ = wait_for_stop(stop) => ReadyEvent::Stopped,
                result = &mut *producer_done => ReadyEvent::Producer(result),
                _ = server_failed(server) => ReadyEvent::ServerStopped,
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    ReadyEvent::Deadline
                }
            };
            match event {
                ReadyEvent::Status(Ok(())) => {
                    tracing::debug!("receiver status check while waiting for HLS readiness");
                    continue;
                }
                ReadyEvent::Status(Err(error)) => {
                    return ReadyWait::Failed(
                        error.context("receiver status check during Cast startup failed"),
                    );
                }
                ReadyEvent::Stopped => return ReadyWait::Stopped,
                ReadyEvent::Producer(result) => {
                    return producer_ready_outcome(join_producer_result(result), stop);
                }
                ReadyEvent::ServerStopped => return ReadyWait::Failed(server_stopped_error()),
                ReadyEvent::Deadline => {
                    return ReadyWait::Failed(readiness_timeout_error(
                        timeout,
                        initial_buffer_secs,
                    ));
                }
                ReadyEvent::Poll => unreachable!("poll is not selected while the status is due"),
            }
        }

        let wake = (now + READY_POLL).min(next_status).min(deadline);
        let event = tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => ReadyEvent::Poll,
            _ = wait_for_stop(stop) => ReadyEvent::Stopped,
            result = &mut *producer_done => ReadyEvent::Producer(result),
            _ = server_failed(server) => ReadyEvent::ServerStopped,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                ReadyEvent::Deadline
            }
        };
        match event {
            ReadyEvent::Poll => {}
            ReadyEvent::Stopped => return ReadyWait::Stopped,
            ReadyEvent::Producer(result) => {
                return producer_ready_outcome(join_producer_result(result), stop);
            }
            ReadyEvent::ServerStopped => return ReadyWait::Failed(server_stopped_error()),
            ReadyEvent::Deadline => {
                return ReadyWait::Failed(readiness_timeout_error(timeout, initial_buffer_secs));
            }
            ReadyEvent::Status(_) => unreachable!("status is only polled in the due branch"),
        }
    }
}

/// The events the readiness wait can observe while sleeping between polls.
enum ReadyEvent {
    Poll,
    Status(anyhow::Result<()>),
    Stopped,
    Producer(ProducerJoin),
    ServerStopped,
    Deadline,
}

/// A producer result observed while the user may already have cancelled:
/// cancellation always wins so startup ends as a clean Stop.
fn producer_ready_outcome(result: anyhow::Result<()>, stop: &AtomicBool) -> ReadyWait {
    if stop.load(Ordering::Relaxed) {
        if let Err(error) = &result {
            tracing::warn!(%error, "desktop capture failed while the cast was being cancelled");
        }
        return ReadyWait::Stopped;
    }
    ReadyWait::Failed(producer_startup_error(result))
}

fn join_producer_result(result: ProducerJoin) -> anyhow::Result<()> {
    match result {
        Ok(result) => result,
        Err(_) => Err(anyhow!("the Cast capture worker panicked")),
    }
}

/// The readiness deadline expired. The text names the initial buffer of the
/// selected latency preset and never promises a sub-second buffer.
fn readiness_timeout_error(timeout: Duration, initial_buffer_secs: u32) -> anyhow::Error {
    anyhow!(
        "timed out after {}s waiting for the initial HLS buffer (about {} seconds of media); \
         capture or encoding stalled",
        timeout.as_secs_f32(),
        initial_buffer_secs
    )
}

fn server_stopped_error() -> anyhow::Error {
    anyhow!("the Cast HLS server stopped before the stream was ready")
}

/// Resolves once the shared stop flag is set, at the readiness poll cadence.
async fn wait_for_stop(stop: &AtomicBool) {
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(READY_POLL).await;
    }
}

fn producer_startup_error(result: anyhow::Result<()>) -> anyhow::Error {
    match result {
        Ok(()) => anyhow!("the Cast capture worker stopped before the HLS stream was ready"),
        Err(error) => {
            error.context("desktop capture/encode failed before the HLS stream was ready")
        }
    }
}

/// Resolves when the HLS server's accept task is no longer running, so the
/// streaming select can end a session whose media can no longer be fetched.
async fn server_failed(server: &HttpServer) {
    loop {
        if server.check_health().is_err() {
            return;
        }
        tokio::time::sleep(SERVER_POLL).await;
    }
}

/// Wait a bounded time for the blocking producer, then let it detach if a
/// native capture call refuses to return.
async fn finish_producer(
    handle: tokio::task::JoinHandle<()>,
    done_rx: &mut tokio::sync::oneshot::Receiver<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    match tokio::time::timeout(PRODUCER_SHUTDOWN_TIMEOUT, handle).await {
        Ok(Ok(())) => match done_rx.try_recv() {
            Ok(result) => result,
            // Already observed by run_session, or the worker genuinely ended Ok.
            Err(_) => Ok(()),
        },
        Ok(Err(join_error)) => Err(anyhow!("the Cast capture worker panicked: {join_error}")),
        Err(_) => {
            tracing::warn!(
                timeout_secs = PRODUCER_SHUTDOWN_TIMEOUT.as_secs(),
                "Cast capture worker did not stop in time; detaching after requesting stop"
            );
            Ok(())
        }
    }
}

/// Sets the producer's private stop token when the session ends.
///
/// The caller's shared flag is never written here: a user pressing Disconnect
/// must not leave a token set that immediately ends the next session.
struct ProducerStopOnDrop(Arc<AtomicBool>);

impl Drop for ProducerStopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy)]
struct ProducerConfig {
    test_mode: bool,
    display_index: Option<u32>,
    audio: bool,
    quality: CastQuality,
    latency: CastLatency,
}

/// Blocking capture -> encode -> HLS mux loop.
///
/// One frame in flight at a time: encoded interframes are never dropped, and
/// no queue can grow without bound. Video PTS and the 44.1 kHz audio grid both
/// come from the shared [`SessionClock`], so variable capture pacing still
/// yields correct wall-clock HLS timing and the two tracks stay anchored.
///
/// Returns the run's [`PipelineSummary`]. The same counters are traced to
/// target `cermin` every [`metrics::REPORT_INTERVAL`] while the loop runs, so
/// a slow or failing session never hides them behind a final line.
fn run_capture_loop(
    config: ProducerConfig,
    clock: SessionClock,
    pcm_rx: Option<tokio::sync::mpsc::Receiver<TimedPcm>>,
    stop: &AtomicBool,
    external_stop: &AtomicBool,
    store: &Mutex<HlsStore>,
) -> anyhow::Result<PipelineSummary> {
    let mut metrics = PipelineMetrics::new(config.quality, config.latency);
    if stop.load(Ordering::Relaxed) || external_stop.load(Ordering::Relaxed) {
        return Ok(metrics.finish());
    }
    if config.audio && !config.test_mode && pcm_rx.is_none() {
        anyhow::bail!("Cast system audio was enabled without a loopback capture stream");
    }

    // Locked for the whole blocking run: the encoder bitrate comes from the
    // quality preset, while the muxer profile and the IDR cadence both come
    // from the latency preset, so store, mux and keyframes always agree.
    let bitrate_kbps = config.quality.bitrate_kbps();
    let idr_interval_us = config.latency.hls_profile().segment_duration_us();

    let mut source = ProducerSource::open(config)?;
    metrics.set_backend(source.backend_name());
    // Created, used and dropped inside this blocking producer: Media
    // Foundation's AAC encoder is thread-affine.
    let mut audio = if config.audio {
        Some(ProducerAudio::new(config.test_mode, pcm_rx)?)
    } else {
        None
    };
    let mut interleaver = AudioVideoInterleaver::new(config.audio);
    let mut muxer = if config.audio {
        HlsMuxer::with_profile(config.latency.hls_profile(), true)
    } else {
        HlsMuxer::with_profile(config.latency.hls_profile(), false)
    };
    let frame_budget = Duration::from_secs_f64(1.0 / f64::from(TARGET_FPS));
    let mut next_frame = Instant::now();
    let mut source_dims: Option<(u32, u32)> = None;
    let mut output_dims: Option<(u32, u32)> = None;
    let mut encoder: Option<SoftwareEncoder> = None;
    let mut last_idr_pts: Option<u64> = None;
    let mut capture_errors: u32 = 0;

    loop {
        if stop.load(Ordering::Relaxed) || external_stop.load(Ordering::Relaxed) {
            break;
        }

        // Service audio before grabbing: the timeline runs 100 ms behind the
        // clock, and both encoders must be ready for the next video deadline.
        if let Some(audio) = audio.as_mut() {
            let started = Instant::now();
            service_audio(
                audio,
                config.test_mode,
                &clock,
                &mut interleaver,
                stop,
                external_stop,
            )?;
            metrics.record_audio(started.elapsed());
        }

        let capture_started = Instant::now();
        let (mut rgba, width, height) = match source.next_frame() {
            Ok(frame) => {
                capture_errors = 0;
                frame
            }
            Err(error) => {
                capture_errors += 1;
                if capture_errors >= CAPTURE_ERROR_LIMIT {
                    return Err(error.context(format!(
                        "desktop capture failed {capture_errors} times in a row; reconnect the cast"
                    )));
                }
                tracing::warn!(
                    %error,
                    failures = capture_errors,
                    "desktop capture failed; retrying"
                );
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };

        // Timestamp immediately after the grab, before scaling and encoding.
        let pts_us = clock.elapsed_us();
        metrics.record_capture(capture_started.elapsed(), pts_us);

        validate_frame(&rgba, width, height)?;
        // Sampled before scaling and encoding: `changed_frames` describes the
        // captured input, so a static desktop is distinguishable from a slow
        // producer.
        metrics
            .record_sampled_frame(&rgba, width, height)
            .context("sampling a captured Cast frame")?;

        match source_dims {
            Some(expected) => check_source_dims(expected, (width, height))?,
            None => {
                source_dims = Some((width, height));
                let fitted = fit_dims_for_quality(config.quality, width, height)?;
                tracing::info!(
                    capture_w = width,
                    capture_h = height,
                    stream_w = fitted.0,
                    stream_h = fitted.1,
                    fps = TARGET_FPS,
                    bitrate_kbps,
                    audio = config.audio,
                    quality = config.quality.name(),
                    latency = config.latency.name(),
                    "cast stream dimensions fixed; capture resolution changes now require a reconnect"
                );
                metrics.set_source_dims((width, height), fitted);
                output_dims = Some(fitted);
            }
        }

        if config.test_mode && config.audio {
            paint_sync_marker(&mut rgba, width, height, frames_from_us(pts_us));
        }

        // A stop may have arrived while the blocking grab was in flight: do not
        // start a late encode that only delays shutdown.
        if stop.load(Ordering::Relaxed) || external_stop.load(Ordering::Relaxed) {
            break;
        }

        let (out_w, out_h) = output_dims.expect("output dims are set with the first frame");
        let scaled;
        let pixels: &[u8] = if width == out_w && height == out_h {
            &rgba
        } else {
            let started = Instant::now();
            scaled = downscale_rgba(&rgba, width, height, out_w, out_h);
            metrics.record_scale(started.elapsed());
            &scaled
        };

        if encoder.is_none() {
            // Balanced keeps the established internal encoder path; High uses
            // the exact-dimension constructor so 1920x1080 stays 1920x1080
            // instead of the 16-pixel macroblock round-up.
            let created = match config.quality {
                CastQuality::Balanced => {
                    SoftwareEncoder::new(out_w, out_h, bitrate_kbps, TARGET_FPS)
                }
                CastQuality::High => {
                    SoftwareEncoder::new_exact(out_w, out_h, bitrate_kbps, TARGET_FPS)
                }
            };
            encoder = Some(created.context("initializing the H.264 encoder for Cast")?);
            tracing::info!(
                width = out_w,
                height = out_h,
                bitrate_kbps,
                quality = config.quality.name(),
                "cast H.264 encoder ready"
            );
        }
        let encoder = encoder.as_mut().expect("encoder initialized above");
        if idr_due(last_idr_pts, pts_us, idr_interval_us) {
            encoder.force_keyframe();
            last_idr_pts = Some(pts_us);
        }

        let encode_started = Instant::now();
        let encoded = encoder
            .encode(pixels, out_w, out_h, pts_us)
            .context("encoding a desktop frame for Cast")?;
        metrics.record_h264(encode_started.elapsed());

        match encoded {
            Some(frame) => {
                metrics.record_encoded_frame(&frame.data);
                // The watermark advances on the input PTS below even when the
                // encoder emits nothing, so audio is never stalled behind a
                // swallowed frame.
                interleaver
                    .push_video(
                        pts_us,
                        Some(VideoAu {
                            pts_us: frame.pts_us,
                            data: frame.data,
                        }),
                    )
                    .context("buffering an encoded Cast video frame")?;
            }
            None => interleaver
                .push_video(pts_us, None)
                .context("advancing the Cast video watermark")?,
        }

        // A slow grab/encode can consume several audio deadlines; service
        // again before flushing so the muxer sees the freshest AAC frames
        // instead of waiting for the next video iteration.
        if let Some(audio) = audio.as_mut() {
            let started = Instant::now();
            service_audio(
                audio,
                config.test_mode,
                &clock,
                &mut interleaver,
                stop,
                external_stop,
            )?;
            metrics.record_audio(started.elapsed());
        }

        let mux_started = Instant::now();
        for segment in interleaver
            .flush(&mut muxer)
            .context("interleaving the Cast video and audio tracks")?
        {
            let codec = muxer
                .codec()
                .ok_or_else(|| {
                    anyhow!("HLS muxer produced a segment before codec parameters were available")
                })?
                .to_owned();
            lock_store(store)
                .publish(segment, &codec)
                .context("publishing an HLS segment")?;
            metrics.record_sealed_segment();
        }
        metrics.record_mux(mux_started.elapsed());

        if let Some(snapshot) = metrics.take_periodic() {
            tracing::info!(target: "cermin", "cast pipeline: {}", snapshot.one_line());
        }

        advance_pacing(&mut next_frame, frame_budget);
    }

    // The unflushable tail is deliberately dropped on stop: it can never be
    // advertised as a complete segment.
    let emitted_audio_frames = audio
        .as_ref()
        .map_or(0, |audio| audio.timeline.emitted_frames());
    let summary = metrics.finish();
    tracing::info!(
        target: "cermin",
        encoded_frames = summary.encoded_frames,
        published_segments = summary.sealed_segments,
        emitted_audio_frames,
        "cast capture loop stopped"
    );
    tracing::info!(
        target: "cermin",
        "cast pipeline summary: {}",
        summary.one_line()
    );
    Ok(summary)
}

/// Drains captured PCM (or generates the deterministic test pulse), assembles
/// exactly-1024-frame AAC blocks and records them in the interleaver. All work
/// per call is bounded; a stalled capture or encoder is reported instead of an
/// unbounded catch-up.
fn service_audio(
    state: &mut ProducerAudio,
    test_mode: bool,
    clock: &SessionClock,
    interleaver: &mut AudioVideoInterleaver,
    stop: &AtomicBool,
    external_stop: &AtomicBool,
) -> anyhow::Result<()> {
    let deadline = frames_from_us(clock.elapsed_us()).saturating_sub(HOLD_BACK_FRAMES);

    if test_mode {
        // Never opens an audio endpoint; the pulse is generated from the same
        // clock that timestamps the test pattern and starts at sample 0.
        if deadline > state.test_cursor {
            let frames = usize::try_from((deadline - state.test_cursor).min(LAG_LIMIT_FRAMES))
                .unwrap_or(usize::MAX);
            state.timeline.push(TimedPcm {
                start_frame: i64::try_from(state.test_cursor).unwrap_or(i64::MAX),
                data: synthetic_pcm(state.test_cursor, frames),
            })?;
            state.test_cursor += frames as u64;
        }
    } else if !state.receiver_closed {
        let Some(receiver) = state.receiver.as_mut() else {
            return Ok(());
        };
        if let Err(error) = drain_pcm(receiver, &mut state.timeline, stop, external_stop) {
            state.receiver_closed = true;
            return Err(error);
        }
    }

    let ProducerAudio {
        encoder, timeline, ..
    } = state;
    timeline
        .emit_ready(
            deadline,
            MAX_BLOCKS_PER_POLL,
            || stop.load(Ordering::Relaxed) || external_stop.load(Ordering::Relaxed),
            |block, start_frame| {
                let frames = encoder
                    .encode(block, start_frame)
                    .context("encoding a 1024-frame AAC block for Cast system audio")?;
                interleaver.push_audio(frames)
            },
        )
        .map(|_| ())
}

/// System-audio state owned by the blocking producer: the thread-affine AAC
/// encoder, the fixed-grid PCM timeline and the bounded loopback receiver.
struct ProducerAudio {
    encoder: AacEncoder,
    timeline: PcmTimeline,
    receiver: Option<tokio::sync::mpsc::Receiver<TimedPcm>>,
    receiver_closed: bool,
    /// Frames of synthetic pulse generated so far in `--test`.
    test_cursor: u64,
}

impl ProducerAudio {
    fn new(
        test_mode: bool,
        receiver: Option<tokio::sync::mpsc::Receiver<TimedPcm>>,
    ) -> anyhow::Result<Self> {
        if !test_mode && receiver.is_none() {
            anyhow::bail!("Cast system audio was enabled without a loopback capture stream");
        }
        Ok(Self {
            encoder: AacEncoder::new().context(
                "initializing the AAC encoder for Cast system audio (Windows Media Foundation); \
                 install the Media Feature Pack or pass --no-audio / turn off \"System audio\" \
                 for a video-only cast",
            )?,
            timeline: PcmTimeline::new(),
            receiver,
            receiver_closed: false,
            test_cursor: 0,
        })
    }
}

/// Drains up to a bounded number of timestamped packets into the timeline.
/// A capture channel that closes while the session is still running is a
/// device failure, not a reason to keep emitting silence forever.
fn drain_pcm(
    receiver: &mut tokio::sync::mpsc::Receiver<TimedPcm>,
    timeline: &mut PcmTimeline,
    stop: &AtomicBool,
    external_stop: &AtomicBool,
) -> anyhow::Result<()> {
    for _ in 0..MAX_PCM_CHUNKS_PER_POLL {
        match receiver.try_recv() {
            Ok(packet) => timeline.push(packet)?,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                if !stop.load(Ordering::Relaxed) && !external_stop.load(Ordering::Relaxed) {
                    anyhow::bail!(
                        "system audio capture stopped unexpectedly during the Cast session; \
                         the audio device may have been disconnected"
                    );
                }
                break;
            }
        }
    }
    Ok(())
}

/// Paints a small white marker while the synthetic 440 Hz pulse is active, so
/// a `--test` cast exposes A/V sync: marker and tone share one clock phase.
fn paint_sync_marker(rgba: &mut [u8], width: u32, height: u32, frame: u64) {
    if !pulse_active(frame) {
        return;
    }
    let side = (width.min(height) / 6).clamp(8, 96);
    let x0 = (width / 16).min(width.saturating_sub(side));
    let y0 = (height / 16).min(height.saturating_sub(side));
    for y in y0..(y0 + side).min(height) {
        for x in x0..(x0 + side).min(width) {
            let index = ((y * width + x) * 4) as usize;
            rgba[index..index + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
}

/// The capture source behind the producer: the chosen desktop display or the
/// synthetic test pattern.
enum ProducerSource {
    Synthetic(SyntheticSource),
    Capture(Box<dyn CaptureBackend>),
}

impl ProducerSource {
    fn open(config: ProducerConfig) -> anyhow::Result<Self> {
        if config.test_mode {
            // The synthetic source matches the selected preset's real encode
            // size (1280x720 balanced, 1920x1080 high), never a smaller
            // stand-in, so a local run measures the real producer.
            let (width, height) = config.quality.synthetic_dims();
            tracing::info!(
                width,
                height,
                quality = config.quality.name(),
                "casting the synthetic test pattern (no desktop capture)"
            );
            return Ok(Self::Synthetic(SyntheticSource::new(width, height)));
        }
        let capture = create_capture_backend(config.display_index, false)
            .context("opening the desktop capture backend for Cast")?;
        let requested = config.display_index.unwrap_or(0);
        let displays = capture
            .displays()
            .context("reading the opened Cast capture display")?;
        let opened = displays
            .first()
            .ok_or_else(|| anyhow!("capture backend reported no displays"))?;
        if opened.index != requested {
            anyhow::bail!(
                "capture backend opened display #{} ({}), not the requested #{requested}",
                opened.index,
                opened.name
            );
        }
        let label = opened.label();
        tracing::info!(
            display = %label,
            backend = capture.backend_name(),
            "Cast desktop capture opened"
        );
        Ok(Self::Capture(capture))
    }

    /// Name of the active source: the real backend (`dxgi`, `gdi`, `x11`) or
    /// the synthetic test pattern. Always reported in the pipeline summary.
    fn backend_name(&self) -> &'static str {
        match self {
            Self::Synthetic(_) => "synthetic test pattern",
            Self::Capture(capture) => capture.backend_name(),
        }
    }

    fn next_frame(&mut self) -> anyhow::Result<(Vec<u8>, u32, u32)> {
        match self {
            Self::Synthetic(source) => {
                let (rgba, width, height) = source
                    .next_frame()
                    .context("generating a synthetic cast frame")?;
                Ok((rgba, width, height))
            }
            Self::Capture(capture) => {
                let frame = capture
                    .grab_frame()
                    .context("grabbing a desktop frame for Cast")?;
                Ok((frame.rgba, frame.width, frame.height))
            }
        }
    }
}

/// Fit capture dimensions for the selected quality preset.
///
/// Callers must reject zero dimensions first (see [`validate_frame`]).
pub(crate) fn fit_dims_for_quality(
    quality: CastQuality,
    width: u32,
    height: u32,
) -> anyhow::Result<(u32, u32)> {
    match quality {
        CastQuality::Balanced => Ok(fit_cast_dims(width, height)),
        CastQuality::High => fit_cast_dims_high(width, height),
    }
}

/// Fit capture dimensions for the Balanced preset inside 1280x720, preserving
/// aspect ratio and rounding both sides down to the 16-pixel H.264 macroblock
/// grid.
///
/// Callers must reject zero dimensions first (see [`validate_frame`]): a zero
/// side has no meaningful fit and silently becoming 16x16 would hide a broken
/// capture source.
///
/// Rounding down (unlike `fit_stream_dims`, which pads 1080 to 1088) keeps the
/// fixed output inside the envelope and avoids a coded height the receiver
/// would letterbox.
pub(crate) fn fit_cast_dims(width: u32, height: u32) -> (u32, u32) {
    debug_assert!(
        width > 0 && height > 0,
        "capture dimensions must be validated before fitting"
    );
    let (scaled_w, scaled_h) = if width <= MAX_WIDTH && height <= MAX_HEIGHT {
        (width, height)
    } else if u64::from(width) * u64::from(MAX_HEIGHT) >= u64::from(height) * u64::from(MAX_WIDTH) {
        // Width is the limiting side (or both match): integer division keeps
        // exact ratios such as 1080p at exactly 720 lines.
        let h = u64::from(height) * u64::from(MAX_WIDTH) / u64::from(width);
        (MAX_WIDTH, h as u32)
    } else {
        let w = u64::from(width) * u64::from(MAX_HEIGHT) / u64::from(height);
        (w as u32, MAX_HEIGHT)
    };
    ((scaled_w & !15).max(16), (scaled_h & !15).max(16))
}

/// Fit capture dimensions for the High preset inside 1920x1080: preserve the
/// aspect ratio, never upscale, and round both sides down to even pixels (not
/// to the 16-pixel macroblock grid). A full-HD capture passes through as
/// exactly 1920x1080 rather than a padded 1920x1088; a native 1366x768 capture
/// passes through unchanged.
///
/// The exact encoder accepts visible sides only in `HIGH_MIN_SIDE..=1920`
/// (width) and `HIGH_MIN_SIDE..=1080` (height), so a capture smaller than that
/// minimum, or an extreme aspect ratio whose scaled side falls below it, is an
/// actionable error. The fit never upsells a tiny capture by upscaling it to
/// the minimum: that would invent pixels and hide a broken capture source.
pub(crate) fn fit_cast_dims_high(width: u32, height: u32) -> anyhow::Result<(u32, u32)> {
    if width < HIGH_MIN_SIDE || height < HIGH_MIN_SIDE {
        anyhow::bail!(
            "desktop capture returned {width}x{height}; the High preset encoder accepts visible \
             sides of at least {HIGH_MIN_SIDE} pixels, so use the Balanced preset or change the \
             display mode"
        );
    }
    let (scaled_w, scaled_h) = if width <= HIGH_MAX_WIDTH && height <= HIGH_MAX_HEIGHT {
        (width, height)
    } else if u64::from(width) * u64::from(HIGH_MAX_HEIGHT)
        >= u64::from(height) * u64::from(HIGH_MAX_WIDTH)
    {
        // Width is the limiting side (or both match).
        let h = u64::from(height) * u64::from(HIGH_MAX_WIDTH) / u64::from(width);
        (HIGH_MAX_WIDTH, h as u32)
    } else {
        let w = u64::from(width) * u64::from(HIGH_MAX_HEIGHT) / u64::from(height);
        (w as u32, HIGH_MAX_HEIGHT)
    };
    let (even_w, even_h) = (scaled_w & !1, scaled_h & !1);
    if even_w < HIGH_MIN_SIDE || even_h < HIGH_MIN_SIDE {
        anyhow::bail!(
            "desktop capture {width}x{height} fits to {even_w}x{even_h}, below the \
             {HIGH_MIN_SIDE}-pixel minimum of the High preset encoder; use the Balanced preset or \
             change the display mode"
        );
    }
    Ok((even_w, even_h))
}

/// Validate a captured frame before it can reach the encoder: a zero side
/// would panic the downscaler, and a short buffer would panic the I420
/// conversion. Both checks use checked arithmetic.
fn validate_frame(rgba: &[u8], width: u32, height: u32) -> anyhow::Result<()> {
    if width == 0 || height == 0 {
        anyhow::bail!(
            "desktop capture returned a zero-sized frame ({width}x{height}); check the display mode"
        );
    }
    let expected = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .unwrap_or(u64::MAX);
    if u64::try_from(rgba.len()).unwrap_or(u64::MAX) < expected {
        anyhow::bail!(
            "desktop capture returned a short RGBA buffer ({} of {expected} bytes) for {width}x{height}",
            rgba.len()
        );
    }
    Ok(())
}

/// Reject a mid-session capture resolution change: the encoder and HLS timing
/// are fixed at the first frame, so the user must reconnect.
fn check_source_dims(expected: (u32, u32), actual: (u32, u32)) -> anyhow::Result<()> {
    if actual != expected {
        anyhow::bail!(
            "desktop resolution changed from {}x{} to {}x{} during the cast; the live stream keeps \
             a fixed size — stop, restore the display mode, and reconnect",
            expected.0,
            expected.1,
            actual.0,
            actual.1
        );
    }
    Ok(())
}

/// True when an IDR is due: immediately for the first frame, then once per
/// `interval_us` of media time on the shared clock. The interval is the
/// selected latency profile's segment duration (1 s stable, 0.5 s responsive).
fn idr_due(last_idr_pts: Option<u64>, pts_us: u64, interval_us: u64) -> bool {
    match last_idr_pts {
        None => true,
        Some(last) => pts_us.saturating_sub(last) >= interval_us,
    }
}

/// Deadline pacing: sleep only the remaining frame budget after capture,
/// conversion and encode consumed their share; falling more than a frame
/// behind resets the deadline instead of accumulating debt.
fn advance_pacing(next_frame: &mut Instant, budget: Duration) {
    *next_frame += budget;
    let now = Instant::now();
    if *next_frame > now {
        std::thread::sleep(*next_frame - now);
    } else if now.duration_since(*next_frame) > budget {
        *next_frame = now;
    }
}

/// HLS publications happen under this short lock; a poisoned lock from the
/// HTTP side must not kill the capture worker.
fn lock_store(store: &Mutex<HlsStore>) -> MutexGuard<'_, HlsStore> {
    store.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cast_config_defaults_match_the_platform_audio_support() {
        let config = CastConfig::default();
        assert_eq!(config.display_index, None);
        assert!(!config.test_mode);
        assert_eq!(config.http_port, 0);
        assert_eq!(config.audio, cfg!(target_os = "windows"));
        assert_eq!(config.quality, CastQuality::Balanced);
        assert_eq!(config.latency, CastLatency::Stable);
    }

    #[test]
    fn fit_caps_full_hd_at_exactly_720p() {
        assert_eq!(fit_cast_dims(1920, 1080), (1280, 720));
        assert_eq!(fit_cast_dims(3840, 2160), (1280, 720));
        assert_eq!(fit_cast_dims(2560, 1440), (1280, 720));
        assert_eq!(fit_cast_dims(1280, 720), (1280, 720));
    }

    #[test]
    fn fit_preserves_aspect_and_rounds_down_to_the_macroblock_grid() {
        assert_eq!(fit_cast_dims(3440, 1440), (1280, 528));
        assert_eq!(fit_cast_dims(1366, 768), (1280, 704));
        assert_eq!(fit_cast_dims(1000, 1000), (720, 720));
        assert_eq!(fit_cast_dims(720, 1280), (400, 720));
        for (width, height) in [
            (3440, 1440),
            (1366, 768),
            (1000, 1000),
            (720, 1280),
            (640, 360),
            (1920, 1080),
        ] {
            let (w, h) = fit_cast_dims(width, height);
            assert_eq!(w % 16, 0, "{width}x{height} -> width {w}");
            assert_eq!(h % 16, 0, "{width}x{height} -> height {h}");
            assert!((16..=MAX_WIDTH).contains(&w));
            assert!((16..=MAX_HEIGHT).contains(&h));
        }
    }

    #[test]
    fn fit_never_upscales_and_keeps_a_sixteen_pixel_floor() {
        assert_eq!(fit_cast_dims(640, 360), (640, 352));
        assert_eq!(fit_cast_dims(16, 16), (16, 16));
        assert_eq!(fit_cast_dims(8, 8), (16, 16));
    }

    /// The High preset must pass a real full-HD capture through unchanged:
    /// 1920x1080 stays 1920x1080 instead of a 16-pixel padded 1920x1088 or
    /// macroblock-rounded 1904x1072.
    #[test]
    fn fit_high_keeps_full_hd_exact() {
        assert_eq!(fit_cast_dims_high(1920, 1080).unwrap(), (1920, 1080));
        for (width, height) in [(3840, 2160), (2560, 1440), (2048, 1152)] {
            assert_eq!(
                fit_cast_dims_high(width, height).unwrap(),
                (1920, 1080),
                "{width}x{height}"
            );
        }
    }

    /// High scales down with the aspect ratio preserved, never upscales, and
    /// uses the even-pixel grid rather than the 16-pixel macroblock grid.
    #[test]
    fn fit_high_preserves_aspect_and_never_upscales() {
        assert_eq!(fit_cast_dims_high(3440, 1440).unwrap(), (1920, 802));
        assert_eq!(fit_cast_dims_high(720, 1280).unwrap(), (606, 1080));
        assert_eq!(fit_cast_dims_high(640, 360).unwrap(), (640, 360));
        assert_eq!(fit_cast_dims_high(1366, 768).unwrap(), (1366, 768));
        assert_eq!(fit_cast_dims_high(100, 50).unwrap(), (100, 50));
        assert_eq!(fit_cast_dims_high(16, 16).unwrap(), (16, 16));
        assert_eq!(fit_cast_dims_high(16, 1080).unwrap(), (16, 1080));
        // Odd inputs round down to even pixels, not to 16. The minimum-16
        // boundary is checked after the even rounding: 17x17 fits to 16x16.
        assert_eq!(fit_cast_dims_high(1919, 1079).unwrap(), (1918, 1078));
        assert_eq!(fit_cast_dims_high(17, 17).unwrap(), (16, 16));
        for (width, height) in [
            (3440, 1440),
            (720, 1280),
            (1919, 1079),
            (1366, 768),
            (100, 50),
            (16, 16),
            (17, 17),
        ] {
            let (w, h) = fit_cast_dims_high(width, height).unwrap();
            assert_eq!(w % 2, 0, "{width}x{height} -> width {w}");
            assert_eq!(h % 2, 0, "{width}x{height} -> height {h}");
            assert!((HIGH_MIN_SIDE..=HIGH_MAX_WIDTH).contains(&w));
            assert!((HIGH_MIN_SIDE..=HIGH_MAX_HEIGHT).contains(&h));
        }
    }

    /// The exact High encoder rejects visible sides below 16, so the fit must
    /// report an actionable error for small captures and for extreme aspect
    /// ratios whose scaled side falls below the minimum. It must never upscale
    /// a too-small capture to 16.
    #[test]
    fn fit_high_rejects_captures_below_the_encoder_minimum() {
        for (width, height) in [
            (0, 0),
            (1, 1),
            (2, 2),
            (3, 3),
            (1, 100),
            (100, 1),
            (14, 1080),
            (1080, 14),
            (15, 15),
            // Both input sides pass the minimum, but the aspect-preserving
            // scale rounds the short side to 8 or 4 pixels.
            (4000, 20),
            (20, 4000),
        ] {
            let error = fit_cast_dims_high(width, height).unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains("High preset") && message.contains("16"),
                "{width}x{height}: {message}"
            );
        }
    }

    /// The quality router keeps the Balanced legacy fit byte-for-byte and uses
    /// the even-grid fit only for High.
    #[test]
    fn fit_router_selects_the_preset_specific_fit() {
        assert_eq!(
            fit_dims_for_quality(CastQuality::Balanced, 1366, 768).unwrap(),
            fit_cast_dims(1366, 768)
        );
        assert_eq!(
            fit_dims_for_quality(CastQuality::High, 1366, 768).unwrap(),
            (1366, 768)
        );
        assert_eq!(
            fit_dims_for_quality(CastQuality::Balanced, 1920, 1080).unwrap(),
            (1280, 720)
        );
        assert_eq!(
            fit_dims_for_quality(CastQuality::High, 1920, 1080).unwrap(),
            (1920, 1080)
        );
        assert!(fit_dims_for_quality(CastQuality::High, 1, 1).is_err());
        assert_eq!(
            fit_dims_for_quality(CastQuality::Balanced, 1, 1).unwrap(),
            (16, 16)
        );
    }

    #[test]
    fn zero_and_short_capture_frames_are_rejected_before_encoding() {
        let error = validate_frame(&[], 0, 720).unwrap_err();
        assert!(error.to_string().contains("zero-sized"), "{error}");
        let error = validate_frame(&[0u8; 16], 1280, 720).unwrap_err();
        assert!(error.to_string().contains("short RGBA"), "{error}");
        // Extreme dimensions must not overflow the size math.
        let error = validate_frame(&[], u32::MAX, u32::MAX).unwrap_err();
        assert!(error.to_string().contains("short RGBA"), "{error}");
        assert!(validate_frame(&vec![0u8; 1280 * 720 * 4], 1280, 720).is_ok());
    }

    #[test]
    fn resolution_change_is_rejected_with_a_reconnect_hint() {
        let error = check_source_dims((1920, 1080), (1600, 900)).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("1920x1080"), "{message}");
        assert!(message.contains("1600x900"), "{message}");
        assert!(message.contains("reconnect"), "{message}");
        assert!(check_source_dims((1920, 1080), (1920, 1080)).is_ok());
    }

    /// The IDR cadence follows the selected latency profile: immediate first
    /// frame, then 1 s (stable) or 0.5 s (responsive) of media time.
    #[test]
    fn idr_cadence_follows_the_selected_profile() {
        const START: u64 = 2_000_000;
        let stable = CastLatency::Stable.hls_profile().segment_duration_us();
        let responsive = CastLatency::Responsive.hls_profile().segment_duration_us();
        assert_eq!(stable, 1_000_000);
        assert_eq!(responsive, 500_000);

        assert!(idr_due(None, START, stable));
        assert!(!idr_due(Some(START), START + stable - 1, stable));
        assert!(idr_due(Some(START), START + stable, stable));
        assert!(!idr_due(Some(START), START + responsive - 1, responsive));
        assert!(idr_due(Some(START), START + responsive, responsive));
        assert!(idr_due(Some(START), START + 5_000_000, stable));
    }

    #[test]
    fn the_sync_marker_is_phase_locked_to_the_test_pulse() {
        let (width, height) = (64u32, 64u32);
        let mut silent = vec![0u8; (width * height * 4) as usize];
        paint_sync_marker(&mut silent, width, height, 20_000);
        assert!(
            silent.iter().all(|&byte| byte == 0),
            "no marker outside the pulse phase"
        );
        let mut pulsing = vec![0u8; (width * height * 4) as usize];
        paint_sync_marker(&mut pulsing, width, height, 10);
        assert!(
            pulsing.as_chunks::<4>().0.contains(&[255, 255, 255, 255]),
            "a white marker must appear during the pulse phase"
        );
    }

    #[test]
    fn producer_stop_guard_leaves_the_shared_flag_alone() {
        let private = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(AtomicBool::new(false));
        {
            let _guard = ProducerStopOnDrop(private.clone());
            assert!(!private.load(Ordering::Relaxed));
        }
        assert!(private.load(Ordering::Relaxed), "private token must be set");
        assert!(
            !shared.load(Ordering::Relaxed),
            "shared token must stay clear"
        );
    }

    #[tokio::test]
    async fn stop_before_connect_never_starts_a_session() {
        let device = CastDevice::manual("127.0.0.1", 9).expect("valid manual device");
        let stop = Arc::new(AtomicBool::new(true));
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            run_cast(device, CastConfig::default(), stop, None),
        )
        .await
        .expect("already-stopped run_cast must return immediately");
        assert!(result.is_ok());
    }

    fn test_clock() -> SessionClock {
        SessionClock::new().expect("session clock")
    }

    /// Bounded readiness deadline for the live synthetic producer tests: the
    /// store advertises nothing until about eight seconds of media exists, and
    /// the shared session clock advances with wall time, so a 15 s budget
    /// covers startup plus one slow CPU-encoded frame. The deadline is explicit
    /// so a stalled producer can never hang the test.
    const LIVE_READY_DEADLINE: Duration = Duration::from_secs(15);

    #[test]
    fn already_stopped_producer_does_not_encode_or_publish() {
        let store = Arc::new(Mutex::new(HlsStore::new()));
        let stop = Arc::new(AtomicBool::new(true));
        let external = Arc::new(AtomicBool::new(false));
        let summary = run_capture_loop(
            ProducerConfig {
                test_mode: true,
                display_index: None,
                audio: false,
                quality: CastQuality::Balanced,
                latency: CastLatency::Stable,
            },
            test_clock(),
            None,
            &stop,
            &external,
            &store,
        )
        .expect("an already-stopped producer is a clean stop");
        assert_eq!(summary.captured_frames, 0);
        assert_eq!(summary.encoded_frames, 0);
        assert_eq!(summary.sealed_segments, 0);
        assert!(!lock_store(&store).ready(), "no segments may be published");
    }

    #[test]
    fn audio_enabled_producer_requires_a_loopback_stream() {
        let store = Arc::new(Mutex::new(HlsStore::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let external = Arc::new(AtomicBool::new(false));
        let error = run_capture_loop(
            ProducerConfig {
                test_mode: false,
                display_index: None,
                audio: true,
                quality: CastQuality::Balanced,
                latency: CastLatency::Stable,
            },
            test_clock(),
            None,
            &stop,
            &external,
            &store,
        )
        .expect_err("an audio session without a loopback stream is a bug");
        assert!(error.to_string().contains("loopback"), "{error}");
    }

    #[tokio::test]
    async fn benchmark_cancelled_before_start_never_touches_capture() {
        let stop = Arc::new(AtomicBool::new(true));
        let started = Instant::now();
        let summary = tokio::time::timeout(
            Duration::from_secs(2),
            run_capture_benchmark(None, false, Duration::from_secs(10), stop),
        )
        .await
        .expect("a pre-cancelled benchmark must return immediately")
        .expect("cancellation before start is not an error");
        assert_eq!(summary.captured_frames, 0);
        assert_eq!(summary.wall_secs, 0.0);
        assert_eq!(summary.captured_fps(), 0.0);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a cancelled benchmark must not run the capture loop"
        );
    }

    #[tokio::test]
    async fn benchmark_rejects_durations_outside_one_to_sixty_seconds() {
        let stop = Arc::new(AtomicBool::new(false));
        for duration in [Duration::ZERO, Duration::from_secs(61)] {
            let error = run_capture_benchmark(None, true, duration, stop.clone())
                .await
                .expect_err("an out-of-range duration must be rejected");
            assert!(
                error.to_string().contains("between 1 and 60 seconds"),
                "{error}"
            );
        }
    }

    /// Exercises the full local pipeline (capture -> scale -> encode -> mux)
    /// through the public benchmark entry point with the same 1280x720
    /// synthetic source the live Cast path uses. No display, audio endpoint or
    /// receiver is touched; there are no FPS assertions because debug
    /// from-source encode speed varies.
    #[tokio::test]
    #[cfg(feature = "encode-source")]
    #[cfg_attr(feature = "encode-dll", ignore = "requires the OpenH264 DLL")]
    async fn test_mode_benchmark_runs_a_short_local_pipeline() {
        let stop = Arc::new(AtomicBool::new(false));
        let summary = tokio::time::timeout(
            Duration::from_secs(30),
            run_capture_benchmark(None, true, Duration::from_secs(1), stop),
        )
        .await
        .expect("a one-second benchmark must finish within 30 seconds")
        .expect("the synthetic benchmark must succeed");
        assert!(summary.captured_frames >= 1, "{summary:?}");
        assert!(summary.encoded_frames >= 1, "{summary:?}");
        assert!(summary.captured_fps() > 0.0, "{summary:?}");
        let (synthetic_w, synthetic_h) = CastQuality::Balanced.synthetic_dims();
        assert_eq!(
            (summary.capture_width, summary.capture_height),
            (synthetic_w, synthetic_h),
            "the public --test benchmark must use the live 1280x720 envelope"
        );
        assert_eq!(
            (summary.stream_width, summary.stream_height),
            fit_cast_dims(synthetic_w, synthetic_h)
        );
        assert_eq!(summary.backend_name, "synthetic test pattern");
        assert_eq!(
            summary.audio.samples, 0,
            "the benchmark never captures audio"
        );
        assert_eq!(summary.audio.total_ms, 0.0);
        assert_eq!(summary.encoder_kind, metrics::encoder_kind());
        assert_eq!(summary.build_profile, metrics::build_profile());
        assert_eq!(summary.quality, "balanced");
        assert_eq!(summary.latency, "stable");
        assert_eq!(summary.target_bitrate_kbps, 4000);
        assert_eq!(summary.hls_target_duration_secs, 2);
    }

    /// The High preset runs the real 1080p producer locally: a 1920x1080
    /// synthetic source encoded at the exact full-HD size. The from-source
    /// encoder is slow in debug builds, so there is no FPS threshold.
    #[tokio::test]
    #[cfg(feature = "encode-source")]
    #[cfg_attr(feature = "encode-dll", ignore = "requires the OpenH264 DLL")]
    async fn high_test_mode_benchmark_uses_full_hd() {
        let stop = Arc::new(AtomicBool::new(false));
        let summary = tokio::time::timeout(
            Duration::from_secs(60),
            run_capture_benchmark_with_presets(
                None,
                true,
                Duration::from_secs(1),
                stop,
                CastQuality::High,
                CastLatency::Stable,
            ),
        )
        .await
        .expect("a one-second 1080p benchmark must finish within 60 seconds")
        .expect("the synthetic High benchmark must succeed");
        assert!(summary.captured_frames >= 1, "{summary:?}");
        assert!(summary.encoded_frames >= 1, "{summary:?}");
        assert_eq!(
            (summary.capture_width, summary.capture_height),
            (1920, 1080),
            "High --test must use the real 1080p synthetic source"
        );
        assert_eq!(
            (summary.stream_width, summary.stream_height),
            (1920, 1080),
            "full HD must stay 1920x1080, not a padded or macroblock-rounded size"
        );
        assert_eq!(summary.quality, "high");
        assert_eq!(summary.latency, "stable");
        assert_eq!(summary.target_bitrate_kbps, 8000);
        assert_eq!(summary.hls_target_duration_secs, 2);
        assert_eq!(summary.backend_name, "synthetic test pattern");
        assert_eq!(summary.audio.samples, 0);
    }

    /// The preset entry point with Balanced/Stable must reproduce the default
    /// wrapper exactly; a pre-cancelled run makes the summaries deterministic.
    #[tokio::test]
    async fn benchmark_with_presets_matches_the_default_wrapper() {
        let wrapper = run_capture_benchmark(
            None,
            false,
            Duration::from_secs(10),
            Arc::new(AtomicBool::new(true)),
        )
        .await
        .expect("cancelled default benchmark");
        let explicit = run_capture_benchmark_with_presets(
            None,
            false,
            Duration::from_secs(10),
            Arc::new(AtomicBool::new(true)),
            CastQuality::Balanced,
            CastLatency::Stable,
        )
        .await
        .expect("cancelled preset benchmark");
        assert_eq!(wrapper, explicit);
        assert_eq!(wrapper.quality, "balanced");
        assert_eq!(wrapper.latency, "stable");
        assert_eq!(wrapper.target_bitrate_kbps, 4000);
        assert_eq!(wrapper.hls_target_duration_secs, 2);
    }

    /// A benchmark cancelled before start with High/Responsive names the
    /// choices the user asked for instead of the defaults.
    #[tokio::test]
    async fn cancelled_preset_benchmark_reports_the_requested_presets() {
        let summary = run_capture_benchmark_with_presets(
            None,
            false,
            Duration::from_secs(10),
            Arc::new(AtomicBool::new(true)),
            CastQuality::High,
            CastLatency::Responsive,
        )
        .await
        .expect("pre-cancelled preset benchmark");
        assert_eq!(summary.quality, "high");
        assert_eq!(summary.latency, "responsive");
        assert_eq!(summary.target_bitrate_kbps, 8000);
        assert_eq!(summary.hls_target_duration_secs, 1);
        assert_eq!(summary.captured_frames, 0);
    }

    #[test]
    fn a_closed_loopback_channel_is_fatal_while_the_session_runs() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<TimedPcm>(1);
        drop(sender);
        let mut timeline = PcmTimeline::new();
        let stop = AtomicBool::new(false);
        let external = AtomicBool::new(false);
        let error = drain_pcm(&mut receiver, &mut timeline, &stop, &external)
            .expect_err("a closed capture channel must fail the running session");
        assert!(
            error.to_string().contains("stopped unexpectedly"),
            "{error}"
        );

        // The same closure during a requested stop is a normal shutdown.
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<TimedPcm>(1);
        drop(sender);
        let stop = AtomicBool::new(true);
        assert!(drain_pcm(&mut receiver, &mut timeline, &stop, &external).is_ok());
    }

    #[tokio::test]
    async fn bounded_start_returns_none_when_cancelled() {
        let stop = Arc::new(AtomicBool::new(false));
        let stopper = {
            let stop = stop.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                stop.store(true, Ordering::Relaxed);
            })
        };
        let started = bounded_start(
            std::future::pending::<anyhow::Result<()>>(),
            &stop,
            Duration::from_secs(5),
        )
        .await
        .expect("cancellation is not an error");
        stopper.await.expect("stopper task");
        assert!(started.is_none());
    }

    #[tokio::test]
    async fn bounded_start_times_out_with_an_actionable_error() {
        let stop = AtomicBool::new(false);
        let error = bounded_start(
            std::future::pending::<anyhow::Result<()>>(),
            &stop,
            Duration::from_millis(50),
        )
        .await
        .expect_err("a stuck start must time out");
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(error.to_string().contains("audio"), "{error}");
    }

    /// End-to-end CPU-only pipeline: synthetic frames -> OpenH264 -> HLS muxer
    /// -> eight-second readiness. Uses the from-source encoder; with the DLL
    /// feature the official library may be absent, so the test is ignored then.
    #[test]
    #[cfg(feature = "encode-source")]
    #[cfg_attr(feature = "encode-dll", ignore = "requires the OpenH264 DLL")]
    fn synthetic_pipeline_publishes_segments_then_stops_cleanly() {
        let store = Arc::new(Mutex::new(HlsStore::new()));
        let private = Arc::new(AtomicBool::new(false));
        let external = Arc::new(AtomicBool::new(false));
        let worker = {
            let store = store.clone();
            let private = private.clone();
            let external = external.clone();
            std::thread::spawn(move || {
                run_capture_loop(
                    ProducerConfig {
                        test_mode: true,
                        display_index: None,
                        audio: false,
                        quality: CastQuality::Balanced,
                        latency: CastLatency::Stable,
                    },
                    test_clock(),
                    None,
                    &private,
                    &external,
                    &store,
                )
            })
        };

        let deadline = Instant::now() + LIVE_READY_DEADLINE;
        let mut ready = false;
        while Instant::now() < deadline {
            if lock_store(&store).ready() {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        external.store(true, Ordering::Relaxed);
        let result = worker.join().expect("cast producer thread panicked");
        assert!(
            ready,
            "synthetic pipeline never advertised the initial eight-second HLS buffer"
        );
        if let Err(error) = result {
            panic!("producer failed: {error:#}");
        }
    }

    /// End-to-end CPU-only A/V pipeline on Windows: synthetic frames with the
    /// sync marker plus the deterministic 440 Hz pulse -> OpenH264 + Media
    /// Foundation AAC -> HLS with AAC -> eight-second readiness. No display,
    /// audio endpoint or receiver is touched. With the DLL feature the official
    /// library may be absent, so the test is ignored then.
    #[test]
    #[cfg(all(feature = "encode-source", target_os = "windows"))]
    #[cfg_attr(feature = "encode-dll", ignore = "requires the OpenH264 DLL")]
    fn synthetic_audio_video_pipeline_publishes_aac_segments_then_stops() {
        let store = Arc::new(Mutex::new(HlsStore::new()));
        let private = Arc::new(AtomicBool::new(false));
        let external = Arc::new(AtomicBool::new(false));
        let worker = {
            let store = store.clone();
            let private = private.clone();
            let external = external.clone();
            std::thread::spawn(move || {
                run_capture_loop(
                    ProducerConfig {
                        test_mode: true,
                        display_index: None,
                        audio: true,
                        quality: CastQuality::Balanced,
                        latency: CastLatency::Stable,
                    },
                    test_clock(),
                    None,
                    &private,
                    &external,
                    &store,
                )
            })
        };

        let deadline = Instant::now() + LIVE_READY_DEADLINE;
        let mut ready = false;
        while Instant::now() < deadline {
            if lock_store(&store).ready() {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        external.store(true, Ordering::Relaxed);
        let result = worker.join().expect("cast A/V producer thread panicked");
        assert!(
            ready,
            "synthetic A/V pipeline never advertised the initial eight-second HLS buffer"
        );
        if let Err(error) = result {
            panic!("producer failed: {error:#}");
        }
    }

    /// Reconstructs the H.264 NAL types from the video PES payloads of an
    /// MPEG-TS segment. This is the decoder-free substitute for asserting that
    /// a real OpenH264 access unit reached the muxed transport stream.
    #[cfg(feature = "encode-source")]
    fn ts_video_nal_types(segment: &[u8]) -> Vec<u8> {
        const TS_PACKET: usize = 188;
        const PID_VIDEO: u16 = 0x0101;
        assert_eq!(segment.len() % TS_PACKET, 0, "not whole TS packets");
        let mut pes = Vec::new();
        let mut types = Vec::new();
        for packet in segment.as_chunks::<TS_PACKET>().0 {
            assert_eq!(packet[0], 0x47, "TS sync byte missing");
            let pid = ((u16::from(packet[1] & 0x1F)) << 8) | u16::from(packet[2]);
            if pid != PID_VIDEO {
                continue;
            }
            let pusi = packet[1] & 0x40 != 0;
            let afc = (packet[3] >> 4) & 0x03;
            let mut offset = 4usize;
            if afc & 0x02 != 0 {
                let adaptation_len = packet[4] as usize;
                assert!(5 + adaptation_len <= TS_PACKET, "adaptation overruns");
                offset = 5 + adaptation_len;
            }
            if pusi && !pes.is_empty() {
                types.extend(pes_video_nal_types(&pes));
                pes.clear();
            }
            if afc & 0x01 != 0 {
                pes.extend_from_slice(&packet[offset..]);
            }
        }
        if !pes.is_empty() {
            types.extend(pes_video_nal_types(&pes));
        }
        types
    }

    #[cfg(feature = "encode-source")]
    fn pes_video_nal_types(pes: &[u8]) -> Vec<u8> {
        if pes.len() < 9 || &pes[..3] != b"\x00\x00\x01" || pes[3] != 0xE0 {
            return Vec::new();
        }
        let header_len = 9 + usize::from(pes[8]);
        if header_len > pes.len() {
            return Vec::new();
        }
        let body = &pes[header_len..];
        let mut types = Vec::new();
        let mut index = 0;
        while index + 3 < body.len() {
            if body[index] == 0 && body[index + 1] == 0 && body[index + 2] == 1 {
                if let Some(&byte) = body.get(index + 3) {
                    types.push(byte & 0x1F);
                }
                index += 4;
            } else {
                index += 1;
            }
        }
        types
    }

    /// The encoder's real Annex B output must reach a sealed TS segment with
    /// SPS/PPS and an IDR, without needing an H.264 decoder dependency.
    #[test]
    #[cfg(feature = "encode-source")]
    #[cfg_attr(feature = "encode-dll", ignore = "requires the OpenH264 DLL")]
    fn openh264_access_units_reach_the_ts_segment() {
        const NAL_SPS: u8 = 7;
        const NAL_PPS: u8 = 8;
        const NAL_IDR: u8 = 5;
        let (synthetic_w, synthetic_h) = CastQuality::Balanced.synthetic_dims();
        let (width, height) = fit_cast_dims(synthetic_w, synthetic_h);
        let mut encoder = SoftwareEncoder::new(
            width,
            height,
            CastQuality::Balanced.bitrate_kbps(),
            TARGET_FPS,
        )
        .expect("software encoder init");
        let mut source = SyntheticSource::new(synthetic_w, synthetic_h);
        let mut muxer = HlsMuxer::new();
        let idr_interval_us = CastLatency::Stable.hls_profile().segment_duration_us();
        let started = Instant::now();
        let frame_budget = Duration::from_secs_f64(1.0 / f64::from(TARGET_FPS));
        let mut next_frame = Instant::now();
        let mut segment = None;
        let mut frame_index = 0u64;
        let mut last_idr: Option<u64> = None;
        while segment.is_none() && frame_index < 300 {
            let now_pts = started.elapsed().as_micros() as u64;
            if idr_due(last_idr, now_pts, idr_interval_us) {
                encoder.force_keyframe();
                last_idr = Some(now_pts);
            }
            let (rgba, w, h) = source.next_frame().expect("synthetic frame");
            let pts_us = started.elapsed().as_micros() as u64;
            if let Some(encoded) = encoder
                .encode(&rgba, w, h, pts_us)
                .expect("encode synthetic frame")
            {
                segment = muxer
                    .push(&encoded.data, encoded.pts_us)
                    .expect("mux frame");
            }
            frame_index += 1;
            advance_pacing(&mut next_frame, frame_budget);
        }

        let segment = segment.expect("a second IDR must seal the first segment");
        assert!((0.9..=2.0).contains(&segment.duration));
        let nal_types = ts_video_nal_types(&segment.data);
        assert!(
            nal_types.contains(&NAL_SPS),
            "SPS missing from the TS segment: {nal_types:?}"
        );
        assert!(
            nal_types.contains(&NAL_PPS),
            "PPS missing from the TS segment: {nal_types:?}"
        );
        assert!(
            nal_types.contains(&NAL_IDR),
            "IDR missing from the TS segment: {nal_types:?}"
        );
    }

    // Deterministic readiness-wait tests: a real loopback HTTP server, a real
    // store, and injected status probes; no receiver or CastClient involved.

    struct PendingProbe;

    impl ReadinessProbe for PendingProbe {
        async fn probe(&mut self) -> anyhow::Result<()> {
            std::future::pending().await
        }
    }

    struct FailingProbe;

    impl ReadinessProbe for FailingProbe {
        async fn probe(&mut self) -> anyhow::Result<()> {
            anyhow::bail!("injected status failure")
        }
    }

    /// Timing policy for deterministic readiness-wait tests: a short status
    /// cadence and the buffer the selected latency preset would promise.
    fn ready_policy(timeout: Duration, initial_buffer_secs: u32) -> ReadyPolicy {
        ReadyPolicy {
            timeout,
            status_interval: Duration::from_millis(10),
            initial_buffer_secs,
        }
    }

    /// Builds a store with `count` one-second segments published on a fake
    /// monotonic clock: segment `n` is stamped `epoch + n` seconds. The store
    /// refreshes its advertised snapshot at most once per wall second, so
    /// offline publishes must advance the fake clock instead of sleeping.
    fn store_with_segments(count: u64) -> Arc<Mutex<HlsStore>> {
        let mut store = HlsStore::new();
        let epoch = Instant::now();
        for sequence in 0..count {
            store
                .publish_at(
                    rotten_cast::hls::Segment {
                        sequence,
                        duration: 1.0,
                        data: vec![0x40 + sequence as u8; 376],
                    },
                    "avc1.640028",
                    epoch + Duration::from_secs(sequence),
                )
                .expect("publish test segment");
        }
        Arc::new(Mutex::new(store))
    }

    /// Builds a store for `profile` with `count` one-second segments published
    /// on the same fake monotonic clock: segment `n` is stamped `epoch + n`
    /// seconds. The store commits a snapshot whenever the fake clock advanced
    /// by at least its own snapshot interval, so readiness is deterministic
    /// without sleeping.
    fn profile_store_with_segments(
        profile: rotten_cast::hls::HlsProfile,
        count: u64,
    ) -> Arc<Mutex<HlsStore>> {
        let mut store = HlsStore::with_profile(profile, CastQuality::Balanced.bandwidth_bps())
            .expect("build profiled test store");
        let epoch = Instant::now();
        for sequence in 0..count {
            store
                .publish_at(
                    rotten_cast::hls::Segment {
                        sequence,
                        duration: 1.0,
                        data: vec![0x40 + sequence as u8; 376],
                    },
                    "avc1.640028",
                    epoch + Duration::from_secs(sequence),
                )
                .expect("publish test segment");
        }
        Arc::new(Mutex::new(store))
    }

    /// The latency preset really drives store readiness: five seconds of media
    /// clears the Responsive four-second watermark but not the Stable
    /// eight-second one, so a fixed watermark can never slip through.
    #[test]
    fn responsive_store_advertises_the_shorter_buffer() {
        let mut responsive = HlsStore::with_profile(
            CastLatency::Responsive.hls_profile(),
            CastQuality::Balanced.bandwidth_bps(),
        )
        .expect("responsive store");
        let mut stable = HlsStore::new();
        let epoch = Instant::now();
        for sequence in 0..5 {
            for store in [&mut responsive, &mut stable] {
                store
                    .publish_at(
                        rotten_cast::hls::Segment {
                            sequence,
                            duration: 1.0,
                            data: vec![0x40 + sequence as u8; 376],
                        },
                        "avc1.640028",
                        epoch + Duration::from_secs(sequence),
                    )
                    .expect("publish test segment");
            }
        }
        assert!(
            responsive.ready(),
            "a responsive store must clear its four-second watermark after five seconds"
        );
        assert!(
            !stable.ready(),
            "a stable store must still wait for its eight-second watermark"
        );
    }

    async fn readiness_test_server() -> HttpServer {
        HttpServer::start(
            std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            Arc::new(Mutex::new(HlsStore::new())),
        )
        .await
        .expect("start loopback HLS server")
    }

    /// The ready wait must accept the store's initial eight-second buffer:
    /// nine one-second segments (stamped 0..=8 s on the fake clock) advertise
    /// more than the eight-second watermark.
    #[tokio::test]
    async fn readiness_wait_returns_ready_once_the_initial_buffer_is_advertised() {
        let store = store_with_segments(9);
        let server = readiness_test_server().await;
        let (_done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        let stop = AtomicBool::new(false);
        let outcome = wait_until_hls_ready(
            &store,
            &mut done_rx,
            &server,
            &stop,
            ready_policy(Duration::from_secs(5), 8),
            &mut PendingProbe,
        )
        .await;
        assert!(matches!(outcome, ReadyWait::Ready), "{outcome:?}");
    }

    /// The ready wait accepts the Responsive store's four-second watermark:
    /// five one-second segments published into a profile-created store must be
    /// enough, proving the wait reads the store's profile rather than a fixed
    /// eight-second constant.
    #[tokio::test]
    async fn readiness_wait_accepts_the_responsive_four_second_buffer() {
        let store = profile_store_with_segments(CastLatency::Responsive.hls_profile(), 5);
        let server = readiness_test_server().await;
        let (_done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        let stop = AtomicBool::new(false);
        let outcome = wait_until_hls_ready(
            &store,
            &mut done_rx,
            &server,
            &stop,
            ready_policy(
                Duration::from_secs(5),
                CastLatency::Responsive.initial_buffer_secs(),
            ),
            &mut PendingProbe,
        )
        .await;
        assert!(matches!(outcome, ReadyWait::Ready), "{outcome:?}");
    }

    #[tokio::test]
    async fn readiness_wait_is_fatal_on_status_failure() {
        let store = Arc::new(Mutex::new(HlsStore::new()));
        let server = readiness_test_server().await;
        let (_done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        let stop = AtomicBool::new(false);
        let outcome = wait_until_hls_ready(
            &store,
            &mut done_rx,
            &server,
            &stop,
            ready_policy(Duration::from_secs(5), 8),
            &mut FailingProbe,
        )
        .await;
        let ReadyWait::Failed(error) = outcome else {
            panic!("expected a fatal status failure, got {outcome:?}");
        };
        let chain = format!("{error:#}");
        assert!(
            chain.contains("receiver status check during Cast startup failed"),
            "{chain}"
        );
        assert!(chain.contains("injected status failure"), "{chain}");
    }

    #[tokio::test]
    async fn readiness_wait_honours_the_deadline_while_status_is_pending() {
        let store = Arc::new(Mutex::new(HlsStore::new()));
        let server = readiness_test_server().await;
        let (_done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        let stop = AtomicBool::new(false);
        let started = Instant::now();
        let outcome = wait_until_hls_ready(
            &store,
            &mut done_rx,
            &server,
            &stop,
            ready_policy(Duration::from_millis(150), 8),
            &mut PendingProbe,
        )
        .await;
        let ReadyWait::Failed(error) = outcome else {
            panic!("expected the readiness deadline, got {outcome:?}");
        };
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(
            error.to_string().contains("about 8 seconds"),
            "the timeout must name the selected initial buffer: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "deadline overrun: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn readiness_wait_stops_promptly_while_status_is_pending() {
        let store = Arc::new(Mutex::new(HlsStore::new()));
        let server = readiness_test_server().await;
        let (_done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopper = {
            let stop = stop.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                stop.store(true, Ordering::Relaxed);
            })
        };
        let started = Instant::now();
        let outcome = wait_until_hls_ready(
            &store,
            &mut done_rx,
            &server,
            &stop,
            ready_policy(Duration::from_secs(5), 8),
            &mut PendingProbe,
        )
        .await;
        stopper.await.expect("stopper task");
        assert!(matches!(outcome, ReadyWait::Stopped), "{outcome:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancellation took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn readiness_wait_reports_producer_failure() {
        let store = Arc::new(Mutex::new(HlsStore::new()));
        let server = readiness_test_server().await;
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        done_tx
            .send(Err(anyhow!("injected capture failure")))
            .expect("send producer result");
        let stop = AtomicBool::new(false);
        let outcome = wait_until_hls_ready(
            &store,
            &mut done_rx,
            &server,
            &stop,
            ready_policy(Duration::from_secs(5), 8),
            &mut PendingProbe,
        )
        .await;
        let ReadyWait::Failed(error) = outcome else {
            panic!("expected a producer failure, got {outcome:?}");
        };
        let chain = format!("{error:#}");
        assert!(chain.contains("injected capture failure"), "{chain}");
    }

    #[tokio::test]
    async fn readiness_wait_ends_stopped_when_cancelled_with_a_finished_producer() {
        let store = Arc::new(Mutex::new(HlsStore::new()));
        let server = readiness_test_server().await;
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        done_tx.send(Ok(())).expect("send producer completion");
        let stop = AtomicBool::new(true);
        let outcome = wait_until_hls_ready(
            &store,
            &mut done_rx,
            &server,
            &stop,
            ready_policy(Duration::from_secs(5), 8),
            &mut PendingProbe,
        )
        .await;
        assert!(matches!(outcome, ReadyWait::Stopped), "{outcome:?}");
    }

    #[test]
    fn cancellation_wins_over_a_finished_producer() {
        let stop = AtomicBool::new(true);
        assert!(matches!(
            producer_ready_outcome(Ok(()), &stop),
            ReadyWait::Stopped
        ));
        let stop = AtomicBool::new(false);
        assert!(matches!(
            producer_ready_outcome(Ok(()), &stop),
            ReadyWait::Failed(_)
        ));
    }

    #[tokio::test]
    async fn readiness_wait_reports_server_failure() {
        let store = Arc::new(Mutex::new(HlsStore::new()));
        let mut server = readiness_test_server().await;
        server.shutdown().await.expect("shutdown loopback server");
        let (_done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        let stop = AtomicBool::new(false);
        let outcome = wait_until_hls_ready(
            &store,
            &mut done_rx,
            &server,
            &stop,
            ready_policy(Duration::from_secs(5), 8),
            &mut PendingProbe,
        )
        .await;
        let ReadyWait::Failed(error) = outcome else {
            panic!("expected a server failure, got {outcome:?}");
        };
        assert!(error.to_string().contains("HLS server stopped"), "{error}");
    }
}
