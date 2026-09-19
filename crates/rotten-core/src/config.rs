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
        ensure_private_dir(credentials_parent(path))?;
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

fn credentials_parent(path: &std::path::Path) -> &std::path::Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."))
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
    use std::os::unix::fs::DirBuilderExt;

    if path.exists() {
        // A custom credentials path may share a directory with other files.
        // Protect the credential file without changing that directory's access.
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
    use std::io::Write;

    // Create beside the destination so replacement stays on the same filesystem.
    // NamedTempFile removes an unfinished write if writing or persistence fails.
    let mut file = tempfile::NamedTempFile::new_in(credentials_parent(path))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(data)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub fn parse_device_id_from_host(host: &str) -> Result<String> {
    if host.is_empty() {
        return Err(RottenError::DeviceNotFound("empty host".into()));
    }
    Ok(host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_replaces_credentials_without_leaving_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let mut store = CredentialsStore::default();
        store.save(&path).unwrap();
        store.upsert(DeviceCredentials {
            device_id: "test-tv".into(),
            identifier: "test-client".into(),
            public_key: vec![1; 32],
            private_key: vec![2; 32],
            server_public_key: vec![],
            hap: false,
            accessory_id: vec![],
        });
        store.save(&path).unwrap();
        assert_eq!(
            CredentialsStore::load(&path).unwrap().devices[0].device_id,
            "test-tv"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_replacement_preserves_destination_and_cleans_up_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing-directory");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep.txt"), "keep").unwrap();
        assert!(CredentialsStore::default().save(&path).is_err());
        assert_eq!(
            std::fs::read_to_string(path.join("keep.txt")).unwrap(),
            "keep"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn bare_filename_uses_current_directory() {
        assert_eq!(
            credentials_parent(std::path::Path::new("credentials.json")),
            std::path::Path::new(".")
        );
    }

    #[cfg(windows)]
    #[test]
    fn failed_replacement_of_locked_file_preserves_saved_credentials() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        CredentialsStore::default().save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        assert!(write_private_file(&path, b"replacement").is_err());
        drop(locked);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn protects_file_without_changing_existing_parent_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        CredentialsStore::default().save(&path).unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }
}
