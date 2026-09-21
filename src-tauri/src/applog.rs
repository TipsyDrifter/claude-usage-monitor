//! Diagnostic log for the collector subsystem.
//!
//! Lifted out of the (now removed) scraper module in M0 — the log helpers
//! are collector-agnostic and `reveal_log_folder` depends on them. Each line
//! is a JSON object `{ts, phase, info}` appended to
//! `<app_data_dir>/collector.log`, with size-capped rotation that keeps the
//! most recent tail.

use chrono::Utc;
use tauri::{AppHandle, Manager};

// Rotate when the log grows past this size...
const LOG_ROTATE_BYTES: u64 = 2_000_000;
// ...and keep this much of the tail when we do.
const LOG_KEEP_BYTES: usize = 1_000_000;

/// Append a log line to `<app_data_dir>/collector.log`. Best-effort — never
/// fails the caller.
pub async fn write_log(app: &AppHandle, phase: &str, info: serde_json::Value) {
    let Ok(dir) = app.path().app_data_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let log_path = dir.join("collector.log");

    // Roll the log when it grows past LOG_ROTATE_BYTES: keep the most
    // recent ~LOG_KEEP_BYTES of the tail (aligned to a newline boundary
    // so we never start mid-line) instead of nuking everything.
    if let Ok(meta) = std::fs::metadata(&log_path) {
        if meta.len() > LOG_ROTATE_BYTES {
            if let Ok(content) = std::fs::read(&log_path) {
                let drop_until = content.len().saturating_sub(LOG_KEEP_BYTES);
                let start = content[drop_until..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map(|p| drop_until + p + 1)
                    .unwrap_or(drop_until);
                let header = b"--- log rolled, kept last ~1 MB ---\n";
                let mut buf = Vec::with_capacity(header.len() + content.len() - start);
                buf.extend_from_slice(header);
                buf.extend_from_slice(&content[start..]);
                let _ = std::fs::write(&log_path, buf);
            }
        }
    }

    let line = serde_json::json!({
        "ts": Utc::now().to_rfc3339(),
        "phase": phase,
        "info": info,
    });
    let line_str = format!("{}\n", line);

    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = f.write_all(line_str.as_bytes());
    }
}

/// Returns the path to collector.log so callers (eg. Settings UI) can reveal
/// it in Explorer.
pub fn log_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("collector.log"))
}
