use reqwest::Client;
use rotten_core::config::{DeviceCredentials, MirrorConfig};
use rotten_core::device::AirPlayDevice;
use rotten_core::error::{Result, RottenError};
use rotten_core::session::{MirrorSession, SessionState};
use rotten_crypto::MirrorVideoCrypto;
use tokio::net::TcpStream;
use tracing::{debug, info};

use crate::http::send_feedback;
use crate::mirror_rtsp::{MirrorRtspResources, setup_mirror_rtsp};

/// Active mirror connection to an Apple TV.
pub struct MirrorConnection {
    pub device: AirPlayDevice,
    pub session: MirrorSession,
    data_port: u16,
    data_stream: Option<TcpStream>,
    rtsp_conn: Option<crate::airplay_conn::AirPlayRtspConn>,
    control_uri: String,
    _rtsp_resources: Option<MirrorRtspResources>,
    client: Client,
}

/// Handle returned after successful mirror setup; used to send video frames.
pub struct MirrorHandle {
    connection: MirrorConnection,
    pub video_crypto: MirrorVideoCrypto,
    pub audio: Option<crate::audio_rtp::MirrorAudioSetup>,
}

impl MirrorHandle {
    pub fn session(&self) -> &MirrorSession {
        &self.connection.session
    }

    pub fn data_port(&self) -> u16 {
        self.connection.data_port
    }

    pub fn control_uri(&self) -> &str {
        &self.connection.control_uri
    }

    /// Take the persistent RTSP socket (must stay open for POST /feedback during streaming).
    pub fn take_rtsp_conn(&mut self) -> Option<crate::airplay_conn::AirPlayRtspConn> {
        self.connection.rtsp_conn.take()
    }

    /// Deprecated: Apple TV listens; we connect to `data_port` on the device host.
    pub fn stream_port(&self) -> u16 {
        self.data_port()
    }

    pub fn device_host(&self) -> &str {
        &self.connection.device.host
    }

    /// Pre-connected video data TCP socket (opened before RTSP RECORD).
    pub fn take_data_stream(&mut self) -> Option<TcpStream> {
        self.connection.data_stream.take()
    }
}

impl MirrorConnection {
    pub async fn connect(
        device: AirPlayDevice,
        creds: &DeviceCredentials,
        config: &MirrorConfig,
    ) -> Result<MirrorHandle> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .pool_max_idle_per_host(1)
            .build()
            .map_err(|e| RottenError::Protocol(e.to_string()))?;

        let mut session = MirrorSession::default();
        session.transition(SessionState::Connecting);

        debug!(host = %device.host, "starting mirror setup");

        session.transition(SessionState::Authenticating);
        let mut airplay_conn = crate::airplay_conn::AirPlayRtspConn::connect(&device).await?;
        let pv = if creds.hap {
            let pv = crate::pair_verify::hap_pair_verify_conn(&mut airplay_conn, &device, creds)
                .await?;
            if let Some(keys) = pv.hap_keys {
                airplay_conn.enable_hap_encryption(keys.out_key, keys.in_key);
            }
            pv
        } else {
            crate::pair_verify::pair_verify_conn(&mut airplay_conn, &device, creds).await?
        };

        session.transition(SessionState::SettingUp);
        // Receivers without FairPlay SAP (feature bit 14 clear, e.g. Samsung TVs)
        // return 404 on /fp-setup; stream keys come from the pair-verify channel instead.
        let fp = if device.features.supports_fairplay_sap() {
            Some(crate::fp_setup::run_fp_setup_conn(&mut airplay_conn, creds).await?)
        } else {
            debug!(
                host = %device.host,
                "receiver does not support FairPlay SAP; skipping fp-setup"
            );
            None
        };
        let setup = setup_mirror_rtsp(
            &mut airplay_conn,
            &device,
            creds,
            fp.as_ref(),
            &pv,
            config.no_encrypt,
            config.cipher,
        )
        .await?;

        session.session_id = Some(setup.session_uuid);
        session.stream_port = Some(setup.data_port);
        session.control_port = setup.event_port;
        session.transition(SessionState::Streaming);

        info!(host = %device.host, data_port = setup.data_port, "mirror session ready");

        let connection = MirrorConnection {
            device,
            session,
            data_port: setup.data_port,
            data_stream: Some(setup.data_stream),
            rtsp_conn: Some(airplay_conn),
            control_uri: setup.control_uri,
            _rtsp_resources: Some(setup.resources),
            client,
        };

        Ok(MirrorHandle {
            connection,
            video_crypto: setup.video_crypto,
            audio: setup.audio,
        })
    }

    pub async fn stop(&mut self) -> Result<()> {
        self._rtsp_resources.take();
        self.session.transition(SessionState::Stopping);
        let _ = send_feedback(&self.client, &self.device).await;
        self.session.transition(SessionState::Disconnected);
        Ok(())
    }
}
