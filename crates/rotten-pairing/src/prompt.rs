use rotten_core::error::{Result, RottenError};

use crate::credentials::format_pin;

/// Read a PIN from stdin after the receiver displays it on screen.
pub fn prompt_pin_interactive() -> Result<String> {
    eprintln!();
    eprintln!("========================================================");
    eprintln!("  Enter the AirPlay code shown on your TV (4 or 6 digits)");
    eprintln!("========================================================");
    eprintln!();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| RottenError::Pairing(format!("read PIN: {e}")))?;
    format_pin(line.trim())
}

/// Async wrapper that does not block the tokio runtime.
pub async fn prompt_pin_interactive_async() -> Result<String> {
    tokio::task::spawn_blocking(prompt_pin_interactive)
        .await
        .map_err(|e| RottenError::Pairing(format!("prompt task: {e}")))?
}
