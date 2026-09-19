//! FairPlay SAP hash via external `fpsap-helper` (GPL-3.0, `tools/fpsap-helper`).

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use rotten_core::debug_log::agent_log;
use rotten_core::error::{Result, RottenError};

include!(concat!(env!("OUT_DIR"), "/fpsap_embed.rs"));

static EXTRACTED: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 20-byte WB-AES hash from m2 bytes 14..142 via fpsap-helper.
pub fn fpsap_hash_from_setup1(setup1: &[u8]) -> Result<[u8; 20]> {
    if setup1.len() != 142 {
        return Err(RottenError::Crypto(format!(
            "fp-setup step1 response must be 142 bytes, got {}",
            setup1.len()
        )));
    }
    let (helper, source) = locate_fpsap_helper()?;
    // #region agent log
    agent_log(
        "fpsap_helper.rs:fpsap_hash_from_setup1",
        "fpsap helper resolved",
        "R",
        serde_json::json!({
            "source": source,
            "helper": helper.display().to_string(),
            "embeddedLen": FPSAP_HELPER_BYTES.len(),
        }),
    );
    // #endregion

    let hex_in = hex::encode(setup1);

    let mut child = Command::new(&helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RottenError::Crypto(format!("spawn {}: {e}", helper.display())))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(hex_in.as_bytes())
            .map_err(|e| RottenError::Crypto(format!("fpsap-helper stdin: {e}")))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| RottenError::Crypto(format!("fpsap-helper wait: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RottenError::Crypto(format!(
            "fpsap-helper failed ({}): {}",
            output.status,
            stderr.trim()
        )));
    }

    let hash_hex = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_ascii_lowercase();
    let bytes = hex::decode(&hash_hex)
        .map_err(|e| RottenError::Crypto(format!("fpsap-helper output: {e}")))?;
    if bytes.len() != 20 {
        return Err(RottenError::Crypto(format!(
            "fpsap-helper returned {} bytes, expected 20",
            bytes.len()
        )));
    }
    let mut hash = [0u8; 20];
    hash.copy_from_slice(&bytes);
    Ok(hash)
}

fn locate_fpsap_helper() -> Result<(PathBuf, &'static str)> {
    let names = ["fpsap-helper.exe", "fpsap-helper"];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Ok((candidate, "external-sibling"));
                }
            }
        }
    }
    for name in names {
        if let Some(path) = find_in_path(name) {
            return Ok((path, "external-path"));
        }
    }
    if !FPSAP_HELPER_BYTES.is_empty() {
        return extract_embedded_helper().map(|p| (p, "embedded"));
    }
    Err(RottenError::Crypto(
        "fpsap-helper not found — place fpsap-helper (or fpsap-helper.exe) next to cermin, on PATH, or build via scripts/build-windows.sh"
            .into(),
    ))
}

fn extract_embedded_helper() -> Result<PathBuf> {
    let mut guard = EXTRACTED
        .lock()
        .map_err(|_| RottenError::Crypto("fpsap-helper cache lock poisoned".into()))?;
    if let Some(path) = guard.as_ref() {
        if path.is_file() {
            return Ok(path.clone());
        }
    }

    let helper_name = if cfg!(windows) {
        "fpsap-helper.exe"
    } else {
        "fpsap-helper"
    };

    let base = std::env::temp_dir().join("cermin-fpsap");
    std::fs::create_dir_all(&base)
        .map_err(|e| RottenError::Crypto(format!("fpsap-helper extract dir: {e}")))?;
    let path = base.join(helper_name);

    let needs_write = match std::fs::read(&path) {
        Ok(existing) => existing != FPSAP_HELPER_BYTES,
        Err(_) => true,
    };
    if needs_write {
        std::fs::write(&path, FPSAP_HELPER_BYTES)
            .map_err(|e| RottenError::Crypto(format!("fpsap-helper extract write: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)
                .map_err(|e| RottenError::Crypto(format!("fpsap-helper chmod: {e}")))?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms)
                .map_err(|e| RottenError::Crypto(format!("fpsap-helper chmod: {e}")))?;
        }
    }

    *guard = Some(path.clone());
    Ok(path)
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}
