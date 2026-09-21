//! Channel C — `~/.claude.json` → `cachedUsageUtilization` + `oauthAccount`.
//!
//! This file is READ-ONLY for us, and we only ever look at those two keys —
//! never `.credentials.json` (架構紅線), never the rest of the config.
//!
//! C is the only channel with a full structured picture (every limit bucket
//! incl. the Fable line, ISO resets_at, severity) AND the seat anchor
//! (accountUuid × organizationUuid). Its trap: `fetchedAtMs` can be DAYS old
//! — TUI traffic does not refresh this cache, only `/usage` does (O5) — so
//! every read must carry the origin timestamp and the caller decides how
//! much to trust it.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::model::{from_epoch_ms, SeatConfidence, Source, TruthSample};

/// The two keys we read, everything else in ~/.claude.json is ignored.
#[derive(Debug, Clone, Deserialize)]
struct ClaudeJson {
    #[serde(rename = "cachedUsageUtilization")]
    cached: Option<serde_json::Value>,
    #[serde(rename = "oauthAccount")]
    oauth: Option<OauthAccount>,
}

/// Seat anchor. All fields optional-by-default so schema drift never breaks
/// the parse — a missing field is data, not an error.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OauthAccount {
    #[serde(rename = "accountUuid")]
    pub account_uuid: Option<String>,
    #[serde(rename = "organizationUuid")]
    pub organization_uuid: Option<String>,
    #[serde(rename = "organizationName")]
    pub organization_name: Option<String>,
    #[serde(rename = "organizationType")]
    pub organization_type: Option<String>,
    #[serde(rename = "organizationRateLimitTier")]
    pub organization_rate_limit_tier: Option<String>,
    #[serde(rename = "seatTier")]
    pub seat_tier: Option<String>,
    #[serde(rename = "emailAddress")]
    pub email_address: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CliCacheReading {
    pub oauth: Option<OauthAccount>,
    /// When the cache was written at the origin — may be days old.
    pub fetched_at: Option<DateTime<Utc>>,
    pub samples: Vec<TruthSample>,
}

fn claude_json_path() -> Result<std::path::PathBuf> {
    let home = std::env::var("USERPROFILE").context("USERPROFILE not set")?;
    Ok(std::path::PathBuf::from(home).join(".claude.json"))
}

/// Read and normalize channel C. Returns Ok even when the cache key is
/// absent (fresh install) — only I/O / JSON-level failures are errors.
pub fn read() -> Result<CliCacheReading> {
    let path = claude_json_path()?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let parsed: ClaudeJson = serde_json::from_str(&text).context("parse ~/.claude.json")?;

    let mut reading = CliCacheReading {
        oauth: parsed.oauth,
        fetched_at: None,
        samples: Vec::new(),
    };

    let Some(cached) = parsed.cached else {
        return Ok(reading);
    };

    let fetched_at = cached
        .get("fetchedAtMs")
        .and_then(|v| v.as_i64())
        .and_then(from_epoch_ms);
    reading.fetched_at = fetched_at;
    let Some(fetched_at) = fetched_at else {
        // No origin timestamp → we can't stamp freshness; skip rather than lie.
        return Ok(reading);
    };

    let account_uuid = cached
        .get("accountUuid")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            reading
                .oauth
                .as_ref()
                .and_then(|o| o.account_uuid.clone())
        });
    let org_uuid = reading
        .oauth
        .as_ref()
        .and_then(|o| o.organization_uuid.clone());

    // `limits[]` is the normalization主來源: open-set kinds, ISO resets_at.
    let limits = cached
        .pointer("/utilization/limits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    for entry in limits {
        let Some(kind) = entry.get("kind").and_then(|v| v.as_str()) else {
            continue; // unknown shape — keep going, raw stays in the log
        };
        let Some(percent) = entry.get("percent").and_then(|v| v.as_f64()) else {
            continue;
        };
        let resets_at = entry
            .get("resets_at")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let scope = entry
            .pointer("/scope/model/display_name")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        reading.samples.push(TruthSample {
            account_uuid: account_uuid.clone(),
            org_uuid: org_uuid.clone(),
            seat_confidence: SeatConfidence::Exact,
            limit_kind: kind.to_string(),
            scope,
            percent: percent as f32,
            resets_at,
            source: Source::CliCache,
            fetched_at,
            raw_json: entry,
        });
    }

    Ok(reading)
}
