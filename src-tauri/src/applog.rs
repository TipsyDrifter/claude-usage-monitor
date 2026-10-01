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
    append_line(&dir, phase, info);
}

/// 同步版：寫一行到 `<dir>/collector.log`（含輪替）。`write_log` 與 log crate 的橋都走這裡。
fn append_line(dir: &std::path::Path, phase: &str, info: serde_json::Value) {
    let _ = std::fs::create_dir_all(dir);
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

// =============================================================================
// v1.8.13（D91 T-08）：把 log crate 接進 collector.log。
//
// 發行版沒有主控台，`log::warn!/error!`（設定檔讀不到改用預設、自啟寫不進登錄檔、通知失敗、
// 門鈴伺服器停掉…約 14 處）以前全部無處可去——畫面與 log 都沒痕跡。與其一處一處改成
// `write_log`（要 AppHandle、要 async），不如讓 log crate 本身多一個出口：warn 以上一律
// 寫進 collector.log（phase＝log-warn／log-error），其餘照舊交給 env_logger（開發時看 stderr）。
// =============================================================================

static LOG_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// `setup` 一開始呼叫：告訴 log 橋 collector.log 在哪。之前的 warn 只會到 stderr。
pub fn set_log_dir(dir: std::path::PathBuf) {
    let _ = LOG_DIR.set(dir);
}

struct Bridge {
    inner: env_logger::Logger,
}

impl log::Log for Bridge {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        meta.level() <= log::Level::Warn || self.inner.enabled(meta)
    }

    fn log(&self, record: &log::Record) {
        if self.inner.matches(record) {
            self.inner.log(record);
        }
        if record.level() <= log::Level::Warn {
            if let Some(dir) = LOG_DIR.get() {
                let phase = if record.level() == log::Level::Error { "log-error" } else { "log-warn" };
                append_line(
                    dir,
                    phase,
                    serde_json::json!({
                        "target": record.target(),
                        "msg": record.args().to_string(),
                    }),
                );
            }
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// 取代 `env_logger::try_init()`：RUST_LOG 照樣管 stderr，warn 以上另外進 collector.log。
pub fn init_logger() {
    let inner = env_logger::Builder::from_default_env().build();
    let max = inner.filter().max(log::LevelFilter::Warn);
    if log::set_boxed_logger(Box::new(Bridge { inner })).is_ok() {
        log::set_max_level(max);
    }
}

/// Returns the path to collector.log so callers (eg. Settings UI) can reveal
/// it in Explorer.
pub fn log_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("collector.log"))
}

#[cfg(test)]
mod bridge_tests {
    //! v1.8.13（D91 T-08）：log::warn!/error! 要進 collector.log，info 不進。
    #[test]
    fn warn_and_error_reach_collector_log() {
        let dir = std::env::temp_dir().join(format!("cum-logbridge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        super::set_log_dir(dir.clone());
        super::init_logger();
        log::info!("只到 stderr 的一行");
        log::warn!("settings: failed to open store, using defaults: boom");
        log::error!("Webhook server stopped: bind 127.0.0.1:17819");
        let text = std::fs::read_to_string(dir.join("collector.log")).unwrap();
        let lines: Vec<serde_json::Value> =
            text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        // 同一個測試程序裡別的測試也可能 warn（全域 logger），只看自己寫的那幾行。
        let msg = |l: &serde_json::Value| l["info"]["msg"].as_str().unwrap_or("").to_string();
        assert!(!lines.iter().any(|l| msg(l).contains("只到 stderr")), "info 不該進檔：{text}");
        let w = lines.iter().find(|l| msg(l).contains("failed to open store")).expect("warn 要在");
        assert_eq!(w["phase"], "log-warn");
        let e = lines.iter().find(|l| msg(l).contains("Webhook server stopped")).expect("error 要在");
        assert_eq!(e["phase"], "log-error");
    }
}
