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
    /// v1.8.13（D91 T-06）：快取屬於別的帳號（見 `seat_mismatch`）時是 None——
    /// 對目前帳號來說這份快取等於沒有，呼叫端會照「快取過期」處理（該探就探）。
    pub fetched_at: Option<DateTime<Utc>>,
    pub samples: Vec<TruthSample>,
    /// v1.8.13（D91 T-06）：`cachedUsageUtilization.accountUuid`（快取是誰的）跟
    /// `oauthAccount.accountUuid`（現在登入的是誰）不一樣＝剛切帳號、快取還是上一個帳號的。
    /// 這時 `samples` 一律清空：以前帳號取快取的、組織取 oauth 的，拼出「舊帳號 × 新組織」
    /// 這種不存在的座位（帳本裡的 ebb95244、6c3ab8d2），還把舊帳號的數字記在它頭上。
    pub seat_mismatch: Option<SeatMismatch>,
}

/// v1.8.13（D91 T-06）：快取帳號與登入帳號對不上的那一刻。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatMismatch {
    pub cache_account: String,
    pub oauth_account: String,
}

fn claude_json_path() -> Result<std::path::PathBuf> {
    let home = std::env::var("USERPROFILE").context("USERPROFILE not set")?;
    Ok(std::path::PathBuf::from(home).join(".claude.json"))
}

/// v1.8.13（D91 T-06）：寫 collector.log 用的 AppHandle——`read()` 是同步函式、呼叫端
/// （collector/mod.rs）不經手 mismatch，這裡自己記一行。setup 開完帳本時設一次；測試裡沒設就不記。
static LOG_APP: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();
/// 上一次記過的 mismatch——同一組只記一次（read() 一輪會被叫兩三次）。
static LAST_LOGGED: std::sync::Mutex<Option<SeatMismatch>> = std::sync::Mutex::new(None);

pub fn init_log(app: &tauri::AppHandle) {
    let _ = LOG_APP.set(app.clone());
}

fn note_mismatch(m: Option<&SeatMismatch>, fetched_at: Option<DateTime<Utc>>, skipped: usize) {
    let Ok(mut last) = LAST_LOGGED.lock() else { return };
    if last.as_ref() == m {
        return;
    }
    *last = m.cloned();
    let (Some(m), Some(app)) = (m, LOG_APP.get()) else { return };
    let short = |s: &str| s.chars().take(8).collect::<String>();
    let info = serde_json::json!({
        "cacheAccount": short(&m.cache_account),
        "oauthAccount": short(&m.oauth_account),
        "cacheFetchedAt": fetched_at.map(|t| t.to_rfc3339()),
        "skipped": skipped,
    });
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        crate::applog::write_log(&app, "cli-cache-seat-mismatch", info).await;
    });
}

/// Read and normalize channel C. Returns Ok even when the cache key is
/// absent (fresh install) — only I/O / JSON-level failures are errors.
pub fn read() -> Result<CliCacheReading> {
    let path = claude_json_path()?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let parsed = parse(&text)?;
    note_mismatch(
        parsed.reading.seat_mismatch.as_ref(),
        parsed.skipped_fetched_at,
        parsed.skipped_samples,
    );
    Ok(parsed.reading)
}

/// `parse` 的結果＋被擋下來的那批的描述（只給 log 用）。
struct Parsed {
    reading: CliCacheReading,
    skipped_fetched_at: Option<DateTime<Utc>>,
    skipped_samples: usize,
}

/// 純解析（測試直接餵字串）。
fn parse(text: &str) -> Result<Parsed> {
    let parsed: ClaudeJson = serde_json::from_str(text).context("parse ~/.claude.json")?;

    let mut reading = CliCacheReading {
        oauth: parsed.oauth,
        fetched_at: None,
        samples: Vec::new(),
        seat_mismatch: None,
    };
    let Some(cached) = parsed.cached else {
        return Ok(Parsed { reading, skipped_fetched_at: None, skipped_samples: 0 });
    };

    let fetched_at = cached
        .get("fetchedAtMs")
        .and_then(|v| v.as_i64())
        .and_then(from_epoch_ms);
    reading.fetched_at = fetched_at;
    let Some(fetched_at) = fetched_at else {
        // No origin timestamp → we can't stamp freshness; skip rather than lie.
        return Ok(Parsed { reading, skipped_fetched_at: None, skipped_samples: 0 });
    };

    let cache_account = cached
        .get("accountUuid")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let oauth_account = reading.oauth.as_ref().and_then(|o| o.account_uuid.clone());
    let account_uuid = cache_account.clone().or_else(|| oauth_account.clone());
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

    // v1.8.13（D91 T-06）：快取帳號 ≠ 登入帳號＝快取還是上一個帳號的——整批不入帳、不建座位，
    // 新鮮度也不算數（fetched_at 清掉，呼叫端當過期、該探就探，探完 CLI 會把快取換成新帳號的）。
    let mut out = Parsed { reading, skipped_fetched_at: None, skipped_samples: 0 };
    if let (Some(c), Some(o)) = (cache_account, oauth_account) {
        if c != o {
            out.skipped_samples = out.reading.samples.len();
            out.skipped_fetched_at = out.reading.fetched_at.take();
            out.reading.samples.clear();
            out.reading.seat_mismatch = Some(SeatMismatch { cache_account: c, oauth_account: o });
        }
    }
    Ok(out)
}

/// v1.8.13（D91 T-06）：快取帳號與登入帳號對不上時整批不入帳。
#[cfg(test)]
mod tests {
    fn json(cache_account: Option<&str>) -> String {
        let acct = cache_account.map(|a| format!(r#""accountUuid":"{a}","#)).unwrap_or_default();
        format!(
            r#"{{"oauthAccount":{{"accountUuid":"new-acct","organizationUuid":"new-org"}},
               "cachedUsageUtilization":{{{acct}"fetchedAtMs":1758528875444,
                 "utilization":{{"limits":[
                   {{"kind":"session","percent":1,"resets_at":"2026-09-22T12:00:00Z"}},
                   {{"kind":"weekly_all","percent":56}}]}}}}}}"#
        )
    }

    /// 09-22 11:55 切帳號那一刻：快取還是舊帳號（df25…）的、oauth 已經是新帳號＋新組織。
    #[test]
    fn cache_of_another_account_is_not_booked() {
        let p = super::parse(&json(Some("old-acct"))).unwrap();
        assert!(p.reading.samples.is_empty(), "不能拼出「舊帳號 × 新組織」的樣本");
        assert!(p.reading.fetched_at.is_none(), "別人的快取對目前帳號不算新鮮");
        let m = p.reading.seat_mismatch.expect("要標出對不上");
        assert_eq!((m.cache_account.as_str(), m.oauth_account.as_str()), ("old-acct", "new-acct"));
        assert_eq!(p.skipped_samples, 2);
        assert!(p.skipped_fetched_at.is_some());
        // oauth 照樣留著——目前座位還是要靠它記。
        assert_eq!(p.reading.oauth.unwrap().account_uuid.as_deref(), Some("new-acct"));
    }

    #[test]
    fn matching_or_missing_cache_account_is_booked_as_before() {
        for acct in [Some("new-acct"), None] {
            let p = super::parse(&json(acct)).unwrap();
            assert_eq!(p.reading.samples.len(), 2);
            assert!(p.reading.seat_mismatch.is_none());
            assert!(p.reading.fetched_at.is_some());
            for s in &p.reading.samples {
                assert_eq!(s.account_uuid.as_deref(), Some("new-acct"));
                assert_eq!(s.org_uuid.as_deref(), Some("new-org"));
            }
        }
    }
}
