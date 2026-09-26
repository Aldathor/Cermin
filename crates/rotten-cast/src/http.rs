//! Token-gated loopback/LAN HTTP server for the experimental Cast HLS stream.
//!
//! [`HttpServer`] binds exactly one caller-provided interface (an ephemeral port
//! when the port is 0), generates a random 128-bit capability token for the URL
//! path, and only answers connections from the receiver IP supplied by the
//! caller. It serves the master and live playlists plus numeric `.ts` segments
//! from [`HlsStore`]; before the store profile's readiness duration of
//! advertised media exists the manifests answer 503.
//!
//! The server never touches the filesystem, refuses to bind `0.0.0.0`/`::`,
//! caps request headers at 8 KiB, applies a 5-second request/response deadline,
//! and bounds concurrent connections to 16. Dropping or shutting the server
//! aborts the accept task and every client task.
//!
//! A single `Range: bytes=a-b` request for a segment is answered with 206; any
//! other `Range` form (including multiple ranges) is ignored and the entire
//! segment is served with 200, which the HTTP specification permits.
//!
//! Delivery is observable through [`HttpServer::stats`]: a fixed set of numeric
//! counters (requests, manifests, completed/missing segments, failed writes,
//! delivered body bytes and the slowest response) that never contain request
//! targets, tokens, headers, bodies or pixels. While the server runs (or exits
//! gracefully) the accept loop reports cumulative counters and per-interval
//! deltas at `INFO` on the `cermin` target.

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior, interval_at, timeout};
use tracing::{debug, warn};

use crate::hls::{HlsStore, StoreStats};

/// Manifest MIME type used by HLS.
const MANIFEST_MIME: &str = "application/vnd.apple.mpegurl";
/// MPEG-2 transport stream MIME type.
const SEGMENT_MIME: &str = "video/mp2t";
/// Error/status bodies.
const TEXT_MIME: &str = "text/plain; charset=utf-8";
/// Manifests must never be cached while the live window slides.
const MANIFEST_CACHE: &str = "no-cache, no-store, must-revalidate";
/// Segments are immutable for a given token.
const SEGMENT_CACHE: &str = "public, max-age=60";

const MAX_HEADER_BYTES: usize = 8 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 16;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Cadence of the bounded delivery-counter summary logged by the accept loop.
const STATS_LOG_INTERVAL: Duration = Duration::from_secs(2);

/// Session-owned HTTP delivery counters.
///
/// Every field is a single monotonic atomic; no request targets, tokens,
/// headers, response bodies or pixel data are ever stored. Counters are updated
/// after a response finished being written (body bytes are the bytes actually
/// written), so the numbers describe real delivery, not attempted work.
#[derive(Debug, Default)]
pub struct HttpStats {
    requests: AtomicU64,
    manifests: AtomicU64,
    ts_completed: AtomicU64,
    ts_missing: AtomicU64,
    writes_failed: AtomicU64,
    total_bytes: AtomicU64,
    max_response_ms: AtomicU64,
}

/// Point-in-time copy of [`HttpStats`]: bounded numeric counters only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HttpStatsSnapshot {
    /// Parsed requests seen on the session (any method, route or result).
    pub requests: u64,
    /// Master/live playlists written with 200.
    pub manifests: u64,
    /// Segment responses written with 200 or 206.
    pub ts_completed: u64,
    /// Authenticated segment requests answered 404 (evicted from the store).
    pub ts_missing: u64,
    /// Responses whose head or body write failed or timed out.
    pub writes_failed: u64,
    /// Body bytes in completed responses (HEAD and failed writes contribute 0).
    pub total_bytes: u64,
    /// Slowest response (handling plus write) observed, in milliseconds.
    pub max_response_ms: u64,
}

impl HttpStats {
    /// Copies the current counters; allocates nothing.
    pub fn snapshot(&self) -> HttpStatsSnapshot {
        HttpStatsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            manifests: self.manifests.load(Ordering::Relaxed),
            ts_completed: self.ts_completed.load(Ordering::Relaxed),
            ts_missing: self.ts_missing.load(Ordering::Relaxed),
            writes_failed: self.writes_failed.load(Ordering::Relaxed),
            total_bytes: self.total_bytes.load(Ordering::Relaxed),
            max_response_ms: self.max_response_ms.load(Ordering::Relaxed),
        }
    }

    fn count_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    fn count_response(&self, route: Option<Route>, status: u16, body_bytes: u64, elapsed_ms: u64) {
        match route {
            Some(Route::Master | Route::Live) if status == 200 => {
                self.manifests.fetch_add(1, Ordering::Relaxed);
            }
            Some(Route::Segment(_)) if status == 200 || status == 206 => {
                self.ts_completed.fetch_add(1, Ordering::Relaxed);
            }
            Some(Route::Segment(_)) if status == 404 => {
                self.ts_missing.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        self.total_bytes.fetch_add(body_bytes, Ordering::Relaxed);
        self.observe_response_ms(elapsed_ms);
    }

    fn count_write_failure(&self, elapsed_ms: u64) {
        self.writes_failed.fetch_add(1, Ordering::Relaxed);
        self.observe_response_ms(elapsed_ms);
    }

    fn observe_response_ms(&self, elapsed_ms: u64) {
        self.max_response_ms
            .fetch_max(elapsed_ms, Ordering::Relaxed);
    }
}

impl fmt::Display for HttpStatsSnapshot {
    /// Renders the numeric counters only; no request paths or tokens can ever
    /// appear here because the snapshot stores numbers.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "requests={} manifests={} ts_completed={} ts_missing={} writes_failed={} bytes={} max_response_ms={}",
            self.requests,
            self.manifests,
            self.ts_completed,
            self.ts_missing,
            self.writes_failed,
            self.total_bytes,
            self.max_response_ms
        )
    }
}

/// Emits one bounded, numeric-only delivery summary. Nothing derived from a
/// request (path, token, headers, body) is ever included.
///
/// The store fields describe the media buffer, not the request: the advertised
/// window duration in seconds, the retained segment/byte counts and the age of
/// the current advertised snapshot. The publication age keeps growing while
/// production is stalled, so a frozen producer is visible instead of appearing
/// fresh.
fn log_delivery_stats(
    current: &HttpStatsSnapshot,
    previous: &HttpStatsSnapshot,
    window: Duration,
    store: &StoreStats,
) {
    tracing::info!(
        target: "cermin",
        requests = current.requests,
        manifests = current.manifests,
        ts_completed = current.ts_completed,
        ts_missing = current.ts_missing,
        writes_failed = current.writes_failed,
        bytes = current.total_bytes,
        max_response_ms = current.max_response_ms,
        requests_delta = current.requests.saturating_sub(previous.requests),
        manifests_delta = current.manifests.saturating_sub(previous.manifests),
        ts_completed_delta = current.ts_completed.saturating_sub(previous.ts_completed),
        ts_missing_delta = current.ts_missing.saturating_sub(previous.ts_missing),
        writes_failed_delta = current.writes_failed.saturating_sub(previous.writes_failed),
        bytes_delta = current.total_bytes.saturating_sub(previous.total_bytes),
        window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX),
        window_seconds = store.advertised_us as f64 / 1_000_000.0,
        retained_segments = store.retained_segments,
        retained_bytes = store.retained_bytes,
        publication_age_ms = ?store
            .publication_age
            .map(|age| u64::try_from(age.as_millis()).unwrap_or(u64::MAX)),
        "cast HTTP delivery stats"
    );
}

/// HTTP server for one Cast HLS session.
pub struct HttpServer {
    url: String,
    local_addr: SocketAddr,
    stats: Arc<HttpStats>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    accept_task: Option<JoinHandle<()>>,
}

impl HttpServer {
    /// Binds `bind_addr`, starts accepting, and returns the running server.
    ///
    /// `allowed_peer` must be the receiver's IP: every other peer is dropped
    /// without a response. Binding an unspecified address is rejected so the
    /// stream is never exposed on all interfaces by accident.
    pub async fn start(
        bind_addr: SocketAddr,
        allowed_peer: IpAddr,
        store: Arc<Mutex<HlsStore>>,
    ) -> Result<Self> {
        if bind_addr.ip().is_unspecified() {
            bail!(
                "refusing to bind unspecified address {bind_addr}; pass the concrete interface address (no 0.0.0.0 fallback)"
            );
        }
        let listener = TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("bind cast HTTP listener on {bind_addr}"))?;
        let local_addr = listener
            .local_addr()
            .context("read cast HTTP listener address")?;
        let token = random_token();
        let url = format!("http://{local_addr}/{token}/master.m3u8");

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let stats = Arc::new(HttpStats::default());
        let accept_task = tokio::spawn(accept_loop(
            listener,
            allowed_peer,
            store,
            Arc::clone(&stats),
            token.clone(),
            shutdown_rx,
        ));

        // Deliberately no token or full URL in the logs: the URL is a capability.
        tracing::info!(
            addr = %local_addr,
            allowed_peer = %allowed_peer,
            "cast HLS HTTP server listening"
        );
        Ok(Self {
            url,
            local_addr,
            stats,
            shutdown_tx: Some(shutdown_tx),
            accept_task: Some(accept_task),
        })
    }

    /// Capability URL for the master playlist, including the random token.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The actual bound address (resolved ephemeral port included).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Shared delivery counters for this session.
    ///
    /// The returned handle observes the live session (including after shutdown);
    /// taking a [`HttpStats::snapshot`] is allocation-free.
    pub fn stats(&self) -> Arc<HttpStats> {
        Arc::clone(&self.stats)
    }

    /// Copy of the delivery counters for this session.
    pub fn stats_snapshot(&self) -> HttpStatsSnapshot {
        self.stats.snapshot()
    }

    /// Stops accepting, aborts any in-flight clients, and waits for the accept
    /// task to finish. Idempotent and cancellation safe: the accept task stays
    /// owned by `self` until it has been awaited or aborted, so dropping this
    /// future mid-flight cannot detach it.
    pub async fn shutdown(&mut self) -> Result<()> {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        let outcome = if let Some(task) = self.accept_task.as_mut() {
            match timeout(SHUTDOWN_GRACE, &mut *task).await {
                Ok(result) => result.context("cast HTTP accept task failed"),
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    Ok(())
                }
            }
        } else {
            Ok(())
        };
        self.accept_task = None;
        outcome
    }

    /// True once the accept task has stopped (shutdown, drop, or failure).
    pub fn is_finished(&self) -> bool {
        self.accept_task
            .as_ref()
            .is_none_or(|task| task.is_finished())
    }

    /// Convenience health probe for the app: `Err` once the server is not
    /// running (including after an explicit [`Self::shutdown`]).
    pub fn check_health(&self) -> Result<()> {
        match &self.accept_task {
            Some(task) if !task.is_finished() => Ok(()),
            _ => bail!("cast HTTP server is no longer running"),
        }
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        // Aborting the accept task drops its JoinSet, which aborts every
        // client task; it also closes the listener so the port is released.
        if let Some(task) = self.accept_task.take() {
            task.abort();
        }
    }
}

/// Random non-zero 128-bit capability token rendered as 32 lowercase hex chars.
fn random_token() -> String {
    loop {
        let value: u128 = rand::random();
        if value != 0 {
            return format!("{value:032x}");
        }
    }
}

/// Compares the capability token without leaking a prefix match through timing.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

async fn accept_loop(
    listener: TcpListener,
    allowed_peer: IpAddr,
    store: Arc<Mutex<HlsStore>>,
    stats: Arc<HttpStats>,
    token: String,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let mut clients: JoinSet<()> = JoinSet::new();
    // The timer is owned by this select loop, so shutdown stays cooperative and
    // no detached task can outlive the server.
    let mut stats_timer = interval_at(Instant::now() + STATS_LOG_INTERVAL, STATS_LOG_INTERVAL);
    stats_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut previous = stats.snapshot();
    let mut last_log = Instant::now();
    loop {
        while clients.try_join_next().is_some() {}
        tokio::select! {
            _ = &mut shutdown_rx => break,
            _ = stats_timer.tick() => {
                let current = stats.snapshot();
                // A short lock only at the 2s diagnostic tick; the numeric
                // store fields describe the media buffer, never a request.
                let store_stats = lock_store(&store).stats();
                let now = Instant::now();
                log_delivery_stats(
                    &current,
                    &previous,
                    now.saturating_duration_since(last_log),
                    &store_stats,
                );
                previous = current;
                last_log = now;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        if peer.ip() != allowed_peer {
                            debug!(%peer, %allowed_peer, "rejecting cast HTTP connection from unexpected peer");
                            drop(stream);
                            continue;
                        }
                        if clients.len() >= MAX_CONNECTIONS {
                            warn!(%peer, max = MAX_CONNECTIONS, "cast HTTP connection limit reached; dropping connection");
                            drop(stream);
                            continue;
                        }
                        let store = Arc::clone(&store);
                        let token = token.clone();
                        let stats = Arc::clone(&stats);
                        clients.spawn(async move {
                            if let Err(error) = serve_connection(stream, &token, &store, &stats).await {
                                debug!(%peer, error = %error, "cast HTTP connection ended with an error");
                            }
                        });
                    }
                    Err(error) => {
                        warn!(error = %error, "cast HTTP accept failed");
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
        }
    }
    // Graceful shutdown (never Drop): one last bounded numeric summary.
    let current = stats.snapshot();
    let store_stats = lock_store(&store).stats();
    log_delivery_stats(
        &current,
        &previous,
        Instant::now().saturating_duration_since(last_log),
        &store_stats,
    );
    clients.abort_all();
    while clients.join_next().await.is_some() {}
}

async fn serve_connection(
    mut stream: TcpStream,
    token: &str,
    store: &Mutex<HlsStore>,
    stats: &HttpStats,
) -> Result<()> {
    let _ = stream.set_nodelay(true);
    let request = match timeout(IO_TIMEOUT, read_request(&mut stream)).await {
        Ok(Ok(request)) => request,
        Ok(Err(RequestReadError::TooLarge)) => {
            let response = HttpResponse::text(
                431,
                "Request Header Fields Too Large",
                "headers too large\n",
            );
            let _ = deliver(&mut stream, &response, true, None, stats).await;
            return Ok(());
        }
        Ok(Err(RequestReadError::Malformed(reason))) => {
            debug!(reason, "malformed cast HTTP request");
            return Ok(());
        }
        Ok(Err(RequestReadError::Io(error))) => {
            debug!(error = %error, "cast HTTP request read failed");
            return Ok(());
        }
        Err(_) => {
            let response = HttpResponse::text(408, "Request Timeout", "request timeout\n");
            let _ = deliver(&mut stream, &response, true, None, stats).await;
            return Ok(());
        }
    };

    stats.count_request();
    // Recomputed only for bounded numeric classification and debug fields; the
    // request target itself is never logged or counted.
    let route = route(&request.target, token);
    let include_body = request.method != "HEAD";
    let response = handle_request(&request, token, store);
    deliver(&mut stream, &response, include_body, route, stats).await
}

/// Writes one response and records it.
///
/// The debug line carries only the numeric segment sequence, response code and
/// actually written body length; request targets, tokens, headers and bodies
/// are never logged, including for rejected or malformed requests. Counters are
/// updated only after the write finished (or failed).
async fn deliver(
    stream: &mut TcpStream,
    response: &HttpResponse,
    include_body: bool,
    route: Option<Route>,
    stats: &HttpStats,
) -> Result<()> {
    let started = Instant::now();
    let outcome = timeout(IO_TIMEOUT, write_response(stream, response, include_body)).await;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let body_bytes = if include_body {
        response.body.len() as u64
    } else {
        0
    };
    debug!(
        target: "cermin",
        sequence = ?segment_sequence(route),
        status = response.status,
        body_len = body_bytes,
        "cast HTTP response"
    );
    match outcome {
        Ok(Ok(())) => {
            stats.count_response(route, response.status, body_bytes, elapsed_ms);
            Ok(())
        }
        Ok(Err(error)) => {
            stats.count_write_failure(elapsed_ms);
            Err(error.into())
        }
        Err(_) => {
            stats.count_write_failure(elapsed_ms);
            bail!("cast HTTP write timed out")
        }
    }
}

/// Numeric segment sequence for bounded logs and stats; other routes have none.
fn segment_sequence(route: Option<Route>) -> Option<u64> {
    match route {
        Some(Route::Segment(sequence)) => Some(sequence),
        _ => None,
    }
}

#[derive(Debug)]
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

enum RequestReadError {
    TooLarge,
    Malformed(&'static str),
    Io(io::Error),
}

/// Reads request headers with a hard 8 KiB cap, returning the head bytes when
/// the terminating blank line arrives.
async fn read_request(stream: &mut TcpStream) -> std::result::Result<Request, RequestReadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        // Never buffer more than one byte past the cap: that byte is enough to
        // detect an oversized head even when a delimiter follows it.
        let allowed = MAX_HEADER_BYTES + 1 - buf.len();
        let take = allowed.min(chunk.len());
        let read = stream
            .read(&mut chunk[..take])
            .await
            .map_err(RequestReadError::Io)?;
        if read == 0 {
            return Err(RequestReadError::Malformed(
                "connection closed before request headers",
            ));
        }
        buf.extend_from_slice(&chunk[..read]);
        if let Some(end) = header_end(&buf) {
            if end > MAX_HEADER_BYTES {
                return Err(RequestReadError::TooLarge);
            }
            return parse_request(&buf[..end]);
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(RequestReadError::TooLarge);
        }
    }
}

fn header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
        .or_else(|| {
            buf.windows(2)
                .position(|window| window == b"\n\n")
                .map(|position| position + 2)
        })
}

fn parse_request(head: &[u8]) -> std::result::Result<Request, RequestReadError> {
    let text = std::str::from_utf8(head)
        .map_err(|_| RequestReadError::Malformed("request headers are not UTF-8"))?;
    let normalized = text.replace("\r\n", "\n");
    let mut lines = normalized.split('\n');

    let request_line = lines
        .next()
        .ok_or(RequestReadError::Malformed("missing request line"))?;
    let mut words = request_line.split_whitespace();
    let method = words
        .next()
        .ok_or(RequestReadError::Malformed("missing method"))?;
    let target = words
        .next()
        .ok_or(RequestReadError::Malformed("missing request target"))?;
    let version = words
        .next()
        .ok_or(RequestReadError::Malformed("missing HTTP version"))?;
    if words.next().is_some() || !version.starts_with("HTTP/1.") {
        return Err(RequestReadError::Malformed("malformed request line"));
    }

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(RequestReadError::Malformed("malformed header line"))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(RequestReadError::Malformed("empty header name"));
        }
        headers.push((name.to_owned(), value.trim().to_owned()));
    }

    Ok(Request {
        method: method.to_owned(),
        target: target.to_owned(),
        headers,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Master,
    Live,
    Segment(u64),
}

/// Matches exactly `/<token>/master.m3u8`, `/<token>/live.m3u8` or
/// `/<token>/<digits>.ts`; anything else (traversal, percent encoding, extra
/// path components) is rejected.
fn route(target: &str, token: &str) -> Option<Route> {
    let path = target.split(['?', '#']).next().unwrap_or("");
    if path.contains('%') || path.contains('\\') {
        return None;
    }
    let rest = path.strip_prefix('/')?;
    let mut parts = rest.split('/');
    let request_token = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if !constant_time_eq(request_token.as_bytes(), token.as_bytes()) {
        return None;
    }
    match name {
        "master.m3u8" => Some(Route::Master),
        "live.m3u8" => Some(Route::Live),
        _ => {
            let digits = name.strip_suffix(".ts")?;
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            Some(Route::Segment(digits.parse().ok()?))
        }
    }
}

fn lock_store(store: &Mutex<HlsStore>) -> MutexGuard<'_, HlsStore> {
    store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn handle_request(request: &Request, token: &str, store: &Mutex<HlsStore>) -> HttpResponse {
    if request.method == "OPTIONS" {
        return HttpResponse::empty(204, "No Content")
            .with_extra("Allow", "GET, HEAD, OPTIONS")
            .with_extra("Access-Control-Max-Age", "600");
    }
    if request.method != "GET" && request.method != "HEAD" {
        return HttpResponse::text(405, "Method Not Allowed", "method not allowed\n")
            .with_extra("Allow", "GET, HEAD, OPTIONS");
    }

    let Some(route) = route(&request.target, token) else {
        return HttpResponse::text(404, "Not Found", "not found\n");
    };

    let store = lock_store(store);
    match route {
        Route::Master => {
            if !store.ready() {
                return HttpResponse::text(503, "Service Unavailable", "stream not ready\n");
            }
            let codec = store.codec().unwrap_or("avc1.42c01f");
            HttpResponse::manifest(master_playlist(store.bandwidth_bps(), codec))
        }
        Route::Live => match store.playlist() {
            Some(playlist) => HttpResponse::manifest(playlist),
            None => HttpResponse::text(503, "Service Unavailable", "stream not ready\n"),
        },
        Route::Segment(sequence) => {
            let Some(segment) = store.segment(sequence) else {
                return HttpResponse::text(404, "Not Found", "segment not found\n");
            };
            let data = segment.data.clone();
            drop(store);
            match range_of(request.header("range"), data.len()) {
                RangeDecision::Full => {
                    HttpResponse::segment(data).with_extra("Accept-Ranges", "bytes")
                }
                RangeDecision::Partial { start, end } => HttpResponse::partial(data, start, end),
                RangeDecision::Unsatisfiable => HttpResponse::empty(416, "Range Not Satisfiable")
                    .with_extra("Content-Range", format!("bytes */{}", data.len()))
                    .with_extra("Accept-Ranges", "bytes"),
            }
        }
    }
}

fn master_playlist(bandwidth_bps: u64, codec: &str) -> String {
    format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-STREAM-INF:BANDWIDTH={bandwidth_bps},CODECS=\"{codec}\"\nlive.m3u8\n"
    )
}

enum RangeDecision {
    Full,
    Partial { start: usize, end: usize },
    Unsatisfiable,
}

/// Parses a single `bytes=a-b`, `bytes=a-` or `bytes=-n` range. Multi-ranges
/// and malformed values fall back to serving the full representation.
fn range_of(header: Option<&str>, len: usize) -> RangeDecision {
    let Some(value) = header else {
        return RangeDecision::Full;
    };
    let Some((unit, spec)) = value.trim().split_once('=') else {
        return RangeDecision::Full;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") || spec.contains(',') {
        return RangeDecision::Full;
    }
    let Some((start_text, end_text)) = spec.split_once('-') else {
        return RangeDecision::Full;
    };

    if start_text.trim().is_empty() {
        let Ok(suffix) = end_text.trim().parse::<usize>() else {
            return RangeDecision::Full;
        };
        if suffix == 0 || len == 0 {
            return RangeDecision::Unsatisfiable;
        }
        return RangeDecision::Partial {
            start: len.saturating_sub(suffix),
            end: len - 1,
        };
    }

    let Ok(start) = start_text.trim().parse::<usize>() else {
        return RangeDecision::Full;
    };
    let end = if end_text.trim().is_empty() {
        len.saturating_sub(1)
    } else {
        match end_text.trim().parse::<usize>() {
            Ok(end) => end.min(len.saturating_sub(1)),
            Err(_) => return RangeDecision::Full,
        }
    };
    if start >= len || start > end {
        return RangeDecision::Unsatisfiable;
    }
    RangeDecision::Partial { start, end }
}

struct HttpResponse {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    cache_control: &'static str,
    extra: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn new(
        status: u16,
        reason: &'static str,
        content_type: &'static str,
        cache_control: &'static str,
        body: Vec<u8>,
    ) -> Self {
        Self {
            status,
            reason,
            content_type,
            cache_control,
            extra: Vec::new(),
            body,
        }
    }

    fn empty(status: u16, reason: &'static str) -> Self {
        Self::new(status, reason, TEXT_MIME, "no-store", Vec::new())
    }

    fn text(status: u16, reason: &'static str, body: &str) -> Self {
        Self::new(
            status,
            reason,
            TEXT_MIME,
            "no-store",
            body.as_bytes().to_vec(),
        )
    }

    fn manifest(body: String) -> Self {
        Self::new(200, "OK", MANIFEST_MIME, MANIFEST_CACHE, body.into_bytes())
    }

    fn segment(data: Vec<u8>) -> Self {
        Self::new(200, "OK", SEGMENT_MIME, SEGMENT_CACHE, data)
    }

    fn partial(data: Vec<u8>, start: usize, end: usize) -> Self {
        let total = data.len();
        Self::new(
            206,
            "Partial Content",
            SEGMENT_MIME,
            SEGMENT_CACHE,
            data[start..=end].to_vec(),
        )
        .with_extra("Content-Range", format!("bytes {start}-{end}/{total}"))
        .with_extra("Accept-Ranges", "bytes")
    }

    fn with_extra(mut self, name: &str, value: impl Into<String>) -> Self {
        self.extra.push((name.to_owned(), value.into()));
        self
    }
}

async fn write_response(
    stream: &mut TcpStream,
    response: &HttpResponse,
    include_body: bool,
) -> io::Result<()> {
    let mut head = String::with_capacity(320);
    head.push_str(&format!(
        "HTTP/1.1 {} {}\r\n",
        response.status, response.reason
    ));
    head.push_str(&format!("Content-Type: {}\r\n", response.content_type));
    head.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    head.push_str(&format!("Cache-Control: {}\r\n", response.cache_control));
    head.push_str("Access-Control-Allow-Origin: *\r\n");
    head.push_str("Access-Control-Allow-Methods: GET, HEAD, OPTIONS\r\n");
    head.push_str("Access-Control-Allow-Headers: *\r\n");
    head.push_str("Access-Control-Expose-Headers: Content-Range, Accept-Ranges\r\n");
    for (name, value) in &response.extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");

    stream.write_all(head.as_bytes()).await?;
    if include_body {
        stream.write_all(&response.body).await?;
    }
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::{HlsProfile, Segment};
    use std::net::Ipv4Addr;

    /// Publishes one-second segments on a fake monotonic clock (segment `n` is
    /// stamped `epoch + n` seconds) so the advertised snapshot commits
    /// deterministically without any wall-clock sleep.
    fn publish_fixture(store: &mut HlsStore, count: u64, codec: &str) {
        let base = std::time::Instant::now();
        for sequence in 0..count {
            store
                .publish_at(
                    Segment {
                        sequence,
                        duration: 1.0,
                        data: vec![(0x40 + sequence % 0x40) as u8; 376],
                    },
                    codec,
                    base + Duration::from_secs(sequence),
                )
                .expect("publish fixture segment");
        }
    }

    fn test_store(count: u64) -> Arc<Mutex<HlsStore>> {
        let mut store = HlsStore::new();
        publish_fixture(&mut store, count, "avc1.640028");
        Arc::new(Mutex::new(store))
    }

    /// Publishes `count` half-second segments on a fake monotonic clock
    /// (segment `n` is stamped `epoch + n * 500ms`) and returns the epoch so a
    /// caller can keep publishing on the same deterministic clock.
    fn responsive_store(
        count: u64,
        bandwidth_bps: u64,
    ) -> (Arc<Mutex<HlsStore>>, std::time::Instant) {
        let mut store = HlsStore::with_profile(HlsProfile::Responsive, bandwidth_bps)
            .expect("responsive store");
        let base = std::time::Instant::now();
        for sequence in 0..count {
            store
                .publish_at(
                    Segment {
                        sequence,
                        duration: 0.5,
                        data: vec![(0x40 + sequence % 0x40) as u8; 376],
                    },
                    "avc1.640028",
                    base + Duration::from_micros(sequence * 500_000),
                )
                .expect("publish responsive fixture segment");
        }
        (Arc::new(Mutex::new(store)), base)
    }

    async fn start_server(count: u64) -> (HttpServer, Arc<Mutex<HlsStore>>) {
        let store = test_store(count);
        let server = HttpServer::start(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            Arc::clone(&store),
        )
        .await
        .unwrap();
        (server, store)
    }

    fn token_of(server: &HttpServer) -> String {
        let url = server.url();
        let rest = url
            .strip_prefix("http://127.0.0.1:")
            .unwrap_or_else(|| panic!("unexpected url {url}"));
        let (port, path) = rest
            .split_once('/')
            .unwrap_or_else(|| panic!("unexpected url {url}"));
        assert!(port.parse::<u16>().is_ok(), "unexpected port in {url}");
        let token = path
            .strip_suffix("/master.m3u8")
            .unwrap_or_else(|| panic!("unexpected path in {url}"));
        assert_eq!(token.len(), 32, "128-bit token expected in {url}");
        assert!(
            token.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "hex token expected in {url}"
        );
        token.to_owned()
    }

    fn request_for(token: &str, path: &str) -> String {
        format!("GET /{token}/{path} HTTP/1.1\r\nHost: cast.local\r\n\r\n")
    }

    fn parse_response(buf: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let split = buf
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("response header terminator")
            + 4;
        let head = std::str::from_utf8(&buf[..split]).unwrap();
        let mut lines = head.split("\r\n");
        let status_line = lines.next().expect("status line");
        let status = status_line
            .split_whitespace()
            .nth(1)
            .expect("status code")
            .parse()
            .expect("numeric status code");
        let headers = lines
            .filter(|line| !line.is_empty())
            .map(|line| {
                let (name, value) = line.split_once(':').expect("header separator");
                (name.trim().to_ascii_lowercase(), value.trim().to_owned())
            })
            .collect();
        (status, headers, buf[split..].to_vec())
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    async fn raw(addr: SocketAddr, request: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
            .await
            .expect("timed out waiting for the HTTP response")
            .unwrap();
        assert!(!buf.is_empty(), "connection closed without a response");
        parse_response(&buf)
    }

    async fn raw_quiet(addr: SocketAddr, request: &str) -> Option<u16> {
        let mut stream = TcpStream::connect(addr).await.ok()?;
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut buf = Vec::new();
        timeout(Duration::from_secs(2), stream.read_to_end(&mut buf))
            .await
            .ok()?
            .ok()?;
        if buf.is_empty() {
            return None;
        }
        Some(parse_response(&buf).0)
    }

    #[tokio::test]
    async fn serves_master_live_and_segments() {
        let (server, store) = start_server(8).await;
        let addr = server.local_addr();
        let token = token_of(&server);

        let (status, headers, body) = raw(addr, &request_for(&token, "master.m3u8")).await;
        assert_eq!(status, 200);
        assert_eq!(
            header(&headers, "content-type"),
            Some("application/vnd.apple.mpegurl")
        );
        assert!(
            header(&headers, "cache-control")
                .unwrap()
                .contains("no-cache"),
            "manifest must not be cached"
        );
        assert_eq!(
            header(&headers, "content-length").and_then(|value| value.parse().ok()),
            Some(body.len())
        );
        let master = String::from_utf8(body).unwrap();
        assert!(master.contains("CODECS=\"avc1.640028\""), "{master}");
        assert!(master.contains("BANDWIDTH=8000000"), "{master}");
        assert!(master.contains("live.m3u8"), "{master}");
        assert!(!master.to_ascii_lowercase().contains("audio"), "{master}");

        let (status, headers, body) = raw(addr, &request_for(&token, "live.m3u8")).await;
        assert_eq!(status, 200);
        assert_eq!(
            header(&headers, "content-type"),
            Some("application/vnd.apple.mpegurl")
        );
        let live = String::from_utf8(body).unwrap();
        assert!(live.contains("#EXT-X-TARGETDURATION:2"), "{live}");
        assert!(live.contains("#EXT-X-MEDIA-SEQUENCE:0"), "{live}");
        assert_eq!(live.matches("#EXTINF:1.000000,").count(), 8, "{live}");
        assert!(
            live.contains("\n0.ts\n") && live.contains("\n7.ts\n"),
            "{live}"
        );
        assert!(!live.contains("#EXT-X-ENDLIST"), "{live}");

        let expected = store.lock().unwrap().segment(1).unwrap().data.clone();
        let (status, headers, body) = raw(addr, &request_for(&token, "1.ts")).await;
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "content-type"), Some("video/mp2t"));
        assert_eq!(
            header(&headers, "content-length").and_then(|value| value.parse().ok()),
            Some(expected.len())
        );
        assert_eq!(body, expected);
    }

    #[tokio::test]
    async fn stats_count_manifest_and_segment_delivery() {
        let (server, store) = start_server(8).await;
        let addr = server.local_addr();
        let token = token_of(&server);
        let stats = server.stats();

        let (status, _headers, master) = raw(addr, &request_for(&token, "master.m3u8")).await;
        assert_eq!(status, 200);
        let (status, _headers, live) = raw(addr, &request_for(&token, "live.m3u8")).await;
        assert_eq!(status, 200);
        let expected = store.lock().unwrap().segment(1).unwrap().data.clone();
        let (status, _headers, segment) = raw(addr, &request_for(&token, "1.ts")).await;
        assert_eq!(status, 200);
        assert_eq!(segment, expected);
        let (status, _headers, missing_body) = raw(addr, &request_for(&token, "99999.ts")).await;
        assert_eq!(status, 404);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.requests, 4, "every parsed request is counted");
        assert_eq!(snapshot.manifests, 2, "served master and live playlists");
        assert_eq!(snapshot.ts_completed, 1, "one completed segment response");
        assert_eq!(snapshot.ts_missing, 1, "one evicted segment was requested");
        assert_eq!(snapshot.writes_failed, 0);
        assert_eq!(
            snapshot.total_bytes,
            (master.len() + live.len() + segment.len() + missing_body.len()) as u64,
            "every body byte actually written is counted"
        );
        assert!(
            snapshot.max_response_ms < 5_000,
            "loopback responses must stay far below the IO timeout"
        );
    }

    #[tokio::test]
    async fn stats_display_is_numeric_and_never_leaks_capabilities() {
        let (server, _store) = start_server(8).await;
        let addr = server.local_addr();
        let token = token_of(&server);

        let (status, _headers, _body) = raw(addr, &request_for(&token, "master.m3u8")).await;
        assert_eq!(status, 200);
        let (status, _headers, _body) = raw(addr, &request_for(&token, "99999.ts")).await;
        assert_eq!(status, 404);

        let text = server.stats_snapshot().to_string();
        assert!(text.contains("requests=2"), "{text}");
        assert!(text.contains("manifests=1"), "{text}");
        assert!(text.contains("ts_missing=1"), "{text}");
        assert!(
            !text.contains(&token),
            "the capability token must never be rendered: {text}"
        );
        assert!(!text.contains("m3u8"), "{text}");
        assert!(!text.contains("HTTP/"), "{text}");
        assert!(!text.contains('/'), "{text}");
        assert!(
            text.chars()
                .all(|character| character.is_ascii_alphanumeric() || " =_".contains(character)),
            "the display must stay bounded numeric fields only: {text}"
        );
    }

    #[tokio::test]
    async fn master_advertises_combined_aac_codec() {
        let mut store = HlsStore::new();
        publish_fixture(&mut store, 8, "avc1.640028,mp4a.40.2");
        let server = HttpServer::start(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            Arc::new(Mutex::new(store)),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let token = token_of(&server);

        let (status, _headers, body) = raw(addr, &request_for(&token, "master.m3u8")).await;
        assert_eq!(status, 200);
        let master = String::from_utf8(body).unwrap();
        assert!(
            master.contains("CODECS=\"avc1.640028,mp4a.40.2\""),
            "the stored codec string must pass through verbatim: {master}"
        );
    }

    /// A responsive session with a 16 Mbit/s hint must advertise that bandwidth,
    /// a 1-second live target and keep serving retired segments inside the
    /// retention grace.
    #[tokio::test]
    async fn responsive_master_live_and_retired_segments() {
        let (store, _base) = responsive_store(40, 16_000_000);
        let server = HttpServer::start(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            Arc::clone(&store),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let token = token_of(&server);

        let (status, _headers, body) = raw(addr, &request_for(&token, "master.m3u8")).await;
        assert_eq!(status, 200);
        let master = String::from_utf8(body).unwrap();
        assert!(master.contains("BANDWIDTH=16000000"), "{master}");
        assert!(master.contains("CODECS=\"avc1.640028\""), "{master}");

        let (status, _headers, body) = raw(addr, &request_for(&token, "live.m3u8")).await;
        assert_eq!(status, 200);
        let live = String::from_utf8(body).unwrap();
        assert!(live.contains("#EXT-X-TARGETDURATION:1\n"), "{live}");
        assert_eq!(live.matches("#EXTINF:0.500000,\n").count(), 24, "{live}");
        assert!(
            !live.contains("\n0.ts\n"),
            "sequence 0 must have left the 12s window: {live}"
        );

        // Retired but inside its retention promise: the old manifest URI still
        // resolves even though the live playlist no longer lists it.
        let (status, headers, body) = raw(addr, &request_for(&token, "0.ts")).await;
        assert_eq!(
            status, 200,
            "a retired responsive segment inside the grace period must be served"
        );
        assert_eq!(header(&headers, "content-type"), Some("video/mp2t"));
        assert_eq!(body.len(), 376);
        let (status, _headers, _body) = raw(addr, &request_for(&token, "39.ts")).await;
        assert_eq!(status, 200, "the newest window segment must be served");
    }

    /// Responsive readiness is measured in advertised media seconds: 3.5s is
    /// not ready, 4.0s is, regardless of how many segments that took.
    #[tokio::test]
    async fn responsive_manifests_become_ready_after_four_advertised_seconds() {
        let (store, base) = responsive_store(7, 16_000_000);
        let server = HttpServer::start(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            Arc::clone(&store),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let token = token_of(&server);

        for path in ["master.m3u8", "live.m3u8"] {
            let (status, _headers, _body) = raw(addr, &request_for(&token, path)).await;
            assert_eq!(status, 503, "{path} must not advertise before 4s of media");
        }

        store
            .lock()
            .unwrap()
            .publish_at(
                Segment {
                    sequence: 7,
                    duration: 0.5,
                    data: vec![0x47; 376],
                },
                "avc1.640028",
                base + Duration::from_micros(3_500_000),
            )
            .unwrap();

        for path in ["master.m3u8", "live.m3u8"] {
            let (status, _headers, body) = raw(addr, &request_for(&token, path)).await;
            assert_eq!(status, 200, "{path} must advertise at 4.0s of media");
            if path == "live.m3u8" {
                let live = String::from_utf8(body).unwrap();
                assert!(live.contains("#EXT-X-TARGETDURATION:1\n"), "{live}");
                assert_eq!(live.matches("#EXTINF:0.500000,\n").count(), 8, "{live}");
            }
        }
    }

    #[tokio::test]
    async fn head_and_options_are_supported_with_cors() {
        let (server, _store) = start_server(8).await;
        let addr = server.local_addr();
        let token = token_of(&server);

        let request = format!("HEAD /{token}/master.m3u8 HTTP/1.1\r\nHost: cast.local\r\n\r\n");
        let (status, headers, body) = raw(addr, &request).await;
        assert_eq!(status, 200);
        assert!(body.is_empty(), "HEAD must not return a body");
        assert!(
            header(&headers, "content-length")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap()
                > 0
        );
        assert_eq!(header(&headers, "access-control-allow-origin"), Some("*"));

        let request = format!("OPTIONS /{token}/live.m3u8 HTTP/1.1\r\nHost: cast.local\r\n\r\n");
        let (status, headers, body) = raw(addr, &request).await;
        assert_eq!(status, 204);
        assert!(body.is_empty());
        assert_eq!(header(&headers, "access-control-allow-origin"), Some("*"));
        assert!(
            header(&headers, "access-control-allow-methods")
                .unwrap()
                .contains("GET")
        );
        assert!(header(&headers, "access-control-allow-headers").is_some());
        assert!(header(&headers, "allow").unwrap().contains("OPTIONS"));

        let request = format!(
            "POST /{token}/master.m3u8 HTTP/1.1\r\nHost: cast.local\r\nContent-Length: 0\r\n\r\n"
        );
        let (status, headers, _body) = raw(addr, &request).await;
        assert_eq!(status, 405);
        assert!(header(&headers, "allow").unwrap().contains("GET"));
    }

    #[tokio::test]
    async fn manifests_return_503_until_ready() {
        let (server, store) = start_server(0).await;
        let addr = server.local_addr();
        let token = token_of(&server);

        for path in ["master.m3u8", "live.m3u8"] {
            let (status, _headers, _body) = raw(addr, &request_for(&token, path)).await;
            assert_eq!(
                status, 503,
                "{path} must not advertise before eight seconds of media"
            );
        }

        let base = std::time::Instant::now();
        for sequence in 0..4u64 {
            store
                .lock()
                .unwrap()
                .publish_at(
                    Segment {
                        sequence,
                        duration: 2.0,
                        data: vec![(0x40 + sequence) as u8; 376],
                    },
                    "avc1.640028",
                    base + Duration::from_secs(2 * sequence),
                )
                .unwrap();
            let (status, _headers, _body) = raw(addr, &request_for(&token, "live.m3u8")).await;
            if sequence < 3 {
                assert_eq!(
                    status, 503,
                    "sequence {sequence} must not advertise the first six seconds"
                );
            } else {
                assert_eq!(status, 200, "eight advertised seconds must be ready");
            }
        }
    }

    #[tokio::test]
    async fn rejects_wrong_token_and_traversal() {
        let (server, _store) = start_server(8).await;
        let addr = server.local_addr();
        let token = token_of(&server);

        let wrong = "0".repeat(32);
        let (status, _headers, _body) = raw(addr, &request_for(&wrong, "master.m3u8")).await;
        assert_eq!(status, 404, "wrong token must not be served");

        let paths = [
            format!("/{token}/../master.m3u8"),
            format!("/../{token}/master.m3u8"),
            format!("/{token}/%2e%2e/master.m3u8"),
            format!("/{token}/..%2fmaster.m3u8"),
            format!("/{token}/master.m3u8/"),
            format!("/{token}/nope.ts"),
            format!("/{token}/99999.ts"),
            "/".to_owned(),
            "/etc/passwd".to_owned(),
        ];
        for path in paths {
            let request = format!("GET {path} HTTP/1.1\r\nHost: cast.local\r\n\r\n");
            let (status, _headers, _body) = raw(addr, &request).await;
            assert_eq!(status, 404, "{path} must be rejected");
        }

        // A query string is allowed and ignored by HLS players.
        let request = format!("GET /{token}/1.ts?session=abc HTTP/1.1\r\nHost: cast.local\r\n\r\n");
        let (status, _headers, _body) = raw(addr, &request).await;
        assert_eq!(status, 200);
    }

    #[tokio::test]
    async fn segment_single_byte_range() {
        let (server, store) = start_server(8).await;
        let addr = server.local_addr();
        let token = token_of(&server);
        let expected = store.lock().unwrap().segment(1).unwrap().data.clone();

        let request =
            format!("GET /{token}/1.ts HTTP/1.1\r\nHost: cast.local\r\nRange: bytes=0-9\r\n\r\n");
        let (status, headers, body) = raw(addr, &request).await;
        assert_eq!(status, 206);
        assert_eq!(
            header(&headers, "content-range"),
            Some(format!("bytes 0-9/{}", expected.len()).as_str())
        );
        assert!(
            header(&headers, "access-control-expose-headers")
                .unwrap()
                .contains("Content-Range"),
            "range clients need Content-Range exposed through CORS"
        );
        assert_eq!(body, expected[..10]);

        let request =
            format!("GET /{token}/1.ts HTTP/1.1\r\nHost: cast.local\r\nRange: bytes=-4\r\n\r\n");
        let (status, headers, body) = raw(addr, &request).await;
        assert_eq!(status, 206);
        assert_eq!(body, expected[expected.len() - 4..]);
        assert_eq!(
            header(&headers, "content-range"),
            Some(
                format!(
                    "bytes {}-{}/{}",
                    expected.len() - 4,
                    expected.len() - 1,
                    expected.len()
                )
                .as_str()
            )
        );

        let request = format!(
            "GET /{token}/1.ts HTTP/1.1\r\nHost: cast.local\r\nRange: bytes=99999-\r\n\r\n"
        );
        let (status, headers, _body) = raw(addr, &request).await;
        assert_eq!(status, 416);
        assert_eq!(
            header(&headers, "content-range"),
            Some(format!("bytes */{}", expected.len()).as_str())
        );

        // A multi-range request is ignored; the full representation is served.
        let request = format!(
            "GET /{token}/1.ts HTTP/1.1\r\nHost: cast.local\r\nRange: bytes=0-1,4-5\r\n\r\n"
        );
        let (status, _headers, body) = raw(addr, &request).await;
        assert_eq!(status, 200);
        assert_eq!(body, expected);
    }

    /// A client that fetched a manifest while a segment was advertised must
    /// still be served after the segment left the active window, until its
    /// retention promise expires. The fractional publish at t11.5 is the
    /// regression: it must not retire sequence 0 early, so its clock starts at
    /// the t12 commit and the URI survives until t25. Everything runs on the
    /// fake monotonic clock with no wall-clock sleep.
    #[tokio::test]
    async fn retired_segment_serves_old_manifest_then_expires() {
        const CODEC: &str = "avc1.640028";
        let mut store = HlsStore::new();
        let base = std::time::Instant::now();
        for sequence in 0..12u64 {
            store
                .publish_at(
                    Segment {
                        sequence,
                        duration: 1.0,
                        data: vec![(0x40 + sequence % 0x40) as u8; 376],
                    },
                    CODEC,
                    base + Duration::from_secs(sequence),
                )
                .unwrap();
        }
        let store = Arc::new(Mutex::new(store));
        let server = HttpServer::start(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            Arc::clone(&store),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let token = token_of(&server);

        // A publish 500ms inside the snapshot interval is stored but must not
        // retire sequence 0: the t11 snapshot still advertises it.
        let fractional = |sequence: u64| Segment {
            sequence,
            duration: 1.0,
            data: vec![(0x40 + sequence % 0x40) as u8; 376],
        };
        store
            .lock()
            .unwrap()
            .publish_at(fractional(12), CODEC, base + Duration::from_millis(11_500))
            .unwrap();
        let (status, headers, body) = raw(addr, &request_for(&token, "0.ts")).await;
        assert_eq!(
            status, 200,
            "a publish inside the commit interval must not retire an advertised segment"
        );
        assert_eq!(header(&headers, "content-type"), Some("video/mp2t"));
        assert_eq!(body.len(), 376);

        // The t12 commit retires sequence 0 with a clock starting there, so the
        // old manifest URI stays served well past the buggy t24.5 deadline.
        for (sequence, seconds) in [(13u64, 12u64), (14, 24)] {
            store
                .lock()
                .unwrap()
                .publish_at(
                    fractional(sequence),
                    CODEC,
                    base + Duration::from_secs(seconds),
                )
                .unwrap();
        }
        let (status, _headers, _body) = raw(addr, &request_for(&token, "0.ts")).await;
        assert_eq!(status, 200);

        // Another publish at t24.999 cannot free it early either.
        store
            .lock()
            .unwrap()
            .publish_at(
                fractional(15),
                CODEC,
                base + Duration::from_secs(24) + Duration::from_millis(999),
            )
            .unwrap();
        let (status, _headers, _body) = raw(addr, &request_for(&token, "0.ts")).await;
        assert_eq!(
            status, 200,
            "retirement must be timed from the t12 commit, not the t11.5 publish"
        );

        // The first eligible publish at t25 expires it (no wall sleep needed).
        store
            .lock()
            .unwrap()
            .publish_at(fractional(16), CODEC, base + Duration::from_secs(25))
            .unwrap();
        let (status, _headers, _body) = raw(addr, &request_for(&token, "0.ts")).await;
        assert_eq!(status, 404, "an expired retired segment must be evicted");

        // The current window is unaffected.
        let (status, _headers, _body) = raw(addr, &request_for(&token, "16.ts")).await;
        assert_eq!(status, 200);
    }

    #[tokio::test]
    async fn restricts_peer_to_receiver_ip() {
        let store = test_store(8);
        let server = HttpServer::start(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
            store,
        )
        .await
        .unwrap();
        let addr = server.local_addr();

        let mut stream = TcpStream::connect(addr).await.unwrap();
        let _ = stream
            .write_all(b"GET /whatever/master.m3u8 HTTP/1.1\r\nHost: cast.local\r\n\r\n")
            .await;
        let mut buf = Vec::new();
        match timeout(Duration::from_secs(2), stream.read_to_end(&mut buf)).await {
            Ok(Ok(_)) => assert!(buf.is_empty(), "unexpected peer was served: {buf:?}"),
            Ok(Err(_)) => {}
            Err(_) => panic!("server did not close the unexpected peer connection"),
        }
    }

    #[tokio::test]
    async fn bounds_stalled_connections_and_recovers() {
        let (server, _store) = start_server(8).await;
        let addr = server.local_addr();
        let token = token_of(&server);

        let mut stalled = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            stalled.push(TcpStream::connect(addr).await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut extra = TcpStream::connect(addr).await.unwrap();
        let _ = extra
            .write_all(request_for(&token, "master.m3u8").as_bytes())
            .await;
        let mut buf = Vec::new();
        match timeout(Duration::from_secs(2), extra.read_to_end(&mut buf)).await {
            Ok(Ok(_)) => assert!(
                buf.is_empty(),
                "a 17th connection was served despite the {MAX_CONNECTIONS}-connection bound"
            ),
            Ok(Err(_)) => {}
            Err(_) => panic!("17th connection was neither served nor closed"),
        }
        drop(extra);
        drop(stalled);

        let mut recovered = false;
        for _ in 0..30 {
            match raw_quiet(addr, &request_for(&token, "master.m3u8")).await {
                Some(200) => {
                    recovered = true;
                    break;
                }
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        assert!(
            recovered,
            "server did not recover after stalled clients were dropped"
        );
    }

    #[tokio::test]
    async fn oversized_headers_are_rejected() {
        let (server, _store) = start_server(8).await;
        let addr = server.local_addr();

        let prefix = "GET / HTTP/1.1\r\nHost: cast.local\r\nX-Fill: ";
        let padding = "a".repeat(MAX_HEADER_BYTES + 1 - prefix.len());
        let request = format!("{prefix}{padding}");

        let mut stream = TcpStream::connect(addr).await.unwrap();
        let _ = stream.write_all(request.as_bytes()).await;
        let mut buf = Vec::new();
        let _ = timeout(Duration::from_secs(2), stream.read_to_end(&mut buf)).await;
        let (status, _headers, _body) = parse_response(&buf);
        assert_eq!(status, 431);
    }

    #[tokio::test]
    async fn header_cap_is_exact() {
        let (server, _store) = start_server(8).await;
        let addr = server.local_addr();

        // A head of exactly 8 KiB including the terminating blank line is
        // parsed (the unknown path then answers 404, not 431).
        let prefix = "GET / HTTP/1.1\r\nHost: cast.local\r\nX-Pad: ";
        let suffix = "\r\n\r\n";
        let pad = MAX_HEADER_BYTES - prefix.len() - suffix.len();
        let request = format!("{prefix}{}{suffix}", "a".repeat(pad));
        assert_eq!(request.len(), MAX_HEADER_BYTES);
        let (status, _headers, _body) = raw(addr, &request).await;
        assert_eq!(
            status, 404,
            "a head of exactly {MAX_HEADER_BYTES} bytes must be parsed, not rejected"
        );

        // One byte over the cap with a complete delimiter is still rejected.
        let request = format!("{prefix}{}{suffix}", "a".repeat(pad + 1));
        assert_eq!(request.len(), MAX_HEADER_BYTES + 1);
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let _ = stream.write_all(request.as_bytes()).await;
        let mut buf = Vec::new();
        let _ = timeout(Duration::from_secs(2), stream.read_to_end(&mut buf)).await;
        let (status, _headers, _body) = parse_response(&buf);
        assert_eq!(status, 431, "a {MAX_HEADER_BYTES}-byte cap is a hard cap");
    }

    #[tokio::test]
    async fn shutdown_and_drop_release_the_port() {
        let (mut server, _store) = start_server(8).await;
        let addr = server.local_addr();
        assert!(!server.is_finished());
        assert!(server.check_health().is_ok());

        server.shutdown().await.unwrap();
        assert!(server.is_finished());
        assert!(server.check_health().is_err());
        TcpListener::bind(addr)
            .await
            .expect("port released after shutdown");

        let (server, _store) = start_server(8).await;
        let addr = server.local_addr();
        drop(server);
        tokio::time::sleep(Duration::from_millis(100)).await;
        TcpListener::bind(addr)
            .await
            .expect("port released after drop");
    }

    #[tokio::test]
    async fn cancelled_shutdown_keeps_owner_and_drop_cleans_up() {
        let (mut server, _store) = start_server(8).await;
        let addr = server.local_addr();
        let mut client = TcpStream::connect(addr).await.unwrap();

        // Poll the shutdown future exactly once: it has sent the cooperative
        // signal and started awaiting the accept task. Then cancel it. On a
        // single-threaded runtime nothing else can advance during the poll, so
        // the server must still own the accept task afterwards.
        {
            let mut shutdown = Box::pin(server.shutdown());
            std::future::poll_fn(|context| {
                assert!(
                    shutdown.as_mut().poll(context).is_pending(),
                    "shutdown must still be waiting for the accept task"
                );
                std::task::Poll::Ready(())
            })
            .await;
        }
        assert!(
            !server.is_finished(),
            "a cancelled shutdown must not detach or forget the accept task"
        );

        // Drop must still abort the accept task and every client task.
        drop(server);
        let mut buf = Vec::new();
        match timeout(Duration::from_secs(2), client.read_to_end(&mut buf)).await {
            Ok(Ok(_)) | Ok(Err(_)) => {}
            Err(_) => panic!("client connection was not closed after drop"),
        }

        let mut rebound = false;
        for _ in 0..20 {
            if TcpListener::bind(addr).await.is_ok() {
                rebound = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            rebound,
            "port was not released after cancelled shutdown + drop"
        );
    }

    #[tokio::test]
    async fn refuses_unspecified_bind_addresses() {
        let store = test_store(8);
        let error = match HttpServer::start(
            SocketAddr::from(([0, 0, 0, 0], 0)),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            store,
        )
        .await
        {
            Ok(_) => panic!("unspecified bind address must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unspecified"), "{error}");
    }
}
