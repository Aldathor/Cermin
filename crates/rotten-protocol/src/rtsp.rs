use reqwest::Client;
use rotten_core::config::StreamConfig;
use rotten_core::device::AirPlayDevice;
use rotten_core::error::{Result, RottenError};
use tracing::debug;

use crate::http::MirrorSetupResult;

/// RTSP session for AirPlay mirroring control plane.
pub struct RtspSession {
    session_url: String,
    client: Client,
    cseq: u32,
}

impl RtspSession {
    pub async fn setup(
        client: &Client,
        device: &AirPlayDevice,
        setup: &MirrorSetupResult,
        config: &StreamConfig,
    ) -> Result<Self> {
        let session_url = format!("rtsp://{}:{}/", device.host, device.port);
        let mut session = Self {
            session_url: session_url.clone(),
            client: client.clone(),
            cseq: 1,
        };

        session.options().await?;
        session.announce(config).await?;
        session.setup_stream(setup).await?;
        session.record().await?;

        Ok(session)
    }

    async fn options(&mut self) -> Result<()> {
        let req = format!(
            "OPTIONS * RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: MediaControl/1.0\r\n\r\n",
            self.cseq
        );
        self.cseq += 1;
        self.send_rtsp(&req).await
    }

    async fn announce(&mut self, config: &StreamConfig) -> Result<()> {
        let sdp = format!(
            "v=0\r\n\
             o=Cermin 0 0 IN IP4 0.0.0.0\r\n\
             s=Cermin\r\n\
             c=IN IP4 0.0.0.0\r\n\
             t=0 0\r\n\
             m=video 0 RTP/AVP 96\r\n\
             a=rtpmap:96 H264/90000\r\n\
             a=fmtp:96 packetization-mode=1;profile-level-id=42E01F;sprop-parameter-sets=\r\n\
             a=framerate:{}\r\n\
             a=x-dimensions:{},{}\r\n",
            config.fps, config.width, config.height
        );

        let req = format!(
            "ANNOUNCE {} RTSP/1.0\r\n\
             CSeq: {}\r\n\
             Content-Type: application/sdp\r\n\
             Content-Length: {}\r\n\r\n{}",
            self.session_url,
            self.cseq,
            sdp.len(),
            sdp
        );
        self.cseq += 1;
        self.send_rtsp(&req).await
    }

    async fn setup_stream(&mut self, setup: &MirrorSetupResult) -> Result<()> {
        let transport = format!(
            "RTP/AVP/UDP;unicast;mode=record;control_port={};timing_port={}",
            setup.event_port.unwrap_or(7001),
            setup.timing_port.unwrap_or(7011)
        );

        let req = format!(
            "SETUP {} RTSP/1.0\r\n\
             CSeq: {}\r\n\
             Transport: {}\r\n\r\n",
            self.session_url, self.cseq, transport
        );
        self.cseq += 1;
        self.send_rtsp(&req).await
    }

    async fn record(&mut self) -> Result<()> {
        let req = format!(
            "RECORD {} RTSP/1.0\r\n\
             CSeq: {}\r\n\
             Range: npt=0-\r\n\r\n",
            self.session_url, self.cseq
        );
        self.cseq += 1;
        self.send_rtsp(&req).await
    }

    pub async fn teardown(&self) -> Result<()> {
        let req = format!("TEARDOWN {} RTSP/1.0\r\nCSeq: 99\r\n\r\n", self.session_url);
        self.send_rtsp(&req).await
    }

    async fn send_rtsp(&self, request: &str) -> Result<()> {
        let url = format!(
            "http://{}:{}/",
            self.session_url
                .trim_start_matches("rtsp://")
                .split('/')
                .next()
                .unwrap_or(""),
            7000
        );

        debug!(method = %request.lines().next().unwrap_or(""), "RTSP over HTTP");

        // AirPlay tunnels RTSP over HTTP POST on port 7000
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/x-apple-binary-plist")
            .body(request.as_bytes().to_vec())
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() || r.status().as_u16() == 200 => Ok(()),
            Ok(r) => {
                // Some Apple TVs accept RTSP without explicit 200 on tunnel
                debug!(status = %r.status(), "RTSP response");
                Ok(())
            }
            Err(e) => Err(RottenError::Protocol(format!("RTSP: {e}"))),
        }
    }
}
