use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Result, RottenError};

/// Video stream parameters for mirroring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate_kbps: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MirrorCipherMode {
    AesCtr,
    #[default]
    ChaCha,
}

/// Full mirror session configuration.
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    pub stream: StreamConfig,
    pub pin: Option<String>,
    pub force_pair: bool,
    pub test_mode: bool,
    pub audio: bool,
    pub hw_accel: HwAccel,
    pub credentials_path: PathBuf,
    pub display_index: Option<u32>,
    pub virtual_display_only: bool,
    /// Send VCL frames without encryption (debug / isolate cipher issues).
    pub no_encrypt: bool,
    pub cipher: MirrorCipherMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HwAccel {
    #[default]
    Auto,
    Nvenc,
    Vaapi,
    None,
}

impl HwAccel {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "nvenc" => Self::Nvenc,
            "vaapi" => Self::Vaapi,
            "none" => Self::None,
            _ => Self::Auto,
        }
    }
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            stream: StreamConfig::default(),
            pin: None,
            force_pair: false,
            test_mode: false,
            audio: false,
            hw_accel: HwAccel::Auto,
            credentials_path: default_credentials_path(),
            display_index: None,
            virtual_display_only: false,
            no_encrypt: false,
            cipher: MirrorCipherMode::default(),
        }
    }
}

/// Stored pairing credentials for a device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCredentials {
    pub device_id: String,
    pub identifier: String,
    pub public_key: Vec<u8>,
    pub private_key: Vec<u8>,
    /// Accessory Ed25519 public key (32 bytes).
    #[serde(default)]
    pub server_public_key: Vec<u8>,
    /// True when credentials came from HAP pair-setup (AirPlay 2).
    #[serde(default)]
    pub hap: bool,
    /// Accessory pairing identifier from HAP M6.
    #[serde(default)]
    pub accessory_id: Vec<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CredentialsStore {
    pub devices: Vec<DeviceCredentials>,
}

impl CredentialsStore {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            ensure_private_dir(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        write_private_file(path, data.as_bytes())?;
        Ok(())
    }

    pub fn get(&self, device_id: &str) -> Option<&DeviceCredentials> {
        self.devices.iter().find(|d| d.device_id == device_id)
    }

    pub fn upsert(&mut self, creds: DeviceCredentials) {
        if let Some(idx) = self
            .devices
            .iter()
            .position(|d| d.device_id == creds.device_id)
        {
            self.devices[idx] = creds;
        } else {
            self.devices.push(creds);
        }
    }

    pub fn remove(&mut self, device_id: &str) {
        self.devices.retain(|d| d.device_id != device_id);
    }
}

pub fn default_credentials_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cermin")
        .join("credentials.json")
}

pub fn resolve_credentials_path(path: Option<PathBuf>) -> PathBuf {
    path.unwrap_or_else(default_credentials_path)
}

#[cfg(unix)]
fn ensure_private_dir(path: &std::path::Path) -> Result<()> {
    use std::fs::DirBuilder;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    if path.exists() {
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(path, perms)?;
        return Ok(());
    }

    DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_dir(path: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    Ok(())
}

fn write_private_file(path: &std::path::Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(data)?;
        return Ok(());
    }

    #[cfg(not(unix))]
    {
        std::fs::write(path, data)?;
        Ok(())
    }
}

pub fn parse_device_id_from_host(host: &str) -> Result<String> {
    if host.is_empty() {
        return Err(RottenError::DeviceNotFound("empty host".into()));
    }
    Ok(host.to_string())
}
