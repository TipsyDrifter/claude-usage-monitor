//! Normalized types shared by the four collection channels (規格書 §4).
//!
//! A `TruthSample` is one observed data point of one rate limit, exactly the
//! shape of a `truth_samples` row (D48 機械項 11). `limit_kind` is an OPEN
//! string set (D46) — we store whatever the source calls the limit and only
//! merge synonyms (`session`≒`five_hour`, `weekly_all`≒`seven_day`) at the
//! view-model layer, never at ingestion.

use chrono::{DateTime, TimeZone, Utc};
use serde::Serialize;

/// Which channel produced a sample. Serialized into the `source` column and
/// shown in freshness stamps, so the names are user-facing and stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// Channel A — `claude -p "/usage"` probe.
    Probe,
    /// Channel B — Desktop `plan-usage-history.json`.
    DesktopHistory,
    /// Channel C — CLI `cachedUsageUtilization`.
    CliCache,
    /// Channel D — statusline forwarder (加分項, not wired in M0).
    #[allow(dead_code)]
    Statusline,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Probe => "probe",
            Source::DesktopHistory => "desktop-history",
            Source::CliCache => "cli-cache",
            Source::Statusline => "statusline",
        }
    }

    /// Short label for freshness stamps in tooltips / notes.
    pub fn label(self) -> &'static str {
        match self {
            Source::Probe => "probe",
            Source::DesktopHistory => "desktop",
            Source::CliCache => "cli",
            Source::Statusline => "statusline",
        }
    }
}

/// Seat attribution confidence (D48 第 4 點).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
pub enum SeatConfidence {
    Exact,
    Inferred,
    Confirmed,
    Conflict,
}

impl SeatConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            SeatConfidence::Exact => "exact",
            SeatConfidence::Inferred => "inferred",
            SeatConfidence::Confirmed => "confirmed",
            SeatConfidence::Conflict => "conflict",
        }
    }
}

/// One observed data point of one rate limit — a `truth_samples` row.
#[derive(Debug, Clone, Serialize)]
pub struct TruthSample {
    /// Account UUID when the source carries one (channel A/C), else None.
    pub account_uuid: Option<String>,
    /// Organization UUID when the source carries one (channel B/C), else None.
    pub org_uuid: Option<String>,
    pub seat_confidence: SeatConfidence,
    /// Open string set: `session` / `weekly_all` / `weekly_scoped` /
    /// `five_hour` / `seven_day` / ... — stored verbatim from the source.
    pub limit_kind: String,
    /// Scope label for scoped limits (e.g. "Fable"), None otherwise.
    pub scope: Option<String>,
    pub percent: f32,
    /// UTC. None when the source doesn't report one (B) or the limit is idle.
    pub resets_at: Option<DateTime<Utc>>,
    pub source: Source,
    /// When this value was true at the origin (NOT when we read the file).
    pub fetched_at: DateTime<Utc>,
    /// Original record, for drift forensics.
    pub raw_json: serde_json::Value,
}

/// Convert an epoch-milliseconds value into a UTC timestamp.
pub fn from_epoch_ms(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}

/// "3 分前" / "2 小時前" style age stamp used in freshness notes.
pub fn age_label(fetched_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let mins = (now - fetched_at).num_minutes();
    if mins < 1 {
        "剛剛".to_string()
    } else if mins < 60 {
        format!("{mins} 分前")
    } else if mins < 48 * 60 {
        format!("{} 小時前", mins / 60)
    } else {
        format!("{} 天前", mins / (24 * 60))
    }
}
