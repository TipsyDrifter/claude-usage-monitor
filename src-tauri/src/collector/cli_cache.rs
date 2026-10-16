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
    /// v1.9.5（D100 E1／E2／F1）：額度種類（`limits[]` 的 kind／scope／percent、`weekly_scoped_shares`
    /// 的 allowance、週窗長、`extra_usage`）。跟 `samples` 同一個規矩：快取是別的帳號的（`seat_mismatch`）
    /// 或沒有 `fetchedAtMs` 時是 None——對目前帳號來說這份等於沒有。
    pub limits: Option<super::limits::LimitsReading>,
    /// v1.9.5（D100 C3 第三版）：`utilization.seven_day_breakdown`——Anthropic 給的「這個 7d 窗的用量
    /// 來自哪裡」。跟 `samples` 同一個規矩：快取是別的帳號的時候是 None。
    pub breakdown: Option<SourceBreakdown>,
}

/// v1.9.5（D100 C3 第三版）：一個 7d 窗的用量來源分佈（`claude_code`／`chat`／`cowork`／`other`，各 %）。
/// 帳號／組織跟同一份快取的樣本同一套解析（座位對得上）。
#[derive(Debug, Clone, PartialEq)]
pub struct SourceBreakdown {
    pub account_uuid: Option<String>,
    pub org_uuid: Option<String>,
    /// 這份分佈算到什麼時候（較新的才覆蓋舊的）
    pub as_of: DateTime<Utc>,
    /// 7d 窗起點。每次讀到的微秒位會抖（08:00:00.008402／.005847），存之前要取整到分鐘。
    pub window_started_at: DateTime<Utc>,
    pub rows: Vec<(String, f64)>,
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

/// 純解析、不碰 AppHandle／log（真資料 probe 與測試用——`read()` 會碰 AppHandle，測試 exe 起不來）。
#[cfg(test)]
pub(crate) fn parse_text(text: &str) -> Result<CliCacheReading> {
    Ok(parse(text)?.reading)
}

/// `parse` 的結果＋被擋下來的那批的描述（只給 log 用）。
struct Parsed {
    reading: CliCacheReading,
    skipped_fetched_at: Option<DateTime<Utc>>,
    skipped_samples: usize,
}

/// `seven_day_breakdown` → [`SourceBreakdown`]。缺 `as_of`／`window_started_at` 或沒有任何有效的列
/// （`rows` 空、或每列都缺 key／percent）→ None：沒有東西可記，不能拿空分佈去當「看不到 100%」。
/// 窗起點取整到分鐘（微秒位每次讀都抖）。
fn parse_breakdown(b: &serde_json::Value, account_uuid: Option<String>, org_uuid: Option<String>) -> Option<SourceBreakdown> {
    let ts = |k: &str| {
        b.get(k)
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc))
    };
    let (as_of, start) = (ts("as_of")?, ts("window_started_at")?);
    let window_started_at = DateTime::from_timestamp((start.timestamp() + 30).div_euclid(60) * 60, 0)?;
    let rows: Vec<(String, f64)> = b
        .get("rows")?
        .as_array()?
        .iter()
        .filter_map(|r| Some((r.get("key")?.as_str()?.to_string(), r.get("percent")?.as_f64()?)))
        .collect();
    if rows.is_empty() {
        return None;
    }
    Some(SourceBreakdown { account_uuid, org_uuid, as_of, window_started_at, rows })
}

/// 純解析（測試直接餵字串）。
fn parse(text: &str) -> Result<Parsed> {
    let parsed: ClaudeJson = serde_json::from_str(text).context("parse ~/.claude.json")?;

    let mut reading = CliCacheReading {
        oauth: parsed.oauth,
        fetched_at: None,
        samples: Vec::new(),
        seat_mismatch: None,
        limits: None,
        breakdown: None,
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

    // v1.9.5（D100）：額度種類另外整理一份（偵測器用）；樣本照舊在下面逐條產生。
    reading.limits = cached.get("utilization").map(super::limits::parse_reading);
    reading.breakdown = cached
        .pointer("/utilization/seven_day_breakdown")
        .and_then(|b| parse_breakdown(b, account_uuid.clone(), org_uuid.clone()));

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
            out.reading.limits = None;
            out.reading.breakdown = None;
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
        assert!(p.reading.limits.is_none(), "v1.9.5：額度種類也不能拿別人的");
        // oauth 照樣留著——目前座位還是要靠它記。
        assert_eq!(p.reading.oauth.unwrap().account_uuid.as_deref(), Some("new-acct"));
    }

    fn json_with_breakdown(cache_account: &str) -> String {
        format!(
            r#"{{"oauthAccount":{{"accountUuid":"new-acct","organizationUuid":"new-org"}},
               "cachedUsageUtilization":{{"accountUuid":"{cache_account}","fetchedAtMs":1758528875444,
                 "utilization":{{"limits":[{{"kind":"session","percent":1}}],
                   "seven_day_breakdown":{{"as_of":"2026-10-11T06:23:20.029638+00:00",
                     "window_started_at":"2026-10-07T08:00:00.005847+00:00",
                     "rows":[{{"key":"claude_code","display_name":"Claude Code","percent":100}},
                             {{"key":"chat","percent":0}},{{"key":"cowork","percent":0}},{{"key":"other","percent":0}}]}}}}}}}}"#
        )
    }

    /// 來源分佈：窗起點取整到分鐘（微秒位會抖）、四列都留、座位跟樣本同一套；別人的快取不記。
    #[test]
    fn source_breakdown_is_parsed_rounded_and_tied_to_the_seat() {
        let p = super::parse(&json_with_breakdown("new-acct")).unwrap();
        let b = p.reading.breakdown.expect("有分佈");
        assert_eq!(b.window_started_at.to_rfc3339(), "2026-10-07T08:00:00+00:00");
        assert_eq!(b.rows.len(), 4);
        assert_eq!(b.rows[0], ("claude_code".to_string(), 100.0));
        assert_eq!((b.account_uuid.as_deref(), b.org_uuid.as_deref()), (Some("new-acct"), Some("new-org")));
        let other = super::parse(&json_with_breakdown("old-acct")).unwrap();
        assert!(other.reading.breakdown.is_none(), "別的帳號的快取不拿來記");
        // 沒有 rows（空陣列）→ 沒有東西可記
        let empty = json_with_breakdown("new-acct").replace(r#""rows":["#, r#""rows":[],"x":["#);
        assert!(super::parse(&empty).unwrap().reading.breakdown.is_none());
    }

    #[test]
    fn matching_or_missing_cache_account_is_booked_as_before() {
        for acct in [Some("new-acct"), None] {
            let p = super::parse(&json(acct)).unwrap();
            assert_eq!(p.reading.samples.len(), 2);
            assert!(p.reading.seat_mismatch.is_none());
            assert!(p.reading.fetched_at.is_some());
            let kinds: Vec<String> = p.reading.limits.as_ref().unwrap().entries.iter().map(|e| e.kind.clone()).collect();
            assert_eq!(kinds, ["session", "weekly_all"]);
            for s in &p.reading.samples {
                assert_eq!(s.account_uuid.as_deref(), Some("new-acct"));
                assert_eq!(s.org_uuid.as_deref(), Some("new-org"));
            }
        }
    }
}
