//! Windows Hybrid-GPU (integrated + discrete) helpers for Desktop Duplication.
//!
//! Windows does not allow `IDXGIOutput1::DuplicateOutput` to run against the
//! discrete GPU of a Microsoft Hybrid system; the call fails with
//! `DXGI_ERROR_UNSUPPORTED` by design. The documented workaround is to run the
//! application on the integrated GPU. Windows 10 build 17093 and later expose a
//! per-application preference under
//! `HKCU\Software\Microsoft\DirectX\UserGpuPreferences`; this module reads and
//! merges that value while preserving entries for other applications.
//!
//! The preference only takes effect for a new process, so the caller should
//! tell the user that a restart enables the faster DXGI backend.

/// Registry subkey that stores per-application GPU preferences.
pub(crate) const USER_GPU_PREFERENCES: &str = r"Software\Microsoft\DirectX\UserGpuPreferences";

/// `GpuPreference` value that selects the power-saving (integrated) GPU.
const INTEGRATED_GPU: &str = "GpuPreference=1";
/// `GpuPreference` value that selects the high-performance (discrete) GPU.
const HIGH_PERFORMANCE_GPU: &str = "GpuPreference=2";

/// Merge the requested GPU preference into an existing multi-token value.
///
/// The stored format is `Token;Token;...`. Unknown tokens are preserved, any
/// previous `GpuPreference` token is replaced, and a trailing `;` is kept so
/// later writers can append without corrupting the value. Returns `None` when
/// the current value already selects the requested GPU.
pub(crate) fn merged_preference(current: Option<&str>, integrated: bool) -> Option<String> {
    let wanted = if integrated {
        INTEGRATED_GPU
    } else {
        HIGH_PERFORMANCE_GPU
    };
    let mut kept: Vec<&str> = Vec::new();
    let mut current_value: Option<&str> = None;
    for token in current.unwrap_or("").split(';') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if let Some((name, value)) = token.split_once('=') {
            if name.trim().eq_ignore_ascii_case("GpuPreference") {
                current_value = Some(value.trim());
                continue;
            }
        }
        kept.push(token);
    }
    if current_value.is_some_and(|value| value.eq_ignore_ascii_case(wanted_value(wanted))) {
        return None;
    }
    kept.push(wanted);
    Some(format!("{};", kept.join(";")))
}

fn wanted_value(wanted: &str) -> &str {
    wanted
        .split_once('=')
        .map(|(_, value)| value)
        .unwrap_or(wanted)
}

/// Ask Windows to run the Cermin executables on the integrated GPU.
///
/// Applies to this process and to sibling `cermin.exe`/`cermin-cli.exe`, so the
/// GUI and CLI do not each need their own failed attempt. Returns `Ok(true)`
/// when at least one preference was written, `Ok(false)` when everything was
/// already set. The change applies the next time a process starts.
#[cfg(target_os = "windows")]
pub(crate) fn ensure_integrated_gpu_preference() -> anyhow::Result<bool> {
    use anyhow::{Context, anyhow};

    let exe = std::env::current_exe().context("reading the current executable path")?;
    let exe = exe
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("the executable path is not valid Unicode"))?;

    let mut updated = ensure_preference_for(&exe)?;
    if let Some(directory) = std::path::Path::new(&exe).parent() {
        for name in ["cermin.exe", "cermin-cli.exe"] {
            let sibling = directory.join(name);
            if sibling.is_file() {
                if let Some(sibling) = sibling.to_str() {
                    updated |= ensure_preference_for(sibling)?;
                }
            }
        }
    }
    Ok(updated)
}

#[cfg(target_os = "windows")]
fn ensure_preference_for(exe: &str) -> anyhow::Result<bool> {
    let current = registry::read_app_preference(USER_GPU_PREFERENCES, exe)?;
    let Some(merged) = merged_preference(current.as_deref(), true) else {
        return Ok(false);
    };
    registry::write_app_preference(USER_GPU_PREFERENCES, exe, &merged)?;
    Ok(true)
}

#[cfg(target_os = "windows")]
mod registry {
    use windows::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR,
    };
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
        RegCloseKey, RegCreateKeyExW, RegGetValueW, RegSetValueExW,
    };
    use windows::core::PCWSTR;

    /// Owns a registry key handle.
    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn key_error(what: &str, status: WIN32_ERROR) -> anyhow::Error {
        anyhow::anyhow!("{what} failed with Windows error {}", status.0)
    }

    /// Read a `REG_SZ` value under `HKCU\<root>`, or `None` when it is absent.
    pub(super) fn read_app_preference(root: &str, name: &str) -> anyhow::Result<Option<String>> {
        let root_w = wide(root);
        let name_w = wide(name);
        let mut size = 0u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(root_w.as_ptr()),
                PCWSTR(name_w.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut size),
            )
        };
        if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(key_error("reading the GPU preference", status));
        }
        if size == 0 {
            return Ok(None);
        }

        let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
        let mut written = size;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(root_w.as_ptr()),
                PCWSTR(name_w.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut written),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(key_error("reading the GPU preference", status));
        }
        let units = (written as usize / 2).min(buffer.len());
        let text = String::from_utf16_lossy(&buffer[..units]);
        Ok(Some(text.trim_end_matches('\0').to_owned()))
    }

    /// Create or update a `REG_SZ` value under `HKCU\<root>`.
    pub(super) fn write_app_preference(root: &str, name: &str, value: &str) -> anyhow::Result<()> {
        let root_w = wide(root);
        let name_w = wide(name);
        let mut raw = HKEY::default();
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(root_w.as_ptr()),
                0,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut raw,
                None,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(key_error("opening the GPU preference key", status));
        }
        let key = Key(raw);

        let mut data: Vec<u8> = Vec::with_capacity((value.len() + 1) * 2);
        for unit in value.encode_utf16() {
            data.extend_from_slice(&unit.to_le_bytes());
        }
        data.extend_from_slice(&0u16.to_le_bytes());

        let status = unsafe {
            RegSetValueExW(
                key.0,
                PCWSTR(name_w.as_ptr()),
                0,
                REG_SZ,
                Some(data.as_slice()),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(key_error("writing the GPU preference", status));
        }
        Ok(())
    }

    /// Remove a value; used by tests so they leave no trace behind.
    #[cfg(test)]
    pub(super) fn delete_app_preference(root: &str, name: &str) -> anyhow::Result<()> {
        use windows::Win32::System::Registry::RegDeleteValueW;

        let root_w = wide(root);
        let name_w = wide(name);
        let mut raw = HKEY::default();
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(root_w.as_ptr()),
                0,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut raw,
                None,
            )
        };
        if status != ERROR_SUCCESS {
            anyhow::bail!(
                "opening the scratch key failed with Windows error {}",
                status.0
            );
        }
        let key = Key(raw);
        let status = unsafe { RegDeleteValueW(key.0, PCWSTR(name_w.as_ptr())) };
        if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
            return Err(key_error("deleting the scratch value", status));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_preserves_other_tokens_and_replaces_a_previous_choice() {
        assert_eq!(
            merged_preference(None, true).as_deref(),
            Some("GpuPreference=1;")
        );
        assert_eq!(merged_preference(Some("GpuPreference=1;"), true), None);
        assert_eq!(
            merged_preference(Some("GpuPreference=2;"), true).as_deref(),
            Some("GpuPreference=1;")
        );
        assert_eq!(
            merged_preference(Some("Other=5;GpuPreference=2;"), true).as_deref(),
            Some("Other=5;GpuPreference=1;")
        );
        assert_eq!(
            merged_preference(Some("garbage"), true).as_deref(),
            Some("garbage;GpuPreference=1;")
        );
        assert_eq!(merged_preference(Some("GpuPreference=2"), false), None);
        assert_eq!(
            merged_preference(None, false).as_deref(),
            Some("GpuPreference=2;")
        );
        // Whitespace and case are tolerated in an existing value.
        assert_eq!(merged_preference(Some(" gpuPreference = 1 ;"), true), None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn registry_round_trip_in_a_scratch_key() {
        const SCRATCH: &str = r"Software\CerminCaptureTest";
        const NAME: &str = "cermin-capture-roundtrip";
        // A previous crashed run could leave the value behind.
        registry::delete_app_preference(SCRATCH, NAME).expect("initial cleanup");

        assert_eq!(
            registry::read_app_preference(SCRATCH, NAME).expect("read missing"),
            None
        );
        registry::write_app_preference(SCRATCH, NAME, "GpuPreference=2;").expect("write");
        assert_eq!(
            registry::read_app_preference(SCRATCH, NAME)
                .expect("read")
                .as_deref(),
            Some("GpuPreference=2;")
        );
        registry::write_app_preference(SCRATCH, NAME, "Other=7;GpuPreference=1;").expect("rewrite");
        assert_eq!(
            registry::read_app_preference(SCRATCH, NAME)
                .expect("read")
                .as_deref(),
            Some("Other=7;GpuPreference=1;")
        );

        registry::delete_app_preference(SCRATCH, NAME).expect("cleanup");
        assert_eq!(
            registry::read_app_preference(SCRATCH, NAME).expect("read after cleanup"),
            None
        );
    }
}
