//! Google Cast V2 control client.
//!
//! [`CastClient`] drives the Default Media Receiver (`CC1AD845`) over the Cast V2
//! TLS control channel (TCP 8009). It queries receiver status, launches the
//! application, connects the virtual transport, loads a live HLS stream and keeps the
//! session alive with heartbeats until stopped or the receiver fails.
//!
//! # Security
//!
//! Google Cast receivers use self-signed certificates, so the Cast connector in this
//! module uses a custom `rustls` verifier that skips certificate-chain and host-name
//! checks while still verifying the TLS 1.2/1.3 handshake signature cryptographically.
//! The receiver identity remains **unauthenticated**: an active attacker on the LAN
//! could impersonate a Cast device. Use Cast only on trusted networks.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::net::TcpStream;

mod proto;
mod session;

#[cfg(test)]
mod test_support;
mod tls;

/// A Cast V2 control connection to one receiver.
///
/// The client owns the TLS stream and never spawns detached tasks, so dropping a
/// [`stream`](CastClient::stream) future leaves the connection usable: call
/// [`stop`](CastClient::stop) afterwards to release the application session.
///
/// # Security
///
/// The Cast TLS connector skips certificate-chain and host-name validation because
/// receivers use self-signed certificates, but it still verifies handshake
/// signatures. The receiver identity is **not authenticated**; use only on trusted
/// networks.
pub struct CastClient {
    session: session::Session<session::BoxedTransport>,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
}

impl CastClient {
    /// Connect to `host:port` and open the Cast virtual connection.
    ///
    /// The whole operation is bounded to 15 seconds and may be cancelled by dropping
    /// the returned future. No receiver certificate chain or host-name verification
    /// is performed (see the type docs); the handshake signature is still checked.
    pub async fn connect(host: &str, port: u16) -> Result<Self> {
        let timeouts = session::Timeouts::default();
        let attempt = Self::connect_inner(host, port, timeouts);
        match tokio::time::timeout(timeouts.connect, attempt).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "timed out connecting to the Google Cast receiver"
            )),
        }
    }

    async fn connect_inner(host: &str, port: u16, timeouts: session::Timeouts) -> Result<Self> {
        let tcp = TcpStream::connect((host, port))
            .await
            .with_context(|| format!("failed to reach Cast receiver at {host}:{port}"))?;
        let local_addr = tcp
            .local_addr()
            .context("failed to read local Cast socket address")?;
        let peer_addr = tcp
            .peer_addr()
            .context("failed to read Cast peer address")?;
        let server_name = tls::server_name(host)?;
        let connector = tls::cast_connector()?;
        let tls_stream = connector
            .connect(server_name, tcp)
            .await
            .context("Cast TLS handshake failed")?;

        let transport: session::BoxedTransport = Box::new(tls_stream);
        let mut session = session::Session::new(transport, timeouts);
        session.handshake(None).await?;
        Ok(Self {
            session,
            local_addr,
            peer_addr,
        })
    }

    /// Local address of the underlying control socket.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Receiver address of the underlying control socket.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Query `GET_STATUS` and return the `RECEIVER_STATUS` payload.
    ///
    /// This performs a status request only; it never launches an application.
    pub async fn receiver_status(&mut self) -> Result<Value> {
        Ok(self.session.receiver_status(None).await?)
    }

    /// Launch the Default Media Receiver, load `url` as live HLS and keep it alive.
    ///
    /// * A busy receiver (any existing non-backdrop media application, including a
    ///   running Default Media Receiver) is rejected instead of being taken over.
    /// * `on_playing` runs exactly once after the receiver reports `PLAYING`.
    /// * `stop` is polled at least every 100 ms during setup and playback.
    /// * On every exit after the client owns an application session - including
    ///   setup or `LOAD` failures and cancellation - a bounded best-effort `STOP` is
    ///   sent for **that session only**. If the receiver has replaced the session,
    ///   nothing is stopped.
    /// * If this future is dropped before it returns, call [`stop`](Self::stop) to
    ///   perform the same cleanup; owned-session state survives cancellation.
    pub async fn stream(
        &mut self,
        url: &str,
        stop: Arc<AtomicBool>,
        on_playing: Option<Box<dyn FnOnce() + Send + 'static>>,
    ) -> Result<()> {
        Ok(self.session.stream(url, &stop, on_playing).await?)
    }

    /// Best-effort bounded cleanup of the application session this client launched.
    ///
    /// Intended for orchestrators whose [`stream`](Self::stream) future was dropped
    /// before it could clean up. Verifies the application and, when media was loaded,
    /// the media session before sending anything. Does nothing when no session is
    /// owned or when the receiver shows it was replaced. If the media session cannot
    /// be verified the session is left running and an error is returned, so a
    /// graceful stop never disrupts playback that may now belong to someone else.
    pub async fn stop(&mut self) -> Result<()> {
        Ok(self.session.cleanup().await?)
    }

    /// Build a client over a loopback transport for unit tests.
    #[cfg(test)]
    pub(crate) fn from_test_transport(
        transport: session::BoxedTransport,
        timeouts: session::Timeouts,
    ) -> Self {
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("valid socket address");
        Self {
            session: session::Session::new(transport, timeouts),
            local_addr: addr,
            peer_addr: addr,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::proto;
    use super::test_support::{MockReceiver, receiver_status, request_id_of, test_timeouts};
    use super::*;

    #[tokio::test]
    async fn public_api_reports_addresses_and_receiver_status() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let mut server = MockReceiver::new(server_io);
        let transport: session::BoxedTransport = Box::new(client_io);
        let mut client = CastClient::from_test_transport(transport, test_timeouts());
        assert_eq!(client.local_addr(), "127.0.0.1:0".parse().unwrap());
        assert_eq!(client.peer_addr(), "127.0.0.1:0".parse().unwrap());

        let client_task = tokio::spawn(async move {
            let status = client.receiver_status().await;
            (client, status)
        });

        let get = server
            .expect_message(proto::NS_RECEIVER, "GET_STATUS")
            .await;
        let expected = receiver_status(Some(request_id_of(&get)), Vec::new());
        server
            .send(
                proto::RECEIVER_ID,
                &get.source,
                proto::NS_RECEIVER,
                expected.clone(),
            )
            .await;

        let (mut client, status) = client_task.await.unwrap();
        assert_eq!(status.unwrap(), expected);
        client.stop().await.unwrap();
    }
}
