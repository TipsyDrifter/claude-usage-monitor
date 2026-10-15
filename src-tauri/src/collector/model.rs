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
/// 前端 `src/lib/ago.ts fmtAgo` 是同一套規則（剛剛／N 分前／N 小時前／N 天前、48 小時換天）。
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

/// v1.9.4（情境稽核 D1）：`age_label` 下一次換字的時刻——「3 分前」到下一分、「2 小時前」
/// 到下一個整點、「3 天前」到下一天（都從樣本時刻起算）。托盤靠它排重畫，讓「N 分前」會走。
pub fn age_label_next_change(fetched_at: DateTime<Utc>, now: DateTime<Utc>) -> DateTime<Utc> {
    let elapsed = (now - fetched_at).num_seconds();
    if elapsed < 0 {
        // 樣本時刻在未來（時鐘差）：現在寫「剛剛」，到樣本後滿一分鐘才變。
        return fetched_at + chrono::Duration::seconds(60);
    }
    let unit = if elapsed < 60 * 60 {
        60
    } else if elapsed < 48 * 3600 {
        3600
    } else {
        24 * 3600
    };
    fetched_at + chrono::Duration::seconds((elapsed / unit + 1) * unit)
}

/// v1.9.4（D1）：`note` 是刷新當下烙的「{來源} · {N 分前}」（`collector::to_item`），之後不會走。
/// 拿掉尾端那段時間、只留來源（「cli」「probe」）；尾端不是 `age_label` 的形狀就原樣回傳
/// （座位最後已知值的「最後已知值」、測試裡單寫的「desktop」）。前端 `ago.ts noteSource` 同一個口徑。
pub fn note_source(note: &str) -> &str {
    match note.rsplit_once(" · ") {
        Some((head, tail)) if is_age_label(tail) => head,
        _ => note,
    }
}

fn is_age_label(s: &str) -> bool {
    if s == "剛剛" {
        return true;
    }
    ["分前", "小時前", "天前"].iter().any(|unit| {
        s.strip_suffix(unit)
            .and_then(|n| n.strip_suffix(' '))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// v1.9.4（D1）：即時的新鮮度戳「cli · 16 分前」——來源取 `note`、時間用樣本時刻＋現在算。
/// 沒有樣本時刻（或解析不了）就退回 `note` 原樣；兩個都沒有寫「來源不明」。
pub fn live_stamp(note: Option<&str>, sampled_at: Option<&str>, now: DateTime<Utc>) -> String {
    let sampled = sampled_at
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc));
    match (note, sampled) {
        (Some(n), Some(t)) => {
            let src = note_source(n);
            if src.is_empty() {
                age_label(t, now)
            } else {
                format!("{src} · {}", age_label(t, now))
            }
        }
        (None, Some(t)) => age_label(t, now),
        (Some(n), None) => n.to_string(),
        (None, None) => "來源不明".to_string(),
    }
}

#[cfg(test)]
mod age_tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn note_source_strips_only_an_age_tail() {
        assert_eq!(note_source("cli · 16 分前"), "cli");
        assert_eq!(note_source("probe · 剛剛"), "probe");
        assert_eq!(note_source("desktop · 3 小時前"), "desktop");
        assert_eq!(note_source("cli · 2 天前"), "cli");
        // 不是時間的尾巴不動
        assert_eq!(note_source("最後已知值"), "最後已知值");
        assert_eq!(note_source("desktop"), "desktop");
        assert_eq!(note_source("a · b"), "a · b");
        assert_eq!(note_source("cli · 分前"), "cli · 分前");
        assert_eq!(note_source("cli · x 分前"), "cli · x 分前");
    }

    /// 情境稽核 D1 的實證場景：10-10 06:36 log 的 `fable_src: "cli · 16 分前"`，30 分一輪的
    /// 設定下以前會一直停在「16 分前」。現在同一份 note 隨現在時刻往前走。
    #[test]
    fn live_stamp_moves_with_now_not_with_the_baked_note() {
        let sampled = "2026-10-10T06:20:00+00:00";
        let note = Some("cli · 16 分前");
        assert_eq!(live_stamp(note, Some(sampled), at("2026-10-10T06:36:00Z")), "cli · 16 分前");
        assert_eq!(live_stamp(note, Some(sampled), at("2026-10-10T06:37:00Z")), "cli · 17 分前");
        assert_eq!(live_stamp(note, Some(sampled), at("2026-10-10T07:20:00Z")), "cli · 1 小時前");
        assert_eq!(live_stamp(note, Some(sampled), at("2026-10-12T06:20:00Z")), "cli · 2 天前");
        // 沒有樣本時刻（座位最後已知值）退回原字串
        assert_eq!(live_stamp(Some("最後已知值"), None, at("2026-10-10T07:00:00Z")), "最後已知值");
        assert_eq!(live_stamp(None, None, at("2026-10-10T07:00:00Z")), "來源不明");
        assert_eq!(live_stamp(None, Some(sampled), at("2026-10-10T06:25:00Z")), "5 分前");
        assert_eq!(live_stamp(Some("x"), Some("壞掉的時刻"), at("2026-10-10T06:25:00Z")), "x");
    }

    #[test]
    fn next_change_is_exactly_when_the_label_changes() {
        let s = at("2026-10-10T06:20:00Z");
        for (now, want) in [
            ("2026-10-10T06:20:00Z", "2026-10-10T06:21:00Z"), // 剛剛 → 1 分前
            ("2026-10-10T06:36:30Z", "2026-10-10T06:37:00Z"), // 16 → 17 分前
            ("2026-10-10T07:19:59Z", "2026-10-10T07:20:00Z"), // 59 分前 → 1 小時前
            ("2026-10-10T08:05:00Z", "2026-10-10T08:20:00Z"), // 1 → 2 小時前
            ("2026-10-12T05:00:00Z", "2026-10-12T05:20:00Z"), // 46 → 47 小時前
            ("2026-10-12T05:59:00Z", "2026-10-12T06:20:00Z"), // 47 小時前 → 2 天前
            ("2026-10-13T00:00:00Z", "2026-10-13T06:20:00Z"), // 2 → 3 天前
            ("2026-10-10T06:19:00Z", "2026-10-10T06:21:00Z"), // 樣本在未來（時鐘差）
        ] {
            let next = age_label_next_change(s, at(now));
            assert_eq!(next, at(want), "now={now}");
            assert!(next > at(now));
            // 前一秒跟當下同字、到點就換字
            let before = next - chrono::Duration::seconds(1);
            assert_ne!(age_label(s, before), age_label(s, next), "now={now}");
        }
    }
}
