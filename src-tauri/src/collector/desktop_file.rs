//! Channel B — Desktop `%APPDATA%\Claude\plan-usage-history.json`.
//!
//! The Desktop app appends one sample every ~900s while it's running:
//! `{version:2, samples:[{t: epoch_ms, org: uuid, u: {fh, sd, xu?}}]}`.
//! `fh` = 5h %, `sd` = 7d %. Traps (實測 2026-08-27):
//!   - `"u":{}` empty object = "couldn't read at that moment" — SKIP, never 0.
//!   - the file is a 30-day rolling window: old samples silently drop off,
//!     so long-term retention is OUR job (backfill into the ledger, M0/M1).
//!   - two orgs interleave freely (8 switches observed) — which org is
//!     "current" must be matched against channel C's organizationUuid at
//!     read time, never hard-coded.
//!   - Desktop writes the file frequently; a half-written JSON parse failure
//!     is retried next round, not treated as schema drift.

use anyhow::{Context, Result};
use serde::Deserialize;

use super::model::{from_epoch_ms, SeatConfidence, Source, TruthSample};

#[derive(Debug, Clone, Deserialize)]
struct HistoryFile {
    #[serde(default)]
    version: Option<i64>,
    #[serde(default)]
    samples: Vec<RawSample>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawSample {
    t: i64,
    #[serde(default)]
    org: Option<String>,
    #[serde(default)]
    u: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct DesktopReading {
    /// Parser version gate: Some(n) when the file declares a version ≠ 2 —
    /// caller should log a drift warning but we still parse best-effort.
    pub unexpected_version: Option<i64>,
    /// Normalized samples in file order (strictly increasing `t` observed).
    pub samples: Vec<TruthSample>,
}

/// v1.8.8（D87）：Claude Desktop 是 MSIX 打包，它寫的 `%APPDATA%\Claude\…` 實際落在
/// `%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude\`；只有跟它同一個 silo
/// 的程序（例如從 Claude Desktop 裡開的 dev 實例）才會在 `%APPDATA%` 看到它。
/// 安裝版在大樓外跑，真實層的 `%APPDATA%\Claude\` 根本沒有這個檔——2026-09-21～25
/// Desktop 管道就這樣靜靜死了四天、一行 log 都沒有。兩處都找，取最新改動的那一份。
/// `bridge-state.json` 同一個坑（bridge.rs 也走這裡）。
pub fn desktop_config_file(name: &str) -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        candidates.push(std::path::PathBuf::from(appdata).join("Claude").join(name));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let packages = std::path::PathBuf::from(local).join("Packages");
        if let Ok(rd) = std::fs::read_dir(&packages) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with("Claude_") {
                    candidates.push(
                        e.path().join("LocalCache").join("Roaming").join("Claude").join(name),
                    );
                }
            }
        }
    }
    candidates
        .into_iter()
        .filter_map(|p| {
            std::fs::metadata(&p)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| (t, p))
        })
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

fn history_path() -> Result<std::path::PathBuf> {
    desktop_config_file("plan-usage-history.json").context(
        "plan-usage-history.json not found in %APPDATA%\\Claude nor in the Claude Desktop MSIX LocalCache",
    )
}

pub fn exists() -> bool {
    history_path().is_ok()
}

/// Where the history file was found (for the startup log) — `None` = nowhere.
pub fn resolved_path() -> Option<std::path::PathBuf> {
    history_path().ok()
}

/// Read and normalize the whole file. Each raw sample yields up to two
/// `TruthSample`s (five_hour + seven_day). `min_t_exclusive` skips samples
/// at-or-below a previously ingested watermark (incremental absorption).
pub fn read(min_t_exclusive: Option<i64>) -> Result<DesktopReading> {
    let path = history_path()?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let parsed: HistoryFile =
        serde_json::from_str(&text).context("parse plan-usage-history.json")?;

    let mut out = DesktopReading {
        unexpected_version: match parsed.version {
            Some(2) => None,
            other => other,
        },
        samples: Vec::new(),
    };

    for raw in parsed.samples {
        if let Some(min_t) = min_t_exclusive {
            if raw.t <= min_t {
                continue;
            }
        }
        // Empty `u` = that moment's read failed at the origin. Skip.
        if raw.u.is_empty() {
            continue;
        }
        let Some(fetched_at) = from_epoch_ms(raw.t) else {
            continue;
        };
        let raw_json = serde_json::json!({ "t": raw.t, "org": raw.org, "u": raw.u });

        for (key, kind) in [("fh", "five_hour"), ("sd", "seven_day")] {
            let Some(percent) = raw.u.get(key).and_then(|v| v.as_f64()) else {
                continue; // partial objects tolerated; unknown keys (xu) stay in raw
            };
            out.samples.push(TruthSample {
                account_uuid: None, // org-only file; account is inferred later
                org_uuid: raw.org.clone(),
                seat_confidence: SeatConfidence::Inferred,
                limit_kind: kind.to_string(),
                scope: None,
                percent: percent as f32,
                resets_at: None,
                source: Source::DesktopHistory,
                fetched_at,
                raw_json: raw_json.clone(),
            });
        }
    }

    Ok(out)
}
