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

fn history_path() -> Result<std::path::PathBuf> {
    let appdata = std::env::var("APPDATA").context("APPDATA not set")?;
    Ok(std::path::PathBuf::from(appdata)
        .join("Claude")
        .join("plan-usage-history.json"))
}

pub fn exists() -> bool {
    history_path().map(|p| p.exists()).unwrap_or(false)
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
