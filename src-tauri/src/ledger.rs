//! The ledger — the "骨" half of 臉＋骨 (D44): long-term truth storage.
//!
//! A NEW database file (`ledger.sqlite`), separate from the legacy
//! `history.sqlite` (which stays on disk untouched, read-only for the old
//! statistics pages until M3/M4 re-wire them). Starting clean avoids the
//! v1 trap of `usage_event` (singular, empty) vs the new `usage_events`
//! coexisting in one file, and keeps the old schema-version gate from
//! fataling old builds (recon: 2026-08-27-M0-db現況與遷移評估).
//!
//! Schema v1 (D48 機械項 11 + 第 4 點「座位＝帳號×組織」):
//!   seats          — (account_uuid, org_uuid) pairs, learned from channel C
//!   truth_samples  — one observed % of one limit (the "truth" timeline)
//!   usage_events   — deduped API calls from the local JSONL transcripts
//!   anchors        — 429 quota-wall events (window-boundary evidence)
//!   gaps           — known observation gaps (Desktop closed, machine off…)
//!   ingest_files   — mtime/size/offset watermarks for incremental JSONL reads
//!   channel_state  — per-channel watermarks (e.g. last absorbed Desktop `t`)

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use crate::collector::model::TruthSample;
use crate::types::{plan_identity, plan_label, plan_step_factor, PlanKind};

/// v1.9.4（D99 A1）：這一輪從 `oauthAccount` 讀到的方案欄位（原字串）。
#[derive(Debug, Clone, Default)]
pub struct PlanObservation {
    pub tier: Option<String>,
    pub org_type: Option<String>,
    pub seat_tier: Option<String>,
}

/// 方案時間軸這一輪的結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanNote {
    /// 沒讀到任何方案欄位，時間軸沒動。
    NoInfo,
    /// 這個座位第一次記到方案（開第一段）。
    First,
    /// 跟開著的那一段相同。
    Same,
    /// 換方案了：舊段已關、新段已開。
    Changed { from_label: String, to_label: String },
}

/// `mark_current_seat_with_plan` 的回傳。
#[derive(Debug, Clone)]
pub struct SeatMark {
    pub seat_id: String,
    pub plan: PlanNote,
}

/// 方案時間軸的一段：這個座位在 [since, until) 是這個方案（until 為 None＝現在仍是）。
/// 時間軸沒有覆蓋到的時刻＝「方案未知」。
#[derive(Debug, Clone)]
pub struct PlanEpoch {
    pub id: i64,
    pub seat_id: String,
    pub tier: Option<String>,
    pub org_type: Option<String>,
    pub seat_tier: Option<String>,
    pub since: String,
    pub until: Option<String>,
    /// 最後一次從 `oauthAccount` 讀到這個方案的時刻（A8：畫面標「方案讀自 HH:MM」）。
    pub observed_at: String,
    pub source: String,
    since_t: chrono::DateTime<chrono::Utc>,
    until_t: Option<chrono::DateTime<chrono::Utc>>,
}

fn parse_utc_or_min(s: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&chrono::Utc))
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC)
}

impl PlanEpoch {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        let since: String = r.get(5)?;
        let until: Option<String> = r.get(6)?;
        Ok(Self {
            id: r.get(0)?,
            seat_id: r.get(1)?,
            tier: r.get(2)?,
            org_type: r.get(3)?,
            seat_tier: r.get(4)?,
            since_t: parse_utc_or_min(&since),
            until_t: until.as_deref().map(parse_utc_or_min),
            since,
            until,
            observed_at: r.get(7)?,
            source: r.get(8)?,
        })
    }
    /// 可比性的鍵（見 [`plan_identity`]）。
    pub fn identity(&self) -> String {
        plan_identity(self.tier.as_deref(), self.org_type.as_deref()).unwrap_or_default()
    }
    pub fn kind(&self) -> PlanKind {
        PlanKind::classify(self.tier.as_deref(), self.org_type.as_deref())
    }
    pub fn label(&self) -> String {
        plan_label(self.tier.as_deref(), self.org_type.as_deref(), None)
    }
    fn covers(&self, t: chrono::DateTime<chrono::Utc>) -> bool {
        t >= self.since_t && self.until_t.is_none_or(|u| t < u)
    }
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "identity": self.identity(),
            "key": self.kind().key(),
            "label": self.label(),
            "tier": self.tier,
            "orgType": self.org_type,
            "seatTier": self.seat_tier,
            "since": self.since,
            "until": self.until,
            "observedAt": self.observed_at,
            "source": self.source,
        })
    }
}

/// 最近一次**換方案**（兩段已知方案之間的切點）。第一次觀測到方案不算換——那只是「從這裡開始知道」。
#[derive(Debug, Clone)]
struct PlanChange {
    at: chrono::DateTime<chrono::Utc>,
    from_label: String,
    to_label: String,
}

impl PlanChange {
    fn latest(epochs: &[PlanEpoch]) -> Option<Self> {
        let [.., prev, cur] = epochs else { return None };
        Some(Self { at: cur.since_t, from_label: prev.label(), to_label: cur.label() })
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "at": self.at.to_rfc3339(), "fromLabel": self.from_label, "toLabel": self.to_label })
    }
}

/// 時刻 `t` 這個座位是哪一段（沒覆蓋＝方案未知）。
fn epoch_at(epochs: &[PlanEpoch], t: chrono::DateTime<chrono::Utc>) -> Option<&PlanEpoch> {
    epochs.iter().rev().find(|e| e.covers(t))
}

/// 一段時間 [t0, t1] 的方案鍵：兩端落在同一個方案等級才有（跨段／任一端方案未知＝None）。
fn plan_of_span(epochs: &[PlanEpoch], t0: chrono::DateTime<chrono::Utc>, t1: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let a = epoch_at(epochs, t0)?.identity();
    let b = epoch_at(epochs, t1)?.identity();
    (a == b).then_some(a)
}

/// 「方案未知」在 payload 裡的鍵。
const PLAN_UNKNOWN: &str = "unknown";

/// RFC 3339 → 本地 "YYYY-MM-DD HH:MM"（人讀的報告用；看不懂就原樣）。
fn local_stamp(iso: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| iso.to_string())
}

/// 429 錨點屬於哪條額度——跟 `HistKind::anchor_sql` 同一套規則的 Rust 版（測試對過兩邊一致）。
/// SQL 的 `LIKE` 對 ASCII 不分大小寫，所以這裡也先轉小寫再比（21r2 F7）。
fn wall_class(rate_limit_type: Option<&str>, text: Option<&str>) -> &'static str {
    match rate_limit_type {
        Some("five_hour") => "five_hour",
        Some("seven_day") => "seven_day",
        Some(t) if t.starts_with("seven_day") && t.to_lowercase().contains("fable") => "fable",
        Some(_) => "other",
        None => {
            let t = text.unwrap_or("").to_lowercase();
            if t.contains("fable") && t.contains("limit") && !t.contains("out of usage credits") {
                "fable"
            } else {
                "five_hour"
            }
        }
    }
}

/// 方案建議：近 14 天（且不跨方案段）至少要有這麼多天有樣本才給（D99 A6，T-12 沒修乾淨的最少天數）。
const PLAN_MIN_STATS_DAYS: usize = 5;
/// 降級的安全線：把用量換算到下一級的池之後，p90／週峰值都還要低於這個百分比（留 20% 餘裕）。
/// 門檻**跟方案階梯連動**（D99 Q3）：Max 20x→5x 池縮 4 倍 → 原本要 <20%；Max 5x→Pro 縮 5 倍 → <16%。
const PLAN_DOWNGRADE_SAFETY_PCT: f64 = 80.0;

struct PlanAdvice {
    suggestion: &'static str,
    reason: String,
    step_factor: Option<f64>,
    target_label: Option<&'static str>,
    projected_peak: Option<f64>,
}

/// 方案建議（D99 A6／A7／Q3）。純函式、測試直接打。`epoch`＝這個座位目前的方案段（None＝方案從沒被記過）。
/// 建議值：`calibrating`（資料不夠／方案未知）、`upgrade`、`add_account`（Max 20x 用滿：上面沒有一級，
/// 建議多訂一個帳號）、`downgrade`、`keep`、`none`（Team／Enterprise／認不得的方案：席次或方案不是使用者能挑的，不給建議）。
#[allow(clippy::too_many_arguments)]
fn plan_advice(
    epoch: Option<&PlanEpoch>,
    coverage_days: usize,
    stats_days: usize,
    sat_days: usize,
    p90: Option<f64>,
    weekly_peak: f64,
    fable_peak: f64,
    limited_by_plan: bool,
    had_previous: bool,
) -> PlanAdvice {
    let plain = |suggestion: &'static str, reason: String| PlanAdvice {
        suggestion,
        reason,
        step_factor: None,
        target_label: None,
        projected_peak: None,
    };
    let Some(epoch) = epoch else {
        return plain("calibrating", "還不知道這個帳號是什麼方案（登入它一次才會開始記），先不給建議".into());
    };
    let kind = epoch.kind();
    let label = epoch.label();
    match kind {
        PlanKind::Team | PlanKind::Enterprise => {
            return plain("none", format!("{label} 的席次由管理員配置，不是自己能升降的，不給方案建議"));
        }
        PlanKind::Free => {
            return plain("none", format!("{label} 沒有可以升降的階梯，不給方案建議"));
        }
        PlanKind::Other => {
            return plain("none", format!("認不得這個方案（{label}），不給方案建議"));
        }
        _ => {}
    }
    if coverage_days < PLAN_MIN_DAYS {
        return plain("calibrating", format!("目前有觀測記錄的天數累計 {coverage_days} 天（看最近 {PLAN_LOOKBACK_DAYS} 天），滿 {PLAN_MIN_DAYS} 天再給建議"));
    }
    let since_md = epoch.since_t.with_timezone(&chrono::Local).format("%m-%d").to_string();
    let win = if limited_by_plan { format!("自 {since_md} 起（{label}）") } else { format!("近 {PLAN_STATS_DAYS} 天") };
    if stats_days < PLAN_MIN_STATS_DAYS {
        return plain(
            "calibrating",
            if limited_by_plan && had_previous {
                format!("{label} 自 {since_md} 起只有 {stats_days} 天記錄，滿 {PLAN_MIN_STATS_DAYS} 天再給建議（換方案前的記錄是另一個池，不混算）")
            } else if limited_by_plan {
                // 只是第一次記到方案：之前的記錄不是「另一個池」，是不知道（21r1 nit 5）
                format!("App 從 {since_md} 才開始記錄方案（{label}），之後只有 {stats_days} 天記錄，滿 {PLAN_MIN_STATS_DAYS} 天再給建議（更早的記錄方案未知，不混算）")
            } else {
                format!("近 {PLAN_STATS_DAYS} 天只有 {stats_days} 天有記錄，滿 {PLAN_MIN_STATS_DAYS} 天再給建議")
            },
        );
    }
    let fable = if fable_peak > 0.0 { format!("、Fable 峰值 {fable_peak:.0}%") } else { String::new() };
    let facts = format!("{win} 5h 見頂 {sat_days} 天、週峰值 {weekly_peak:.0}%{fable}");
    // Fable 週峰值也算（有人 5h 沒事、Fable 週週見頂——以前規則不看，被判「剛好」）
    if sat_days >= 4 || weekly_peak >= 90.0 || fable_peak >= 90.0 {
        return match kind.next_up() {
            Some(up) => PlanAdvice {
                suggestion: "upgrade",
                reason: format!("{facts}，考慮升級到 {}", up.display()),
                // 升級後的池是現在的幾倍（Pro→Max 5x＝5、Max 5x→Max 20x＝4）
                step_factor: plan_step_factor(up, kind),
                target_label: Some(up.display()),
                projected_peak: None,
            },
            // Max 20x 沒有上一級：上面只有 extra usage——主人的實際做法是多訂一個帳號
            None => plain("add_account", format!("{facts}；Max 20x 已是最高一級，用不夠可以多訂一個帳號")),
        };
    }
    if let (Some(down), Some(p)) = (kind.next_down(), p90) {
        if let Some(f) = plan_step_factor(kind, down) {
            let projected = (p * f).max(weekly_peak * f).max(fable_peak * f);
            if p * f < PLAN_DOWNGRADE_SAFETY_PCT && weekly_peak * f < PLAN_DOWNGRADE_SAFETY_PCT && fable_peak * f < PLAN_DOWNGRADE_SAFETY_PCT {
                return PlanAdvice {
                    suggestion: "downgrade",
                    reason: format!(
                        "{win} 5h 每日最高 p90 {p:.0}%、週峰值 {weekly_peak:.0}%{fable}；降到 {} 池小 {f:.0} 倍，同樣的用量約是 p90 {:.0}%、最高 {projected:.0}%",
                        down.display(),
                        p * f
                    ),
                    step_factor: Some(f),
                    target_label: Some(down.display()),
                    projected_peak: Some(projected),
                };
            }
        }
    }
    plain("keep", format!("{win} 5h 見頂 {sat_days} 天、每日最高 p90 {:.0}%、週峰值 {weekly_peak:.0}%{fable}", p90.unwrap_or(0.0)))
}

/// "YYYY-MM" 兩個月相差幾個月（b − a）。看不懂的字串回 1（不影響判斷，只影響措辭）。
fn months_between(a: &str, b: &str) -> i64 {
    let ym = |s: &str| -> Option<i64> {
        let (y, m) = s.split_once('-')?;
        Some(y.parse::<i64>().ok()? * 12 + m.parse::<i64>().ok()?)
    };
    match (ym(a), ym(b)) {
        (Some(x), Some(y)) => y - x,
        _ => 1,
    }
}

/// 這個方案等級「最近一段」是哪天開始的（本地日 "MM-DD"）——「這期間換過方案（X→Y，MM-DD）」用。
fn plan_switch_date(epochs: &[PlanEpoch], plan: &str) -> Option<String> {
    epochs
        .iter()
        .rev()
        .find(|e| e.identity() == plan)
        .map(|e| e.since_t.with_timezone(&chrono::Local).format("%m-%d").to_string())
}

/// 月份分組：鍵＝（本地月 "YYYY-MM", 方案鍵）。
type GroupKey = (String, String);

/// 月份分組＋每組最後一個區間的結束時刻。
/// **輸出與「最近一組」的判斷一律走 [`MonthGroups::chrono`]（時間順序），不走 BTreeMap 的鍵序**
///（審查 21r1 M2）：鍵序是（月, 方案字串）的字母序，同一個月有兩組時 `…20x…` 排在 `…5x…` 前面、
/// `unknown` 排在所有已知方案後面，與誰先誰後無關——「現在的方案」和「相鄰列斷線」都會判錯。
#[derive(Default)]
struct MonthGroups {
    map: std::collections::BTreeMap<GroupKey, Vec<crate::stats::FamIv>>,
    last_t: std::collections::HashMap<GroupKey, String>,
}

impl MonthGroups {
    fn get(&self, k: &GroupKey) -> Option<&Vec<crate::stats::FamIv>> {
        self.map.get(k)
    }
    /// 時間順序：先看月，同月再看組內最後一個區間的結束時刻。
    fn chrono(&self) -> Vec<(&GroupKey, &Vec<crate::stats::FamIv>)> {
        let mut v: Vec<_> = self.map.iter().collect();
        v.sort_by(|a, b| {
            a.0 .0
                .cmp(&b.0 .0)
                .then_with(|| self.last_t.get(a.0).cmp(&self.last_t.get(b.0)))
                .then_with(|| a.0 .1.cmp(&b.0 .1))
        });
        v
    }
}

impl std::ops::Index<&GroupKey> for MonthGroups {
    type Output = Vec<crate::stats::FamIv>;
    fn index(&self, k: &GroupKey) -> &Self::Output {
        &self.map[k]
    }
}

/// 方案鍵（[`plan_identity`] 或 `PLAN_UNKNOWN`）→ 顯示用的（類別鍵, 名稱）。
#[derive(Default)]
struct PlanLabels(std::collections::HashMap<String, (String, String)>);

impl PlanLabels {
    fn from_epochs(all: &std::collections::HashMap<String, Vec<PlanEpoch>>) -> Self {
        let mut m = std::collections::HashMap::new();
        for e in all.values().flatten() {
            m.entry(e.identity()).or_insert_with(|| (e.kind().key().to_string(), e.label()));
        }
        Self(m)
    }
    /// (類別鍵, 名稱, 已知?)
    fn info(&self, plan: &str) -> (String, String, bool) {
        if plan == PLAN_UNKNOWN {
            return ("unknown".into(), "方案未知".into(), false);
        }
        match self.0.get(plan) {
            Some((k, l)) => (k.clone(), l.clone(), true),
            None => ("other".into(), plan.to_string(), true),
        }
    }
    fn label(&self, plan: &str) -> String {
        self.info(plan).1
    }
    /// 把 `plan`／`planKey`／`planLabel`／`planKnown` 四個欄位蓋到一列 payload 上。
    fn stamp(&self, row: &mut serde_json::Value, plan: &str) {
        let (k, l, known) = self.info(plan);
        row["plan"] = serde_json::json!(plan);
        row["planKey"] = serde_json::json!(k);
        row["planLabel"] = serde_json::json!(l);
        row["planKnown"] = serde_json::json!(known);
    }
}

/// 一個座位「現在的方案」摘要（最後一段）；沒有任何一段＝`known: false`。
/// 這是 payload 的唯一形狀——list_seats／seat_snapshot／today_outlook／dashboard 都用它。
fn plan_summary(epochs: &[PlanEpoch]) -> serde_json::Value {
    let Some(cur) = epochs.last() else {
        return serde_json::json!({
            "known": false, "key": null, "label": null, "identity": null, "tier": null, "orgType": null,
            "seatTier": null, "since": null, "observedAt": null, "previous": null, "epochs": 0,
        });
    };
    let prev = epochs.len().checked_sub(2).map(|i| &epochs[i]);
    let mut v = cur.to_json();
    v["known"] = serde_json::json!(true);
    v["epochs"] = serde_json::json!(epochs.len());
    v["previous"] = prev
        .map(|p| serde_json::json!({ "key": p.kind().key(), "label": p.label(), "identity": p.identity(), "until": p.until }))
        .unwrap_or(serde_json::Value::Null);
    v
}

/// (seat_id, account_uuid, org_uuid, total_samples, desktop_samples) — the row
/// shape shared by the seat picker queries.
type SeatRow = (String, Option<String>, Option<String>, i64, i64, Option<String>, Option<String>);
/// (seat_id, account_uuid, org_uuid, desktop_sample_count) — the seat a page
/// ends up rendering.
type SelectedSeat = (String, Option<String>, Option<String>, i64);

/// v1.8.11（D89，主人）：全量視角＝不分帳號。趨勢頁把每個座位各自配對的區間全部
/// 合起來算（配對仍按座位——跨座位配對會把 A 的 56% 接到 B 的 3% 當成下降）；期間
/// 消耗／撞牆／來源歸因不套登入時段；歷史頁把每個座位的窗口合成一條時間軸。
/// 拖尾（D88）與最早觀測前的用量在這個視角下都不是問題：反正全部都算。
pub const ALL_SEATS: &str = "*";

/// v1.8.12（D90）：即時面（今日頁三條裁決、本窗階梯圖、方案建議、懸浮窗燃燒速度、
/// 切換器的「最後已知值」）讀哪些來源。v1.8.9 的 Desktop 補位讓懸浮窗在 CLI 沒新值時
/// 改用**同帳號**的 Desktop 樣本，但這幾個查詢還停在 D51 時代的 probe／cli-cache——
/// 懸浮窗 9%（desktop · 8 分前）、今日頁 0%（cli · 25 分前），主人 2026-09-28 回報。
/// Desktop 樣本寫進帳本時已按 bridge map 記在它真正的座位上，`seat_id = ?` 就是
/// 「同一個帳號」的保證；不同帳號的 Desktop 仍隔離（D51／D88）。
const LIVE_SOURCES: &str = "'probe','cli-cache','desktop-history'";

/// v1.9.1：「這筆用量算哪個專案」——worktree 裡的對話歸回它的主專案。
/// `project_dir` 是 `~/.claude/projects/` 底下的資料夾名（路徑把非英數字元換成 `-`），
/// worktree 的路徑是 `<專案>\.claude\worktrees\<名字>`，資料夾名就變成
/// `<專案>--claude-worktrees-<名字>`；不歸併的話名字只剩 worktree 的亂碼尾巴
/// （主人 2026-10-02 回報）。帳本原值不改，只在分組時歸併。
const PROJECT_KEY_SQL: &str = "(CASE WHEN instr(project_dir, '--claude-worktrees-') > 0 \
     THEN substr(project_dir, 1, instr(project_dir, '--claude-worktrees-') - 1) \
     ELSE project_dir END)";

/// 來源分組鍵：沒記到 entrypoint 的歸「unknown」（前端有對應標籤）。
const ENTRYPOINT_KEY_SQL: &str = "COALESCE(entrypoint,'unknown')";

/// v1.8.7（D86 蟲 3）：座位的一段「登入中」時段——entrypoint 說是 CLI 側還是 Desktop 側的
/// 登入，[start, end) 為 UTC ISO 字串（end 可能是 9999 年＝延到現在）。
#[derive(Debug, Clone)]
struct TenureSpan {
    entrypoint: &'static str,
    start: String,
    end: String,
}

/// 把登入時段拼成 SQL 條件。`by_entrypoint`＝true 時每段還要對 `entrypoint` 欄
/// （usage_events）；false 只對時間（anchors 沒有帳號／入口欄位，取兩側聯集）。
/// 沒有任何時段 → 條件恆假（這個座位什麼都不該看到）。回傳 (sql, params)。
fn tenure_where(spans: &[TenureSpan], by_entrypoint: bool) -> (String, Vec<String>) {
    if spans.is_empty() {
        return ("0".to_string(), Vec::new());
    }
    let mut parts: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    for s in spans {
        if by_entrypoint {
            parts.push("(entrypoint = ? AND ts >= ? AND ts < ?)".to_string());
            params.push(s.entrypoint.to_string());
        } else {
            parts.push("(ts >= ? AND ts < ?)".to_string());
        }
        params.push(s.start.clone());
        params.push(s.end.clone());
    }
    (parts.join(" OR "), params)
}

/// v2 (M1/D51): B-channel samples quarantined onto (NULL, org) placeholder
/// seats — Desktop can be a different account than the CLI.
/// v3 (M2/D52): B samples now resolve their account through bridge-state's
/// org→account mapping BEFORE seat lookup (confidence stays inferred), so
/// the 30-day Desktop history books onto the right exact seat. Rebuild
/// re-attributes everything; all data is re-derivable from the sources.
const SCHEMA_VERSION: i64 = 3;

/// M5 (D64) estimation constants — one place, mirrored into the payload's
/// `method` block so the UI's method note can never drift from the code.
const MIN_N: usize = 20;
const MIN_DELTA_SUM: f64 = 30.0;
const BOOT_REPS: usize = 1000;
const PERM_REPS: usize = 2000;
const ALPHA: f64 = 0.05;
const SEED_5H: u64 = 0x4d35_0001;
const SEED_7D: u64 = 0x4d35_0007;
const SEED_BASKET: u64 = 0x4d35_00b0;
const SEED_FAMILY: u64 = 0x4d35_00fa;
const SEED_PERM: u64 = 0x4d35_0e00;

/// v1.4.1（D78 #7）方案建議的樣本門檻：近 PLAN_LOOKBACK_DAYS 天內累計 PLAN_MIN_DAYS 天有真值樣本。
const PLAN_LOOKBACK_DAYS: i64 = 60;
/// v1.8.13（D91 T-04／L9）：重置時刻與樣本時刻比先後時的容差（秒）——Desktop 會在重置後
/// 幾百毫秒寫一筆舊窗的值。
const RESET_TOLERANCE_SECS: i64 = 60;
const PLAN_MIN_DAYS: usize = 7;
/// v1.8.13（D91 T-12）：方案建議的統計窗（見頂天數、每日最高 P90、週峰值、每日峰值長條）——
/// 近 14 個本地日，跟畫面「近 14 天」一致；上面的 60 天只管「夠不夠給建議」的門檻。
const PLAN_STATS_DAYS: i64 = 14;
/// v1.8.15（D93）：7d／Fable 的短窗（小時）。人的作息以一天為週期——14 小時（窗長 ÷ 12）
/// 只框得到白天的工作時段、漏掉夜裡睡覺不燒的時間，近期速度系統性偏高；24 小時剛好
/// 一整個晝夜，日內起伏互相抵銷。
const WEEKLY_SHORT_HOURS: f64 = 24.0;

/// Sample gate (D64-3): a month must hold this much before it gets a CI or
/// enters a test. Below it the UI shows the number as 「校準中」.
fn month_qualified(v: &[crate::stats::FamIv]) -> bool {
    v.len() >= MIN_N && v.iter().map(|x| x.delta).sum::<f64>() >= MIN_DELTA_SUM
}

fn month_seed(m: &str) -> u64 {
    m.bytes().fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64))
}

/// One paired truth interval with everything the M5 layer needs.
#[derive(Clone)]
struct Interval {
    month: String,
    /// Truth timestamps bounding the interval (RFC 3339) — the evidence
    /// export needs them so every row is re-computable.
    t0: String,
    t1: String,
    iv: crate::stats::FamIv,
    /// v1.6（D80）：同一段的 API$ 再按模型 id 拆（`claude-fable-5-1`…），
    /// 回歸估計分模型倍率用；家族只是分組標題。稀疏、依 id 排序。
    by_model: Vec<(String, f64)>,
    turns: i64,
    out_tokens: i64,
    /// Local-time start hour / weekday (Mon=0) for the peak heatmap.
    hour: u32,
    weekday: u32,
    seat_id: String,
    /// v1.9.4（D99 A2）：這段區間落在哪個方案等級（[`plan_identity`] 的鍵）；兩端不在同一個
    /// 等級、或任一端方案未知＝None（不跟任何方案比）。
    plan: Option<String>,
    /// v1.9.4（D99 B2）：區間裡有沒有沒牌價的用量——有的話 `iv.usd` 是下限，匯率統計要剔除。
    unpriced: bool,
    /// 有牌價、但不屬於任何家族欄的模型（例如 Mythos）有用量：美元在 `iv.usd` 裡，
    /// `by_family`／`by_model` 沒有它——分模型回歸會把這段 Δ% 錯怪到別的模型身上，所以不進回歸。
    no_family: bool,
}

/// 讓 `stats::trim` 直接修剪 Interval（v1.6 回歸要保留 by_model，不能先降成 FamIv）。
impl crate::stats::Pair for Interval {
    fn delta(&self) -> f64 {
        self.iv.delta
    }
    fn usd(&self) -> f64 {
        self.iv.usd
    }
}

#[derive(Clone)]
pub struct Ledger {
    conn: Arc<Mutex<Connection>>,
    /// v1.8.2 效能：趨勢頁／歷史頁 payload 快取。key＝配方字串；指紋＝帳本幾張表的
    /// 筆數與最新樣本時刻。指紋沒變直接回快取；變了但快取還新鮮就先回舊的、背景重算。
    cache: Arc<std::sync::Mutex<std::collections::HashMap<String, CacheEntry>>>,
    /// 正在背景重算的 key，避免同一份算兩次。
    inflight: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

/// 快取裡一份算好的 payload 與它的來歷。
struct CacheEntry {
    recipe: Recipe,
    fingerprint: String,
    computed_at: std::time::Instant,
    /// v1.8.13（D91 T-15）：最後一次有頁面來讀它的時刻（背景重算不算讀）。
    /// `warm_cache` 只預熱 CACHE_WARM_IDLE 內被讀過的；滿了先淘汰最久沒人讀的。
    last_read_at: std::time::Instant,
    value: serde_json::Value,
}

/// 「怎麼算出這份 payload」——背景重算與開機預熱都靠它重跑。
#[derive(Clone, Debug)]
enum Recipe {
    Dashboard { seat: Option<String>, days: u32 },
    History { seat: Option<String>, unit: String, offset: i32, kind: HistKind },
}

impl Recipe {
    fn key(&self) -> String {
        match self {
            Recipe::Dashboard { seat, days } => format!("dash|{}|{days}", seat.as_deref().unwrap_or("")),
            Recipe::History { seat, unit, offset, kind } => {
                format!("hist|{}|{unit}|{offset}|{}", seat.as_deref().unwrap_or(""), kind.key())
            }
        }
    }
    fn run(&self, conn: &Connection) -> Result<serde_json::Value> {
        match self {
            Recipe::Dashboard { seat, days } => Ledger::dashboard_sync(conn, seat.as_deref(), *days),
            Recipe::History { seat, unit, offset, kind } => {
                Ledger::history_sync(conn, seat.as_deref(), unit, *offset, *kind)
            }
        }
    }
}

/// 指紋變了但快取不到這麼舊 → 先回舊的、背景重算（主人：數字幾分鐘更新一次就夠）。
const CACHE_STALE_OK: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// 背景重算的最短間隔——jsonl-scan 每分鐘都會動指紋，不值得每分鐘燒 3 秒 CPU。
const CACHE_MIN_RECOMPUTE: std::time::Duration = std::time::Duration::from_secs(90);
/// 快取最多留幾份（座位 × 期間 × 歷史頁翻頁很快就會長）。
const CACHE_MAX_ENTRIES: usize = 16;
/// v1.8.13（D91 T-15）：多久沒人讀的配方就不再每輪採集後背景重算。以前快取裡的每一份
/// （包括趨勢頁掛載時 seat=null 多打的那份別人的趨勢頁）都每輪重算、永不退場。
const CACHE_WARM_IDLE: std::time::Duration = std::time::Duration::from_secs(30 * 60);

impl Ledger {
    /// 帳本內容的便宜指紋：幾張表的筆數＋最新樣本時刻。任何寫入都會動到其中之一。
    fn fingerprint_sync(conn: &Connection) -> String {
        conn.query_row(
            "SELECT (SELECT COUNT(*) FROM truth_samples) || ':' || \
                    COALESCE((SELECT MAX(fetched_at) FROM truth_samples), '') || ':' || \
                    (SELECT COUNT(*) FROM usage_events) || ':' || \
                    (SELECT COUNT(*) FROM anchors) || ':' || \
                    (SELECT COUNT(*) FROM seats) || ':' || \
                    (SELECT COUNT(*) FROM seat_epochs)",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default()
    }

    async fn fingerprint(&self) -> String {
        let conn = self.conn.lock().await;
        Self::fingerprint_sync(&conn)
    }

    /// 在阻塞執行緒上跑配方——重算 3 秒不佔 async runtime，其他 IPC 與採集不用排隊。
    async fn compute_blocking(&self, recipe: Recipe) -> Result<serde_json::Value> {
        let conn = self.conn.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            recipe.run(&conn)
        })
        .await
        .map_err(|e| anyhow::anyhow!("compute task failed: {e}"))?
    }

    /// `read`＝這次存是因為有頁面在等（算完就交給它）；背景重算／預熱傳 false，
    /// 保留原本的 last_read_at（新 key 則從現在起算，讓開機預熱那份有 30 分鐘的機會被看到）。
    fn cache_store(&self, recipe: Recipe, fingerprint: String, value: serde_json::Value, read: bool) {
        let key = recipe.key();
        let now = std::time::Instant::now();
        let mut cache = self.cache.lock().unwrap();
        let last_read_at = match cache.get(&key) {
            Some(e) if !read => e.last_read_at,
            _ => now,
        };
        if !cache.contains_key(&key) && cache.len() >= CACHE_MAX_ENTRIES {
            // v1.8.13（D91 T-15）：淘汰最久沒人讀的（以前是最早算的——一直被預熱的永遠不老）。
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, e)| e.last_read_at)
                .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(key, CacheEntry { recipe, fingerprint, computed_at: now, last_read_at, value });
    }

    /// 快取優先的取用：指紋同→直接回；指紋變但還新鮮→回舊的並背景重算；否則現算（阻塞執行緒）。
    async fn cached(&self, recipe: Recipe) -> Result<serde_json::Value> {
        let key = recipe.key();
        let fp = self.fingerprint().await;
        let hit = {
            let mut cache = self.cache.lock().unwrap();
            cache.get_mut(&key).map(|e| {
                e.last_read_at = std::time::Instant::now();
                (e.fingerprint == fp, e.computed_at.elapsed(), e.value.clone())
            })
        };
        if let Some((fresh, age, value)) = hit {
            if fresh {
                return Ok(value);
            }
            if age < CACHE_STALE_OK {
                self.spawn_recompute(recipe);
                return Ok(value);
            }
        }
        let value = self.compute_blocking(recipe.clone()).await?;
        self.cache_store(recipe, fp, value.clone(), true);
        Ok(value)
    }

    /// 背景重算一份配方（同 key 同時只跑一個；指紋沒變或剛算過就略過）。
    fn spawn_recompute(&self, recipe: Recipe) {
        let key = recipe.key();
        {
            let mut inflight = self.inflight.lock().unwrap();
            if !inflight.insert(key.clone()) {
                return;
            }
        }
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            let fp = me.fingerprint().await;
            let skip = {
                let cache = me.cache.lock().unwrap();
                cache
                    .get(&key)
                    .is_some_and(|e| e.fingerprint == fp || e.computed_at.elapsed() < CACHE_MIN_RECOMPUTE)
            };
            if !skip {
                let started = std::time::Instant::now();
                match me.compute_blocking(recipe.clone()).await {
                    Ok(v) => {
                        log::debug!("cache recompute {key} in {:.0} ms", started.elapsed().as_secs_f64() * 1e3);
                        me.cache_store(recipe, fp, v, false);
                    }
                    Err(e) => log::warn!("cache recompute {key} failed: {e:#}"),
                }
            }
            me.inflight.lock().unwrap().remove(&key);
        });
    }

    /// 採集完叫一次：把快取裡**最近 30 分鐘有人讀過的**在背景更新，主人開頁時已經是新的。
    /// 快取還是空的（剛開機）就先預熱目前座位的趨勢頁（180 天）。
    /// v1.8.13（D91 T-15）：以前是每一份都重算、永不退場——看過一次的別人的趨勢頁、翻過的
    /// 十週歷史頁，之後每輪採集都在背景重算並佔著帳本連線。太久沒人讀的留在快取裡（指紋沒變
    /// 時開頁還是秒回），只是不再主動算；下次被讀走「回舊值＋背景重算」或現算。
    pub fn warm_cache(&self) {
        let (empty, recipes) = self.recipes_to_warm();
        if empty {
            let me = self.clone();
            tauri::async_runtime::spawn(async move {
                let seat = me.get_channel_state("current_seat_id").await.ok().flatten();
                me.spawn_recompute(Recipe::Dashboard { seat, days: 180 });
            });
            return;
        }
        for r in recipes {
            self.spawn_recompute(r);
        }
    }

    /// (快取是不是全空, 該預熱的配方)。拆出來給測試直接看。
    fn recipes_to_warm(&self) -> (bool, Vec<Recipe>) {
        let cache = self.cache.lock().unwrap();
        let recipes = cache
            .values()
            .filter(|e| e.last_read_at.elapsed() < CACHE_WARM_IDLE)
            .map(|e| e.recipe.clone())
            .collect();
        (cache.is_empty(), recipes)
    }
}

impl Ledger {
    pub fn open(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create app data dir {}", dir.display()))?;
        let path = dir.join("ledger.sqlite");
        let conn = Connection::open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let version = Self::schema_version_sync(&conn)?;

        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "ledger.sqlite schema v{version} is newer than this build supports (v{SCHEMA_VERSION})"
            );
        }
        if version < 3 {
            // Pre-v3 (incl. fresh file): rebuild from scratch — see the
            // SCHEMA_VERSION note for why dropping is safe here.
            // v1.8.13（D91 T-02）：DROP 包進一個交易——中途失敗整批退回，不會只刪一半。
            // （SCHEMA_V2 自己帶 BEGIN／COMMIT，所以建表另外跑。）
            conn.execute_batch(
                "BEGIN IMMEDIATE;
                 DROP TABLE IF EXISTS truth_samples;
                 DROP TABLE IF EXISTS usage_events;
                 DROP TABLE IF EXISTS anchors;
                 DROP TABLE IF EXISTS gaps;
                 DROP TABLE IF EXISTS ingest_files;
                 DROP TABLE IF EXISTS channel_state;
                 DROP TABLE IF EXISTS seats;
                 DROP TABLE IF EXISTS meta;
                 COMMIT;",
            )?;
            conn.execute_batch(SCHEMA_V2)?;
        }
        // v1.9.4（D99 A1）：純新增的表——每次開檔都跑、全部 IF NOT EXISTS。**不**升 schema_version
        //（升了舊版 App 會拒絕開這個帳本）、不碰任何既有的表與資料。放在版本檢查與重建之後：
        // 帳本壞掉／版本太新時上面已經回錯，不會對一個不認得的檔案動手。
        conn.execute_batch(SCHEMA_ADDITIVE)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            inflight: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        })
    }

    /// v1.8.13（D91 T-02）：讀帳本的 schema 版本。以前查詢一出錯就 `unwrap_or(0)`——
    /// 「meta 那一頁壞了」跟「全新檔」被當成同一件事，接著 DROP 七張完好的表再閃退
    /// （副本重演實證，01-異常態 A2）。現在只有「meta 表不存在、其他表也沒有資料」才回 0（新檔）；
    /// 其他任何情況（sqlite_master 讀不出、meta 讀不出、版本列不見、版本不是數字）一律回錯、絕不動表。
    fn schema_version_sync(conn: &Connection) -> Result<i64> {
        let table_exists = |name: &str| -> Result<bool> {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    params![name],
                    |r| r.get(0),
                )
                .context("讀不出 ledger.sqlite 的資料表清單（檔案可能損毀）")?;
            Ok(n > 0)
        };
        if !table_exists("meta")? {
            // v1.8.13（D91 T-02 審查 R2）：以前只看 truth_samples——它剛好是空的（或它那一列也不見了）
            // 時，seats／usage_events／anchors 裡的資料照樣被 DROP。我們的建表是一整個交易、meta 跟
            // 其他表一起出生，所以「沒有 meta 卻有任何一張表有資料」＝不是我們建出來的形狀，不刪。
            // 表在但全空就放行：DROP 空表不丟任何東西，也不會把一個沒資料的殘檔卡成打不開。
            for t in DATA_TABLES {
                if !table_exists(t)? {
                    continue;
                }
                let rows: i64 = conn
                    .query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
                    .with_context(|| format!("ledger.sqlite 沒有 meta 表、{t} 也讀不出來（檔案可能損毀）"))?;
                if rows > 0 {
                    anyhow::bail!(
                        "ledger.sqlite 沒有 meta 表，{t} 卻有 {rows} 筆資料——不當成新檔、不刪任何資料表"
                    );
                }
            }
            return Ok(0);
        }
        let raw: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })
            .context("讀不出 ledger.sqlite 的版本（meta 表可能損毀）——不動任何資料表")?;
        match raw {
            // v1.8.13（D91 T-02 審查 R2）：以前這裡回 0（當「建表被打斷的殘骸」重建）。但 SCHEMA_V2
            // 是 BEGIN…COMMIT 一整個交易、版本那一列也在裡面，建表被打斷不會留下「有 meta、沒版本」；
            // 這種狀態只會來自損毀（meta 的表頁＋索引頁一起變成合法的空頁，查詢不報錯、回空）或手改。
            None => anyhow::bail!(
                "ledger.sqlite 的 meta 表在、版本那一列卻不見了（檔案可能損毀）——不當成新檔、不動任何資料表"
            ),
            Some(v) => v
                .trim()
                .parse::<i64>()
                .with_context(|| format!("ledger.sqlite 的版本值看不懂（{v:?}）——不動任何資料表")),
        }
    }

    /// Find-or-create the seat for an (account, org) pair — EXACT pair match
    /// only. v2/D51: a sample that only knows its org (channel B) gets a
    /// (account=NULL, org) placeholder seat and STAYS there; we never guess
    /// it onto some account's exact seat, because Desktop may be logged into
    /// a different account than the CLI (the very bug v1 had). Merging a
    /// placeholder into a real seat is a future explicit user-confirmed
    /// action, not an automatic one.
    fn seat_id_sync(
        conn: &Connection,
        account_uuid: Option<&str>,
        org_uuid: Option<&str>,
        now_iso: &str,
    ) -> Result<String> {
        if let Ok(id) = conn.query_row(
            "SELECT id FROM seats WHERE account_uuid IS ?1 AND org_uuid IS ?2",
            params![account_uuid, org_uuid],
            |r| r.get::<_, String>(0),
        ) {
            conn.execute(
                "UPDATE seats SET last_seen_at = ?2 WHERE id = ?1",
                params![id, now_iso],
            )?;
            return Ok(id);
        }

        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO seats (id, account_uuid, org_uuid, first_seen_at, last_seen_at) \
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![id, account_uuid, org_uuid, now_iso],
        )?;
        Ok(id)
    }

    /// Seed the seats table with (account, org) pairs learned from Desktop's
    /// bridge-state.json (read-only) — D48 wanted an org→account mapping
    /// without asking the user; the bridge file simply has it.
    pub async fn seed_seats(&self, pairs: &[(String, String)]) -> Result<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().await;
        let now_iso = chrono::Utc::now().to_rfc3339();
        for (account, org) in pairs {
            Self::seat_id_sync(&conn, Some(account), Some(org), &now_iso)?;
        }
        Ok(())
    }

    /// Burn-rate stats for the W5 expanded panel (M2). Computed from the
    /// exact-source (probe / cli-cache) session-limit samples of the last
    /// two hours: we take the monotonic tail since the last window reset
    /// (a percent DROP means a new 5h window started — samples before it
    /// describe a different window and must not enter the slope).
    ///
    /// The ▼ pace mark assumes linear burn (W5 自評弱點 4); this is the
    /// "precise language" companion the panel pairs with it.
    pub async fn burn_stats(&self, seat: Option<&str>) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;
        let seat_id = Self::resolve_seat_sync(&conn, seat)?;
        let change = PlanChange::latest(&Self::plan_epochs_sync(&conn, &seat_id)?);
        Self::limit_outlook_sync(&conn, &seat_id, &["session", "five_hour"], None, 5.0, 1.0, change.as_ref())
    }

    /// v1.3（D75 決策點 4）：查詢要看的座位——明講的 > CLI 目前登入的
    /// （collector 每次 refresh 寫進 channel_state）> 樣本最多的 exact 座位。
    /// 什麼都沒有時回空字串，讓下游查詢自然查無樣本。
    fn resolve_seat_sync(conn: &Connection, seat: Option<&str>) -> Result<String> {
        // v1.8.11：全量視角對「即時面」沒有意義（百分比是每個帳號各自的），退回目前座位。
        if let Some(s) = seat.filter(|s| !s.is_empty() && *s != ALL_SEATS) {
            return Ok(s.to_string());
        }
        if let Ok(s) = conn.query_row(
            "SELECT value FROM channel_state WHERE key = 'current_seat_id'",
            [],
            |r| r.get::<_, String>(0),
        ) {
            if !s.is_empty() {
                return Ok(s);
            }
        }
        let fallback = conn
            .query_row(
                "SELECT s.id FROM seats s WHERE s.account_uuid IS NOT NULL \
                 ORDER BY (SELECT COUNT(*) FROM truth_samples t WHERE t.seat_id = s.id \
                           AND t.source IN ('probe','cli-cache')) DESC LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok();
        Ok(fallback.unwrap_or_default())
    }

    /// v1.3：把 CLI 目前登入的（帳號, 組織）記成「目前座位」，並把組織名／email
    /// 寫進 seats 表（COALESCE：讀到才覆寫，不會被空值洗掉）。回傳 seat id。
    #[cfg(test)]
    pub async fn mark_current_seat(
        &self,
        account_uuid: Option<&str>,
        org_uuid: Option<&str>,
        email: Option<&str>,
        org_name: Option<&str>,
    ) -> Result<String> {
        Ok(self
            .mark_current_seat_with_plan(account_uuid, org_uuid, email, org_name, None)
            .await?
            .seat_id)
    }

    /// v1.9.4（D99 A1）：同上，另外把這一輪從 `oauthAccount` 讀到的方案等級對一次
    /// （[`PlanObservation`]）——方案等級變了才切一段 `seat_epochs`，沒變只更新「最後讀到的時刻」。
    /// 回傳的 [`SeatMark::plan`] 告訴呼叫端這一輪是第一次看到、沒變、還是換方案了。
    pub async fn mark_current_seat_with_plan(
        &self,
        account_uuid: Option<&str>,
        org_uuid: Option<&str>,
        email: Option<&str>,
        org_name: Option<&str>,
        plan: Option<&PlanObservation>,
    ) -> Result<SeatMark> {
        let conn = self.conn.lock().await;
        let now_iso = chrono::Utc::now().to_rfc3339();
        let seat_id = Self::seat_id_sync(&conn, account_uuid, org_uuid, &now_iso)?;
        conn.execute(
            "UPDATE seats SET account_email = COALESCE(?2, account_email), \
                              org_name = COALESCE(?3, org_name) WHERE id = ?1",
            params![seat_id, email, org_name],
        )?;
        conn.execute(
            "INSERT INTO channel_state (key, value) VALUES ('current_seat_id', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![seat_id],
        )?;
        let plan_note = match plan {
            Some(p) => Self::note_plan_sync(&conn, &seat_id, p, &now_iso)?,
            None => PlanNote::NoInfo,
        };
        Ok(SeatMark { seat_id, plan: plan_note })
    }

    /// v1.9.4（D99 A1）：座位的方案時間軸記帳。比對的是這個座位**目前開著的那一段**（`until IS NULL`）。
    ///
    /// 規則（審查 21r1 S1／21r2 F1 之後）：**只有讀到一個非空、而且跟上一段不同的方案等級才切段**。
    /// - tier 與 organizationType 都沒讀到（只剩 seatTier，或全空）→ `NoInfo`，時間軸不動；
    /// - 這一輪只讀到一部分欄位（例如 tier 暫時掉了）、而讀到的跟目前這段不矛盾 → 沿用目前這段，
    ///   不切段、不算「確認」（不更新 `observed_at`）；
    /// - `seatTier` 新出現／換字串**不算換方案**（只在目前這段還沒有時補寫進去）；
    /// - 兩邊都是認得的等級 → 比等級（`PlanKind`），tier 字串細節不同不切；有一邊認不得 → 比兩邊都有值的欄位，
    ///   有欄位互相矛盾才切；
    /// - 同一等級再讀到 → 補寫缺的欄位、更新 `observed_at`（＝「最後一次確認」）。
    ///
    /// 切段時（審查 21r1 M1）：舊段的 `until` ＝它的 `observed_at`（**最後一次確認是它的時刻**），
    /// 不是偵測時刻；新段 `since`＝偵測時刻。中間 [舊段最後確認, 新段開始] 是「方案未知」的洞——
    /// 以前把洞也算在舊方案頭上，洞裡的用量會被標錯方案、能造出假的「放寬 300%」宣告。
    ///
    /// 沒有手填、也不回溯：第一段的 `since` 就是第一次看到的此刻，之前的歷史是「方案未知」（D99 Q1）。
    fn note_plan_sync(
        conn: &Connection,
        seat_id: &str,
        obs: &PlanObservation,
        now_iso: &str,
    ) -> Result<PlanNote> {
        let clean = |s: &Option<String>| s.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
        let (tier, org_type, seat_tier) = (clean(&obs.tier), clean(&obs.org_type), clean(&obs.seat_tier));
        if tier.is_none() && org_type.is_none() {
            return Ok(PlanNote::NoInfo);
        }
        let obs_kind = PlanKind::classify(tier.as_deref(), org_type.as_deref());
        let open = Self::plan_epochs_sync(conn, seat_id)?.into_iter().rev().find(|e| e.until.is_none());
        let Some(e) = open else {
            conn.execute(
                "INSERT INTO seat_epochs (seat_id, tier, org_type, seat_tier, since, until, observed_at, source) \
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?5, 'observed')",
                params![seat_id, tier, org_type, seat_tier, now_iso],
            )?;
            return Ok(PlanNote::First);
        };
        // 兩邊都有值的欄位必須一致才算「不矛盾」
        let agrees = |old: &Option<String>, new: &Option<String>| match (old.as_deref(), new.as_deref()) {
            (Some(a), Some(b)) => a.trim().eq_ignore_ascii_case(b),
            _ => true,
        };
        let open_kind = e.kind();
        let changed = if obs_kind != PlanKind::Other && open_kind != PlanKind::Other {
            obs_kind != open_kind
        } else {
            !(agrees(&e.tier, &tier) && agrees(&e.org_type, &org_type))
        };
        if changed {
            let tx = conn.unchecked_transaction()?;
            // 舊段停在「最後一次確認」的時刻（不覆寫 observed_at）
            tx.execute("UPDATE seat_epochs SET until = observed_at WHERE id = ?1", params![e.id])?;
            tx.execute(
                "INSERT INTO seat_epochs (seat_id, tier, org_type, seat_tier, since, until, observed_at, source) \
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?5, 'observed')",
                params![seat_id, tier, org_type, seat_tier, now_iso],
            )?;
            tx.commit()?;
            return Ok(PlanNote::Changed {
                from_label: e.label(),
                to_label: plan_label(tier.as_deref(), org_type.as_deref(), None),
            });
        }
        // 沒換：把這次多讀到的欄位補進目前這段（只補缺的，不覆寫）
        conn.execute(
            "UPDATE seat_epochs SET tier = COALESCE(tier, ?2), org_type = COALESCE(org_type, ?3), \
                                    seat_tier = COALESCE(seat_tier, ?4) WHERE id = ?1",
            params![e.id, tier, org_type, seat_tier],
        )?;
        if obs_kind != PlanKind::Other || (tier.is_some() && org_type.is_some()) {
            // 這一輪確實讀到一個認得的等級（或認不得但兩個欄位都讀全了）、而且跟目前這段不矛盾＝最後一次確認
            conn.execute("UPDATE seat_epochs SET observed_at = ?2 WHERE id = ?1", params![e.id, now_iso])?;
            Ok(PlanNote::Same)
        } else {
            Ok(PlanNote::NoInfo)
        }
    }

    /// 一個座位的方案時間軸，按 `since` 由舊到新。沒有任何一段＝這個座位的方案從沒被觀測過（全部「方案未知」）。
    fn plan_epochs_sync(conn: &Connection, seat_id: &str) -> Result<Vec<PlanEpoch>> {
        let mut stmt = conn.prepare(
            "SELECT id, seat_id, tier, org_type, seat_tier, since, until, observed_at, source \
             FROM seat_epochs WHERE seat_id = ?1 ORDER BY since ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![seat_id], PlanEpoch::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 全部座位的方案時間軸（seat_id → 段）。
    fn plan_epochs_all_sync(conn: &Connection) -> Result<std::collections::HashMap<String, Vec<PlanEpoch>>> {
        let mut stmt = conn.prepare(
            "SELECT id, seat_id, tier, org_type, seat_tier, since, until, observed_at, source \
             FROM seat_epochs ORDER BY since ASC, id ASC",
        )?;
        let mut map: std::collections::HashMap<String, Vec<PlanEpoch>> = Default::default();
        for e in stmt.query_map([], PlanEpoch::from_row)? {
            let e = e?;
            map.entry(e.seat_id.clone()).or_default().push(e);
        }
        Ok(map)
    }

    /// 公開給外面（測試／診斷）看一個座位的方案時間軸。
    #[cfg(test)]
    pub async fn plan_timeline(&self, seat_id: &str) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().await;
        Ok(Self::plan_epochs_sync(&conn, seat_id)?.iter().map(PlanEpoch::to_json).collect())
    }

    /// v1.8.9（D88，O10 實證）：切帳號後 CLI 會拖著**上一個帳號**的額度走——`oauthAccount`
    /// 已經是新帳號，`/usage` 卻連續 20 小時回切換前那組數字（09-22 11:55 切到 thomaskuo100
    /// 之後 7d 56／Fable 100 一動不動，直到週重置才歸零），App 照它說的帳號記，就把
    /// 別人的 Fable 100% 記到新帳號頭上。這裡在寫進帳本之前對一次：進來的 CLI 樣本若
    /// 7d（與 Fable）跟「上一個座位最後一筆」一模一樣且非零，就當拖尾——不記、不顯示，
    /// 直到數字真的變了才解除。回傳 Some((上一個座位 id, 7d, Fable)) 表示拖尾。
    /// 狀態放 channel_state：`prev_seat_id`／`seat_switched_at`（超過 7 天自動放棄比對）。
    pub async fn carryover_check(&self, samples: &[TruthSample]) -> Result<Option<(String, f32, Option<f32>)>> {
        let Some(first) = samples.iter().find(|s| s.account_uuid.is_some()) else {
            return Ok(None);
        };
        let conn = self.conn.lock().await;
        let now = chrono::Utc::now();
        let now_iso = now.to_rfc3339();
        // v1.8.13（D91 T-06）：只查、不建。以前這裡用 seat_id_sync——查不到就 INSERT，
        // 就算下面判定是拖尾、樣本一筆都不記，座位列已經先生出來了（1.8.9 之後幽靈座位的來源）。
        // 還沒有座位的配對＝一定不是目前座位，照「正在切換」往下比；座位留給真的入帳時再建。
        let incoming: Option<String> = conn
            .query_row(
                "SELECT id FROM seats WHERE account_uuid IS ?1 AND org_uuid IS ?2",
                params![first.account_uuid.as_deref(), first.org_uuid.as_deref()],
                |r| r.get::<_, String>(0),
            )
            .ok();
        let incoming = incoming.as_deref();
        let get = |k: &str| -> Option<String> {
            conn.query_row("SELECT value FROM channel_state WHERE key = ?1", params![k], |r| r.get::<_, String>(0))
                .ok()
                .filter(|v| !v.is_empty())
        };
        let set = |k: &str, v: &str| -> rusqlite::Result<usize> {
            conn.execute(
                "INSERT INTO channel_state (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![k, v],
            )
        };
        let current = get("current_seat_id");
        let remembered = get("prev_seat_id");
        let prev: Option<String> = match (current.as_deref(), remembered) {
            (Some(cur), _) if Some(cur) != incoming => Some(cur.to_string()), // 正在切換
            (_, Some(p)) if Some(p.as_str()) != incoming => Some(p),          // 之前切過、還在盯
            _ => None,
        };
        let Some(prev) = prev else {
            set("prev_seat_id", "")?;
            set("seat_switched_at", "")?;
            return Ok(None);
        };
        let switched_at = get("seat_switched_at");
        if let Some(t) = switched_at.as_deref().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
            if (now - t.with_timezone(&chrono::Utc)).num_days() > 7 {
                set("prev_seat_id", "")?;
                set("seat_switched_at", "")?;
                return Ok(None);
            }
        }
        let last = |seat: &str, kinds: &str, scope: &str| -> Option<f32> {
            conn.query_row(
                &format!(
                    "SELECT percent FROM truth_samples WHERE seat_id = ?1 AND limit_kind IN ({kinds}) \
                     AND scope = ?2 AND source IN ('probe','cli-cache') ORDER BY fetched_at DESC LIMIT 1"
                ),
                params![seat, scope],
                |r| r.get::<_, f64>(0),
            )
            .ok()
            .map(|v| v as f32)
        };
        let prev_weekly = last(&prev, "'weekly_all','seven_day'", "");
        let prev_fable = last(&prev, "'weekly_scoped'", "Fable");
        let no_scope = |s: &TruthSample| s.scope.as_deref().map(|x| x.is_empty()).unwrap_or(true);
        let in_weekly = samples
            .iter()
            .find(|s| matches!(s.limit_kind.as_str(), "weekly_all" | "seven_day") && no_scope(s))
            .map(|s| s.percent);
        let in_fable = samples
            .iter()
            .find(|s| s.limit_kind == "weekly_scoped" && s.scope.as_deref().is_some_and(|x| x.eq_ignore_ascii_case("fable")))
            .map(|s| s.percent);
        let same = match (prev_weekly, in_weekly) {
            (Some(pw), Some(iw)) if pw > 0.0 && (pw - iw).abs() < 0.5 => match (prev_fable, in_fable) {
                (Some(pf), Some(inf)) => (pf - inf).abs() < 0.5,
                _ => true,
            },
            _ => false,
        };
        if same {
            set("prev_seat_id", &prev)?;
            if switched_at.is_none() {
                set("seat_switched_at", &now_iso)?;
            }
            Ok(Some((prev, prev_weekly.unwrap_or(0.0), prev_fable)))
        } else {
            set("prev_seat_id", "")?;
            set("seat_switched_at", "")?;
            Ok(None)
        }
    }

    /// v1.3 切換器用的座位清單：只列有帳號 uuid 的 exact 座位（決策點 9），
    /// 附名字欄位與 A／C 樣本數；`currentSeatId` 是 CLI 目前登入的那一個。
    pub async fn list_seats(&self) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;
        let current = conn
            .query_row(
                "SELECT value FROM channel_state WHERE key = 'current_seat_id'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.account_uuid, s.org_uuid, s.account_email, s.org_name, s.last_seen_at, \
                    (SELECT COUNT(*) FROM truth_samples t WHERE t.seat_id = s.id \
                      AND t.source IN ('probe','cli-cache')), \
                    (SELECT MAX(t.fetched_at) FROM truth_samples t WHERE t.seat_id = s.id \
                      AND t.source IN ('probe','cli-cache')) \
             FROM seats s WHERE s.account_uuid IS NOT NULL ORDER BY 7 DESC",
        )?;
        let epochs = Self::plan_epochs_all_sync(&conn)?;
        let mut seats = Vec::new();
        for row in stmt.query_map([], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, String>(0)?,
                "accountUuid": r.get::<_, Option<String>>(1)?,
                "orgUuid": r.get::<_, Option<String>>(2)?,
                "email": r.get::<_, Option<String>>(3)?,
                "orgName": r.get::<_, Option<String>>(4)?,
                "lastSeenAt": r.get::<_, String>(5)?,
                "liveSamples": r.get::<_, i64>(6)?,
                "lastSampleAt": r.get::<_, Option<String>>(7)?,
            }))
        })? {
            let mut s = row?;
            // v1.9.4（D99 A8／A12）：每個座位帶自己的方案（切換器、MCP list_accounts 用）。
            let id = s["id"].as_str().unwrap_or_default().to_string();
            s["plan"] = plan_summary(epochs.get(&id).map(Vec::as_slice).unwrap_or(&[]));
            seats.push(s);
        }
        Ok(serde_json::json!({ "seats": seats, "currentSeatId": current }))
    }

    /// v1.3（決策點 5）：某座位三條額度的**最後已知值**——每個額度家族各取
    /// probe／cli-cache 的最後一筆。形狀對齊 `UsageSnapshot`，前端的懸浮窗列
    /// 可以直接吃；`scrapedAt`＝三筆裡最新的 fetched_at，前端據此標「N 小時前」。
    pub async fn seat_snapshot(&self, seat_id: &str) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;
        let last = |kinds: &str, scope: &str| -> Result<(Option<f64>, Option<String>, Option<String>)> {
            let sql = format!(
                "SELECT percent, resets_at, fetched_at FROM truth_samples \
                 WHERE seat_id = ?1 AND limit_kind IN ({kinds}) AND scope = ?2 \
                   AND source IN ({LIVE_SOURCES}) \
                 ORDER BY fetched_at DESC LIMIT 1"
            );
            let row = conn
                .query_row(&sql, params![seat_id, scope], |r| {
                    Ok((r.get::<_, f64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?))
                })
                .ok();
            Ok(match row {
                Some((p, reset, at)) => (Some(p), reset, Some(at)),
                None => (None, None, None),
            })
        };
        let session = last("'session','five_hour'", "")?;
        let weekly = last("'weekly_all','seven_day'", "")?;
        let fable = last("'weekly_scoped'", "Fable")?;
        let newest = [&session.2, &weekly.2, &fable.2]
            .into_iter()
            .flatten()
            .max()
            .cloned();
        let item = |v: &(Option<f64>, Option<String>, Option<String>)| {
            serde_json::json!({ "usedPercent": v.0, "resetAt": v.1, "note": v.2.as_ref().map(|_| "最後已知值") })
        };
        // v1.9.4（D99 A8）：方案名讀這個座位自己的時間軸（以前寫死 null，前端退回「目前登入帳號」的方案，
        // 切換器看別的座位時掛錯）。沒觀測過＝null，前端不要退回別人的。
        let plan = plan_summary(&Self::plan_epochs_sync(&conn, seat_id)?);
        Ok(serde_json::json!({
            "seatId": seat_id,
            "planName": plan["label"],
            "plan": plan,
            "currentSession": item(&session),
            "weeklyAllModels": item(&weekly),
            "weeklyFable": item(&fable),
            "scrapedAt": newest,
        }))
    }

    /// Outlook for ONE limit family（D71，SRE 式雙窗）：
    /// - 長窗＝整個額度窗至今（用量 ÷ 窗內已過時間）——與 pace ▼ 同一把尺；
    /// - 短窗：7d／Fable＝24 小時（D93：一整個晝夜，見 WEEKLY_SHORT_HOURS）；5h＝60 分（D73：25 分只有 2–5 筆整數樣本，
    ///   回放證明報分鐘會抖；60 分跳動 3%、誤差中位數 24 分，比長窗還準）；
    /// - R ＝ burn ÷ par（par ＝ 100% ÷ 窗長）；「會提早見底」只在**兩個 R 都 ≥ 1** 時成立；
    /// - 重置後未滿一個短窗 → insufficient，先不裁決（AWS／GCP 資料不足不預測）。
    fn limit_outlook_sync(
        conn: &Connection,
        seat_id: &str,
        kinds: &[&str],
        scope: Option<&str>,
        window_hours: f64,
        short_hours: f64,
        plan_change: Option<&PlanChange>,
    ) -> Result<serde_json::Value> {
        let now = chrono::Utc::now();
        let since = (now - chrono::Duration::seconds((window_hours * 3600.0) as i64)).to_rfc3339();
        let kind_list = kinds
            .iter()
            .map(|k| format!("'{k}'"))
            .collect::<Vec<_>>()
            .join(",");
        let scope_clause = match scope {
            Some(_) => "AND scope = ?3",
            None => "AND scope = ''",
        };
        // v1.3（D75）：按座位算——兩個帳號的樣本不能接成一條曲線。
        let sql = format!(
            "SELECT percent, fetched_at, resets_at FROM truth_samples \
             WHERE seat_id = ?1 AND limit_kind IN ({kind_list}) \
               AND source IN ({LIVE_SOURCES}) \
               AND fetched_at >= ?2 {scope_clause} \
             ORDER BY fetched_at ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let map_row = |r: &rusqlite::Row| -> rusqlite::Result<(f64, String, Option<String>)> {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        };
        let rows: Vec<(f64, String, Option<String>)> = match scope {
            Some(s) => stmt
                .query_map(params![seat_id, since, s], map_row)?
                .collect::<std::result::Result<_, _>>()?,
            None => stmt
                .query_map(params![seat_id, since], map_row)?
                .collect::<std::result::Result<_, _>>()?,
        };

        // 重置切尾：最後一次「站得住」的 >2% 下跌之後才是本窗。
        // v1.8.12：Desktop 與 CLI 同刻可能差 1～3 點（D88 驗過平均 <1.5），下跌要往後看
        // 30 分鐘內全都低才算重置（規則同 segment_windows 的 LOOKAHEAD_MIN）——否則兩個
        // 來源打一下架就把本窗切成一筆，燃燒速度直接變「樣本累積中」。
        let mut tail_start = 0usize;
        for i in 1..rows.len() {
            let floor = rows[i - 1].0 - 2.0;
            if rows[i].0 >= floor {
                continue;
            }
            let Ok(ti) = chrono::DateTime::parse_from_rfc3339(&rows[i].1) else { continue };
            let horizon = ti + chrono::Duration::minutes(LOOKAHEAD_MIN);
            let confirmed = rows[i + 1..]
                .iter()
                .take_while(|n| {
                    chrono::DateTime::parse_from_rfc3339(&n.1)
                        .map(|t| t <= horizon)
                        .unwrap_or(false)
                })
                .all(|n| n.0 < floor);
            if confirmed {
                tail_start = i;
            }
        }
        // v1.9.4（D99 A11）：換方案的瞬間 % 會階躍（升級＝掉下去，會被上面當成重置；降級＝跳上去，
        // 會被短窗斜率吃進去，出一個假的「會提早見底」）。方案切點之前的樣本屬於另一個池，
        // 一律切掉——本窗只看新方案之後的；而且這個窗的 % 是追溯換算還是重置沒驗過（U3），
        // 所以到下一次重置前都標「重新觀察」（insufficient），不下見底的裁決。
        let cut_idx: Option<usize> = plan_change.and_then(|c| {
            let idx = rows
                .iter()
                .position(|r| chrono::DateTime::parse_from_rfc3339(&r.1).map(|t| t >= c.at).unwrap_or(false))
                .unwrap_or(rows.len());
            (idx > 0).then_some(idx)
        });
        // 切點之後又發生了一次**真的重置**（重置切尾比方案切點更靠後）→ 本窗已經是乾淨的新窗，
        // 不再掛 planChange／insufficient（21r1 S2；否則 5h 條白白空白到切點滑出 5 小時、週條最久 7 天）。
        // 兩者落在同一筆（升級的下跌剛好就是切點後第一筆）→ 無法分辨，保守地仍算換方案。
        let plan_cut = cut_idx.is_some_and(|ci| ci >= tail_start);
        if let Some(ci) = cut_idx {
            tail_start = tail_start.max(ci);
        }
        let tail = &rows[tail_start..];

        let par = 100.0 / window_hours;
        let short_h = short_hours;
        let round1 = |x: f64| (x * 10.0).round() / 10.0;
        let round2 = |x: f64| (x * 100.0).round() / 100.0;

        let mut out = serde_json::json!({
            "samples": tail.len(),
            "percent": tail.last().map(|r| r.0),
            "fetchedAt": tail.last().map(|r| r.1.clone()),
            "resetsAt": null,          // 下面借窗內最新一筆帶重置時刻的（v1.8.12）
            "windowHours": window_hours,
            "shortWindowMinutes": (short_h * 60.0).round(),
            "burnPerHour": null,       // 短窗平滑斜率（%/h）——燃燒速度顯示、pace-flow 流速、副句預警
            "paceBurnPerHour": null,   // 長窗平均（%/h）——與 ▼ 同尺
            "rShort": null,
            "rLong": null,
            "projectedAtReset": null,  // 長窗外推到重置時的 %
            "etaShort": null,          // 短窗速度若持續的見底時刻（rShort ≥ 1 才有）
            "etaLong": null,           // 長窗平均外推的見底時刻（rLong ≥ 1 才有）——5h 的數字用它（D72）
            "etaAt": null,             // 裁決見底時刻：兩窗都 ≥ 1 才有
            "insufficient": false,
            "expired": false,          // v1.4.1：最後樣本的窗已過重置時間——之後未知
            "lastKnownPercent": null,
            "resetsAtLatest": null,    // v1.8.13（D91 T-04）：借不到重置時刻時，用窗長推的「最晚這時已重置」
            // v1.9.4（D99 D2）：最後一筆樣本比短窗還老、或預測的見底時刻早已過去＝資料太舊，不給任何
            // 見底時刻（以前 etaAt 會落在過去，MCP 回負分鐘）。
            "stale": false,
            // v1.9.4（D99 A11）：這個窗內剛換過方案（切點之前的樣本已切掉）：{at, fromLabel, toLabel}。
            "planChange": if plan_cut { plan_change.map(PlanChange::to_json) } else { None },
            // 換方案後還沒有任何新方案的樣本（切完是空的）——不是「已重置」，是「等新方案的第一筆」。
            "awaitingAfterPlanChange": false,
        });
        if plan_cut && tail.is_empty() {
            out["insufficient"] = serde_json::json!(true);
            out["awaitingAfterPlanChange"] = serde_json::json!(true);
            out["lastKnownPercent"] = serde_json::json!(rows.last().map(|r| r.0));
            out["fetchedAt"] = serde_json::json!(rows.last().map(|r| r.1.clone()));
            return Ok(out);
        }
        let Some(last) = tail.last() else {
            // v1.4.2：窗內一筆樣本都沒有（例如 5h 窗、最後一筆是三天前）——
            // 往前找這條額度的最後一筆；它的窗必然已過，標 expired 並附上次的值，
            // 讓「已重置 · 上次 X%」跟 7d／Fable 同一套說法，而不是硬邦邦的「無資料」。
            let sql = format!(
                "SELECT percent, fetched_at, resets_at FROM truth_samples \
                 WHERE seat_id = ?1 AND limit_kind IN ({kind_list}) \
                   AND source IN ({LIVE_SOURCES}) {scope_clause} \
                 ORDER BY fetched_at DESC LIMIT 1"
            );
            let mut stmt = conn.prepare(&sql)?;
            // scope_clause 用的是 ?3，所以 Some 時要佔住 ?2（rusqlite 檢查參數個數）。
            let older: Option<(f64, String, Option<String>)> = match scope {
                Some(s) => stmt.query_row(params![seat_id, "", s], map_row).ok(),
                None => stmt.query_row(params![seat_id], map_row).ok(),
            };
            if let Some((p, at, reset)) = older {
                out["expired"] = serde_json::json!(true);
                out["lastKnownPercent"] = serde_json::json!(p);
                out["fetchedAt"] = serde_json::json!(at);
                out["resetsAt"] = serde_json::json!(reset);
            }
            return Ok(out);
        };
        let t_last = chrono::DateTime::parse_from_rfc3339(&last.1)?;

        // v1.8.12：重置時刻借窗內最新一筆有帶的——probe 與 Desktop 樣本都不帶，只有 cli-cache
        // 有（D88：「Desktop 補位、重置時刻借 CLI」）。借來的若比最後一筆樣本還早，那是上一窗
        // 的（跨過重置卻沒被切尾＝% 沒掉超過 2），當作未知——不能拿它判 expired 或算配速。
        // v1.8.13（D91 T-04／L9）：比對給 60 秒容差——Desktop 會在重置後幾百毫秒寫一筆**舊窗**的
        // 100%（帳本實例：CLI 重置 07:19:59.885、Desktop 100% 在 07:20:00.483），嚴格比會把
        // 借來的重置當成上一窗的、丟掉，那筆 100% 就被當成本窗一直掛著。
        let borrowed_reset = tail
            .iter()
            .rev()
            .find_map(|r| r.2.as_deref().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()))
            .filter(|reset| *reset >= t_last - chrono::Duration::seconds(RESET_TOLERANCE_SECS));
        out["resetsAt"] = serde_json::json!(borrowed_reset.map(|r| r.to_rfc3339()));

        // v1.4.1（D78 #1，也收掉 backlog「閒置超過一個窗顯示無資料」）：最後一筆樣本的窗
        // 已經過了重置時間——之後用了多少沒人知道（別的座位沒登入、或目前座位閒置）。
        // 不假裝：標 expired，前端把數字換「—」、寫「HH:MM 已重置」，上次的值留給提示。
        // 百分比與時間仍照常回傳，畫面才有東西可以說「上次是多少」。
        if let Some(reset) = borrowed_reset {
            if reset < chrono::Utc::now() {
                out["expired"] = serde_json::json!(true);
                out["lastKnownPercent"] = serde_json::json!(last.0);
                return Ok(out);
            }
        } else if let Some(latest) =
            Self::reset_upper_bound_sync(
                conn,
                seat_id,
                &kind_list,
                scope,
                window_hours,
                &last.1,
                plan_change.filter(|_| plan_cut).map(|c| c.at),
            )?
        {
            // v1.8.13（D91 T-04）：窗尾全是 Desktop（或探針）樣本時借不到重置時刻，以前 expired
            // 永遠是 false——重置後最多 5 小時今日頁都掛著舊百分比。窗長是固定的：這一窗第一筆
            // 有用量的樣本之後最晚一個窗長就重置，過了就標 expired（只用來判過期，不拿來算配速）。
            out["resetsAtLatest"] = serde_json::json!(latest.to_rfc3339());
            if latest < chrono::Utc::now() {
                out["expired"] = serde_json::json!(true);
                out["lastKnownPercent"] = serde_json::json!(last.0);
                return Ok(out);
            }
        }

        // ---- 短窗：取最近 short_h 內的樣本；窗內只剩一筆就往前多拿一筆，
        //      斜率至少跨過一個短窗（Chromium／curl 的固定滑動窗精神）。
        let cutoff = t_last - chrono::Duration::seconds((short_h * 3600.0) as i64);
        let mut i0 = tail.len() - 1;
        while i0 > 0 && chrono::DateTime::parse_from_rfc3339(&tail[i0 - 1].1)? >= cutoff {
            i0 -= 1;
        }
        if i0 == tail.len() - 1 && i0 > 0 {
            i0 -= 1;
        }
        let mut burn_short: Option<f64> = None;
        if i0 < tail.len() - 1 {
            let t0 = chrono::DateTime::parse_from_rfc3339(&tail[i0].1)?;
            let hours = (t_last - t0).num_seconds() as f64 / 3600.0;
            if hours >= 5.0 / 60.0 {
                burn_short = Some((last.0 - tail[i0].0) / hours);
            }
        }
        if let Some(b) = burn_short {
            out["burnPerHour"] = serde_json::json!(round1(b));
            out["rShort"] = serde_json::json!(round2(b / par));
        }

        // ---- 長窗：用量 ÷ 窗內已過時間。
        let reset = borrowed_reset;
        if let Some(reset) = reset {
            let window_start =
                reset - chrono::Duration::seconds((window_hours * 3600.0) as i64);
            let elapsed_h = (t_last - window_start).num_seconds() as f64 / 3600.0;
            let insufficient = elapsed_h < short_h || plan_cut;
            out["insufficient"] = serde_json::json!(insufficient);
            let mut r_long: Option<f64> = None;
            if elapsed_h >= 5.0 / 60.0 {
                let pace_burn = last.0 / elapsed_h;
                out["paceBurnPerHour"] = serde_json::json!(round1(pace_burn));
                r_long = Some(pace_burn / par);
                out["rLong"] = serde_json::json!(round2(pace_burn / par));
                out["projectedAtReset"] =
                    serde_json::json!((pace_burn * window_hours).min(100.0).round());
                if pace_burn / par >= 1.0 && last.0 < 100.0 {
                    let eta_long = window_start
                        + chrono::Duration::seconds((elapsed_h * 100.0 / last.0 * 3600.0) as i64);
                    if eta_long < reset {
                        out["etaLong"] = serde_json::json!(eta_long.to_rfc3339());
                    }
                }
            }
            if let Some(b) = burn_short {
                if b / par >= 1.0 && last.0 < 100.0 {
                    let eta = t_last
                        + chrono::Duration::seconds(((100.0 - last.0) / b * 3600.0) as i64);
                    out["etaShort"] = serde_json::json!(eta.to_rfc3339());
                    if !insufficient && r_long.is_some_and(|r| r >= 1.0) && eta < reset {
                        out["etaAt"] = serde_json::json!(eta.to_rfc3339());
                    }
                }
            }
        }
        // v1.9.4（D99 D2）：預測是從「最後一筆樣本」往後外推的。那筆樣本比短窗還老（5h 條 1 小時、週條
        // 24 小時），或外推出來的見底時刻已經在過去，都表示資料跟不上現在——不給任何見底時刻，
        // 標 stale（以前 etaAt 會是「約 10/07 14:00 觸頂」這種過去的時間，MCP 的 hitsLimitInMinutes 變負數）。
        let age_h = (now - t_last.with_timezone(&chrono::Utc)).num_seconds() as f64 / 3600.0;
        let eta_in_past = ["etaAt", "etaShort", "etaLong"].iter().any(|k| {
            out[*k]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .is_some_and(|t| t.with_timezone(&chrono::Utc) < now)
        });
        if age_h > short_h || eta_in_past {
            for k in ["etaAt", "etaShort", "etaLong"] {
                out[k] = serde_json::Value::Null;
            }
            out["stale"] = serde_json::json!(true);
        }
        Ok(out)
    }

    /// v1.8.13（D91 T-04）：借不到重置時刻時，用窗長推「這一窗最晚什麼時候重置」。
    ///
    /// 從最後一筆樣本往回走、收集「同一窗」的樣本：前一筆是 0、比後一筆高（任何下跌，含
    /// Desktop／CLI 差一兩點的雜訊——雜訊只會讓推出來的時刻變晚、不會變早）、前一筆帶的重置
    /// 時刻落在兩筆之間、或兩筆相隔滿一個窗長，都算換窗、停下。走到的最早一筆有用量的樣本
    /// 一定在這一窗裡，而一窗從開始到重置正好一個窗長 → 重置 ≤ 那一筆＋窗長。
    /// 最後一筆是 0（這一窗還沒用過）→ 回 None，沒東西會過期。
    fn reset_upper_bound_sync(
        conn: &Connection,
        seat_id: &str,
        kind_list: &str,
        scope: Option<&str>,
        window_hours: f64,
        t_last_raw: &str,
        not_before: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Option<chrono::DateTime<chrono::FixedOffset>>> {
        let window = chrono::Duration::seconds((window_hours * 3600.0) as i64);
        let tolerance = chrono::Duration::seconds(RESET_TOLERANCE_SECS);
        let t_last = chrono::DateTime::parse_from_rfc3339(t_last_raw)?;
        // v1.9.4（D99 A11）：換方案的切點之前是另一個池，往回走不能越過它。
        let mut lo_t = t_last - window;
        if let Some(nb) = not_before {
            lo_t = lo_t.max(nb.fixed_offset());
        }
        let lo = lo_t.to_rfc3339();
        let scope_clause = if scope.is_some() { "AND scope = ?4" } else { "AND scope = ''" };
        let sql = format!(
            "SELECT percent, fetched_at, resets_at FROM truth_samples              WHERE seat_id = ?1 AND limit_kind IN ({kind_list})                AND source IN ({LIVE_SOURCES})                AND fetched_at >= ?2 AND fetched_at <= ?3 {scope_clause}              ORDER BY fetched_at ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let map_row = |r: &rusqlite::Row| -> rusqlite::Result<(f64, String, Option<String>)> {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        };
        let rows: Vec<(f64, String, Option<String>)> = match scope {
            Some(sc) => stmt.query_map(params![seat_id, lo, t_last_raw, sc], map_row)?.collect::<std::result::Result<_, _>>()?,
            None => stmt.query_map(params![seat_id, lo, t_last_raw], map_row)?.collect::<std::result::Result<_, _>>()?,
        };
        let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
        let Some(last) = rows.last() else { return Ok(None) };
        if last.0 <= 0.0 {
            return Ok(None);
        }
        let mut e = rows.len() - 1;
        while e > 0 {
            let (p_prev, t_prev, r_prev) = &rows[e - 1];
            let (p_cur, t_cur, _) = &rows[e];
            let (Some(t_prev), Some(t_cur)) = (parse(t_prev), parse(t_cur)) else { break };
            let reset_between = r_prev
                .as_deref()
                .and_then(parse)
                .is_some_and(|r| r <= t_cur - tolerance);
            if *p_prev <= 0.0 || *p_prev > *p_cur || reset_between || t_cur - t_prev >= window {
                break;
            }
            e -= 1;
        }
        Ok(parse(&rows[e].1).map(|t| t + window))
    }

    /// M3「今天會不會撞牆」decision-layer payload: outlook for all three
    /// limits + what THIS 5h window's local activity looks like. Local JSONL
    /// carries no account field (O10), so the activity table is machine-wide
    /// and labeled as such — the truth percentages remain CLI-account (D51).
    pub async fn today_outlook(&self, seat: Option<&str>) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;
        let seat_id = Self::resolve_seat_sync(&conn, seat)?;
        // v1.9.4（D99 A8）：這個座位（被看的那個，不一定是登入中的）自己的方案時間軸。
        let epochs = Self::plan_epochs_sync(&conn, &seat_id)?;
        let change = PlanChange::latest(&epochs);
        let session =
            Self::limit_outlook_sync(&conn, &seat_id, &["session", "five_hour"], None, 5.0, 1.0, change.as_ref())?;
        let weekly = Self::limit_outlook_sync(
            &conn,
            &seat_id,
            &["weekly_all", "seven_day"],
            None,
            168.0,
            WEEKLY_SHORT_HOURS,
            change.as_ref(),
        )?;
        let fable = Self::limit_outlook_sync(
            &conn,
            &seat_id,
            &["weekly_scoped"],
            Some("Fable"),
            168.0,
            WEEKLY_SHORT_HOURS,
            change.as_ref(),
        )?;

        // Current 5h window start — from the session limit's resets_at.
        let window_start = session
            .get("resetsAt")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|reset| (reset - chrono::Duration::hours(5)).to_rfc3339());

        let mut by_entrypoint = Vec::new();
        let mut top_projects = Vec::new();
        let mut top_models = Vec::new();
        if let Some(start) = &window_start {
            // v1.9.3（D97）：三張表都按 API 計價排（四種 tokens 照牌價折算）。
            // 2026-10-03（主人：今日頁也要看模型，跟歷史頁一樣跟專案並排）——同專案表，本機全量、不分帳號。
            let p = [start.clone()];
            by_entrypoint = usage_mix(&conn, ENTRYPOINT_KEY_SQL, "entrypoint", "ts >= ?1", &p, 0)?;
            top_projects = usage_mix(&conn, PROJECT_KEY_SQL, "project", "ts >= ?1", &p, 6)?;
            top_models = usage_mix(&conn, "model", "model", "ts >= ?1", &p, 6)?;
        }

        // ---- D56/M5: this window's % staircase (exact sources) + 429 ticks ----
        let mut window_samples: Vec<serde_json::Value> = Vec::new();
        let mut window_anchors: Vec<String> = Vec::new();
        if let Some(start) = &window_start {
            let mut stmt = conn.prepare(&format!(
                "SELECT percent, fetched_at, source FROM truth_samples \
                 WHERE seat_id = ?2 AND limit_kind IN ('session','five_hour') AND scope = '' \
                   AND source IN ({LIVE_SOURCES}) AND fetched_at >= ?1 \
                 ORDER BY fetched_at ASC"
            ))?;
            for row in stmt.query_map(params![start, seat_id], |r| {
                Ok(serde_json::json!({
                    "at": r.get::<_, String>(1)?,
                    "percent": r.get::<_, f64>(0)?,
                    "source": r.get::<_, String>(2)?,
                }))
            })? {
                window_samples.push(row?);
            }
            // E4：這張 5h 階梯圖只畫 5h 牆的刻度——Fable 牆、非額度牆走同一個分類（HistKind::anchor_sql）。
            let mut stmt = conn.prepare(&format!(
                "SELECT ts FROM anchors WHERE {} AND ts >= ?1 ORDER BY ts ASC",
                HistKind::FiveHour.anchor_sql()
            ))?;
            for row in stmt.query_map(params![start], |r| r.get::<_, String>(0))? {
                window_anchors.push(row?);
            }
        }

        // ---- D56/M5 方案建議: last 14 days of exact samples, % only ----
        let cur_epoch = epochs.last();
        let plan = {
            // v1.4.1（D78 #7）：門檻改「累計 ≥7 天有樣本」——看近 60 天，不再要求 14 天內滿 7 天，
            // 用得不頻繁的人也拿得到建議。
            // v1.8.13（D91 T-12）：D78 #7 只想放寬**門檻**，當時連統計窗也一起放成 60 天——畫面寫
            // 「近 14 天」、長條圖畫 14 天，見頂天數／P90／週峰值卻是 60 天算的（fcf0：60 天見頂 5 天
            // 判「考慮升級」，真正的近 14 天只見頂 1 天）。現在門檻看 60 天、統計只看近 14 個本地日
            // （今天＋前 13 天，跟長條圖同一組日子）。
            let since_lookback = (chrono::Utc::now() - chrono::Duration::days(PLAN_LOOKBACK_DAYS)).to_rfc3339();
            let window_from = chrono::Local::now().date_naive() - chrono::Duration::days(PLAN_STATS_DAYS - 1);
            // v1.9.4（D99 A6）：統計窗起點＝max(近 14 個本地日, 目前這一段方案的起點)——換方案之前的
            // 百分比是另一個池的，留在窗內會在升級後還叫人「考慮升級」、降級後叫人「可考慮降級」。
            // 方案是第一次記到（之前「方案未知」）也一樣不往前借：未知的時段不跟已知的比。
            let epoch_from = cur_epoch.map(|e| e.since_t.with_timezone(&chrono::Local).date_naive());
            let stats_from = epoch_from.map_or(window_from, |d| d.max(window_from));
            let limited_by_plan = epoch_from.is_some_and(|d| d > window_from);
            let mut day_peak: std::collections::BTreeMap<String, f64> = Default::default();
            let mut days_seen: std::collections::BTreeSet<String> = Default::default();
            let mut stats_days: std::collections::BTreeSet<String> = Default::default();
            let mut weekly_peak = 0.0f64;
            let mut fable_peak = 0.0f64;
            let mut stmt = conn.prepare(&format!(
                "SELECT limit_kind, scope, percent, fetched_at FROM truth_samples \
                 WHERE seat_id = ?2 AND source IN ({LIVE_SOURCES}) AND fetched_at >= ?1"
            ))?;
            for row in stmt.query_map(params![since_lookback, seat_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })? {
                let (kind, scope, pct, at) = row?;
                // v1.8.13（D91 T-13）：按本地日分（以前 substr 取 UTC 日，台北早上 8 點前的
                // 見頂算到前一天；今日頁長條圖畫的是本地日）。
                let Some(local_day) = local_day_of(&at) else { continue };
                let day = local_day.to_string();
                days_seen.insert(day.clone());
                // 只算門檻，不進統計：窗外的日子，以及同一天裡方案切點之前的樣本（切點是時刻，不是整天）
                if local_day < stats_from || cur_epoch.is_some_and(|e| parse_utc_or_min(&at) < e.since_t) {
                    continue;
                }
                stats_days.insert(day.clone());
                match kind.as_str() {
                    "session" | "five_hour" => {
                        let e = day_peak.entry(day).or_insert(0.0);
                        if pct > *e {
                            *e = pct;
                        }
                    }
                    "weekly_all" | "seven_day" => weekly_peak = weekly_peak.max(pct),
                    "weekly_scoped" if scope == "Fable" => fable_peak = fable_peak.max(pct),
                    _ => {}
                }
            }
            let coverage_days = days_seen.len();
            let sat_days = day_peak.values().filter(|&&p| p >= 100.0).count();
            let mut peaks: Vec<f64> = day_peak.values().copied().collect();
            peaks.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p90 = if peaks.is_empty() { None } else { Some(crate::stats::quantile(&peaks, 0.9)) };
            let advice = plan_advice(
                cur_epoch,
                coverage_days,
                stats_days.len(),
                sat_days,
                p90,
                weekly_peak,
                fable_peak,
                limited_by_plan,
                epochs.len() >= 2,
            );
            let (suggestion, reason) = (advice.suggestion, advice.reason.clone());
            serde_json::json!({
                // v1.9.4（D99 A6／A7）：建議是對**這個座位目前的方案**說的
                "planKey": cur_epoch.map(|e| e.kind().key()),
                "planLabel": cur_epoch.map(|e| e.label()),
                "planSince": cur_epoch.map(|e| e.since.clone()),
                "statsFrom": stats_from.to_string(),
                "limitedByPlan": limited_by_plan,
                "minStatsDays": PLAN_MIN_STATS_DAYS,
                "stepFactor": advice.step_factor,
                "targetLabel": advice.target_label,
                "projectedPeakOnTarget": advice.projected_peak,
                "downgradeSafetyPercent": PLAN_DOWNGRADE_SAFETY_PCT,
                "coverageDays": coverage_days,
                "coverageLookbackDays": PLAN_LOOKBACK_DAYS,
                "coverageMinDays": PLAN_MIN_DAYS,
                // v1.8.13（D91 T-12）：統計窗（天）與窗內有樣本的天數；dailyPeaks 只剩這 14 天。
                "statsWindowDays": PLAN_STATS_DAYS,
                "statsDays": stats_days.len(),
                "sessionSatDays": sat_days,
                "sessionPeakP90": p90,
                "weeklyPeak": weekly_peak,
                "fablePeak": fable_peak,
                "dailyPeaks": day_peak.iter().map(|(d, p)| serde_json::json!({ "day": d, "peak": p })).collect::<Vec<_>>(),
                "suggestion": suggestion,
                "reason": reason,
            })
        };

        Ok(serde_json::json!({
            "seatId": seat_id,
            // v1.9.4（D99 A8）：被看的座位自己的方案（不是目前登入的帳號的）＋讀取時刻 observedAt
            "seatPlan": plan_summary(&epochs),
            "session": session,
            "weekly": weekly,
            "fable": fable,
            "windowStart": window_start,
            "windowSamples": window_samples,
            "windowAnchors": window_anchors,
            "plan": plan,
            "byEntrypoint": by_entrypoint,
            "topProjects": top_projects,
            "topModels": top_models,
        }))
    }

    /// v1.8.7（D86 蟲 3）：這個座位「在哪些時段是登入中的那個帳號」。
    /// CLI 側看 probe／cli-cache 樣本、Desktop 側看 desktop-history 樣本：把該來源
    /// **所有座位**的樣本按時間排好，座位換了就切一段，只留這個座位的段；最後一段
    /// 若還是它就延到現在（登入狀態是階梯函數——沒觀測到換帳號就當沒換）。
    /// 最早一筆樣本之前的用量沒有依據、不歸任何座位（D86 決策點 1，評估後主人可翻）。
    fn seat_tenure_sync(conn: &Connection, seat_id: &str, since: &str) -> Result<Vec<TenureSpan>> {
        const OPEN_END: &str = "9999-12-31T23:59:59Z";
        let mut out: Vec<TenureSpan> = Vec::new();
        for (entrypoint, sources) in [
            ("cli", "'probe','cli-cache'"),
            ("claude-desktop", "'desktop-history'"),
        ] {
            let mut stmt = conn.prepare(&format!(
                "SELECT seat_id, MIN(fetched_at) FROM truth_samples WHERE source IN ({sources}) \
                 GROUP BY seat_id, fetched_at ORDER BY 2 ASC"
            ))?;
            let rows: Vec<(String, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            let mut run: Option<(String, String)> = None; // (seat, run start)
            for (s, t) in rows {
                match &run {
                    Some((cur, _)) if *cur == s => {}
                    Some((cur, start)) => {
                        if cur == seat_id {
                            out.push(TenureSpan { entrypoint, start: start.clone(), end: t.clone() });
                        }
                        run = Some((s, t));
                    }
                    None => run = Some((s, t)),
                }
            }
            if let Some((cur, start)) = run {
                if cur == seat_id {
                    out.push(TenureSpan { entrypoint, start, end: OPEN_END.to_string() });
                }
            }
        }
        // Clip to the requested period.
        out.retain(|s| s.end.as_str() > since);
        for s in &mut out {
            if s.start.as_str() < since {
                s.start = since.to_owned();
            }
        }
        Ok(out)
    }

    /// Pair consecutive truth samples of ONE seat / limit family into
    /// intervals (≤45 min apart, strictly increasing %), and price the
    /// entrypoint-filtered usage_events inside each with the API table,
    /// split by model family (stats::FAMILIES) so the fixed basket can be
    /// built downstream. Shared by the 5h and 7d ledgers (D64).
    fn pair_intervals(
        conn: &Connection,
        seat_id: &str,
        kinds_sql: &str,
        since: &str,
        entrypoint: &str,
        epochs: &[PlanEpoch],
    ) -> Result<Vec<Interval>> {
        use chrono::{Datelike, Timelike};
        let mut stmt = conn.prepare(&format!(
            "SELECT percent, fetched_at FROM truth_samples \
             WHERE seat_id = ?1 AND limit_kind IN ({kinds_sql}) AND scope = '' \
               AND fetched_at >= ?2 ORDER BY fetched_at ASC"
        ))?;
        let samples: Vec<(f64, String)> = stmt
            .query_map(params![seat_id, since], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;

        let mut ev_stmt = conn.prepare(&format!(
            "SELECT model, COALESCE(speed,''), COUNT(*), SUM(input_tokens), \
                    SUM(cache_creation), SUM(COALESCE(cache_1h,0)), SUM(cache_read), \
                    SUM(output_tokens), SUM(COALESCE(web_searches,0)), {LONG_PROMPT_SQL} \
             FROM usage_events WHERE ts > ?1 AND ts <= ?2 AND entrypoint = ?3 \
             GROUP BY model, COALESCE(speed,''), {LONG_PROMPT_SQL}"
        ))?;
        let mut intervals: Vec<Interval> = Vec::new();
        for w in samples.windows(2) {
            let (p0, t0) = (&w[0].0, &w[0].1);
            let (p1, t1) = (&w[1].0, &w[1].1);
            let (Ok(a), Ok(b)) = (
                chrono::DateTime::parse_from_rfc3339(t0),
                chrono::DateTime::parse_from_rfc3339(t1),
            ) else {
                continue;
            };
            let mins = (b - a).num_minutes();
            if mins <= 0 || mins > 45 || p1 <= p0 {
                continue;
            }
            let delta = p1 - p0;
            let mut usd = 0.0f64;
            let mut by_family = [0.0f64; 5];
            let mut by_model: std::collections::BTreeMap<String, f64> = Default::default();
            let mut turns = 0i64;
            let mut out_tokens = 0i64;
            let mut unpriced = false;
            let mut no_family = false;
            for row in ev_stmt.query_map(params![t0, t1, entrypoint], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                    r.get::<_, i64>(9)? != 0,
                ))
            })? {
                let (model, speed, calls, inp, cw, cw1, cr, out, ws, long) = row?;
                // B2：沒牌價的用量不能當 $0——這一段標 unpriced，匯率統計會把它剔除。
                let Some(v) = price_usd(&model, &speed, inp, cw, cw1, cr, out, ws, long) else {
                    unpriced = true;
                    turns += calls;
                    out_tokens += out;
                    continue;
                };
                usd += v;
                if let Some(f) = crate::stats::family_index(&model) {
                    by_family[f] += v;
                    *by_model.entry(model.clone()).or_insert(0.0) += v;
                } else if v > 0.0 {
                    no_family = true;
                }
                turns += calls;
                out_tokens += out;
            }
            let local = a.with_timezone(&chrono::Local);
            // v1.8.13（D91 T-13）：配對月份用本地月（以前 `t1[..7]` 是 UTC——台北每月 1 號
            // 00:00～08:00 的區間會被算進上個月）；同一段的熱圖時段本來就是本地時間。
            intervals.push(Interval {
                month: b.with_timezone(&chrono::Local).format("%Y-%m").to_string(),
                t0: t0.clone(),
                t1: t1.clone(),
                iv: crate::stats::FamIv { delta, usd, by_family },
                by_model: by_model.into_iter().collect(),
                turns,
                out_tokens,
                hour: local.hour(),
                weekday: local.weekday().num_days_from_monday(),
                seat_id: seat_id.to_string(),
                plan: plan_of_span(epochs, a.with_timezone(&chrono::Utc), b.with_timezone(&chrono::Utc)),
                unpriced,
                no_family,
            });
        }
        Ok(intervals)
    }

    /// Per-month exchange-rate rows (D64-1/2/3): raw + trimmed Δ-weighted
    /// ratio, bootstrap CI only for months past the sample gate.
    fn fx_months(
        ivs: &[Interval],
        seed: u64,
        labels: &PlanLabels,
    ) -> (Vec<serde_json::Value>, MonthGroups) {
        let months = Self::group_months(ivs);
        let rows = Self::fx_rows(&months, seed, labels);
        (rows, months)
    }

    /// v1.9.4（D99 A3）：區間按（月, 方案等級）分組——同一個月裡換過方案就是兩組，各算各的匯率；
    /// 同一個方案等級的不同時段（5x→20x→5x）併成一組（同等級的池一樣大，可以互比）。
    /// 方案未知／跨段的區間歸 `PLAN_UNKNOWN`。呼叫端要先剔掉沒牌價的區間（B2）。
    fn group_months(ivs: &[Interval]) -> MonthGroups {
        let mut months = MonthGroups::default();
        for i in ivs {
            let plan = i.plan.clone().unwrap_or_else(|| PLAN_UNKNOWN.to_string());
            let key = (i.month.clone(), plan);
            let last = months.last_t.entry(key.clone()).or_default();
            if i.t1 > *last {
                *last = i.t1.clone();
            }
            months.map.entry(key).or_default().push(i.iv.clone());
        }
        months
    }

    /// 輸出列序＝時間順序（見 [`MonthGroups::chrono`]），前端「相鄰列方案不同＝斷線」才斷在對的地方。
    fn fx_rows(months: &MonthGroups, seed: u64, labels: &PlanLabels) -> Vec<serde_json::Value> {
        use crate::stats;
        months
            .chrono()
            .into_iter()
            .map(|((m, plan), v)| {
                let n = v.len();
                let dsum: f64 = v.iter().map(|x| x.delta).sum();
                let usum: f64 = v.iter().map(|x| x.usd).sum();
                let qualified = month_qualified(v);
                let rate_raw = (dsum >= 5.0).then(|| usum / dsum);
                let rate = if dsum >= 5.0 { stats::trimmed_ratio(v) } else { None };
                let ci = if qualified {
                    stats::bootstrap_ci(v, stats::trimmed_ratio, BOOT_REPS, seed ^ month_seed(m))
                } else {
                    None
                };
                let mut row = serde_json::json!({
                    "month": m,
                    "samples": n,
                    "deltaSum": dsum,
                    "usdSum": usum,
                    "rateRaw": rate_raw,
                    "rate": rate,
                    "ci": ci.map(|(a, b)| vec![a, b]),
                    "qualified": qualified,
                });
                labels.stamp(&mut row, plan);
                row
            })
            .collect()
    }

    /// v1.9.4（D99 A13）：「這段期間跨了方案嗎」——期間內這個座位的方案分段與換方案的時間點。
    /// 方案時間軸沒覆蓋到的部分是「方案未知」段（不補、不回溯，D99 Q1）。
    fn plan_block(
        epochs: &[PlanEpoch],
        from: chrono::DateTime<chrono::Utc>,
        labels: &PlanLabels,
        iv5_ok: &[Interval],
        mixed_plans: bool,
    ) -> serde_json::Value {
        let mut segs: Vec<serde_json::Value> = Vec::new();
        let count_in = |plan: &str, a: chrono::DateTime<chrono::Utc>, b: Option<chrono::DateTime<chrono::Utc>>| -> usize {
            iv5_ok
                .iter()
                .filter(|i| {
                    let t = parse_utc_or_min(&i.t0);
                    t >= a
                        && b.is_none_or(|b| t < b)
                        && i.plan.as_deref().unwrap_or(PLAN_UNKNOWN) == plan
                })
                .count()
        };
        let mut push = |plan: &str, since: chrono::DateTime<chrono::Utc>, until: Option<chrono::DateTime<chrono::Utc>>| {
            let mut v = serde_json::json!({
                "since": since.to_rfc3339(),
                "until": until.map(|u| u.to_rfc3339()),
                "intervals": count_in(plan, since, until),
            });
            labels.stamp(&mut v, plan);
            segs.push(v);
        };
        match epochs.first() {
            None => push(PLAN_UNKNOWN, from, None),
            Some(f) if f.since_t > from => push(PLAN_UNKNOWN, from, Some(f.since_t)),
            _ => {}
        }
        for e in epochs.iter().filter(|e| e.until_t.is_none_or(|u| u > from)) {
            push(&e.identity(), e.since_t.max(from), e.until_t);
        }
        // 換方案時舊段停在「最後一次確認」、新段從偵測時刻開始——中間是「方案未知」的洞（21r1 M1）
        for w in epochs.windows(2) {
            if let Some(u) = w[0].until_t {
                if u < w[1].since_t && w[1].since_t > from {
                    push(PLAN_UNKNOWN, u.max(from), Some(w[1].since_t));
                }
            }
        }
        // 期間起點到第一次記方案之間若根本沒有區間（帳本還沒那麼老），那段「方案未知」是空的，不列。
        segs.retain(|s| s["plan"] != PLAN_UNKNOWN || s["intervals"].as_u64().unwrap_or(0) > 0);
        segs.sort_by(|a, b| a["since"].as_str().cmp(&b["since"].as_str()));
        // crossesPlan／crossesKnownPlans 只看真的有區間的段（剛開始記、還沒有區間的新段不算跨方案，21r1 nit 8）
        let distinct: std::collections::BTreeSet<String> = segs
            .iter()
            .filter(|s| s["intervals"].as_u64().unwrap_or(0) > 0)
            .filter_map(|s| s["plan"].as_str().map(str::to_string))
            .collect();
        let known: std::collections::BTreeSet<&String> = distinct.iter().filter(|p| p.as_str() != PLAN_UNKNOWN).collect();
        let changes: Vec<serde_json::Value> = epochs
            .windows(2)
            .filter(|w| w[1].since_t > from)
            .map(|w| {
                serde_json::json!({
                    "at": w[1].since, "fromLabel": w[0].label(), "toLabel": w[1].label(),
                    "fromKey": w[0].kind().key(), "toKey": w[1].kind().key(),
                })
            })
            .collect();
        serde_json::json!({
            "current": plan_summary(epochs),
            "segments": segs,
            "changes": changes,
            // 期間裡有不只一種方案（含「方案未知」段）——匯率／分佈跨了池，tooltip 要註明
            "crossesPlan": distinct.len() > 1,
            // 期間裡有兩種以上**已知**的方案（真的換過）
            "crossesKnownPlans": known.len() > 1,
            "mixedPlans": mixed_plans,
            "firstObservedAt": epochs.first().map(|e| e.since.clone()),
        })
    }

    /// v1.9.4（D99 B2）：有用量卻沒牌價的模型（`<synthetic>` 這類零 tokens 的不算）。
    /// `since`＝只看這個時刻之後的事件；None＝全帳本。
    fn unpriced_models_sync(conn: &Connection, since: Option<&str>) -> Result<Vec<serde_json::Value>> {
        let mut stmt = conn.prepare(
            "SELECT model, COUNT(*), SUM(COALESCE(input_tokens,0) + COALESCE(cache_creation,0) + \
                    COALESCE(cache_read,0) + COALESCE(output_tokens,0)), MIN(ts), MAX(ts) \
             FROM usage_events WHERE ts >= ?1 GROUP BY model",
        )?;
        let mut out: Vec<serde_json::Value> = Vec::new();
        for row in stmt.query_map(params![since.unwrap_or("")], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?.unwrap_or(0), r.get::<_, String>(3)?, r.get::<_, String>(4)?))
        })? {
            let (model, calls, tokens, first, last) = row?;
            if tokens > 0 && !has_price(&model) {
                out.push(serde_json::json!({
                    "model": model, "label": crate::stats::model_label(&model),
                    "calls": calls, "tokens": tokens, "firstSeen": first, "lastSeen": last,
                }));
            }
        }
        out.sort_by(|a, b| b["tokens"].as_i64().cmp(&a["tokens"].as_i64()));
        Ok(out)
    }

    /// M4 dashboard payload — the research layer (這半年發生了什麼).
    ///
    /// M5 (D64) 匯率轉正: trimmed Δ-weighted ratio + bootstrap CI per month,
    /// permutation month-over-month test, 5h/7d cross-check, fixed-basket
    /// shrink index, per-family quota multipliers, local-time peak heatmap
    /// and truth reconciliation. Pure math lives in `stats.rs`; this
    /// function builds intervals and assembles JSON. Events are entrypoint-
    /// filtered to the seat (Desktop seat ↔ claude-desktop, CLI seat ↔ cli;
    /// 推定 O10). `saturatedDays` (D62) keeps its semantics untouched.
    pub async fn dashboard(
        &self,
        seat_account: Option<&str>,
        days: u32, // D60 自定義期間；0 = 全部（實作上限 3650）
    ) -> Result<serde_json::Value> {
        // v1.8.2 效能：走快取（見 Recipe／cached）。真正的計算在 dashboard_sync。
        self.cached(Recipe::Dashboard { seat: seat_account.map(str::to_owned), days }).await
    }

    fn dashboard_sync(
        conn: &Connection,
        seat_account: Option<&str>,
        days: u32,
    ) -> Result<serde_json::Value> {
        use crate::stats;
        let days = if days == 0 { 3650 } else { days.clamp(7, 3650) };
        // v1.8.2 效能量測：CUM_TIMING=1 時把每一段的耗時印到 stderr（cargo test probe::timing）。
        let timing = std::env::var_os("CUM_TIMING").is_some();
        let t_start = std::time::Instant::now();
        let mut t_last = t_start;
        let mut mark = |label: &str| {
            if timing {
                let now = std::time::Instant::now();
                eprintln!("[timing] {label:<14} {:>7.1} ms (cum {:>7.1} ms)", now.duration_since(t_last).as_secs_f64()*1e3, now.duration_since(t_start).as_secs_f64()*1e3);
                t_last = now;
            }
        };

        // ---- seats (pickable; default = most sampled) ----
        let mut seats: Vec<SeatRow> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT s.id, s.account_uuid, s.org_uuid, \
                        (SELECT COUNT(*) FROM truth_samples t WHERE t.seat_id = s.id), \
                        (SELECT COUNT(*) FROM truth_samples t WHERE t.seat_id = s.id \
                          AND t.source = 'desktop-history'), \
                        s.account_email, s.org_name \
                 FROM seats s ORDER BY 4 DESC",
            )?;
            for row in stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
            })? {
                seats.push(row?);
            }
        }
        let selected = seats
            .iter()
            .find(|s| match seat_account {
                // v1.1.1（D69）：先比 seat id，再退回 account uuid（舊呼叫相容）。
                Some(a) => s.0 == a || s.1.as_deref() == Some(a),
                None => true, // first row = most sampled
            })
            .or(seats.first())
            .cloned();
        // v1.8.11（D89）：全量視角——沒有「選中的座位」，下面凡是按座位的查詢都放寬。
        let all = seat_account == Some(ALL_SEATS);
        let (seat_id, seat_acct, seat_org, desktop_n, seat_email, seat_org_name): (String, Option<String>, Option<String>, i64, Option<String>, Option<String>) = if all {
            if seats.is_empty() {
                return Ok(serde_json::json!({ "seats": [], "empty": true }));
            }
            (ALL_SEATS.to_string(), None, None, 0, None, None)
        } else {
            let Some((id, a, o, _, dn, em, on)) = selected else {
                return Ok(serde_json::json!({ "seats": [], "empty": true }));
            };
            (id, a, o, dn, em, on)
        };
        // v1.4.1（D78 #4）：真值資料的實際起點——期間選 180 天但資料只有 20 天時，畫面要說清楚。
        let data_since: Option<String> = conn
            .query_row(
                "SELECT MIN(fetched_at) FROM truth_samples \
                 WHERE (seat_id = ?1 OR ?1 = '*') AND source IN ('probe','cli-cache')",
                params![seat_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        // Desktop-owned seat gets 'claude-desktop' events; the other 'cli'.
        let entrypoint = if all { "all" } else if desktop_n > 0 { "claude-desktop" } else { "cli" };

        mark("seats");
        // ---- interval pairing over the selected period (5h and 7d ledgers) ----
        let now_utc = chrono::Utc::now();
        let since = (now_utc - chrono::Duration::days(days as i64)).to_rfc3339();
        // v1.9.4（D99）：方案時間軸——區間按所屬方案等級分組，跨方案不比。
        let epochs_all = Self::plan_epochs_all_sync(conn)?;
        let plan_labels = PlanLabels::from_epochs(&epochs_all);
        let no_epochs: Vec<PlanEpoch> = Vec::new();
        let epochs_of = |id: &str| -> &[PlanEpoch] { epochs_all.get(id).map(Vec::as_slice).unwrap_or(&no_epochs) };
        // A4：固定籃基期不受期間選擇影響——5h 配對從帳本最早一筆樣本起做一次，期間內的再從中篩。
        let ledger_start: Option<String> = conn
            .query_row(
                "SELECT MIN(fetched_at) FROM truth_samples WHERE (seat_id = ?1 OR ?1 = '*')",
                params![seat_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        let since_full = match &ledger_start {
            Some(s) if s.as_str() < since.as_str() => s.clone(),
            _ => since.clone(),
        };
        // 全量：每個座位各自配對（跨座位不能配），再合成一疊；下游統計全部吃這一疊。
        let (iv5_full, iv7) = if all {
            let mut a: Vec<Interval> = Vec::new();
            let mut b: Vec<Interval> = Vec::new();
            for (id, _, _, _, dn, _, _) in &seats {
                let ep = if *dn > 0 { "claude-desktop" } else { "cli" };
                a.extend(Self::pair_intervals(conn, id, "'session','five_hour'", &since_full, ep, epochs_of(id))?);
                b.extend(Self::pair_intervals(conn, id, "'weekly_all','seven_day'", &since, ep, epochs_of(id))?);
            }
            a.sort_by(|x, y| x.t0.cmp(&y.t0));
            b.sort_by(|x, y| x.t0.cmp(&y.t0));
            (a, b)
        } else {
            (
                Self::pair_intervals(conn, &seat_id, "'session','five_hour'", &since_full, entrypoint, epochs_of(&seat_id))?,
                Self::pair_intervals(conn, &seat_id, "'weekly_all','seven_day'", &since, entrypoint, epochs_of(&seat_id))?,
            )
        };
        let iv5: Vec<Interval> = iv5_full.iter().filter(|i| i.t0.as_str() >= since.as_str()).cloned().collect();
        // B2：含沒牌價用量的區間，usd 是下限——匯率／檢定／回歸一律剔除，另外報 `unpriced`。
        let iv5_full_ok: Vec<Interval> = iv5_full.iter().filter(|i| !i.unpriced).cloned().collect();
        let iv5_ok: Vec<Interval> = iv5.iter().filter(|i| !i.unpriced).cloned().collect();
        let iv7_ok: Vec<Interval> = iv7.iter().filter(|i| !i.unpriced).cloned().collect();
        // A10：全量視角不合匯率（各座位的池可能不一樣大）——期間消耗照合計，但月匯率／檢定／固定籃不出。
        let (fx5, months5) = if all { (Vec::new(), MonthGroups::default()) } else { Self::fx_months(&iv5_ok, SEED_5H, &plan_labels) };
        let (fx7, months7) = if all { (Vec::new(), MonthGroups::default()) } else { Self::fx_months(&iv7_ok, SEED_7D, &plan_labels) };

        mark("intervals");
        // ---- fixed basket (縮水指數, D64-5): frozen at the first qualified month ----
        // v1.9.4（D99 A4）：基期**按方案等級**各凍一個——該方案等級在整本帳本裡第一個合格月，
        // 不受所選期間影響（以前是「所選期間內第一個合格月」，切 30／90／180 天就換基期，
        // 畫面卻寫「凍結不動」），換方案後也不會整條線永久台階位移。
        // 方案未知的區間自成一個「未知」基期，只為了讓舊月份的圖還畫得出來（verdict 不用它）。
        let mut bases: std::collections::BTreeMap<String, (String, [f64; 5])> = Default::default();
        if !all {
            let full_groups = Self::group_months(&iv5_full_ok);
            for ((m, p), v) in full_groups.chrono() {
                if month_qualified(v) && !bases.contains_key(p) {
                    bases.insert(p.clone(), (m.clone(), stats::basket_shares(v)));
                }
            }
        }
        let basket_est = |s: &[stats::FamIv], b: &[f64; 5]| stats::basket_index(s, b).map(|x| x.index);
        let mut shrink_monthly: Vec<serde_json::Value> = Vec::new();
        {
            let mut prev_plan: Option<&String> = None;
            for ((m, p), v) in months5.chrono() {
                let Some((base_m, b)) = bases.get(p) else { continue };
                let bi = stats::basket_index(v, b);
                let ci = if month_qualified(v) && bi.is_some() {
                    stats::bootstrap_ci(v, |s| basket_est(s, b), BOOT_REPS, SEED_BASKET ^ month_seed(m))
                } else {
                    None
                };
                let mut row = serde_json::json!({
                    "month": m,
                    "isBase": base_m == m,
                    // 前一列是別的方案等級＝圖上的線要在這裡斷開
                    "planBreak": prev_plan.is_some_and(|x| x != p),
                    "index": bi.as_ref().map(|x| x.index),
                    "coverage": bi.as_ref().map(|x| x.coverage),
                    "ci": ci.map(|(a, c)| vec![a, c]),
                });
                plan_labels.stamp(&mut row, p);
                shrink_monthly.push(row);
                prev_plan = Some(p);
            }
        }
        // 舊欄位（baseMonth／basket）＝**目前這個方案**的基期，舊前端照讀不會壞。
        // 「最近」是時間順序的最後一組（不是鍵序的最後一組，見 MonthGroups）
        let latest_plan: Option<String> = months5.chrono().last().map(|(k, _)| k.1.clone());
        let cur_base = latest_plan.as_ref().and_then(|p| bases.get(p));
        let basket_json = |b: &[f64; 5]| {
            (0..5)
                .filter(|&f| b[f] > 0.0005)
                .map(|f| serde_json::json!({ "family": stats::FAMILIES[f], "share": b[f] }))
                .collect::<Vec<_>>()
        };
        let mut bases_json: Vec<serde_json::Value> = bases
            .iter()
            .map(|(p, (m, b))| {
                let mut row = serde_json::json!({ "baseMonth": m, "basket": basket_json(b) });
                plan_labels.stamp(&mut row, p);
                row
            })
            .collect();
        // 按基期月份由舊到新（鍵是方案字串，字母序對前端沒意義）
        bases_json.sort_by(|a, b| a["baseMonth"].as_str().cmp(&b["baseMonth"].as_str()));

        mark("basket");
        // ---- verdict: four gates (D64-6 / 規格 §7.3) ----
        // v1.9.4（D99 A2）：第五道「同一個方案」——兩個月只有在**同一個方案等級、且都不是方案未知**
        // 才可比；同等級但不同時段的月份可以比（主人常在 5x／20x 間切換，只禁跨比會讓多數月份都不能比；
        // 前提是 Anthropic 沒在中間調整同一方案的池——這正是縮水判定要偵測的事）。
        // 時間順序（同一個月有兩個方案組時，先後由組內最後一個區間的時刻決定）
        let qualified5: Vec<&GroupKey> = months5
            .chrono()
            .into_iter()
            .filter(|(_, v)| month_qualified(v) && stats::trimmed_ratio(v).is_some())
            .map(|(k, _)| k)
            .collect();
        let mut gates: Vec<serde_json::Value> = Vec::new();
        let gate = |key: &str, label: &str, pass: bool, detail: String| {
            serde_json::json!({ "key": key, "label": label, "pass": pass, "detail": detail })
        };
        let mut verdict = serde_json::json!({ "declare": false, "comparable": true });
        // 最後一個合格月 ＝ cur；prev ＝ 它之前最近一個**同方案等級**的合格月（可以不是上個月）。
        let comparable_pair: Option<((String, String), (String, String))> = qualified5.split_last().and_then(|(cur, rest)| {
            (cur.1 != PLAN_UNKNOWN)
                .then(|| rest.iter().rev().find(|k| k.1 == cur.1).map(|prev| ((*cur).clone(), (*prev).clone())))
                .flatten()
        });
        let seat_epochs: &[PlanEpoch] = if all { &[] } else { epochs_of(&seat_id) };
        if all {
            gates.push(gate(
                "plan",
                "同一個方案",
                false,
                "全部帳號視角不合併匯率——各帳號的額度池可能不一樣大，請選單一帳號再看".into(),
            ));
            verdict["comparable"] = serde_json::json!(false);
            verdict["reason"] = serde_json::json!("all_seats");
            verdict["sentence"] = serde_json::json!("全部帳號視角不合併匯率（各帳號的額度池可能不一樣大），請選單一帳號。");
        } else if qualified5.len() < 2 {
            // v1.7（D81）：四項檢查的名字與說明改白話（畫面與匯出報告共用這些字串；
            // 報告的 method 段仍寫術語）。
            gates.push(gate(
                "sample",
                "樣本數",
                false,
                format!("樣本足夠的月份只有 {} 個，比較需要兩個（每月需 {MIN_N} 筆配對、額度累計掉 {MIN_DELTA_SUM:.0}%）", qualified5.len()),
            ));
        } else if let Some((cur_k, prev_k)) = comparable_pair {
            let (cur_m, prev_m) = (cur_k.0.clone(), prev_k.0.clone());
            let cur = &months5[&cur_k];
            let prev = &months5[&prev_k];
            let r_cur = stats::trimmed_ratio(cur).unwrap();
            let r_prev = stats::trimmed_ratio(prev).unwrap();
            let mom_pct = (r_cur / r_prev - 1.0) * 100.0;
            let wallet_pct = (r_prev / r_cur - 1.0) * 100.0;
            let plan_name = plan_labels.label(&cur_k.1);
            let apart = months_between(&prev_m, &cur_m);
            gates.push(gate(
                "plan",
                "同一個方案",
                true,
                if apart == 1 {
                    format!("兩個月都是 {plan_name}")
                } else {
                    format!("兩個月都是 {plan_name}（中間隔了 {} 個月，那段不是這個方案或資料不夠）", apart - 1)
                },
            ));
            gates.push(gate(
                "sample",
                "樣本數",
                true,
                format!("{prev_m} {} 筆 · {cur_m} {} 筆", prev.len(), cur.len()),
            ));
            let t5 = stats::perm_test(prev, cur, stats::trimmed_ratio, PERM_REPS, SEED_PERM);
            let (p5, d5) = match &t5 {
                Some(t) => (Some(t.p), t.diff),
                None => (None, 0.0),
            };
            let pass5 = p5.map(|p| p < ALPHA).unwrap_or(false);
            gates.push(gate(
                "test5h",
                "5h 匯率變化",
                pass5,
                match p5 {
                    Some(p) => format!(
                        "5h 匯率 ${r_prev:.2} → ${r_cur:.2}，差異{}顯著（p={p:.3}）",
                        if pass5 { "夠" } else { "不夠" }
                    ),
                    None => "算不出來（樣本太少）".into(),
                },
            ));
            // 7d cross-check: same direction and past the same gate.
            let (pass7, detail7, p7) = match (months7.get(&prev_k), months7.get(&cur_k)) {
                (Some(a), Some(b)) if month_qualified(a) && month_qualified(b) => {
                    match stats::perm_test(a, b, stats::trimmed_ratio, PERM_REPS, SEED_PERM ^ 7) {
                        Some(t) => {
                            let same = (t.diff > 0.0) == (d5 > 0.0);
                            let ra = stats::trimmed_ratio(a).unwrap_or(0.0);
                            let rb = stats::trimmed_ratio(b).unwrap_or(0.0);
                            (
                                same && t.p < ALPHA,
                                format!(
                                    "7d 匯率 ${ra:.2} → ${rb:.2}，與 5h {}（p={:.3}）",
                                    if same { "同向" } else { "反向" },
                                    t.p
                                ),
                                Some(t.p),
                            )
                        }
                        None => (false, "7d 算不出來（樣本太少）".into(), None),
                    }
                }
                _ => (false, "7d 樣本不夠".into(), None),
            };
            gates.push(gate("cross7d", "7d 方向一致", pass7, detail7));
            // fixed-basket gate
            let (pass_b, detail_b, p_b) = match bases.get(&cur_k.1).map(|(_, b)| b) {
                Some(b) => match stats::perm_test(prev, cur, |s| basket_est(s, b), PERM_REPS, SEED_PERM ^ 11) {
                    Some(t) => {
                        let same = (t.diff > 0.0) == (d5 > 0.0);
                        let ia = basket_est(prev, b).unwrap_or(0.0);
                        let ib = basket_est(cur, b).unwrap_or(0.0);
                        (
                            same && t.p < ALPHA,
                            format!(
                                "固定模型組合的匯率 ${ia:.2} → ${ib:.2}，{}（p={:.3}）",
                                if same { "同向" } else { "反向" },
                                t.p
                            ),
                            Some(t.p),
                        )
                    }
                    None => (false, "有一個月算不出來（單一模型的記錄不夠）".into(), None),
                },
                None => (false, "沒有可當基準的月份".into(), None),
            };
            gates.push(gate("basket", "固定模型組合", pass_b, detail_b));
            let declare = gates.iter().all(|g| g["pass"].as_bool() == Some(true));
            verdict = serde_json::json!({
                "declare": declare,
                "comparable": true,
                "curMonth": cur_m,
                "prevMonth": prev_m,
                // prevMonth 不一定是上個月（隔幾個月才有同方案的合格月）——前端別寫死「較上月」
                "monthsApart": apart,
                "plan": cur_k.1,
                "planLabel": plan_name,
                "rateCur": r_cur,
                "ratePrev": r_prev,
                "momPct": mom_pct,
                "walletPct": wallet_pct,
                "p5h": p5,
                "p7d": p7,
                "pBasket": p_b,
            });
        } else {
            // 兩個合格月以上，但最近的那個月找不到同方案等級的對手：換過方案、或方案未知。
            // 不宣告、不給 momPct（前端就不會畫「較上月」chip）；句子照 D99 Q2。
            let (cur_k, other_k) = (qualified5[qualified5.len() - 1], qualified5[qualified5.len() - 2]);
            let r_cur = stats::trimmed_ratio(&months5[cur_k]);
            let (reason, sentence, from, to, switch_on) = if cur_k.1 == PLAN_UNKNOWN {
                // 兩種情況：這個座位從沒被記過方案；或 App 剛開始記（最近的合格月多半還在記錄之前）
                let began = seat_epochs.first().map(|e| e.since_t.with_timezone(&chrono::Local).format("%m-%d").to_string());
                (
                    if began.is_some() { "plan_unknown_history" } else { "plan_unknown" },
                    match &began {
                        Some(d) => format!("App 從 {d} 才開始記錄方案，最近的月份大部分還在那之前，方案未知，不判斷縮水。"),
                        None => "這個帳號的方案還沒被記錄過（登入它一次才會開始記），最近的月份方案未知，不判斷縮水。".to_string(),
                    },
                    plan_labels.label(&other_k.1),
                    plan_labels.label(&cur_k.1),
                    began,
                )
            } else {
                let on = plan_switch_date(seat_epochs, &cur_k.1);
                let to = plan_labels.label(&cur_k.1);
                if other_k.1 == PLAN_UNKNOWN {
                    (
                        "plan_unknown_before",
                        format!(
                            "方案從 {} 才開始有記錄，{} 以前的方案未知，不能跨比。",
                            on.as_deref().unwrap_or("最近"),
                            on.as_deref().unwrap_or("那時"),
                        ),
                        plan_labels.label(&other_k.1),
                        to,
                        on,
                    )
                } else {
                    let from = plan_labels.label(&other_k.1);
                    (
                        "plan_changed",
                        format!("這期間換過方案（{from}→{to}，{}），不能跨比。", on.as_deref().unwrap_or("日期不明")),
                        from,
                        to,
                        on,
                    )
                }
            };
            gates.push(gate("plan", "同一個方案", false, sentence.clone()));
            verdict = serde_json::json!({
                "declare": false,
                "comparable": false,
                "reason": reason,
                "sentence": sentence,
                "curMonth": cur_k.0,
                "prevMonth": other_k.0,
                "plan": cur_k.1,
                "planLabel": plan_labels.label(&cur_k.1),
                "rateCur": r_cur,
                "planCross": {
                    "fromLabel": from, "toLabel": to, "fromPlan": other_k.1, "toPlan": cur_k.1, "switchOn": switch_on,
                },
            });
        }
        verdict["gates"] = serde_json::Value::Array(gates);

        mark("verdict");
        // ---- per-model quota multipliers over the whole period (D55 → M5 → v1.6 D80 回歸) ----
        // v1.9.4（D99 A5）：模型倍率只用「最近一個方案等級」的區間——方案效應（20x 的 Δ% 只有 5x 的四分之一）
        // 進殘差會讓所有倍率失真。最近的方案＝最近一個「有方案可歸」的區間所屬的等級（同等級不同時段併用）；
        // 整個座位都沒有方案記錄（從沒登入過）→ 退回用全部，並標 `scope.planKnown=false`。
        // 剛換方案、新方案的區間還不夠門檻 → 回歸自然回「校準中」。全量視角不做（各座位的池不一樣大）。
        let period5_all: Vec<stats::FamIv> = iv5_ok.iter().map(|i| i.iv.clone()).collect();
        // 全量視角不做（各座位的池不一樣大）→ 不宣稱任何方案（21r1 nit 1）
        let reg_plan: Option<String> =
            if all { None } else { iv5_ok.iter().rev().find_map(|i| i.plan.clone()) };
        // 有牌價、但不屬於任何家族欄的模型（例如 Mythos）：美元算進匯率，回歸沒有它的自變數 → 這些區間不進回歸
        let reg_ivs: Vec<Interval> = if all {
            Vec::new()
        } else {
            match &reg_plan {
                Some(p) => iv5_ok.iter().filter(|i| !i.no_family && i.plan.as_ref() == Some(p)).cloned().collect(),
                None => iv5_ok.iter().filter(|i| !i.no_family).cloned().collect(),
            }
        };
        let reg_scope = serde_json::json!({
            "plan": if all { serde_json::Value::Null } else { serde_json::json!(reg_plan.clone().unwrap_or_else(|| PLAN_UNKNOWN.to_string())) },
            "planLabel": if all { serde_json::Value::Null } else { serde_json::json!(plan_labels.label(reg_plan.as_deref().unwrap_or(PLAN_UNKNOWN))) },
            "planKnown": reg_plan.is_some(),
            "intervals": reg_ivs.len(),
            // 合計（舊欄位，別處還在讀）＝ 下面三種之和
            "excludedIntervals": if all { 0 } else { iv5_ok.len() - reg_ivs.len() },
            // 審查 23r（線 F 交辦）：排除的原因拆開——換方案前（另一個已知方案的池）vs 方案未知（不確定是不是同一個）
            "excludedOtherPlan": if all || reg_plan.is_none() { 0 } else {
                iv5_ok.iter().filter(|i| !i.no_family && i.plan.is_some() && i.plan != reg_plan).count()
            },
            "excludedUnknownPlan": if all || reg_plan.is_none() { 0 } else {
                iv5_ok.iter().filter(|i| !i.no_family && i.plan.is_none()).count()
            },
            "excludedNoFamily": if all { 0 } else { iv5_ok.iter().filter(|i| i.no_family).count() },
            "allSeats": all,
        });
        // 期間裡混了好幾個方案（含「方案未知」）→ 下面幾個「每 1% 多少」的數字改只算目前方案（21r1 S4）
        let mixed_plans = iv5_ok
            .iter()
            .map(|i| i.plan.clone().unwrap_or_else(|| PLAN_UNKNOWN.to_string()))
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            > 1;
        let all5: Vec<stats::FamIv> = reg_ivs.iter().map(|i| i.iv.clone()).collect();
        let period_trim = stats::trimmed_ratio(&all5);
        let period_raw = stats::ratio(&all5);
        // 回歸吃跟 house 匯率同一套修剪（砍每段匯率下尾 25%／上尾 5%——本機看不到的用量
        // 把低尾污染了），但修剪對象是 Interval 本身，才留得住 by_model。
        let kept: Vec<&Interval> = stats::trim(&reg_ivs, stats::TRIM_LO, stats::TRIM_HI);
        let design = stats::Design::from_sparse(kept.iter().map(|i| (i.by_model.as_slice(), i.iv.delta)));
        let reg = stats::regress(&design, BOOT_REPS, SEED_FAMILY);
        // 家族綜合費率＝各模型合併成一欄再回歸（主人 2026-09-18：四家族，Sonnet 5 與 4.x
        // 同一家）；同一列也掛純區間法當交叉驗證（舊法保留當對照，D80 決策點 4）。
        let fam_reg = stats::regress(&design.collapse_to_groups(), BOOT_REPS, SEED_FAMILY ^ 0x5a5a);
        let fam_rates = stats::group_rates(&all5);
        let families: Vec<serde_json::Value> = (0..stats::GROUPS.len())
            .filter_map(|f| {
                let pure = fam_rates[f];
                let r = fam_reg.rows.iter().find(|r| r.model == stats::GROUPS[f])?;
                if pure.is_none() && r.n_seg == 0 {
                    return None;
                }
                // 一致＝純區間匯率落在回歸區間內，或兩者差不到 25%
                let agree = match (pure, r.rate, r.ci) {
                    (Some((p, _)), Some(rr), ci) => {
                        let in_ci = ci.map(|(lo, hi)| p >= lo && hi.is_none_or(|h| p <= h)).unwrap_or(false);
                        Some(in_ci || (p / rr).ln().abs() < 0.25f64.ln_1p())
                    }
                    _ => None,
                };
                Some(serde_json::json!({
                    "family": stats::GROUPS[f],
                    "pureRate": pure.map(|(r, _)| r),
                    "pureN": pure.map(|(_, n)| n),
                    "regRate": r.rate,
                    "regCi": r.ci.map(|(lo, hi)| vec![Some(lo), hi]),
                    "regStatus": r.status,
                    "zeroShare": r.zero_share,
                    "nSeg": r.n_seg,
                    // v1.8.7（D86 優化 1）：倍率反轉成「這個模型的 $/1% ÷ 期間 $/1%」——
                    // >1＝同樣 1% 額度換到比平均多的 API 用量（划算），量條越長越好。
                    // 以前是倒數（>1＝傷額度），主人說跟「數值大＝用量多＝划算」的直覺相反。
                    "multiplier": match (period_trim, r.rate) {
                        (Some(p), Some(rate)) if r.status == "ok" && p > 0.0 => Some(rate / p),
                        _ => None,
                    },
                    "agree": agree,
                }))
            })
            .collect();
        let rows: Vec<serde_json::Value> = reg
            .rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "model": r.model,
                    "label": stats::model_label(&r.model),
                    "family": r.family,
                    "rate": r.rate,
                    "ci": r.ci.map(|(lo, hi)| vec![Some(lo), hi]),
                    "nSeg": r.n_seg,
                    "zeroShare": r.zero_share,
                    "status": r.status,
                    "collinearWith": r.collinear_with.as_deref().map(stats::model_label),
                    "multiplier": match (period_trim, r.rate) {
                        (Some(p), Some(rate)) if r.status == "ok" && p > 0.0 => Some(rate / p),
                        _ => None,
                    },
                })
            })
            .collect();
        let by_model = serde_json::json!({
            "method": "wnnls",
            "n": reg.n,
            "r2": reg.r2,
            "rows": rows,
            "families": families,
            // D99 A5：這份回歸用了哪個方案的哪些區間（畫面要寫「只用目前方案 X 的 N 段」）
            "scope": reg_scope,
        });

        mark("regression");
        // ---- peak heatmap (local weekday × hour, Σ positive Δ%) + 6h bands ----
        let mut heat = vec![0.0f64; 7 * 24];
        for i in &iv5 {
            heat[(i.weekday * 24 + i.hour) as usize] += i.iv.delta;
        }
        // 21r1 S4：「每 1% 多少」這一類的數字隨池大小縮放——全量視角（各座位的池不同）不給；
        // 單座位但期間混了好幾個方案時只算目前方案（跟 byModel／planScoped 同一批區間）。
        let scoped_ivs: &[Interval] = if mixed_plans { &reg_ivs } else { &iv5_ok };
        let bands: Vec<serde_json::Value> = (0..4)
            .map(|b| {
                let sub: Vec<stats::FamIv> = scoped_ivs
                    .iter()
                    .filter(|i| i.hour / 6 == b)
                    .map(|i| i.iv.clone())
                    .collect();
                serde_json::json!({
                    "label": format!("{:02}–{:02}", b * 6, b * 6 + 6),
                    "n": sub.len(),
                    "rate": if sub.len() >= MIN_N && !all { stats::trimmed_ratio(&sub) } else { None },
                })
            })
            .collect();

        mark("heatmap");
        // ---- truth reconciliation (D64-7): Δ% the local ledger cannot explain ----
        // 單一方案的期間：吃整個期間；混了方案：只吃目前方案（all5）——修剪下尾 25% 會把
        // 「池比較大所以每 1% 美元比較多」的區間當成「本機看不到的用量」，混池的 1−raw÷trim 不是真的未解釋比例。
        // 期間匯率另外給「只算目前方案」的版本（planScoped）。全量視角一律不給。
        let recon_set: &[stats::FamIv] = if mixed_plans { &all5 } else { &period5_all };
        let zero_delta: f64 = recon_set.iter().filter(|x| x.usd < 0.01).map(|x| x.delta).sum();
        let all_delta: f64 = recon_set.iter().map(|x| x.delta).sum();
        let period_raw_all = stats::ratio(&period5_all);
        let period_trim_all = stats::trimmed_ratio(&period5_all);
        let unexplained = match (stats::ratio(recon_set), stats::trimmed_ratio(recon_set)) {
            (Some(r), Some(t)) if t > 0.0 && !all => Some((1.0 - r / t).clamp(0.0, 1.0)),
            _ => None,
        };

        // work rate from the last 30 days of intervals
        // v1.8.13（D91 T-13）：跟 Interval.month 同一把尺（本地月）。
        // 21r1 S4：turns／output tokens「每 1%」同樣隨池大小縮放——混方案時只算目前方案，全量視角不給。
        let cutoff = (chrono::Local::now() - chrono::Duration::days(30)).format("%Y-%m").to_string();
        let (mut wd, mut wt, mut wo) = (0.0f64, 0i64, 0i64);
        if !all {
            for iv in scoped_ivs.iter().filter(|i| i.month >= cutoff) {
                wd += iv.iv.delta;
                wt += iv.turns;
                wo += iv.out_tokens;
            }
        }

        mark("reconcile");
        // ---- daily USD + walls + gaps + attribution: this seat's tenure only ----
        // v1.8.7（D86 蟲 3）：以前這三塊只按 entrypoint 過濾（兩個座位都有 Desktop 樣本
        // → 都是 'claude-desktop' → 看到同一批事件），切帳號怎麼切都一樣。現在按
        // 「這個座位在 CLI／Desktop 各自登入中的時段」過濾（見 seat_tenure_sync）。
        let since60 = since.clone();
        let tenure = if all { Vec::new() } else { Self::seat_tenure_sync(conn, &seat_id, &since60)? };
        // 全量視角不套登入時段：條件恆真（含最早觀測之前的用量、含雲端 session）。
        let (tenure_ev_sql, tenure_ev_params) = if all { ("1".to_string(), Vec::new()) } else { tenure_where(&tenure, true) };
        let (tenure_ts_sql, tenure_ts_params) = if all { ("1".to_string(), Vec::new()) } else { tenure_where(&tenure, false) };
        let mut daily: Vec<serde_json::Value> = Vec::new();
        {
            // v1.8.13（D91 T-13）：日條按本地日切（跟歷史頁熱圖 local_day_of 同口徑；以前
            // substr(ts,1,10) 是 UTC，台北 00:00～08:00 的用量歸到前一天）。SQLite 的
            // 'localtime' 跟 chrono::Local 讀同一個系統時區，local_day_tests 逐筆對過。
            let mut stmt = conn.prepare(&format!(
                "SELECT date(ts,'localtime'), model, COALESCE(speed,''), COUNT(*), \
                        SUM(input_tokens), SUM(cache_creation), SUM(COALESCE(cache_1h,0)), \
                        SUM(cache_read), SUM(output_tokens), SUM(COALESCE(web_searches,0)), {LONG_PROMPT_SQL} \
                 FROM usage_events WHERE ts >= ?1 AND ({tenure_ev_sql}) \
                 GROUP BY 1, 2, 3, 11 ORDER BY 1"
            ))?;
            let mut per_day: std::collections::BTreeMap<String, f64> = Default::default();
            let mut p: Vec<String> = vec![since60.clone()];
            p.extend(tenure_ev_params.iter().cloned());
            for row in stmt.query_map(rusqlite::params_from_iter(p.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(9)?.unwrap_or(0),
                    r.get::<_, i64>(10)? != 0,
                ))
            })? {
                let (day, model, speed, inp, cw, cw1, cr, out, ws, long) = row?;
                // B2：沒牌價的用量這裡不加（日條是「有牌價的部分」；沒牌價的另在 `unpriced` 報）。
                *per_day.entry(day).or_default() +=
                    price_usd(&model, &speed, inp, cw, cw1, cr, out, ws, long).unwrap_or(0.0);
            }
            for (day, usd) in per_day {
                daily.push(serde_json::json!({ "day": day, "usd": (usd * 100.0).round() / 100.0 }));
            }
        }

        let mut walls: Vec<serde_json::Value> = Vec::new();
        {
            // 429 錨點來自本機 JSONL、沒有帳號欄位——用這個座位的登入時段（CLI＋Desktop 聯集）挑。
            // E4：伺服器限流（內文 "not your usage limit"）不是額度牆，不列。
            let mut stmt = conn.prepare(&format!(
                "SELECT ts, rate_limit_type, content_text FROM anchors \
                 WHERE ts >= ?1 AND ({tenure_ts_sql}) \
                   AND NOT (rate_limit_type IS NULL AND COALESCE(content_text,'') LIKE '%not your usage limit%') \
                 ORDER BY ts"
            ))?;
            let mut p: Vec<String> = vec![since60.clone()];
            p.extend(tenure_ts_params.iter().cloned());
            for row in stmt.query_map(rusqlite::params_from_iter(p.iter()), |r| {
                let kind: Option<String> = r.get(1)?;
                let text: Option<String> = r.get(2)?;
                Ok(serde_json::json!({
                    "at": r.get::<_, String>(0)?,
                    "kind": kind,
                    // 型別是 NULL 的照內文歸（Fable 牆不再被算成 5h 牆）：five_hour／seven_day／fable／other
                    "class": wall_class(kind.as_deref(), text.as_deref()),
                }))
            })? {
                walls.push(row?);
            }
        }
        // Days where any limit of the selected seat was observed at 100%
        // (truth samples). Web-side 429s never reach local JSONL anchors,
        // but the probe still sees the saturated percent — this is the
        // wall signal a web-only day would otherwise hide.
        let mut saturated_days: Vec<String> = Vec::new();
        {
            // v1.8.13（D91 T-13）：見頂日也按本地日。
            let mut stmt = conn.prepare(
                "SELECT DISTINCT date(fetched_at,'localtime') FROM truth_samples \
                 WHERE (seat_id = ?1 OR ?1 = '*') AND percent >= 100 AND fetched_at >= ?2 ORDER BY 1",
            )?;
            for row in stmt.query_map(params![seat_id, since60], |r| r.get::<_, String>(0))? {
                saturated_days.push(row?);
            }
        }
        let mut gaps: Vec<serde_json::Value> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT started_at, ended_at FROM gaps WHERE started_at >= ?1 ORDER BY started_at DESC LIMIT 40",
            )?;
            for row in stmt.query_map(params![since60], |r| {
                Ok(serde_json::json!({
                    "from": r.get::<_, String>(0)?,
                    "to": r.get::<_, String>(1)?,
                }))
            })? {
                gaps.push(row?);
            }
        }

        mark("daily");
        // ---- attribution (this seat's tenure, period, by entrypoint) ----
        let since30 = since.clone();
        // v1.9.3（D97）：按 API 計價分（原本按輸出 tokens）。
        let attribution = {
            let mut p: Vec<String> = vec![since30.clone()];
            p.extend(tenure_ev_params.iter().cloned());
            usage_mix(
                conn,
                ENTRYPOINT_KEY_SQL,
                "entrypoint",
                &format!("ts >= ?1 AND ({tenure_ev_sql})"),
                &p,
                0,
            )?
        };

        mark("attribution");
        // ---- v1.9.4（D99）方案區塊、無牌價、全量視角的各座位摘要 ----
        let plan_block = if all {
            serde_json::json!({ "current": null, "segments": [], "changes": [], "crossesPlan": false,
                                "crossesKnownPlans": false, "mixedPlans": mixed_plans, "allSeats": true })
        } else {
            Self::plan_block(
                epochs_of(&seat_id),
                parse_utc_or_min(&since),
                &plan_labels,
                &iv5_ok,
                mixed_plans,
            )
        };
        let per_seat: Vec<serde_json::Value> = if all {
            seats
                .iter()
                .map(|(id, _, _, _, _, em, on)| {
                    let mine: Vec<&Interval> = iv5_ok.iter().filter(|i| &i.seat_id == id).collect();
                    let plan = mine.iter().rev().find_map(|i| i.plan.clone());
                    let scoped: Vec<stats::FamIv> = mine
                        .iter()
                        .filter(|i| plan.is_none() || i.plan == plan)
                        .map(|i| i.iv.clone())
                        .collect();
                    serde_json::json!({
                        "seat": id, "email": em, "orgName": on,
                        "plan": plan_summary(epochs_of(id)),
                        "scopePlan": plan.clone().unwrap_or_else(|| PLAN_UNKNOWN.to_string()),
                        "scopePlanLabel": plan_labels.label(plan.as_deref().unwrap_or(PLAN_UNKNOWN)),
                        "intervals": scoped.len(),
                        "deltaSum": scoped.iter().map(|x| x.delta).sum::<f64>(),
                        "rateTrim": stats::trimmed_ratio(&scoped),
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        let tainted: Vec<&Interval> = iv5.iter().filter(|i| i.unpriced).collect();
        let tainted_delta: f64 = tainted.iter().map(|i| i.iv.delta).sum();
        let all_iv5_delta: f64 = iv5.iter().map(|i| i.iv.delta).sum();
        let unpriced_block = serde_json::json!({
            "models": Self::unpriced_models_sync(conn, Some(&since))?,
            "intervals": tainted.len(),
            "deltaShare": if all_iv5_delta > 0.0 { Some(tainted_delta / all_iv5_delta) } else { None },
        });
        let sel_epochs: &[PlanEpoch] = if all { &[] } else { epochs_of(&seat_id) };
        Ok(serde_json::json!({
            "seats": seats.iter().map(|(id, a, o, n, dn, em, on)| serde_json::json!({
                "id": id, "accountUuid": a, "orgUuid": o, "samples": n,
                "isDesktop": *dn > 0, "email": em, "orgName": on,
                "plan": plan_summary(epochs_of(id)),
            })).collect::<Vec<_>>(),
            "selected": {
                "id": seat_id, "accountUuid": seat_acct, "orgUuid": seat_org, "entrypoint": entrypoint,
                "email": seat_email, "orgName": seat_org_name,
                // A8：方案跟著「被看的座位」（不是目前登入的帳號），並帶讀取時刻（plan.observedAt）
                "plan": if all { serde_json::Value::Null } else { plan_summary(sel_epochs) },
            },
            "plan": plan_block,
            "perSeat": per_seat,
            "unpriced": unpriced_block,
            // "month": 全量視角 fx5／fx7 為空（A10）；其餘視角每列是（月, 方案等級）一組，同一個月可以有兩列
            "fxMode": if all { "per_seat" } else { "plan_month" },
            "fx5": fx5,
            "fx7": fx7,
            "dataSince": data_since,
            // v1.8.7（D86 蟲 3）：期間消耗／撞牆／來源歸因只算這些登入時段；最早一段之前
            // 的用量沒有帳號依據、不歸任何座位（前端用 from 說清楚）。
            "tenure": {
                "all": all,
                "spans": tenure.len(),
                "cli": tenure.iter().filter(|s| s.entrypoint == "cli").count(),
                "desktop": tenure.iter().filter(|s| s.entrypoint == "claude-desktop").count(),
                "from": tenure.iter().map(|s| s.start.clone()).min(),
            },
            "verdict": verdict,
            "shrink": {
                // 舊欄位＝目前這個方案的基期；每個方案等級各一個基期在 `bases`（A4）
                "baseMonth": cur_base.map(|(m, _)| m.clone()),
                "basket": cur_base.map(|(_, b)| basket_json(b)),
                "bases": bases_json,
                "monthly": shrink_monthly,
            },
            "byModel": by_model,
            "heat": { "cells": heat, "bands": bands },
            "reconcile": {
                // 整個期間（可能混了好幾個方案——看 mixedPlans；全量視角兩個匯率不給）
                "rateRaw": if all { None } else { period_raw_all },
                "rateTrim": if all { None } else { period_trim_all },
                "mixedPlans": mixed_plans,
                // 只算目前方案（跟 byModel.scope 同一批區間）的期間匯率
                "planScoped": if all { serde_json::Value::Null } else {
                    let mut v = serde_json::json!({
                        "rateRaw": period_raw, "rateTrim": period_trim, "intervals": all5.len(),
                        // 審查 23r（線 F 交辦）：前端守門要同時看段數（minN）與 Δ 合計（minDeltaSum）
                        "deltaSum": all5.iter().map(|i| i.delta).sum::<f64>(),
                    });
                    v["scope"] = by_model["scope"].clone();
                    v
                },
                "unexplainedShare": unexplained,
                "zeroEventDeltaShare": if all_delta > 0.0 && !all { Some(zero_delta / all_delta) } else { None },
                "intervals": period5_all.len(),
            },
            "method": {
                "trim": [stats::TRIM_LO, stats::TRIM_HI],
                "bootstrapReps": BOOT_REPS,
                "permReps": PERM_REPS,
                "alpha": ALPHA,
                "minN": MIN_N,
                "minDeltaSum": MIN_DELTA_SUM,
                "pureShare": stats::PURE_SHARE,
                "familyMinN": stats::FAMILY_MIN_N,
                "basketMinCoverage": stats::BASKET_MIN_COVERAGE,
                // v1.6（D80）：分模型倍率的回歸參數
                "regression": {
                    "weight": "1/usd",
                    "intercept": false,
                    "minSegments": stats::REG_MIN_SEG,
                    "zeroShareMax": stats::REG_ZERO_SHARE_MAX,
                    "maxCiRatio": stats::REG_MAX_CI_RATIO,
                    "collinearR": stats::REG_COLLINEAR_R,
                },
            },
            "workRate": {
                "turnsPerPercent": if wd >= 3.0 { Some(wt as f64 / wd) } else { None },
                "outputTokensPerPercent": if wd >= 3.0 { Some(wo as f64 / wd) } else { None },
            },
            "daily": daily,
            "walls": walls,
            "saturatedDays": saturated_days,
            "gaps": gaps,
            "attribution": attribution,
        }))
    }

    /// One-stop data-health snapshot for the M1 健康度 page. Returned as
    /// loose JSON — the page renders whatever is here, so the shape can grow
    /// without an IPC/type-mirror dance on every addition.
    pub async fn health_snapshot(&self) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;

        let count = |sql: &str| -> Result<i64> {
            Ok(conn.query_row(sql, [], |r| r.get(0))?)
        };

        let mut by_source = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT source, COUNT(*), MIN(fetched_at), MAX(fetched_at) \
                 FROM truth_samples GROUP BY source ORDER BY source",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(serde_json::json!({
                    "source": r.get::<_, String>(0)?,
                    "count": r.get::<_, i64>(1)?,
                    "first": r.get::<_, Option<String>>(2)?,
                    "last": r.get::<_, Option<String>>(3)?,
                }))
            })?;
            for row in rows {
                by_source.push(row?);
            }
        }

        let mut seats = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT s.account_uuid, s.org_uuid, s.first_seen_at, \
                        (SELECT COUNT(*) FROM truth_samples t WHERE t.seat_id = s.id), \
                        s.account_email, s.org_name, s.id \
                 FROM seats s ORDER BY s.last_seen_at DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(serde_json::json!({
                    "accountUuid": r.get::<_, Option<String>>(0)?,
                    "orgUuid": r.get::<_, Option<String>>(1)?,
                    "firstSeenAt": r.get::<_, String>(2)?,
                    "sampleCount": r.get::<_, i64>(3)?,
                    "email": r.get::<_, Option<String>>(4)?,
                    "orgName": r.get::<_, Option<String>>(5)?,
                    "id": r.get::<_, String>(6)?,
                }))
            })?;
            for row in rows {
                seats.push(row?);
            }
        }

        // Coverage: distinct hours holding ≥1 truth sample over the window's
        // total hours — an honest "how much of the timeline do we actually
        // see" number (Desktop off / machine asleep hours count against it).
        let coverage = |days: i64| -> Result<f64> {
            let since = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
            let hours: i64 = conn.query_row(
                "SELECT COUNT(DISTINCT substr(fetched_at, 1, 13)) FROM truth_samples \
                 WHERE fetched_at >= ?1",
                params![since],
                |r| r.get(0),
            )?;
            Ok((hours as f64 / (days as f64 * 24.0)).min(1.0))
        };

        let mut gaps = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT source, started_at, ended_at FROM gaps \
                 ORDER BY started_at DESC LIMIT 20",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(serde_json::json!({
                    "source": r.get::<_, String>(0)?,
                    "startedAt": r.get::<_, String>(1)?,
                    "endedAt": r.get::<_, String>(2)?,
                }))
            })?;
            for row in rows {
                gaps.push(row?);
            }
        }

        // v1.9.3（D97）：按 API 計價排（原本按輸出 tokens）。
        let top_models = usage_mix(&conn, "model", "model", "1", &[], 8)?;

        let doorbell_futile: i64 = conn
            .query_row(
                "SELECT value FROM channel_state WHERE key = 'doorbell_futile_count'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        // v1.8.7（D86 蟲 5）／v1.8.8（D87 按來源分開）：門鈴按了但密碼牌對不上
        // （server.rs 的 401 路徑記的）。回傳每個來源一列，只列還有計數的。
        let mut doorbell_unauthorized: Vec<serde_json::Value> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT substr(key, 23), value FROM channel_state \
                 WHERE key LIKE 'doorbell_unauth_count:%' AND CAST(value AS INTEGER) > 0",
            )?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
                let (source, n) = row?;
                let last: Option<String> = conn
                    .query_row(
                        "SELECT value FROM channel_state WHERE key = ?1",
                        params![format!("doorbell_unauth_last:{source}")],
                        |r| r.get::<_, String>(0),
                    )
                    .ok();
                doorbell_unauthorized.push(serde_json::json!({
                    "source": source,
                    "count": n.parse::<i64>().unwrap_or(0),
                    "last": last,
                }));
            }
        }

        Ok(serde_json::json!({
            "doorbellFutileCount": doorbell_futile,
            "doorbellUnauthorized": doorbell_unauthorized,
            "truthSamples": count("SELECT COUNT(*) FROM truth_samples")?,
            "usageEvents": count("SELECT COUNT(*) FROM usage_events")?,
            "anchors": count("SELECT COUNT(*) FROM anchors")?,
            "gapCount": count("SELECT COUNT(*) FROM gaps")?,
            "bySource": by_source,
            "seats": seats,
            "coverage7d": coverage(7)?,
            "coverage30d": coverage(30)?,
            "recentGaps": gaps,
            "topModels": top_models,
            // v1.9.4（D99 B2）：有用量卻沒牌價的模型（它們的 API 計價沒算進任何匯率與分佈）
            "unpricedModels": Self::unpriced_models_sync(&conn, None)?,
            "eventsSpan": {
                "first": conn.query_row("SELECT MIN(ts) FROM usage_events", [], |r| r.get::<_, Option<String>>(0))?,
                "last": conn.query_row("SELECT MAX(ts) FROM usage_events", [], |r| r.get::<_, Option<String>>(0))?,
            },
        }))
    }

    /// Online backup via `VACUUM INTO` — compact, safe against concurrent writers.
    ///
    /// v1.8.13（D91 T-05）：以前按「檔數留 7 份」——每次開機一份＋每 24 小時一份，重裝日一天
    /// 就吃掉好幾份（09-21 一天 11 份，現存 7 份只涵蓋 3 天）；VACUUM INTO 被打斷會留下
    /// `-journal` 孤兒、清理只認 `.sqlite` 永遠清不掉。現在：
    /// - 先寫 `.tmp`、寫完才 rename 成正式檔名——半成品永遠不會長得像一份備份；
    /// - 每個**本地日曆日**只留最新一份，留最近 `keep_days` 個有備份的日子
    ///   （不是「今天往前 7 天」：App 關了十天再開，舊備份不會一口氣被清光）；
    /// - 一小時前的 `.tmp`／`-journal` 殘骸一起清（剛寫的不碰，可能是另一個實例正在寫）。
    ///
    /// 檔名時間仍是 UTC（`ledger-YYYYMMDD-HHMMSS.sqlite`，跟舊檔同格式），分日時才換成本地日。
    pub async fn backup_into(&self, dir: &std::path::Path, keep_days: usize) -> Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
        let dest = dir.join(format!("ledger-{stamp}.sqlite"));
        let tmp = dir.join(format!("ledger-{stamp}.sqlite.tmp"));
        // VACUUM INTO 要求目標不存在（或是空檔）。
        let _ = std::fs::remove_file(&tmp);
        let vacuum = {
            let conn = self.conn.lock().await;
            conn.execute("VACUUM INTO ?1", params![tmp.to_string_lossy().to_string()])
        };
        if let Err(e) = vacuum {
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::remove_file(dir.join(format!("ledger-{stamp}.sqlite.tmp-journal")));
            return Err(e.into());
        }
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("rename {} → {}", tmp.display(), dest.display()))?;
        prune_backups(dir, keep_days, std::time::SystemTime::now())?;
        Ok(dest)
    }

    /// Batch-insert truth samples inside ONE transaction (the B backfill is
    /// 4,000+ rows — per-row commits would stall the async runtime behind the
    /// connection mutex). Duplicate observations (same seat/kind/scope/source/
    /// fetched_at) are ignored, which makes re-reading a whole channel file
    /// idempotent. Returns the number of rows actually inserted.
    pub async fn insert_truth_samples(&self, samples: &[TruthSample]) -> Result<usize> {
        if samples.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock().await;
        let now_iso = chrono::Utc::now().to_rfc3339();
        let tx = conn.unchecked_transaction()?;
        let mut inserted = 0usize;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO truth_samples \
                 (id, seat_id, seat_confidence, limit_kind, scope, percent, \
                  resets_at, source, fetched_at, raw_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for s in samples {
                let seat_id = Self::seat_id_sync(
                    &tx,
                    s.account_uuid.as_deref(),
                    s.org_uuid.as_deref(),
                    &now_iso,
                )?;
                let n = stmt.execute(params![
                    uuid::Uuid::new_v4().to_string(),
                    seat_id,
                    s.seat_confidence.as_str(),
                    s.limit_kind,
                    s.scope.as_deref().unwrap_or(""),
                    s.percent as f64,
                    s.resets_at.map(|t| t.to_rfc3339()),
                    s.source.as_str(),
                    s.fetched_at.to_rfc3339(),
                    serde_json::to_string(&s.raw_json).ok(),
                ])?;
                inserted += n;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Record an observation gap (e.g. Desktop was closed between two B
    /// samples). Idempotent on (source, started_at).
    pub async fn insert_gap(
        &self,
        source: &str,
        started_at: &str,
        ended_at: &str,
        note: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR IGNORE INTO gaps (id, source, started_at, ended_at, note) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                uuid::Uuid::new_v4().to_string(),
                source,
                started_at,
                ended_at,
                note
            ],
        )?;
        Ok(())
    }

    /// Stored (mtime_ms, size, last_offset) for a transcript file, if we've
    /// ingested it before.
    pub async fn get_ingest_file(&self, path: &str) -> Result<Option<(i64, i64, i64)>> {
        let conn = self.conn.lock().await;
        let row = conn
            .query_row(
                "SELECT mtime_ms, size, last_offset FROM ingest_files WHERE path = ?1",
                params![path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();
        Ok(row)
    }

    /// Apply one parsed transcript file to the ledger in a single
    /// transaction: upsert its usage events (global message.id dedup keeping
    /// max output_tokens — streaming snapshots only carry the real value on
    /// their last line), insert its 429 anchors, and advance the file's
    /// ingest watermark. Returns (events_upserted, anchors_inserted).
    pub async fn apply_file_batch(
        &self,
        batch: &crate::collector::jsonl::FileBatch,
    ) -> Result<(usize, usize)> {
        let conn = self.conn.lock().await;
        let tx = conn.unchecked_transaction()?;
        let mut events_n = 0usize;
        let mut anchors_n = 0usize;
        {
            let mut ev = tx.prepare_cached(
                "INSERT INTO usage_events \
                 (msg_id, request_id, ts, model, session_id, agent_id, project_dir, \
                  entrypoint, cc_version, input_tokens, output_tokens, cache_creation, \
                  cache_read, cache_1h, cache_5m, thinking_tokens, service_tier, speed, \
                  web_searches) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19) \
                 ON CONFLICT(msg_id) DO UPDATE SET \
                   output_tokens = excluded.output_tokens, \
                   ts = excluded.ts, \
                   thinking_tokens = excluded.thinking_tokens, \
                   cache_1h = excluded.cache_1h, \
                   cache_5m = excluded.cache_5m, \
                   web_searches = excluded.web_searches \
                 WHERE excluded.output_tokens >= usage_events.output_tokens",
            )?;
            for e in &batch.events {
                events_n += ev.execute(params![
                    e.msg_id,
                    e.request_id,
                    e.ts,
                    e.model,
                    e.session_id,
                    e.agent_id,
                    e.project_dir,
                    e.entrypoint,
                    e.cc_version,
                    e.input_tokens,
                    e.output_tokens,
                    e.cache_creation,
                    e.cache_read,
                    e.cache_1h,
                    e.cache_5m,
                    e.thinking_tokens,
                    e.service_tier,
                    e.speed,
                    e.web_searches,
                ])?;
            }

            let mut an = tx.prepare_cached(
                "INSERT OR IGNORE INTO anchors \
                 (id, ts, session_id, rate_limit_type, resets_at, overage_status, \
                  overage_disabled_reason, content_text, raw_json) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            )?;
            for a in &batch.anchors {
                anchors_n += an.execute(params![
                    uuid::Uuid::new_v4().to_string(),
                    a.ts,
                    a.session_id,
                    a.rate_limit_type,
                    a.resets_at,
                    a.overage_status,
                    a.overage_disabled_reason,
                    a.content_text,
                    a.raw_json,
                ])?;
            }

            tx.execute(
                "INSERT INTO ingest_files (path, mtime_ms, size, last_offset) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(path) DO UPDATE SET \
                   mtime_ms = excluded.mtime_ms, \
                   size = excluded.size, \
                   last_offset = excluded.last_offset",
                params![batch.path, batch.mtime_ms, batch.size, batch.new_offset],
            )?;
        }
        tx.commit()?;
        Ok((events_n, anchors_n))
    }

    /// Per-channel watermark (e.g. `desktop_last_t` = the largest absorbed
    /// Desktop epoch-ms). None when the channel has never been absorbed.
    pub async fn get_channel_state(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().await;
        let v = conn
            .query_row(
                "SELECT value FROM channel_state WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .ok();
        Ok(v)
    }

    pub async fn set_channel_state(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO channel_state (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// API 價目表 — 匯率粗估的分子（USD / MTok）。
// 欄位：input / 5m cache write / 1h cache write / cache read / output。
// web_search $0.01/次；web_fetch $0。
//
// v1.9.4（D99 B1）：先按**模型 id 精確比對**（`PRICE_EXACT`），比不到才退家族字串。
// 以前只有家族後備：Opus 5.5／Sonnet 5.5／Haiku 5.5／Fable 5.1 四個新版全部誤價
// （情境稽核 02-F1：09-01 起 API 計價高估約 44%，全帳本 18%）。
// 來源：https://platform.claude.com/docs/en/about-claude/pricing（Model pricing 全表＋
// Fast mode pricing＋Long context pricing，2026-10-10 核對，與 https://claude.com/pricing 一致；
// 逐字引用見 docs/research/2026-10-10-情境稽核/11-反駁.md §1.1）。
// 官方頁面沒有「最後更新」日期——發版 SOP 要再對一次官方頁（B1 修法最後一句）。
// 注意：快取讀取倍率不是固定 0.1×——Fable 5.1 是 0.025×、Opus 5.5／Sonnet 5.5 是 0.05×；
// 快取倍率疊在 fast mode 價上（官方：「Prompt caching multipliers apply on top of fast mode pricing」）。
// ---------------------------------------------------------------------------

/// 一個模型的牌價：標準價、（有的話）長 prompt 檔、（有的話）fast mode 價。
#[derive(Clone, Copy, Debug)]
struct PriceRow {
    std: [f64; 5],
    /// Haiku 5.5：單次請求的 prompt（input＋快取寫＋快取讀）超過 `LONG_PROMPT_TOKENS` 的那檔。
    long: Option<[f64; 5]>,
    /// fast mode 價；沒有就用 `std`。
    fast: Option<[f64; 5]>,
}

const fn row(std: [f64; 5]) -> PriceRow {
    PriceRow { std, long: None, fast: None }
}

/// Haiku 5.5 長 prompt 檔的門檻。官方（Long context pricing）：「A request's prompt length counts
/// all of its input tokens, including cache reads and cache writes. Each request is priced on its own」
/// ——所以是**單筆請求**的 input＋cache_creation＋cache_read，不是整段加總。
#[cfg(test)]
const LONG_PROMPT_TOKENS: i64 = 100_000;
/// 同一個條件寫成 SQL（usage_events 逐筆判斷後才能分組加總）。所有按牌價折算的查詢都 GROUP BY 它。
const LONG_PROMPT_SQL: &str =
    "((COALESCE(input_tokens,0) + COALESCE(cache_creation,0) + COALESCE(cache_read,0)) > 100000)";

const FABLE_FAST_LIKE: [f64; 5] = [10.0, 12.50, 20.0, 1.00, 50.0];
const OPUS_STD: [f64; 5] = [5.0, 6.25, 10.0, 0.50, 25.0];
const SONNET_4X: [f64; 5] = [3.0, 3.75, 6.0, 0.30, 15.0];

/// 精確 id 表（已去掉日期尾巴與 `[1m]` 之類後綴，見 `normalize_model_id`）。
const PRICE_EXACT: &[(&str, PriceRow)] = &[
    // Fable／Mythos：5.1 的快取讀 $0.25（0.025×），5 是 $1
    ("claude-fable-5-1", row([10.0, 12.50, 20.0, 0.25, 50.0])),
    ("claude-fable-5", row([10.0, 12.50, 20.0, 1.00, 50.0])),
    ("claude-mythos-5-1", row([10.0, 12.50, 20.0, 0.25, 50.0])),
    ("claude-mythos-5", row([10.0, 12.50, 20.0, 1.00, 50.0])),
    // Opus 5.5：$4／5／8／0.20／20；fast $8／$40，快取倍率疊上去（寫 1.25×／2×、讀 0.05×）
    (
        "claude-opus-5-5",
        PriceRow { std: [4.0, 5.0, 8.0, 0.20, 20.0], long: None, fast: Some([8.0, 10.0, 16.0, 0.40, 40.0]) },
    ),
    // Opus 5／4.8：fast $10／$50（快取讀 0.1×＝$1）
    ("claude-opus-5", PriceRow { std: OPUS_STD, long: None, fast: Some(FABLE_FAST_LIKE) }),
    ("claude-opus-4-8", PriceRow { std: OPUS_STD, long: None, fast: Some(FABLE_FAST_LIKE) }),
    ("claude-opus-4-7", row(OPUS_STD)),
    ("claude-opus-4-6", row(OPUS_STD)),
    ("claude-opus-4-5", row(OPUS_STD)),
    // Opus 4／4.1 是 $15／$75（家族後備會低估 3 倍）
    ("claude-opus-4-1", row([15.0, 18.75, 30.0, 1.50, 75.0])),
    ("claude-opus-4", row([15.0, 18.75, 30.0, 1.50, 75.0])),
    ("claude-sonnet-5-5", row([2.0, 2.50, 4.0, 0.10, 10.0])),
    ("claude-sonnet-5", row([2.0, 2.50, 4.0, 0.20, 10.0])),
    ("claude-sonnet-4-6", row(SONNET_4X)),
    ("claude-sonnet-4-5", row(SONNET_4X)),
    ("claude-sonnet-4", row(SONNET_4X)),
    (
        "claude-haiku-5-5",
        PriceRow {
            std: [0.10, 0.125, 0.20, 0.01, 0.50],
            long: Some([0.50, 0.625, 1.0, 0.05, 2.50]),
            fast: None,
        },
    ),
    ("claude-haiku-4-5", row([1.0, 1.25, 2.0, 0.10, 5.0])),
    ("claude-3-5-haiku", row([0.80, 1.0, 1.60, 0.08, 4.0])),
];

/// 模型 id → 比對用的鍵：小寫、去 `[1m]` 這類括號後綴、去 `-latest`、去 `-YYYYMMDD` 日期尾巴。
fn normalize_model_id(model: &str) -> String {
    let mut m = model.trim().to_lowercase();
    if let Some(i) = m.find('[') {
        m.truncate(i);
    }
    if let Some(s) = m.strip_suffix("-latest") {
        m = s.to_string();
    }
    if let Some((head, tail)) = m.rsplit_once('-') {
        if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
            m = head.to_string();
        }
    }
    m
}

/// 牌價依據：`Exact`＝精確 id 命中；`Family`＝id 沒收錄、按家族字串估（畫面要標「按 X 家族估價」）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriceBasis {
    Exact,
    Family,
}

impl PriceBasis {
    pub fn key(self) -> &'static str {
        match self {
            PriceBasis::Exact => "exact",
            PriceBasis::Family => "family",
        }
    }
}

/// 查一個模型的牌價。沒命中（`<synthetic>`、不認得的家族）回 None——呼叫端**不能**當 $0
///（B2：沒牌價的用量要出聲，不是「沒花錢」）。
fn price_lookup(model: &str) -> Option<(PriceRow, PriceBasis)> {
    let key = normalize_model_id(model);
    if let Some((_, r)) = PRICE_EXACT.iter().find(|(id, _)| *id == key) {
        return Some((*r, PriceBasis::Exact));
    }
    price_table_family(model).map(|std| {
        let fast = key.contains("opus").then_some(FABLE_FAST_LIKE);
        (PriceRow { std, long: None, fast }, PriceBasis::Family)
    })
}

/// 家族後備（舊表）。只在精確 id 沒收錄時用——新版若沒收錄就會落到這裡、被標「按家族估價」。
fn price_table_family(model: &str) -> Option<[f64; 5]> {
    let m = model.to_lowercase();
    Some(if m.contains("fable") {
        [10.0, 12.50, 20.0, 1.00, 50.0]
    } else if m.contains("opus") {
        // opus 家族（含 opus-5）同一價
        [5.0, 6.25, 10.0, 0.50, 25.0]
    } else if m.contains("sonnet-5") || m.contains("sonnet5") {
        [2.0, 2.50, 4.0, 0.20, 10.0]
    } else if m.contains("sonnet") {
        [3.0, 3.75, 6.0, 0.30, 15.0]
    } else if m.contains("haiku") {
        [1.0, 1.25, 2.0, 0.10, 5.0]
    } else {
        return None; // <synthetic> 等
    })
}

/// Price one aggregated event group. `cw` is total cache_creation, `cw1` the
/// 1h-TTL part of it — the 5m part is the difference (same split the analyze
/// script uses). fast mode 用該模型的 fast 價（見 `PRICE_EXACT`；家族後備的 Opus 用 $10／$50）。
/// `long_prompt`＝這一組事件是「單次 prompt 超過 10 萬 tokens」那一檔（只有 Haiku 5.5 有兩檔）。
///
/// v1.9.4（D99 B2）：沒牌價的模型回 `None`，不是 0.0——除非這組根本沒有 tokens
///（`<synthetic>` 這類零用量列：沒東西可估，回 Some(0.0)）。呼叫端要自己決定怎麼出聲。
#[allow(clippy::too_many_arguments)]
fn price_usd(
    model: &str,
    speed: &str,
    input: i64,
    cw: i64,
    cw1: i64,
    cr: i64,
    out: i64,
    web_searches: i64,
    long_prompt: bool,
) -> Option<f64> {
    let Some((row, _)) = price_lookup(model) else {
        return if input + cw + cr + out + web_searches <= 0 { Some(0.0) } else { None };
    };
    let p = if speed == "fast" {
        row.fast.unwrap_or(row.std)
    } else if long_prompt {
        row.long.unwrap_or(row.std)
    } else {
        row.std
    };
    let cw5 = (cw - cw1).max(0);
    Some(
        (input as f64 * p[0]
            + cw5 as f64 * p[1]
            + cw1 as f64 * p[2]
            + cr as f64 * p[3]
            + out as f64 * p[4])
            / 1e6
            + web_searches as f64 * 0.01,
    )
}

/// 這個 model id 有沒有牌價（精確或家族）——健康度列「無牌價模型」用。
fn has_price(model: &str) -> bool {
    price_lookup(model).is_some()
}

/// v1.9.3（D97）：分佈表（模型／專案／來源）的共用查詢——按 `key_sql` 分組，每組四種
/// tokens 照 `price_usd` 折成 API 計價，按美元排、取前 `limit`（0＝不限）。同一組裡可能
/// 混用好幾個模型，所以先按（鍵, 模型, speed）撈、在 Rust 端折算再加總。
/// `where_sql` 的 `?N` 對 `params`。回傳的每列：`{key: 名, calls, usd, inputTokens,
/// cacheWriteTokens, cacheReadTokens, outputTokens}`。
fn usage_mix(
    conn: &Connection,
    key_sql: &str,
    key: &str,
    where_sql: &str,
    params: &[String],
    limit: usize,
) -> Result<Vec<serde_json::Value>> {
    #[derive(Default)]
    struct Acc {
        calls: i64,
        input: i64,
        cw: i64,
        cr: i64,
        out: i64,
        usd: f64,
        /// B2：沒牌價的那幾次（usd 沒算進去）。
        unpriced_calls: i64,
        /// 這組有用量是按家族估價（id 沒收錄）。
        family_priced: bool,
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT COALESCE({key_sql},'?'), COALESCE(model,'?'), COALESCE(speed,''), COUNT(*), \
                SUM(input_tokens), SUM(cache_creation), SUM(COALESCE(cache_1h,0)), SUM(cache_read), \
                SUM(output_tokens), SUM(COALESCE(web_searches,0)), {LONG_PROMPT_SQL} \
         FROM usage_events WHERE {where_sql} GROUP BY 1, 2, 3, 11"
    ))?;
    let mut groups: std::collections::HashMap<String, Acc> = Default::default();
    for row in stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            r.get::<_, Option<i64>>(5)?.unwrap_or(0),
            r.get::<_, Option<i64>>(6)?.unwrap_or(0),
            r.get::<_, Option<i64>>(7)?.unwrap_or(0),
            r.get::<_, Option<i64>>(8)?.unwrap_or(0),
            r.get::<_, Option<i64>>(9)?.unwrap_or(0),
            r.get::<_, i64>(10)? != 0,
        ))
    })? {
        let (name, model, speed, calls, inp, cw, cw1, cr, out, ws, long) = row?;
        let a = groups.entry(name).or_default();
        a.calls += calls;
        a.input += inp;
        a.cw += cw;
        a.cr += cr;
        a.out += out;
        match price_usd(&model, &speed, inp, cw, cw1, cr, out, ws, long) {
            Some(v) => {
                a.usd += v;
                if matches!(price_lookup(&model), Some((_, PriceBasis::Family))) {
                    a.family_priced = true;
                }
            }
            None => a.unpriced_calls += calls,
        }
    }
    // B2：整組都沒牌價的列 usd 是 null（不是 0）、排最後；部分沒牌價的列 usd 是下限。
    let usd_of = |a: &Acc| -> Option<f64> { (a.unpriced_calls < a.calls || a.calls == 0).then_some(a.usd) };
    let mut rows: Vec<(String, Acc)> = groups.into_iter().collect();
    // 美元同分（例如都沒牌價）就看呼叫數，再看名字，順序才穩定；沒牌價的排在有牌價的後面。
    rows.sort_by(|a, b| {
        usd_of(&b.1)
            .unwrap_or(-1.0)
            .total_cmp(&usd_of(&a.1).unwrap_or(-1.0))
            .then(b.1.calls.cmp(&a.1.calls))
            .then(a.0.cmp(&b.0))
    });
    if limit > 0 {
        rows.truncate(limit);
    }
    Ok(rows
        .into_iter()
        .map(|(name, a)| {
            let usd = usd_of(&a);
            let mut v = serde_json::json!({
                key: name,
                "calls": a.calls,
                "usd": usd.map(|u| (u * 100.0).round() / 100.0),
                "inputTokens": a.input,
                "cacheWriteTokens": a.cw,
                "cacheReadTokens": a.cr,
                "outputTokens": a.out,
                // B2：>0＝這一組有幾次沒牌價、沒算進 usd（usd 是下限）；usd 為 null＝整組都沒牌價。
                "unpricedCalls": a.unpriced_calls,
                // id 沒收錄、按家族估價（畫面標「按 X 家族估價」）。
                "familyPriced": a.family_priced,
            });
            if key == "model" {
                v["label"] = serde_json::Value::String(crate::stats::model_label(&name));
                v["priceBasis"] = serde_json::json!(match price_lookup(&name) {
                    Some((_, b)) => b.key(),
                    None => "none",
                });
            }
            v
        })
        .collect())
}

/// v1.8.13（D91 T-02 審查 R2）：SCHEMA_V2 裡除了 meta 以外的七張表——`schema_version_sync`
/// 在沒有 meta 時逐一檢查有沒有資料；改 SCHEMA_V2 加表時這裡要跟著加。
const DATA_TABLES: [&str; 8] = [
    "seats",
    "truth_samples",
    "usage_events",
    "anchors",
    "gaps",
    "ingest_files",
    "channel_state",
    "seat_epochs",
];

/// v1.9.4（D99 A1）：座位的方案時間軸。純新增、`IF NOT EXISTS`、不帶外鍵（舊座位表被重建時
/// 不會被這張表卡住）、**不在 [`Ledger::open`] 的 DROP 清單裡**。
/// - `tier`／`org_type`／`seat_tier`：`oauthAccount` 的 `organizationRateLimitTier`／`organizationType`／`seatTier` 原字串
/// - `since`／`until`：UTC ISO；`until` 為 NULL＝目前這一段
/// - `observed_at`：最後一次讀到這個方案的時刻
/// - `source`：目前只有 `observed`（觀測到的）；保留欄位給日後可能的手填／推定（D99 Q1 決定現在不做）
///
/// 沒被任何一段覆蓋的時刻＝「方案未知」——舊資料不補、不回溯。
const SCHEMA_ADDITIVE: &str = r#"
CREATE TABLE IF NOT EXISTS seat_epochs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    seat_id     TEXT NOT NULL,
    tier        TEXT,
    org_type    TEXT,
    seat_tier   TEXT,
    since       TEXT NOT NULL,
    until       TEXT,
    observed_at TEXT NOT NULL,
    source      TEXT NOT NULL DEFAULT 'observed'
);
CREATE INDEX IF NOT EXISTS idx_seat_epochs_seat ON seat_epochs(seat_id, since);
"#;

const SCHEMA_V2: &str = r#"
BEGIN;

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
INSERT INTO meta (key, value) VALUES ('schema_version', '3');

-- 座位＝（帳號, 組織）這一對 (D48 第 4 點). Either half may be unknown while
-- the other is being learned; the pair is what percentages bind to.
CREATE TABLE seats (
    id            TEXT PRIMARY KEY,          -- uuid
    account_uuid  TEXT,
    org_uuid      TEXT,
    account_email TEXT,
    org_name      TEXT,
    first_seen_at TEXT NOT NULL,             -- UTC ISO-8601
    last_seen_at  TEXT NOT NULL,
    UNIQUE(account_uuid, org_uuid)
);

-- One observed data point of one rate limit. limit_kind is an OPEN string
-- set (D46) stored verbatim from the source; scope is '' when unscoped so
-- the UNIQUE dedup key works (SQLite treats NULLs as distinct).
CREATE TABLE truth_samples (
    id              TEXT PRIMARY KEY,        -- uuid
    seat_id         TEXT NOT NULL REFERENCES seats(id),
    seat_confidence TEXT NOT NULL
        CHECK (seat_confidence IN ('exact','inferred','confirmed','conflict')),
    limit_kind      TEXT NOT NULL,
    scope           TEXT NOT NULL DEFAULT '',
    percent         REAL NOT NULL,
    resets_at       TEXT,                    -- UTC ISO-8601, nullable
    source          TEXT NOT NULL,           -- probe / desktop-history / cli-cache / statusline
    fetched_at      TEXT NOT NULL,           -- UTC ISO-8601, origin time
    raw_json        TEXT,
    UNIQUE(seat_id, limit_kind, scope, source, fetched_at)
);
CREATE INDEX idx_truth_fetched ON truth_samples(fetched_at);
CREATE INDEX idx_truth_seat_kind ON truth_samples(seat_id, limit_kind, fetched_at);

-- Deduped API calls from ~/.claude/projects/**/*.jsonl. Global dedup key is
-- message.id; same-id streaming snapshots resolve to max(output_tokens)
-- (recon: 2026-08-27-M0-JSONL讀取器格式規格 §4).
CREATE TABLE usage_events (
    msg_id          TEXT PRIMARY KEY,        -- message.id (msg_*)
    request_id      TEXT,
    ts              TEXT NOT NULL,           -- UTC ISO-8601
    model           TEXT NOT NULL,
    session_id      TEXT,
    agent_id        TEXT,
    project_dir     TEXT,
    entrypoint      TEXT,
    cc_version      TEXT,
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_creation  INTEGER NOT NULL DEFAULT 0,
    cache_read      INTEGER NOT NULL DEFAULT 0,
    cache_1h        INTEGER,
    cache_5m        INTEGER,
    thinking_tokens INTEGER,
    service_tier    TEXT,
    speed           TEXT,
    web_searches    INTEGER
);
CREATE INDEX idx_usage_events_ts ON usage_events(ts);
CREATE INDEX idx_usage_events_session ON usage_events(session_id);

-- 429 quota-wall events — first-hand evidence of window boundaries.
CREATE TABLE anchors (
    id                      TEXT PRIMARY KEY, -- uuid
    ts                      TEXT NOT NULL,    -- UTC ISO-8601
    session_id              TEXT,
    rate_limit_type         TEXT,             -- open set; only 'five_hour' observed so far
    resets_at               TEXT,             -- UTC ISO-8601 (from epoch s)
    overage_status          TEXT,
    overage_disabled_reason TEXT,
    content_text            TEXT,
    raw_json                TEXT,
    UNIQUE(session_id, ts)
);

-- Known observation gaps (Desktop closed, machine asleep, …).
CREATE TABLE gaps (
    id         TEXT PRIMARY KEY,             -- uuid
    source     TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at   TEXT NOT NULL,
    note       TEXT,
    UNIQUE(source, started_at)
);

-- Incremental-read watermarks for the JSONL reader (mtime+size+offset model,
-- same shape Claude Code itself uses in .session_cache.json).
CREATE TABLE ingest_files (
    path        TEXT PRIMARY KEY,
    mtime_ms    INTEGER NOT NULL,
    size        INTEGER NOT NULL,
    last_offset INTEGER NOT NULL
);

-- Per-channel absorption watermarks (e.g. desktop_last_t).
CREATE TABLE channel_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

COMMIT;
"#;

// ---------------------------------------------------------------------------
// M6 (D65) — 歷史檢視窗口切分 ＋ 證據包匯出
// ---------------------------------------------------------------------------

/// One reconstructed limit window (D65-4; v1.5／D79 generalised to 5h／7d／
/// Fable). Windows are cut from the truth timeline itself — a % drop of >2
/// points, a silence longer than the window span, span (+tolerance) elapsed
/// since the window's first non-zero sample, or a `resets_at` that jumps by
/// more than 30 min (cli-cache samples carry it; the probe and the Desktop
/// history don't, so those still rely on the drop rule). The first
/// `resets_at` seen inside the window is attached as `reset`.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistWindow {
    pub start: String,
    pub end: String,
    pub reset: Option<String>,
    pub peak: f64,
    pub samples: usize,
    pub sources: Vec<String>,
    pub anchors: usize,
    pub saturated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub points: Option<Vec<(String, f64)>>,
    /// v1.9.4（D99 A11）：這個窗是**換方案**切出來的（不是額度重置）——% 在切點會階躍，
    /// 舊窗在切點結束、這個窗從切點後第一筆樣本開始。歷史頁的窗口列標「換方案」。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub plan_change: bool,
}

/// v1.5（D79）：歷史頁重建哪一條額度的窗口。三條的樣本種類、scope、窗長、
/// 429 錨點類別各不同，集中在這裡，history 與證據匯出共用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistKind {
    FiveHour,
    SevenDay,
    Fable,
}

impl HistKind {
    /// IPC 字串 → kind；不認得的一律當 5h（舊前端沒傳 kind 也走這裡）。
    pub fn parse(s: &str) -> Self {
        match s {
            "7d" => Self::SevenDay,
            "fable" => Self::Fable,
            _ => Self::FiveHour,
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::SevenDay => "7d",
            Self::Fable => "fable",
        }
    }
    /// truth_samples.limit_kind 的 SQL IN 清單（D46：開放字串集，兩種拼法並存）。
    fn kinds_sql(self) -> &'static str {
        match self {
            Self::FiveHour => "'session','five_hour'",
            Self::SevenDay => "'weekly_all','seven_day'",
            Self::Fable => "'weekly_scoped'",
        }
    }
    fn scope(self) -> &'static str {
        match self {
            Self::Fable => "Fable",
            _ => "",
        }
    }
    /// 名目窗長。
    pub fn span(self) -> chrono::Duration {
        match self {
            Self::FiveHour => chrono::Duration::hours(5),
            _ => chrono::Duration::hours(24 * 7),
        }
    }
    /// 「從第一筆算起超過窗長多少就一定是下一窗」的寬容：窗的起點是重置後
    /// 第一筆樣本，永遠晚於真正的重置時刻，所以只會低估已過時間。
    fn tolerance(self) -> chrono::Duration {
        match self {
            Self::FiveHour => chrono::Duration::minutes(15),
            _ => chrono::Duration::hours(1),
        }
    }
    /// anchors.rate_limit_type 的篩選。真帳本（2026-10-10）見過的型別只有 `five_hour` 與 NULL；
    /// 7d 的型別字串仍是**假設**（Claude Code 的 rate_limit_type 開放集，週限歷來叫 seven_day、
    /// 模型限定加後綴）——出現新值時錨點會落到哪一頁，看這裡。
    ///
    /// v1.9.4（D99 E4）：NULL 型別**不是**只有舊回填。帳本裡 NULL 共 59 筆：session limit 33、
    /// 「out of usage credits」8、**Fable 牆 17**（08-23 十筆、10-06 七筆，內文是 "You've reached your
    /// Fable limit"）、伺服器過載 1（內文 "not your usage limit"）。以前 NULL 一律歸 5h 頁，
    /// Fable 牆被算成 5h 撞牆。現在 NULL 的照內文歸類：
    /// - 內文有 Fable 且有 limit（排除 out of usage credits）→ Fable 頁；
    /// - 內文是 "not your usage limit"（伺服器限流，不是額度）→ 哪一頁都不算；
    /// - 其餘 NULL（session limit、out of credits）→ 5h 頁，跟以前一樣。
    fn anchor_sql(self) -> &'static str {
        match self {
            Self::FiveHour => {
                "(rate_limit_type = 'five_hour' OR (rate_limit_type IS NULL \
                  AND NOT (COALESCE(content_text,'') LIKE '%Fable%' AND COALESCE(content_text,'') LIKE '%limit%' \
                           AND COALESCE(content_text,'') NOT LIKE '%out of usage credits%') \
                  AND COALESCE(content_text,'') NOT LIKE '%not your usage limit%'))"
            }
            Self::SevenDay => "rate_limit_type = 'seven_day'",
            Self::Fable => {
                "((rate_limit_type LIKE 'seven_day%' AND lower(rate_limit_type) LIKE '%fable%') \
                  OR (rate_limit_type IS NULL AND content_text LIKE '%Fable%' AND content_text LIKE '%limit%' \
                      AND content_text NOT LIKE '%out of usage credits%'))"
            }
        }
    }
}

/// (percent, fetched_at, resets_at, source), ascending by fetched_at.
type SampleRow = (f64, String, Option<String>, String);

/// cli-cache 的 resets_at 在同一窗內會在整點前後 ±1 秒抖（真帳本：
/// 07:59:59.9 ↔ 08:00:00.4 交替），所以「換窗」要差超過這個數才算。
const RESET_JUMP_MIN: i64 = 30;
/// 下降之後往後看這麼多分鐘，期間所有樣本都低才算重置（沒有樣本＝算數）。
/// 30 分鐘：一週額度不可能在半小時內彈回原高度；5h 只有原本就很低（<~17%）
/// 又緊接著猛燒才可能誤合併，可接受。
const LOOKAHEAD_MIN: i64 = 30;
/// 重置時刻跳動要單獨成立，% 得低於這個數（剛換窗不可能已經燒掉一大截）。
const FRESH_WINDOW_MAX_PCT: f64 = 5.0;

fn reset_jumped(known: &Option<String>, new: &Option<String>) -> bool {
    let (Some(a), Some(b)) = (known, new) else { return false };
    let (Ok(a), Ok(b)) = (
        chrono::DateTime::parse_from_rfc3339(a),
        chrono::DateTime::parse_from_rfc3339(b),
    ) else {
        return false;
    };
    (b - a).num_minutes().abs() > RESET_JUMP_MIN
}

/// 階梯只需要「變了」的點：連續同值只留第一個，最後一筆一定留（階梯要
/// 延伸到窗尾）。7d 的 Desktop 歷史每 10 分鐘一筆、一週幾千筆，去重後
/// 只剩幾十個台階。
fn thin_points(points: Vec<(String, f64)>) -> Vec<(String, f64)> {
    let n = points.len();
    let mut out: Vec<(String, f64)> = Vec::with_capacity(n.min(128));
    for (i, pt) in points.into_iter().enumerate() {
        let changed = out.last().map(|l: &(String, f64)| l.1 != pt.1).unwrap_or(true);
        if changed || i + 1 == n {
            out.push(pt);
        }
    }
    out
}

pub fn segment_windows(
    rows: &[SampleRow],
    keep_points: bool,
    kind: HistKind,
    plan_cuts: &[chrono::DateTime<chrono::Utc>],
) -> Vec<HistWindow> {
    struct Cur {
        plan_change: bool,
        start: chrono::DateTime<chrono::FixedOffset>,
        last_t: chrono::DateTime<chrono::FixedOffset>,
        last_p: f64,
        peak: f64,
        n: usize,
        reset: Option<String>,
        sources: Vec<String>,
        points: Vec<(String, f64)>,
    }
    let span = kind.span();
    let span_tol = span + kind.tolerance();
    let mut out = Vec::new();
    let mut cur: Option<Cur> = None;
    let flush = |c: Cur, out: &mut Vec<HistWindow>| {
        if c.peak <= 0.0 {
            return; // idle stretch, not a window
        }
        out.push(HistWindow {
            start: c.start.to_rfc3339(),
            end: c.last_t.to_rfc3339(),
            reset: c.reset,
            peak: c.peak,
            samples: c.n,
            sources: c.sources,
            anchors: 0,
            saturated: c.peak >= 100.0,
            points: if keep_points { Some(thin_points(c.points)) } else { None },
            plan_change: c.plan_change,
        });
    };
    for (i, (p, t, r, s)) in rows.iter().enumerate() {
        let Ok(tt) = chrono::DateTime::parse_from_rfc3339(t) else { continue };
        let mut outlier = false;
        let mut by_plan_change = false;
        let start_new = match &cur {
            None => true,
            Some(c) => {
                // 下降要「站得住」才算重置：真的重置之後 % 會一直低，而來源之間
                // 打架（探針＋CLI 快取連續半小時報 0、Desktop 同時報 11；真帳本
                // 09-11 早上）過一會就彈回原來的高度。往後看 30 分鐘內的樣本，
                // 全都低才切——用時間不用筆數，兩個來源的取樣節奏才不會左右結果。
                let floor = c.last_p - 2.0;
                let dropped = *p < floor;
                let horizon = tt + chrono::Duration::minutes(LOOKAHEAD_MIN);
                let drop_confirmed = dropped
                    && rows[i + 1..]
                        .iter()
                        .take_while(|n| {
                            chrono::DateTime::parse_from_rfc3339(&n.1)
                                .map(|nt| nt <= horizon)
                                .unwrap_or(false)
                        })
                        .all(|n| n.0 < floor);
                // 重置時刻跳動：真的換窗時 % 一定接近 0；% 還高卻換了重置時刻＝
                // 快取的重置欄位不可信（同一批資料裡出現過 09-13T01 → 09-16T08），
                // 不切、改記新值。
                let jumped = reset_jumped(&c.reset, r);
                let jump_confirmed = jumped && *p <= FRESH_WINDOW_MAX_PCT;
                // 換方案的切點落在上一筆與這一筆之間：不管 % 怎麼階躍（升級掉下去、降級跳上去）
                // 都在這裡切一刀，標 plan_change——不當成重置，也不讓兩個池的 % 接成一個窗。
                // （切點落在一段比窗長還長的靜默空檔裡時，這一刀本來就會因為超過窗長而切——
                //  新窗不是「換方案切出來的」，不標，21r1 nit 9。）
                by_plan_change = plan_cuts.iter().any(|pc| c.last_t.to_utc() < *pc && *pc <= tt.to_utc())
                    && tt - c.last_t <= span;
                let cut = drop_confirmed
                    || jump_confirmed
                    || by_plan_change
                    || tt - c.last_t > span
                    || tt - c.start > span_tol;
                outlier = !cut && dropped;
                cut
            }
        };
        if start_new {
            if let Some(c) = cur.take() {
                flush(c, &mut out);
            }
            cur = Some(Cur {
                plan_change: by_plan_change,
                start: tt,
                last_t: tt,
                last_p: *p,
                peak: *p,
                n: 1,
                reset: r.clone(),
                sources: vec![s.clone()],
                points: vec![(t.clone(), *p)],
            });
            continue;
        }
        let c = cur.as_mut().unwrap();
        if !c.sources.contains(s) {
            c.sources.push(s.clone());
        }
        c.n += 1;
        if outlier {
            // 孤立的低值：算樣本，但不讓它拉動窗的尾巴、階梯與峰值。
            continue;
        }
        if c.peak <= 0.0 && *p <= 0.0 {
            // still idle: the window hasn't started — slide the start along
            c.start = tt;
            c.points.clear();
        }
        c.last_t = tt;
        c.last_p = *p;
        c.peak = c.peak.max(*p);
        if c.reset.is_none() || reset_jumped(&c.reset, r) {
            // 第一次看到、或（沒切窗的）跳動＝後來的值比較可信
            c.reset = r.clone();
        }
        c.points.push((t.clone(), *p));
    }
    if let Some(c) = cur.take() {
        flush(c, &mut out);
    }
    out
}

/// Local-time span for the history pager.
fn history_span(unit: &str, offset: i32) -> (chrono::NaiveDate, chrono::NaiveDate, String) {
    use chrono::Datelike;
    let today = chrono::Local::now().date_naive();
    match unit {
        "day" => {
            let d = today + chrono::Duration::days(offset as i64);
            (d, d + chrono::Duration::days(1), d.format("%Y-%m-%d（%a）").to_string())
        }
        "week" => {
            let monday = today - chrono::Duration::days(today.weekday().num_days_from_monday() as i64)
                + chrono::Duration::weeks(offset as i64);
            let sunday = monday + chrono::Duration::days(6);
            (
                monday,
                monday + chrono::Duration::days(7),
                format!("{} – {}", monday.format("%m-%d"), sunday.format("%m-%d")),
            )
        }
        _ => {
            let total = today.year() * 12 + (today.month0() as i32) + offset;
            let (y, m0) = (total.div_euclid(12), total.rem_euclid(12));
            let first = chrono::NaiveDate::from_ymd_opt(y, m0 as u32 + 1, 1).unwrap();
            let next = if m0 == 11 {
                chrono::NaiveDate::from_ymd_opt(y + 1, 1, 1).unwrap()
            } else {
                chrono::NaiveDate::from_ymd_opt(y, m0 as u32 + 2, 1).unwrap()
            };
            (first, next, first.format("%Y 年 %m 月").to_string())
        }
    }
}

fn local_midnight_utc(d: chrono::NaiveDate) -> String {
    d.and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(chrono::Local)
        .earliest()
        .map(|t| t.with_timezone(&chrono::Utc).to_rfc3339())
        .unwrap_or_else(|| format!("{d}T00:00:00+00:00"))
}

fn local_day_of(iso: &str) -> Option<chrono::NaiveDate> {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|t| t.with_timezone(&chrono::Local).date_naive())
}

/// v1.8.13（D91 T-02）：帳本打不開時給主人看的對話框。這時事件迴圈還沒跑（還在 setup），
/// dialog plugin 要排到主執行緒、會卡死自己；直接叫 Win32 的 MessageBoxW——同步、不靠事件迴圈，
/// windows-sys 本來就有（全螢幕閃避在用）。
pub fn show_open_failed_dialog(dir: &std::path::Path, detail: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };
    let text = format!(
        "帳本（ledger.sqlite）打不開，App 這次先不啟動。帳本裡的資料一筆都沒有動。\n\n\
         通常是檔案壞了（例如斷電或磁碟錯誤）。可以從自動備份還原：\n\
         1. 打開資料夾 {dir}\n\
         2. 新建一個資料夾（例如「壞掉的帳本」），把 ledger.sqlite、\n    ledger.sqlite-wal、ledger.sqlite-shm（有的話）三個一起搬進去\n\
         3. 到 backups 資料夾挑最新一份 ledger-日期-時間.sqlite，\n    複製回來並改名成 ledger.sqlite\n\
         4. 再開一次 App\n\n\
         錯誤細節也寫進了同一個資料夾的 collector.log：\n{detail}",
        dir = dir.display(),
    );
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let text = wide(&text);
    let caption = wide("Claude Usage Monitor · 帳本打不開");
    // SAFETY：兩個字串都是以 0 結尾的 UTF-16，活到呼叫結束；沒有父視窗（null）。
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

/// v1.8.13（D91 T-05）：備份殘骸（`.tmp`、`-journal`）放多久沒動才算孤兒。
const BACKUP_ORPHAN_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// v1.8.13（D91 T-05）：`ledger-YYYYMMDD-HHMMSS.sqlite`（UTC）→ 它屬於哪個本地日。
/// 認不出的檔名回 None——不是我們的格式就不碰。
fn backup_local_day(name: &str) -> Option<chrono::NaiveDate> {
    let stamp = name.strip_prefix("ledger-")?.strip_suffix(".sqlite")?;
    let t = chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M%S").ok()?;
    Some(t.and_utc().with_timezone(&chrono::Local).date_naive())
}

/// v1.8.13（D91 T-05）：備份保留規則——每個本地日留最新一份、留最近 `keep_days` 個有備份的日子；
/// `now` 往前超過 BACKUP_ORPHAN_AGE 的 `ledger-*.tmp`／`ledger-*-journal` 殘骸一起刪。
/// 回傳刪掉的檔名（測試與 log 用）。
fn prune_backups(dir: &std::path::Path, keep_days: usize, now: std::time::SystemTime) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    // (本地日, 檔名)；檔名按時間排序＝字串排序。
    let mut dated: Vec<(chrono::NaiveDate, String)> = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("ledger-") {
            continue;
        }
        if let Some(day) = backup_local_day(&name) {
            dated.push((day, name));
            continue;
        }
        let leftover = name.ends_with(".tmp") || name.ends_with("-journal");
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= BACKUP_ORPHAN_AGE);
        if leftover && old && std::fs::remove_file(e.path()).is_ok() {
            removed.push(name);
        }
    }
    // 新到舊：每個日子第一次看到的是它最新的那份。
    dated.sort_by(|a, b| b.1.cmp(&a.1));
    let mut kept_days: Vec<chrono::NaiveDate> = Vec::new();
    for (day, name) in dated {
        let keep = if kept_days.contains(&day) {
            false // 同一天較舊的
        } else if kept_days.len() < keep_days {
            kept_days.push(day);
            true
        } else {
            false // 超出保留天數
        };
        if !keep && std::fs::remove_file(dir.join(&name)).is_ok() {
            removed.push(name);
        }
    }
    Ok(removed)
}

impl Ledger {
    /// Pick the seat the research pages work on: explicit account, else the
    /// most-sampled one. Returns (seat_id, account, org, desktop_sample_count).
    fn select_seat_sync(
        conn: &Connection,
        seat_account: Option<&str>,
    ) -> Result<(Vec<serde_json::Value>, Option<SelectedSeat>)> {
        let mut seats: Vec<SeatRow> = Vec::new();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.account_uuid, s.org_uuid, \
                    (SELECT COUNT(*) FROM truth_samples t WHERE t.seat_id = s.id), \
                    (SELECT COUNT(*) FROM truth_samples t WHERE t.seat_id = s.id \
                      AND t.source = 'desktop-history'), \
                    s.account_email, s.org_name \
             FROM seats s ORDER BY 4 DESC",
        )?;
        for row in stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))? {
            seats.push(row?);
        }
        let selected = seats
            .iter()
            .find(|s| match seat_account {
                // v1.1.1（D69）：先比 seat id，再退回 account uuid（舊呼叫相容）。
                Some(a) => s.0 == a || s.1.as_deref() == Some(a),
                None => true,
            })
            .or(seats.first())
            .map(|s| (s.0.clone(), s.1.clone(), s.2.clone(), s.4));
        let seats_json = seats
            .iter()
            .map(|(id, a, o, n, dn, em, on)| {
                serde_json::json!({ "id": id, "accountUuid": a, "orgUuid": o, "samples": n, "isDesktop": *dn > 0, "email": em, "orgName": on })
            })
            .collect();
        Ok((seats_json, selected))
    }

    /// v1.5（D79）：一條額度在 [from, to) 的窗口重建——history 頁與證據匯出
    /// 共用。回 (look-back 起的樣本列, 窗口, 期間內該類 429 時刻)。
    ///
    /// look-back＝窗長＋6h：7d 的窗跨進期間之前就開始了，不往前撈起點與峰值
    /// 都會錯（5h 原本就往前撈 6h 對付跨午夜）。窗口只留「尾巴落在期間內」
    /// 的；階梯點裁到期間內，但保留期間前最後一個台階、時間夾到 from，
    /// 階梯才會從正確的高度進場。
    fn windows_sync(
        conn: &Connection,
        seat_id: &str,
        kind: HistKind,
        from_utc: &str,
        to_utc: &str,
        keep_points: bool,
    ) -> Result<(Vec<SampleRow>, Vec<HistWindow>, Vec<String>)> {
        let from_probe = chrono::DateTime::parse_from_rfc3339(from_utc)
            .map(|t| (t - kind.span() - chrono::Duration::hours(6)).to_rfc3339())
            .unwrap_or_else(|_| from_utc.to_string());

        let mut stmt = conn.prepare(&format!(
            "SELECT percent, fetched_at, resets_at, source FROM truth_samples \
             WHERE seat_id = ?1 AND limit_kind IN ({}) AND scope = ?2 \
               AND fetched_at >= ?3 AND fetched_at < ?4 ORDER BY fetched_at ASC",
            kind.kinds_sql()
        ))?;
        let rows: Vec<SampleRow> = stmt
            .query_map(params![seat_id, kind.scope(), from_probe, to_utc], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        // v1.9.4（D99 A11）：換方案的切點（兩段已知方案之間）——窗口在這裡切開並標 planChange。
        // 切在「舊段最後一次確認」的時刻（洞的起點）而不是偵測時刻：洞裡的樣本可能已經是新池的 %，
        // 不能讓它們留在舊窗裡（21r1 M1）。
        let plan_cuts: Vec<chrono::DateTime<chrono::Utc>> = Self::plan_epochs_sync(conn, seat_id)?
            .windows(2)
            .map(|w| w[0].until_t.unwrap_or(w[1].since_t).min(w[1].since_t))
            .collect();
        let mut windows: Vec<HistWindow> = segment_windows(&rows, keep_points, kind, &plan_cuts)
            .into_iter()
            .filter(|w| w.end.as_str() >= from_utc)
            .collect();
        for w in windows.iter_mut() {
            if let Some(pts) = w.points.as_mut() {
                let first_in = pts.iter().position(|(t, _)| t.as_str() >= from_utc).unwrap_or(pts.len());
                if first_in > 0 {
                    let carry = pts[first_in - 1].1;
                    let mut kept: Vec<(String, f64)> = vec![(from_utc.to_string(), carry)];
                    kept.extend(pts.drain(first_in..));
                    *pts = kept;
                }
            }
        }

        let mut anchors: Vec<String> = Vec::new();
        {
            let mut stmt = conn.prepare(&format!(
                "SELECT ts FROM anchors WHERE {} AND ts >= ?1 AND ts < ?2 ORDER BY ts ASC",
                kind.anchor_sql()
            ))?;
            for row in stmt.query_map(params![from_probe, to_utc], |r| r.get::<_, String>(0))? {
                anchors.push(row?);
            }
        }
        for a in &anchors {
            // attach to the window it fell in (15 min grace after the last sample)
            if let Some(w) = windows.iter_mut().find(|w| {
                let end_grace = chrono::DateTime::parse_from_rfc3339(&w.end)
                    .map(|t| (t + chrono::Duration::minutes(15)).to_rfc3339())
                    .unwrap_or_else(|_| w.end.clone());
                a.as_str() >= w.start.as_str() && a.as_str() <= end_grace.as_str()
            }) {
                w.anchors += 1;
            }
        }
        Ok((rows, windows, anchors))
    }

    /// M6 歷史檢視 payload: one local-time span (day / week / month, paged by
    /// `offset` ≤ 0) of reconstructed windows of ONE limit (v1.5／D79: 5h／7d／
    /// Fable, `kind`) for one seat, plus per-day aggregates. truth_samples +
    /// anchors only (D56: never the transcripts).
    pub async fn history(
        &self,
        seat_account: Option<&str>,
        unit: &str,
        offset: i32,
        kind: HistKind,
    ) -> Result<serde_json::Value> {
        // v1.8.2 效能：走快取（歷史頁本身只要幾十 ms，快取主要是讓翻頁零等待）。
        self.cached(Recipe::History {
            seat: seat_account.map(str::to_owned),
            unit: unit.to_owned(),
            offset,
            kind,
        })
        .await
    }

    fn history_sync(
        conn: &Connection,
        seat_account: Option<&str>,
        unit: &str,
        offset: i32,
        kind: HistKind,
    ) -> Result<serde_json::Value> {
        let (seats, selected) = Self::select_seat_sync(conn, seat_account)?;
        // v1.8.11（D89）：全量視角——每個座位各自重建窗口，合成一條時間軸（窗口帶 seat）。
        let all = seat_account == Some(ALL_SEATS);
        let (seat_id, seat_acct, seat_org): (String, Option<String>, Option<String>) = if all {
            if seats.is_empty() {
                return Ok(serde_json::json!({ "seats": [], "empty": true }));
            }
            (ALL_SEATS.to_string(), None, None)
        } else {
            let Some((id, a, o, _)) = selected else {
                return Ok(serde_json::json!({ "seats": [], "empty": true }));
            };
            (id, a, o)
        };
        let (from_d, to_d, label) = history_span(unit, offset.min(0));
        let from_utc = local_midnight_utc(from_d);
        let to_utc = local_midnight_utc(to_d);
        // 階梯點：5h 只在日檢視畫（一週幾十個窗擠不下）；7d／Fable 一週才一個
        // 窗、爬升就是重點，三個尺度都畫（去重後每窗只剩台階數個點）。
        let keep_points = unit == "day" || kind != HistKind::FiveHour;
        let (rows, windows, anchors, windows_json): (Vec<SampleRow>, Vec<HistWindow>, Vec<String>, Vec<serde_json::Value>) = if all {
            let mut rows = Vec::new();
            let mut wins = Vec::new();
            let mut anchors = Vec::new();
            let mut wj = Vec::new();
            for s in &seats {
                let Some(id) = s["id"].as_str() else { continue };
                let (r, w, a) = Self::windows_sync(conn, id, kind, &from_utc, &to_utc, keep_points)?;
                for win in &w {
                    let mut v = serde_json::to_value(win)?;
                    v["seat"] = serde_json::Value::String(id.to_string());
                    wj.push(v);
                }
                rows.extend(r);
                wins.extend(w);
                anchors = a; // 429 錨點不分座位，每次回的都一樣
            }
            wins.sort_by(|x, y| x.start.cmp(&y.start));
            wj.sort_by(|x, y| x["start"].as_str().cmp(&y["start"].as_str()));
            (rows, wins, anchors, wj)
        } else {
            let (r, w, a) = Self::windows_sync(conn, &seat_id, kind, &from_utc, &to_utc, keep_points)?;
            let wj = w.iter().map(serde_json::to_value).collect::<std::result::Result<Vec<_>, _>>()?;
            (r, w, a, wj)
        };

        // 三條額度在期間內的峰值（脈絡列：看 7d 時也知道 5h／Fable 到哪）
        let peak_of = |k: HistKind| -> Result<Option<f64>> {
            Ok(conn.query_row(
                &format!(
                    "SELECT MAX(percent) FROM truth_samples WHERE (seat_id = ?1 OR ?1 = '*') \
                     AND limit_kind IN ({}) AND scope = ?2 AND fetched_at >= ?3 AND fetched_at < ?4",
                    k.kinds_sql()
                ),
                params![seat_id, k.scope(), from_utc, to_utc],
                |r| r.get::<_, Option<f64>>(0),
            )?)
        };
        let peaks = serde_json::json!({
            "5h": peak_of(HistKind::FiveHour)?,
            "7d": peak_of(HistKind::SevenDay)?,
            "fable": peak_of(HistKind::Fable)?,
        });

        // per local day aggregates
        let mut days: Vec<serde_json::Value> = Vec::new();
        let mut d = from_d;
        while d < to_d {
            let day_rows: Vec<&SampleRow> = rows
                .iter()
                .filter(|r| local_day_of(&r.1) == Some(d))
                .collect();
            let peak = day_rows.iter().map(|r| r.0).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |x| x.max(v))));
            let n_win = windows.iter().filter(|w| local_day_of(&w.start) == Some(d)).count();
            let n_anchor = anchors.iter().filter(|a| local_day_of(a) == Some(d)).count();
            days.push(serde_json::json!({
                "day": d.to_string(),
                "peak": peak,
                "samples": day_rows.len(),
                "windows": n_win,
                "anchors": n_anchor,
                "saturated": peak.map(|p| p >= 100.0).unwrap_or(false),
            }));
            d += chrono::Duration::days(1);
        }

        let sat = windows.iter().filter(|w| w.saturated).count();
        let samples: usize = rows.iter().filter(|r| r.1.as_str() >= from_utc.as_str()).count();

        // v1.8.8（D87，主人：歷史頁也要看得到模型與專案分佈）：期間內本機事件按模型／
        // 專案分，只計這個座位登入中的時段（規則同趨勢頁 D86）。
        let tenure = if all { Vec::new() } else { Self::seat_tenure_sync(conn, &seat_id, &from_utc)? };
        let (tsql, tparams) = if all { ("1".to_string(), Vec::new()) } else { tenure_where(&tenure, true) };
        // v1.9.3（D97）：按 API 計價排（原本按輸出 tokens）。
        let mut p: Vec<String> = vec![from_utc.clone(), to_utc.clone()];
        p.extend(tparams.iter().cloned());
        let where_sql = format!("ts >= ?1 AND ts < ?2 AND ({tsql})");
        let usage = serde_json::json!({
            "byModel": usage_mix(conn, "model", "model", &where_sql, &p, 8)?,
            "byProject": usage_mix(conn, PROJECT_KEY_SQL, "project", &where_sql, &p, 8)?,
            "tenureSpans": tenure.len(),
            "all": all,
        });

        let plan_changes = {
            let epochs_all = Self::plan_epochs_all_sync(conn)?;
            let mut v: Vec<serde_json::Value> = Vec::new();
            for (sid, eps) in &epochs_all {
                if !all && *sid != seat_id {
                    continue;
                }
                for w in eps.windows(2) {
                    if w[1].since.as_str() >= from_utc.as_str() && w[1].since.as_str() < to_utc.as_str() {
                        v.push(serde_json::json!({
                            "seat": sid, "at": w[1].since,
                            "fromLabel": w[0].label(), "toLabel": w[1].label(),
                        }));
                    }
                }
            }
            v.sort_by(|a, b| a["at"].as_str().cmp(&b["at"].as_str()));
            v
        };
        Ok(serde_json::json!({
            "unit": unit,
            "offset": offset.min(0),
            "kind": kind.key(),
            "usage": usage,
            "label": label,
            "from": from_utc,
            "to": to_utc,
            "fromLocal": from_d.to_string(),
            "toLocal": to_d.to_string(),
            "seats": seats,
            "selected": { "id": seat_id, "accountUuid": seat_acct, "orgUuid": seat_org },
            // v1.9.4（D99 A11）：期間內的換方案時間點（窗口列上標「換方案」、時間軸畫記號用）
            "planChanges": plan_changes,
            "windows": windows_json,
            "days": days,
            "peaks": peaks,
            "totals": {
                "samples": samples,
                "windows": windows.len(),
                "saturated": sat,
                "anchors": anchors.iter().filter(|a| a.as_str() >= from_utc.as_str()).count(),
            },
        }))
    }

    /// v1.8.12（D90）：歷史頁點一個窗口看詳情。回窗內每一筆樣本（含來源與重置時刻）、
    /// 窗內 429 時刻、這段期間本機用了什麼（模型／專案／入口＋API 計價）。
    /// 窗口＝歷史頁那一列的 [start, end]（首末樣本時刻）。本機事件的範圍寬一點：
    /// 有重置時刻就用真正的額度窗 [reset−窗長, reset)，沒有就 [start, end+15 分)；
    /// 只算這個座位登入中的時段（規則同歷史頁「這段期間用了什麼」，D87）。
    pub async fn history_window(
        &self,
        seat_id: &str,
        kind: HistKind,
        start: &str,
        end: &str,
    ) -> Result<serde_json::Value> {
        let conn = self.conn.lock().await;
        Self::history_window_sync(&conn, seat_id, kind, start, end)
    }

    fn history_window_sync(
        conn: &Connection,
        seat_id: &str,
        kind: HistKind,
        start: &str,
        end: &str,
    ) -> Result<serde_json::Value> {
        let t_start = chrono::DateTime::parse_from_rfc3339(start)?;
        let t_end = chrono::DateTime::parse_from_rfc3339(end)?;
        if t_end < t_start {
            anyhow::bail!("window end before start");
        }
        let seat = conn
            .query_row(
                "SELECT account_uuid, org_uuid, account_email, org_name FROM seats WHERE id = ?1",
                params![seat_id],
                |r| {
                    Ok(serde_json::json!({
                        "id": seat_id,
                        "accountUuid": r.get::<_, Option<String>>(0)?,
                        "orgUuid": r.get::<_, Option<String>>(1)?,
                        "email": r.get::<_, Option<String>>(2)?,
                        "orgName": r.get::<_, Option<String>>(3)?,
                    }))
                },
            )
            .unwrap_or_else(|_| serde_json::json!({ "id": seat_id }));

        // ---- 窗內樣本：全部來源都列（來源欄讓人看得出哪筆是 Desktop、哪筆是 CLI）
        let fetch = |until: &str| -> Result<Vec<SampleRow>> {
            let mut stmt = conn.prepare(&format!(
                "SELECT percent, fetched_at, resets_at, source FROM truth_samples \
                 WHERE seat_id = ?1 AND limit_kind IN ({}) AND scope = ?2 \
                   AND fetched_at >= ?3 AND fetched_at <= ?4 ORDER BY fetched_at ASC",
                kind.kinds_sql()
            ))?;
            let rows = stmt
                .query_map(params![seat_id, kind.scope(), start, until], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<std::result::Result<_, _>>()?;
            Ok(rows)
        };
        let mut rows = fetch(end)?;
        // 歷史頁的窗口只到「期間內的末筆」（週檢視的 7d 窗到週日就被截掉）——詳情要看整個窗：
        // 知道重置時刻就把樣本補到重置前（差 1 分鐘避開 cli-cache 重置欄位 ±1 秒的抖動）。
        let reset = rows.iter().rev().find_map(|r| r.2.clone());
        if let Some(r) = reset
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .filter(|r| *r - chrono::Duration::minutes(1) > t_end)
        {
            rows = fetch(&(r - chrono::Duration::minutes(1)).to_rfc3339())?;
        }
        let end = rows.last().map(|r| r.1.as_str()).unwrap_or(end);
        let t_end = chrono::DateTime::parse_from_rfc3339(end)?;
        let peak = rows.iter().map(|r| r.0).fold(0.0f64, f64::max);
        let reset = rows.iter().rev().find_map(|r| r.2.clone());
        let mut source_counts: std::collections::BTreeMap<String, usize> = Default::default();
        for r in &rows {
            *source_counts.entry(r.3.clone()).or_insert(0) += 1;
        }
        // 平均燒速：首筆到峰值那一筆的爬升 ÷ 時間（%/h）；不到 5 分鐘不算。
        let first_pct = rows.first().map(|r| r.0).unwrap_or(0.0);
        let peak_at = rows.iter().find(|r| r.0 >= peak).map(|r| r.1.clone());
        let avg_burn = peak_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| (t - t_start).num_seconds() as f64 / 3600.0)
            .filter(|h| *h >= 5.0 / 60.0)
            .map(|h| ((peak - first_pct) / h * 10.0).round() / 10.0);
        let samples_json: Vec<serde_json::Value> = rows
            .iter()
            .map(|(p, t, r, s)| serde_json::json!({ "at": t, "percent": p, "resetsAt": r, "source": s }))
            .collect();

        // ---- 429：窗內＋末筆後 15 分（同 windows_sync 的寬限）
        let end_grace = (t_end + chrono::Duration::minutes(15)).to_rfc3339();
        let mut anchors: Vec<String> = Vec::new();
        {
            let mut stmt = conn.prepare(&format!(
                "SELECT ts FROM anchors WHERE {} AND ts >= ?1 AND ts <= ?2 ORDER BY ts ASC",
                kind.anchor_sql()
            ))?;
            for row in stmt.query_map(params![start, end_grace], |r| r.get::<_, String>(0))? {
                anchors.push(row?);
            }
        }

        // ---- 本機事件：範圍見函式說明
        let reset_t = reset
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
        let ev_from = match reset_t {
            Some(r) if r - kind.span() < t_start => (r - kind.span()).to_rfc3339(),
            _ => start.to_string(),
        };
        let ev_to = match reset_t {
            Some(r) if r > t_end => r.to_rfc3339(),
            _ => end_grace.clone(),
        };
        let tenure = Self::seat_tenure_sync(conn, seat_id, &ev_from)?;
        let (tsql, tparams) = tenure_where(&tenure, true);
        let mut p: Vec<String> = vec![ev_from.clone(), ev_to.clone()];
        p.extend(tparams.iter().cloned());

        // v1.9.3（D97）：模型／專案／來源都按 API 計價排（price_usd，四種 tokens 照牌價折算）；
        // 總計用同一個輔助、鍵設成常數＝整段一組，跟各表同一把尺。
        let where_sql = format!("ts >= ?1 AND ts < ?2 AND ({tsql})");
        let by_model = usage_mix(conn, "model", "model", &where_sql, &p, 0)?;
        let by_project = usage_mix(conn, PROJECT_KEY_SQL, "project", &where_sql, &p, 8)?;
        let by_entrypoint = usage_mix(conn, ENTRYPOINT_KEY_SQL, "entrypoint", &where_sql, &p, 6)?;
        let total = usage_mix(conn, "'all'", "all", &where_sql, &p, 0)?
            .pop()
            .unwrap_or_else(|| serde_json::json!({ "usd": 0.0 })); // 窗口內一筆事件都沒有＝$0（不是無牌價）
        let tot = |k: &str| total[k].as_i64().unwrap_or(0);

        Ok(serde_json::json!({
            "seat": seat,
            "kind": kind.key(),
            "start": start,
            "end": end,
            "reset": reset,
            "peak": peak,
            "saturated": peak >= 100.0,
            "durationMinutes": (t_end - t_start).num_minutes(),
            "avgBurnPerHour": avg_burn,
            "samples": samples_json,
            "sourceCounts": source_counts,
            "anchors": anchors,
            "usage": {
                "from": ev_from,
                "to": ev_to,
                "byModel": by_model,
                "byProject": by_project,
                "byEntrypoint": by_entrypoint,
                "calls": tot("calls"),
                // B2（21r2 F2）：整段都沒牌價時是 null，不是 0；部分沒牌價時是下限，unpricedCalls 說明幾次沒算
                "usd": total["usd"].clone(),
                "unpricedCalls": tot("unpricedCalls"),
                "inputTokens": tot("inputTokens"),
                "cacheWriteTokens": tot("cacheWriteTokens"),
                "cacheReadTokens": tot("cacheReadTokens"),
                "outputTokens": tot("outputTokens"),
                "tenureSpans": tenure.len(),
            },
        }))
    }

    /// M6 證據包 (D65-2): JSON + two CSVs + Markdown, written by the ledger
    /// itself so the numbers never pass through the UI. Returns the paths.
    pub async fn export_evidence(
        &self,
        seat_account: Option<&str>,
        days: u32,
        dir: &std::path::Path,
        app_version: &str,
    ) -> Result<Vec<PathBuf>> {
        use std::fmt::Write as _;
        let dash = self.dashboard(seat_account, days).await?;
        if dash.get("empty").and_then(|v| v.as_bool()) == Some(true) {
            anyhow::bail!("帳本裡還沒有任何座位，沒東西可匯出");
        }
        // raw intervals — the re-computable evidence
        let all = seat_account == Some(ALL_SEATS);
        let intervals: Vec<Interval> = {
            let conn = self.conn.lock().await;
            let (seats_json, selected) = Self::select_seat_sync(&conn, seat_account)?;
            let d = if days == 0 { 3650 } else { days.clamp(7, 3650) };
            let since = (chrono::Utc::now() - chrono::Duration::days(d as i64)).to_rfc3339();
            let epochs_all = Self::plan_epochs_all_sync(&conn)?;
            let none: Vec<PlanEpoch> = Vec::new();
            if all {
                // v1.8.11：全量＝每個座位各自配對再合起來（同趨勢頁）
                let mut v = Vec::new();
                for s in &seats_json {
                    let Some(id) = s["id"].as_str() else { continue };
                    let ep = if s["isDesktop"].as_bool().unwrap_or(false) { "claude-desktop" } else { "cli" };
                    let eps = epochs_all.get(id).unwrap_or(&none);
                    v.extend(Self::pair_intervals(&conn, id, "'session','five_hour'", &since, ep, eps)?);
                }
                v.sort_by(|x, y| x.t0.cmp(&y.t0));
                v
            } else {
                let (seat_id, _, _, desktop_n) = selected.unwrap();
                let entrypoint = if desktop_n > 0 { "claude-desktop" } else { "cli" };
                let eps = epochs_all.get(&seat_id).unwrap_or(&none);
                Self::pair_intervals(&conn, &seat_id, "'session','five_hour'", &since, entrypoint, eps)?
            }
        };
        // v1.9.4（D99 A9）：方案時間軸也要進證據包——使用者自己換方案造成的台階，事後不能被當成官方縮水的證據。
        let plan_timeline_json: serde_json::Value = {
            let conn = self.conn.lock().await;
            let epochs_all = Self::plan_epochs_all_sync(&conn)?;
            let mut seats: Vec<(&String, &Vec<PlanEpoch>)> = epochs_all.iter().collect();
            seats.sort_by(|a, b| a.0.cmp(b.0));
            serde_json::Value::Array(
                seats
                    .into_iter()
                    .filter(|(id, _)| all || dash["selected"]["id"].as_str() == Some(id.as_str()))
                    .map(|(id, eps)| serde_json::json!({ "seat": id, "epochs": eps.iter().map(PlanEpoch::to_json).collect::<Vec<_>>() }))
                    .collect(),
            )
        };

        std::fs::create_dir_all(dir)?;
        let now = chrono::Local::now();
        let stamp = now.format("%Y%m%d-%H%M").to_string();
        let seat8 = dash["selected"]["accountUuid"]
            .as_str()
            .map(|s| s.chars().take(8).collect::<String>())
            .unwrap_or_else(|| if all { "all-accounts".into() } else { "unknown".into() });
        let period = if days == 0 { "all".to_string() } else { format!("{days}d") };
        let base = format!("evidence-{seat8}-{period}-{stamp}");
        let generated_at = now.to_rfc3339();

        // ---- JSON ----
        let fields = serde_json::json!({
            "fx5[].rate": "修剪 Δ-加權匯率（USD 每 1% 5h 額度）：砍每段區間匯率下尾 25%／上尾 5% 後 Σusd÷Σδ",
            "fx5[].rateRaw": "未修剪 Σusd÷Σδ",
            "fx5[].ci": "rate 的 bootstrap 95% 信心區間 [lo, hi]（區間層級重抽，固定種子）",
            "fx5[].samples": "該月配對區間數 n",
            "fx5[].deltaSum": "該月 Δ% 合計（分母）",
            "fx5[].usdSum": "該月本機事件 API 計價 USD 合計（分子）",
            "fx5[].qualified": "是否達樣本閘（n≥minN 且 Σδ≥minDeltaSum）",
            "fx7": "同 fx5，但分母是 7d 週額度的 Δ%",
            "verdict.declare": "四閘全過才 true；gates[] 逐閘 pass 與說明",
            "verdict.momPct": "匯率方向：本月匯率÷上月−1（正＝1% 買到更多）",
            "verdict.walletPct": "荷包方向：上月匯率÷本月−1（正＝同樣工作要多花額度）",
            "shrink": "固定籃指數：籃＝基期月各模型家族 API$ 佔比；index＝1÷Σ(w_m÷r_m)",
            "byModel": "分模型倍率（v1.6 回歸法）：rows[] 每個模型 id 一列——rate＝$/1%（加權 NNLS 係數的倒數，修剪後區間、w=1/usd、無截距）、ci＝bootstrap 95%（上界 null＝碰到 β→0 無上界）、nSeg 有花錢的段數、zeroShare bootstrap 卡 0 的比例、status ok/calibrating/collinear、multiplier＝rate÷期間修剪匯率（v1.8.7 起；>1＝同樣 1% 額度換到的 API 用量比平均多、較划算，只有 ok 才給；v1.6～v1.8.6 是倒數）；families[] 家族層級的純區間法 vs 回歸交叉驗證（agree）",
            "reconcile.unexplainedShare": "1−rateRaw÷rateTrim：找不到本機事件的 Δ% 佔比（推估）",
            "intervals[]": "每段配對區間：t0/t1 真值時刻、deltaPct、usd（本機事件 API 計價）、byFamily 各家族 USD",
            "windows.<5h|7d|fable>[]": "期間內重建的額度窗口（與歷史頁同一套切法）：start/end 首末樣本、reset 重置時刻（CLI 快取才有）、peak 峰值 %、samples、anchors 窗內 429、saturated 見頂",
            "method": "所有門檻與重抽次數，與畫面方法註記同源",
            "fx5[].plan": "v1.9.4：該列的方案等級鍵（organizationRateLimitTier|organizationType|seatTier 小寫原字串；\"unknown\"＝方案未知）。月匯率按（月, 方案等級）分組，同一個月換過方案就有兩列",
            "planTimeline": "每個座位的方案時間軸：since／until 是 App 偵測到方案的時刻（不回溯）；時間軸沒覆蓋到的時段＝方案未知，不跟已知方案的時段比",
            "verdict.comparable": "false＝這兩個月不是同一個方案等級（或方案未知），不宣告；planCross 說明哪個方案換到哪個、何時換",
            "intervals[].plan": "該區間所屬方案等級；兩端不在同一等級或方案未知＝null",
            "unpriced": "沒有牌價的模型（用量未計入任何匯率）與受影響的區間數",
        });
        // v1.5（D79）：歷史頁三窗的重建同步進證據包——見頂幾次、每窗峰值是
        // 「額度縮水」最直白的證據。與歷史頁走同一個 windows_sync。
        let windows_by_kind: Vec<(HistKind, Vec<HistWindow>)> = {
            let conn = self.conn.lock().await;
            let seat_id = dash["selected"]["id"].as_str().unwrap_or_default().to_string();
            let d = if days == 0 { 3650 } else { days.clamp(7, 3650) };
            let now_utc = chrono::Utc::now();
            let from_utc = (now_utc - chrono::Duration::days(d as i64)).to_rfc3339();
            let to_utc = now_utc.to_rfc3339();
            let mut v = Vec::new();
            for k in [HistKind::FiveHour, HistKind::SevenDay, HistKind::Fable] {
                if all {
                    // v1.8.11：全量＝每個座位各自重建再合起來
                    let mut merged: Vec<HistWindow> = Vec::new();
                    for s in dash["seats"].as_array().into_iter().flatten() {
                        let Some(id) = s["id"].as_str() else { continue };
                        let (_, w, _) = Self::windows_sync(&conn, id, k, &from_utc, &to_utc, false)?;
                        merged.extend(w);
                    }
                    merged.sort_by(|x, y| x.start.cmp(&y.start));
                    v.push((k, merged));
                } else {
                    let (_, w, _) = Self::windows_sync(&conn, &seat_id, k, &from_utc, &to_utc, false)?;
                    v.push((k, w));
                }
            }
            v
        };
        let windows_json: serde_json::Map<String, serde_json::Value> = windows_by_kind
            .iter()
            .map(|(k, w)| (k.key().to_string(), serde_json::to_value(w).unwrap_or_default()))
            .collect();
        let iv_json: Vec<serde_json::Value> = intervals
            .iter()
            .map(|i| {
                serde_json::json!({
                    "t0": i.t0, "t1": i.t1, "deltaPct": i.iv.delta, "usd": i.iv.usd,
                    "plan": i.plan, "unpriced": i.unpriced,
                    "byFamily": (0..5).map(|f| (crate::stats::FAMILIES[f].to_string(), serde_json::json!(i.iv.by_family[f]))).collect::<serde_json::Map<String, serde_json::Value>>(),
                })
            })
            .collect();
        let json = serde_json::json!({
            "schema": "cum-evidence/1",
            "generatedAt": generated_at,
            "appVersion": app_version,
            "periodDays": days,
            "fields": fields,
            "seat": dash["selected"],
            "plan": dash["plan"], "planTimeline": plan_timeline_json, "unpriced": dash["unpriced"],
            "fx5": dash["fx5"], "fx7": dash["fx7"], "verdict": dash["verdict"], "shrink": dash["shrink"],
            "byModel": dash["byModel"], "reconcile": dash["reconcile"], "method": dash["method"],
            "workRate": dash["workRate"], "walls": dash["walls"], "saturatedDays": dash["saturatedDays"],
            "intervals": iv_json,
            "windows": windows_json,
        });
        let mut paths = Vec::new();
        let p = dir.join(format!("{base}.json"));
        std::fs::write(&p, serde_json::to_string_pretty(&json)?)?;
        paths.push(p);

        // ---- monthly.csv ----
        let mut csv = String::from("window,month,samples,deltaSum,usdSum,rateRaw,rateTrim,ciLo,ciHi,qualified,plan,planLabel\n");
        for (win, key) in [("5h", "fx5"), ("7d", "fx7")] {
            for m in dash[key].as_array().into_iter().flatten() {
                let f = |k: &str| m[k].as_f64().map(|v| format!("{v:.4}")).unwrap_or_default();
                let ci = m["ci"].as_array();
                let _ = writeln!(
                    csv,
                    "{win},{},{},{},{},{},{},{},{},{},{},{}",
                    m["month"].as_str().unwrap_or(""),
                    m["samples"].as_u64().unwrap_or(0),
                    f("deltaSum"),
                    f("usdSum"),
                    f("rateRaw"),
                    f("rate"),
                    ci.and_then(|c| c.first()).and_then(|v| v.as_f64()).map(|v| format!("{v:.4}")).unwrap_or_default(),
                    ci.and_then(|c| c.get(1)).and_then(|v| v.as_f64()).map(|v| format!("{v:.4}")).unwrap_or_default(),
                    m["qualified"].as_bool().unwrap_or(false),
                    // 新欄放最後，舊的重放腳本按欄位順序讀才不會錯位。
                    // plan＝方案鍵（max_20x／other|…／unknown，跟 intervals.csv 同一個口徑，用它對得起來）；
                    // 同一個月換過方案就有兩列，(window, month) 不再唯一，要用 (window, month, plan)。
                    m["plan"].as_str().unwrap_or("unknown").replace(',', ";"),
                    m["planLabel"].as_str().unwrap_or("").replace(',', ";"),
                );
            }
        }
        let p = dir.join(format!("{base}-monthly.csv"));
        std::fs::write(&p, csv)?;
        paths.push(p);

        // ---- intervals.csv ----
        let mut csv = String::from("t0,t1,deltaPct,usd,fable,opus,sonnet5,sonnet,haiku,plan,unpriced\n");
        for i in &intervals {
            let b = &i.iv.by_family;
            let _ = writeln!(
                csv,
                "{},{},{:.2},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{},{}",
                i.t0, i.t1, i.iv.delta, i.iv.usd, b[0], b[1], b[2], b[3], b[4],
                i.plan.as_deref().unwrap_or(PLAN_UNKNOWN).replace(',', ";"),
                i.unpriced
            );
        }
        let p = dir.join(format!("{base}-intervals.csv"));
        std::fs::write(&p, csv)?;
        paths.push(p);

        // ---- windows.csv（v1.5／D79）----
        let mut csv = String::from("limit,start,end,reset,peak,samples,anchors,saturated,sources\n");
        for (k, ws) in &windows_by_kind {
            for w in ws {
                let _ = writeln!(
                    csv,
                    "{},{},{},{},{:.0},{},{},{},{}",
                    k.key(),
                    w.start,
                    w.end,
                    w.reset.as_deref().unwrap_or(""),
                    w.peak,
                    w.samples,
                    w.anchors,
                    w.saturated,
                    w.sources.join("+"),
                );
            }
        }
        let p = dir.join(format!("{base}-windows.csv"));
        std::fs::write(&p, csv)?;
        paths.push(p);

        // ---- report.md ----
        let p = dir.join(format!("{base}.md"));
        let mut md = evidence_markdown(&dash, days, intervals.len(), app_version, &generated_at);
        md.push_str(&windows_markdown(&windows_by_kind));
        std::fs::write(&p, md)?;
        paths.push(p);
        Ok(paths)
    }
}

/// v1.5（D79）：證據報告的「額度窗口」段——三條額度各幾個窗、見頂幾次、
/// 最高峰值、窗內 429。數字與歷史頁同源（同一個 windows_sync）。
fn windows_markdown(by_kind: &[(HistKind, Vec<HistWindow>)]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "\n## 額度窗口（重建）\n");
    let _ = writeln!(s, "與歷史檢視頁同一套切法：真值 % 下降 >2、靜默超過窗長、窗長到期、或重置時刻跳動即為新窗；峰值＝窗內最高真值。\n");
    let _ = writeln!(s, "| 額度 | 窗口數 | 見頂窗（100%） | 最高峰值 | 窗內 429 | 帶重置時刻的窗 |\n|---|---|---|---|---|---|");
    for (k, ws) in by_kind {
        let name = match k {
            HistKind::FiveHour => "5h",
            HistKind::SevenDay => "7d",
            HistKind::Fable => "Fable 7d",
        };
        let peak = ws.iter().map(|w| w.peak).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |x| x.max(v))));
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {} |",
            name,
            ws.len(),
            ws.iter().filter(|w| w.saturated).count(),
            peak.map(|p| format!("{p:.0}%")).unwrap_or_else(|| "—".into()),
            ws.iter().map(|w| w.anchors).sum::<usize>(),
            ws.iter().filter(|w| w.reset.is_some()).count(),
        );
    }
    let _ = writeln!(s, "\n逐窗明細在 `-windows.csv`（limit／start／end／reset／peak／samples／anchors／saturated／sources）。");
    s
}

/// The paste-able report (D65-2). Every number comes from the dashboard
/// payload — the same one the UI renders — so report and screen never drift.
fn evidence_markdown(dash: &serde_json::Value, days: u32, n_intervals: usize, app_version: &str, generated_at: &str) -> String {
    use std::fmt::Write as _;
    let f2 = |v: &serde_json::Value| v.as_f64().map(|x| format!("{x:.2}")).unwrap_or_else(|| "—".into());
    let mut s = String::new();
    let seat = dash["selected"]["accountUuid"].as_str().map(|a| a.chars().take(8).collect::<String>()).unwrap_or_default();
    let ep = if dash["selected"]["entrypoint"] == "claude-desktop" { "Desktop" } else { "CLI" };
    let period = if days == 0 { "全部資料".to_string() } else { format!("近 {days} 天") };
    let _ = writeln!(s, "# Claude 訂閱額度匯率 · 證據報告\n");
    let _ = writeln!(s, "- 產生時間：{generated_at}（Claude Usage Monitor v{app_version}）");
    let _ = writeln!(s, "- 座位：{seat}（{ep} 帳號）· 期間：{period} · 配對區間 {n_intervals} 段");
    let _ = writeln!(s, "- 匯率 R ＝「1% 的 5h 額度約當多少美金 API 用量」。**R 下降＝同樣 1% 買到的變少＝對使用者是壞消息。**\n");

    let v = &dash["verdict"];
    let _ = writeln!(s, "## 結論\n");
    if v["declare"].as_bool() == Some(true) {
        let wallet = v["walletPct"].as_f64().unwrap_or(0.0);
        let _ = writeln!(
            s,
            // 月份帶年（YYYY-MM）：隔滿一年不會寫成「10 月比 10 月」
            "**{} 比 {}{} {:.1}%**（匯率 ${} → ${}，五道檢查全過：同一個方案、樣本、5h 置換檢定、7d 雙視窗同向、固定籃指數）。\n",
            v["curMonth"].as_str().unwrap_or(""),
            v["prevMonth"].as_str().unwrap_or(""),
            if wallet > 0.0 { "貴" } else { "便宜" },
            wallet.abs(),
            f2(&v["ratePrev"]),
            f2(&v["rateCur"]),
        );
    } else {
        let failed = v["gates"].as_array().and_then(|g| g.iter().find(|x| x["pass"].as_bool() != Some(true)));
        let _ = writeln!(
            s,
            "**不宣告縮水或放寬。** 卡在「{}」——{}。\n",
            failed.and_then(|g| g["label"].as_str()).unwrap_or("檢定"),
            // 有的說明句本身已經以「。」結尾，不要寫成「。。」
            failed.and_then(|g| g["detail"].as_str()).unwrap_or("").trim_end_matches('。'),
        );
        if let (Some(mom), Some(wallet)) = (v["momPct"].as_f64(), v["walletPct"].as_f64()) {
            // 兩個月不一定相鄰（同方案才比，可以隔月）：相鄰才寫「較上月」，否則寫出是哪個月
            let vs = if v["monthsApart"].as_i64().unwrap_or(1) <= 1 {
                "較上月".to_string()
            } else {
                format!("較 {}", v["prevMonth"].as_str().unwrap_or("前一個同方案的月份"))
            };
            let _ = writeln!(
                s,
                "資料本身：匯率 ${} → ${}，{}{} {:.1}%（同樣的工作要{}花 {:.1}% 的額度）——未過檢定，當雜訊看。\n",
                f2(&v["ratePrev"]), f2(&v["rateCur"]), vs,
                if mom < 0.0 { "跌" } else { "升" }, mom.abs(),
                if wallet > 0.0 { "多" } else { "少" }, wallet.abs(),
            );
        }
    }
    if let Some(gates) = v["gates"].as_array() {
        let _ = writeln!(s, "| 閘 | 結果 | 說明 |\n|---|---|---|");
        for g in gates {
            let _ = writeln!(
                s,
                "| {} | {} | {} |",
                g["label"].as_str().unwrap_or(""),
                if g["pass"].as_bool() == Some(true) { "✅ 過" } else { "❌ 未過" },
                g["detail"].as_str().unwrap_or("")
            );
        }
        s.push('\n');
    }

    // v1.9.4（D99 A9）：方案時間軸——結論句只在同一個方案等級時才給（上面的 declare 已經守門），
    // 這裡把「這段期間是什麼方案」寫明，事後有人拿報告當縮水證據時，換方案造成的台階看得出來。
    let _ = writeln!(s, "## 方案時間軸\n");
    match dash["plan"]["segments"].as_array().filter(|a| !a.is_empty()) {
        Some(segs) => {
            let _ = writeln!(s, "| 自 | 到 | 方案 | 區間數 |\n|---|---|---|---|");
            for g in segs {
                let _ = writeln!(
                    s,
                    "| {} | {} | {} | {} |",
                    local_stamp(g["since"].as_str().unwrap_or("")),
                    g["until"].as_str().map(local_stamp).unwrap_or_else(|| "現在".into()),
                    g["planLabel"].as_str().unwrap_or(""),
                    g["intervals"].as_u64().unwrap_or(0),
                );
            }
            let _ = writeln!(
                s,
                "\nApp 只記錄偵測到的方案（不回溯、不手填）；「方案未知」的時段不跟已知方案的時段比較。匯率只在**同一個方案等級**的月份之間比，下表每列是（月, 方案）一組。\n"
            );
        }
        None => {
            let _ = writeln!(s, "（全部帳號視角不合併匯率——各帳號的方案與額度池不同。）\n");
        }
    }
    if dash["unpriced"]["models"].as_array().is_some_and(|a| !a.is_empty()) {
        let names: Vec<String> = dash["unpriced"]["models"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| m["model"].as_str().unwrap_or("?").to_string())
            .collect();
        let _ = writeln!(s, "> 沒有牌價的模型（用量未計入任何匯率）：{}。\n", names.join("、"));
    }

    for (title, key, unit) in [("5h 額度月匯率", "fx5", "5h"), ("7d 額度月匯率（只比方向）", "fx7", "7d")] {
        let _ = writeln!(s, "## {title}\n");
        let _ = writeln!(s, "| 月 | 方案 | n | Δ% 合計 | USD 合計 | 未修剪 | 修剪匯率 $/1% {unit} | 95% CI | 樣本閘 |\n|---|---|---|---|---|---|---|---|---|");
        for m in dash[key].as_array().into_iter().flatten() {
            let ci = m["ci"].as_array().map(|c| format!("{}–{}", f2(&c[0]), f2(&c[1]))).unwrap_or_else(|| "—".into());
            let _ = writeln!(
                s,
                "| {} | {} | {} | {:.0} | {:.0} | {} | {} | {} | {} |",
                m["month"].as_str().unwrap_or(""),
                m["planLabel"].as_str().unwrap_or(""),
                m["samples"].as_u64().unwrap_or(0),
                m["deltaSum"].as_f64().unwrap_or(0.0),
                m["usdSum"].as_f64().unwrap_or(0.0),
                f2(&m["rateRaw"]),
                f2(&m["rate"]),
                ci,
                if m["qualified"].as_bool() == Some(true) { "過" } else { "校準中" },
            );
        }
        s.push('\n');
    }

    let sh = &dash["shrink"];
    if let Some(basket) = sh["basket"].as_array() {
        let b: Vec<String> = basket
            .iter()
            .map(|x| format!("{} {:.0}%", x["family"].as_str().unwrap_or(""), x["share"].as_f64().unwrap_or(0.0) * 100.0))
            .collect();
        let _ = writeln!(s, "## 固定籃指數（縮水指數）\n");
        let _ = writeln!(s, "籃＝{} 月的模型組合（{}），凍結不動；展示匯率跟著用法走，固定籃只看廠商給的有沒有變。\n", sh["baseMonth"].as_str().map(|m| &m[5..]).unwrap_or(""), b.join("／"));
        let _ = writeln!(s, "| 月 | 指數 $/1% | 95% CI | 籃覆蓋 |\n|---|---|---|---|");
        for m in sh["monthly"].as_array().into_iter().flatten() {
            let ci = m["ci"].as_array().map(|c| format!("{}–{}", f2(&c[0]), f2(&c[1]))).unwrap_or_else(|| "—".into());
            let _ = writeln!(s, "| {} | {} | {} | {} |", m["month"].as_str().unwrap_or(""), f2(&m["index"]), ci,
                m["coverage"].as_f64().map(|c| format!("{:.0}%", c * 100.0)).unwrap_or_else(|| "—".into()));
        }
        s.push('\n');
    }

    if let Some(rows) = dash["byModel"]["rows"].as_array().filter(|a| !a.is_empty()) {
        let bm = &dash["byModel"];
        let _ = writeln!(s, "## 模型別額度倍率（回歸法，v1.6）\n");
        let _ = writeln!(
            s,
            "加權非負最小平方：每段區間 Δ% ＝ Σ 各模型 API$ × 係數（係數 ≥0、無截距、權重 1/usd、與匯率同一套修剪）；$/1% ＝ 係數倒數。修剪後 n={}，R²（無截距）{}。\n",
            bm["n"].as_u64().unwrap_or(0),
            f2(&bm["r2"])
        );
        let ci_of = |m: &serde_json::Value, key: &str| {
            m[key].as_array().map(|c| {
                let hi = c.get(1).and_then(|v| v.as_f64()).map(|v| format!("{v:.2}")).unwrap_or_else(|| "∞".into());
                format!("{}–{}", f2(&c[0]), hi)
            }).unwrap_or_else(|| "—".into())
        };
        let status_of = |st: &str, with: Option<&str>| match st {
            "ok" => "✅".to_string(),
            "collinear" => format!("共線（與 {} 一起看）", with.unwrap_or("?")),
            _ => "校準中".to_string(),
        };
        let mult_of = |m: &serde_json::Value| m["multiplier"].as_f64().map(|v| format!("{v:.2}×")).unwrap_or_else(|| "—".into());
        let fam_label = |f: &str| -> String {
            match f { "fable" => "Fable", "opus" => "Opus", "sonnet" => "Sonnet", "haiku" => "Haiku", other => other }.to_string()
        };
        let _ = writeln!(s, "| 模型 | $/1% | 95% CI | 有花錢的段數 | 狀態 | 每 1% 額度換到的 API 用量（vs 平均，>1 較划算） |\n|---|---|---|---|---|---|");
        // 家族綜合費率（各模型合併再回歸）粗體一列，底下縮排該家族的模型
        let empty = Vec::new();
        let fams = bm["families"].as_array().unwrap_or(&empty);
        for fam in ["fable", "opus", "sonnet", "haiku"] {
            let members: Vec<&serde_json::Value> = rows.iter().filter(|m| m["family"].as_str() == Some(fam)).collect();
            let f = fams.iter().find(|x| x["family"].as_str() == Some(fam));
            if members.is_empty() && f.is_none() {
                continue;
            }
            if let Some(f) = f {
                let agree = match f["agree"].as_bool() {
                    Some(true) => "、純區間法一致",
                    Some(false) => "、與純區間法不一致",
                    None => "",
                };
                let _ = writeln!(
                    s,
                    "| **{}（合併）** | **{}** | {} | {} | {}{} | **{}** |",
                    fam_label(fam),
                    f2(&f["regRate"]),
                    ci_of(f, "regCi"),
                    f["nSeg"].as_u64().unwrap_or(0),
                    status_of(f["regStatus"].as_str().unwrap_or(""), None),
                    agree,
                    mult_of(f)
                );
            }
            for m in members {
                let _ = writeln!(
                    s,
                    "| ・{} | {} | {} | {} | {} | {} |",
                    m["label"].as_str().unwrap_or(""),
                    f2(&m["rate"]),
                    ci_of(m, "ci"),
                    m["nSeg"].as_u64().unwrap_or(0),
                    status_of(m["status"].as_str().unwrap_or(""), m["collinearWith"].as_str()),
                    mult_of(m)
                );
            }
        }
        if !fams.is_empty() {
            let _ = writeln!(s, "\n交叉驗證（家族層級：純區間法 vs 回歸）：");
            for f in fams {
                let agree = match f["agree"].as_bool() {
                    Some(true) => "一致",
                    Some(false) => "不一致",
                    None => "純區間不足，無法對照",
                };
                let _ = writeln!(
                    s,
                    "- {}：純區間 ${}（n={}）· 回歸 ${} → {}",
                    fam_label(f["family"].as_str().unwrap_or("")),
                    f2(&f["pureRate"]),
                    f["pureN"].as_u64().unwrap_or(0),
                    f2(&f["regRate"]),
                    agree
                );
            }
        }
        s.push('\n');
    }

    let rc = &dash["reconcile"];
    let _ = writeln!(s, "## 真值對帳\n");
    let _ = writeln!(
        s,
        "期間未修剪匯率 ${}、修剪匯率 ${}；找不到本機事件的 Δ% 佔比（推估）{}，其中完全零事件的區間佔 {}。\n",
        f2(&rc["rateRaw"]), f2(&rc["rateTrim"]),
        rc["unexplainedShare"].as_f64().map(|x| format!("{:.0}%", x * 100.0)).unwrap_or_else(|| "—".into()),
        rc["zeroEventDeltaShare"].as_f64().map(|x| format!("{:.0}%", x * 100.0)).unwrap_or_else(|| "—".into()),
    );

    let m = &dash["method"];
    let _ = writeln!(s, "## 方法與已知弱點\n");
    let _ = writeln!(s, "- 分子＝本機 Claude 對話記錄（JSONL）的 API 計價 USD（官方價目表，含快取寫入／讀取、web search）；分母＝訂閱額度真值百分比的 Δ%（探針 `/usage`、CLI 快取、Desktop 歷史檔），同一座位、≤45 分鐘、嚴格遞增的相鄰樣本配成一段區間。");
    let _ = writeln!(s, "- 估計量＝Δ-加權比值 Σusd÷Σδ；非對稱修剪：砍每段區間匯率下尾 {:.0}%／上尾 {:.0}%（下尾被「本機看不到的用量」污染）。", m["trim"][0].as_f64().unwrap_or(0.25) * 100.0, (1.0 - m["trim"][1].as_f64().unwrap_or(0.95)) * 100.0);
    let _ = writeln!(s, "- 信心區間＝區間層級 bootstrap {} 次（固定種子）；月差＝置換檢定 {} 次、α={}；樣本閘 n≥{} 且 Σδ≥{}。", m["bootstrapReps"], m["permReps"], m["alpha"], m["minN"], m["minDeltaSum"]);
    let _ = writeln!(s, "- 縮水宣告要四道閘全過：樣本閘、5h 檢定、7d 雙視窗同向且顯著、固定籃指數同向且顯著。任一沒過只呈現數據。");
    let _ = writeln!(s, "- **已知弱點**：區間彼此有時序相關，p 值偏樂觀（雜訊閘，不是證明）；真值百分比實質精度 1 個百分點；網頁端與其他裝置的用量不在分子裡（對帳段已估其佔比）；權重＝API 價目表常數，未從資料估。");
    let _ = writeln!(s, "- 所有數字與畫面同源、可由 `-intervals.csv` 重算。工具：Claude Usage Monitor（自用、本機、不爬網頁）。");
    s
}

#[cfg(test)]
mod segment_tests {
    //! v1.5（D79）：三窗切分規則的單元測試。樣本形態照真帳本：cli-cache 的
    //! resets_at 在整點前後 ±1 秒抖；Desktop 歷史沒有 resets_at 只能靠下降。
    use super::{segment_windows, thin_points, HistKind};

    fn t(h: i64, m: i64) -> String {
        // 2026-09-01 起算的 UTC 時刻
        (chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00+00:00").unwrap()
            + chrono::Duration::minutes(h * 60 + m))
            .to_rfc3339()
    }
    fn row(p: f64, h: i64, m: i64, reset: Option<&str>, src: &str) -> (f64, String, Option<String>, String) {
        (p, t(h, m), reset.map(|s| s.to_string()), src.to_string())
    }

    #[test]
    fn reset_jitter_does_not_split_a_window() {
        let rows = vec![
            row(10.0, 0, 0, Some("2026-09-01T05:00:00.4+00:00"), "cli-cache"),
            row(12.0, 0, 10, Some("2026-09-01T04:59:59.7+00:00"), "cli-cache"),
            row(15.0, 0, 20, Some("2026-09-01T05:00:00.1+00:00"), "cli-cache"),
        ];
        let w = segment_windows(&rows, false, HistKind::FiveHour, &[]);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].peak, 15.0);
        assert_eq!(w[0].samples, 3);
        assert_eq!(w[0].reset.as_deref(), Some("2026-09-01T05:00:00.4+00:00"));
    }

    #[test]
    fn reset_jump_with_a_fresh_percent_splits_even_without_a_big_drop() {
        // 4% → 2% 只降 2（下降規則不切），但 resets_at 往後跳了 5 小時且 % 很低＝新窗
        let rows = vec![
            row(4.0, 0, 0, Some("2026-09-01T05:00:00+00:00"), "cli-cache"),
            row(4.0, 1, 0, Some("2026-09-01T05:00:00+00:00"), "cli-cache"),
            row(2.0, 2, 0, Some("2026-09-01T10:00:00+00:00"), "cli-cache"),
            row(3.0, 2, 10, Some("2026-09-01T10:00:00+00:00"), "cli-cache"),
        ];
        let w = segment_windows(&rows, false, HistKind::FiveHour, &[]);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].samples, 2);
        assert_eq!(w[1].samples, 2);
        assert_eq!(w[1].reset.as_deref(), Some("2026-09-01T10:00:00+00:00"));
    }

    #[test]
    fn reset_jump_with_a_high_percent_is_a_bad_cache_field_not_a_new_window() {
        // 真帳本 09-11：17% 帶著 09-13T01 的重置，五分鐘後同樣 17% 改帶 09-16T08——
        // 剛換窗不可能已經 17%，所以不切，重置欄改記後來的值。
        let rows = vec![
            row(17.0, 0, 0, Some("2026-09-13T01:00:00+00:00"), "cli-cache"),
            row(17.0, 0, 5, Some("2026-09-16T08:00:00+00:00"), "cli-cache"),
            row(25.0, 3, 0, Some("2026-09-16T08:00:00+00:00"), "cli-cache"),
        ];
        let w = segment_windows(&rows, false, HistKind::SevenDay, &[]);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].reset.as_deref(), Some("2026-09-16T08:00:00+00:00"));
    }

    #[test]
    fn a_lone_dip_between_two_sources_is_not_a_reset() {
        // 真帳本 09-11 08:54–09:21：Desktop 報 10、11、12，探針＋CLI 快取連續四筆 0
        // 又彈回來——半小時內彈回原高度，不是重置。
        let rows = vec![
            row(10.0, 0, 0, None, "desktop-history"),
            row(0.0, 0, 3, Some("2026-09-13T01:00:00+00:00"), "cli-cache"),
            row(11.0, 0, 15, None, "desktop-history"),
            row(0.0, 0, 19, None, "probe"),
            row(0.0, 0, 19, Some("2026-09-13T01:00:00+00:00"), "cli-cache"),
            row(0.0, 0, 29, None, "probe"),
            row(0.0, 0, 29, Some("2026-09-13T01:00:00+00:00"), "cli-cache"),
            row(12.0, 0, 30, None, "desktop-history"),
            row(31.0, 30, 0, None, "desktop-history"),
        ];
        let w = segment_windows(&rows, true, HistKind::SevenDay, &[]);
        assert_eq!(w.len(), 1, "連續幾筆孤立的 0 不是重置：{w:?}");
        assert_eq!(w[0].samples, 9, "孤立值仍算樣本");
        assert_eq!(w[0].peak, 31.0);
        let pts = w[0].points.as_ref().unwrap();
        assert!(pts.iter().all(|(_, p)| *p > 0.0), "孤立值不進階梯：{pts:?}");
        assert!(w[0].reset.is_none(), "孤立值帶的重置時刻不採信");
        // 但連續三筆都低就是真的重置
        let rows2 = vec![
            row(40.0, 0, 0, None, "desktop-history"),
            row(0.0, 1, 0, None, "desktop-history"),
            row(1.0, 1, 10, None, "desktop-history"),
            row(2.0, 1, 20, None, "desktop-history"),
            row(3.0, 1, 30, None, "desktop-history"),
        ];
        assert_eq!(segment_windows(&rows2, false, HistKind::SevenDay, &[]).len(), 2);
    }

    #[test]
    fn seven_day_window_survives_days_of_silence_and_cuts_on_drop() {
        // Desktop 歷史：沒有 resets_at，三天沒樣本也還是同一窗；% 掉回 0 才換窗。
        let rows = vec![
            row(10.0, 0, 0, None, "desktop-history"),
            row(40.0, 24, 0, None, "desktop-history"),
            row(58.0, 24 * 4, 0, None, "desktop-history"), // 3 天靜默（5h 規則會切，7d 不會）
            row(0.0, 24 * 7 + 1, 0, None, "desktop-history"), // 重置後第一筆
            row(12.0, 24 * 7 + 2, 0, None, "desktop-history"),
        ];
        let w7 = segment_windows(&rows, false, HistKind::SevenDay, &[]);
        assert_eq!(w7.len(), 2, "7d: 一週一窗，中間的靜默不切");
        assert_eq!(w7[0].peak, 58.0);
        assert_eq!(w7[0].samples, 3);
        assert_eq!(w7[1].peak, 12.0);
        let w5 = segment_windows(&rows, false, HistKind::FiveHour, &[]);
        assert!(w5.len() >= 4, "5h: 每段靜默 >5h 都切開，{} 窗", w5.len());
    }

    #[test]
    fn seven_day_span_expiry_cuts_a_stale_climb() {
        // 沒有下降、沒有 resets_at，但從第一筆算起已過 7 天又 2 小時＝一定是下一窗
        let rows = vec![
            row(20.0, 0, 0, None, "desktop-history"),
            row(50.0, 24 * 6, 0, None, "desktop-history"),
            row(55.0, 24 * 7 + 2, 0, None, "desktop-history"),
        ];
        let w = segment_windows(&rows, false, HistKind::SevenDay, &[]);
        assert_eq!(w.len(), 2);
        assert_eq!(w[1].peak, 55.0);
    }

    #[test]
    fn fable_uses_the_seven_day_span() {
        assert_eq!(HistKind::Fable.span(), HistKind::SevenDay.span());
        assert_eq!(HistKind::parse("fable"), HistKind::Fable);
        assert_eq!(HistKind::parse("7d"), HistKind::SevenDay);
        assert_eq!(HistKind::parse("whatever"), HistKind::FiveHour);
    }

    #[test]
    fn thinning_keeps_steps_and_the_last_point() {
        let pts: Vec<(String, f64)> = [(0, 10.0), (1, 10.0), (2, 10.0), (3, 20.0), (4, 20.0), (5, 20.0)]
            .iter()
            .map(|(i, p)| (t(0, *i), *p))
            .collect();
        let out = thin_points(pts);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].1, 10.0);
        assert_eq!(out[1].1, 20.0);
        assert_eq!(out[2].0, t(0, 5), "最後一筆一定留，階梯才延伸到窗尾");
        // keep_points 走同一條去重
        let rows = vec![
            row(10.0, 0, 0, None, "probe"),
            row(10.0, 0, 10, None, "probe"),
            row(30.0, 0, 20, None, "probe"),
            row(30.0, 0, 30, None, "probe"),
        ];
        let w = segment_windows(&rows, true, HistKind::FiveHour, &[]);
        assert_eq!(w[0].points.as_ref().unwrap().len(), 3);
        assert_eq!(w[0].samples, 4, "去重只動階梯點，樣本數照舊");
    }
}

#[cfg(test)]
mod probe {
    //! Real-ledger probe (not a unit test): `CUM_LEDGER_DIR=<dir> cargo test
    //! --lib probe -- --nocapture` dumps the M5 payloads to CUM_PROBE_OUT.
    /// v1.8.2 效能量測：`CUM_LEDGER_DIR=<dir> CUM_TIMING=1 cargo test --lib probe::timing -- --nocapture`
    /// 印出趨勢頁／歷史頁／今日頁各算一次要多久（第二次呼叫看快取有沒有接住）。
    #[tokio::test]
    async fn timing() {
        let Ok(dir) = std::env::var("CUM_LEDGER_DIR") else { return };
        let ledger = super::Ledger::open(dir.into()).unwrap();
        let t = |label: &str, d: std::time::Duration| eprintln!("[probe] {label:<22} {:>8.1} ms", d.as_secs_f64() * 1e3);
        let s = std::time::Instant::now();
        let _ = ledger.dashboard(None, 180).await.unwrap();
        t("dashboard 180d #1", s.elapsed());
        let s = std::time::Instant::now();
        let _ = ledger.dashboard(None, 180).await.unwrap();
        t("dashboard 180d #2", s.elapsed());
        let s = std::time::Instant::now();
        let _ = ledger.dashboard(None, 30).await.unwrap();
        t("dashboard 30d", s.elapsed());
        let s = std::time::Instant::now();
        let _ = ledger.history(None, "week", 0, super::HistKind::FiveHour).await.unwrap();
        t("history week 5h", s.elapsed());
        let s = std::time::Instant::now();
        let _ = ledger.history(None, "month", 0, super::HistKind::SevenDay).await.unwrap();
        t("history month 7d", s.elapsed());
        let s = std::time::Instant::now();
        let _ = ledger.today_outlook(None).await.unwrap();
        t("today_outlook", s.elapsed());
        let s = std::time::Instant::now();
        let _ = ledger.burn_stats(None).await.unwrap();
        t("burn_stats", s.elapsed());
        let s = std::time::Instant::now();
        let _ = ledger.health_snapshot().await.unwrap();
        t("health_snapshot", s.elapsed());
    }

    #[tokio::test]
    async fn dump_m5_payloads() {
        let Ok(dir) = std::env::var("CUM_LEDGER_DIR") else { return };
        let out = std::env::var("CUM_PROBE_OUT").unwrap_or_else(|_| "probe-out".into());
        let ledger = super::Ledger::open(dir.into()).unwrap();
        let dash = ledger.dashboard(None, 180).await.unwrap();
        let dash_cli = ledger.dashboard(Some("646c6f2e-eb7b-431b-8c89-911546a9e437"), 180).await.unwrap();
        // v1.8.11（D89）：全量視角也倒一份，驗「所有座位合起來」的數字比單一座位大
        let dash_all = ledger.dashboard(Some(super::ALL_SEATS), 180).await.unwrap();
        let today = ledger.today_outlook(None).await.unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(format!("{out}/dashboard.json"), serde_json::to_string_pretty(&dash).unwrap()).unwrap();
        std::fs::write(format!("{out}/dashboard-cli.json"), serde_json::to_string_pretty(&dash_cli).unwrap()).unwrap();
        std::fs::write(format!("{out}/dashboard-all.json"), serde_json::to_string_pretty(&dash_all).unwrap()).unwrap();
        for kind in [super::HistKind::FiveHour, super::HistKind::SevenDay, super::HistKind::Fable] {
            let h = ledger.history(Some(super::ALL_SEATS), "week", -1, kind).await.unwrap();
            std::fs::write(format!("{out}/history-week-{}-all.json", kind.key()), serde_json::to_string_pretty(&h).unwrap()).unwrap();
        }
        std::fs::write(format!("{out}/today.json"), serde_json::to_string_pretty(&today).unwrap()).unwrap();
        // M6: history spans + evidence export
        // D72 原型回放：CUM_PROBE_SAMPLES=1 時倒出 5h／7d 的真值樣本原始列，
        // 給 prototypes/predictor-replay.cjs 比較長窗／短窗預測器。
        if std::env::var("CUM_PROBE_SAMPLES").is_ok() {
            let conn = ledger.conn.lock().await;
            for (name, kinds) in [
                ("samples-5h", "'session','five_hour'"),
                ("samples-7d", "'weekly_all','seven_day'"),
            ] {
                let sql = format!(
                    "SELECT percent, fetched_at, resets_at, source, seat_id FROM truth_samples \
                     WHERE limit_kind IN ({kinds}) \
                     ORDER BY fetched_at ASC"
                );
                let mut stmt = conn.prepare(&sql).unwrap();
                let rows: Vec<serde_json::Value> = stmt
                    .query_map([], |r| {
                        Ok(serde_json::json!({
                            "percent": r.get::<_, f64>(0)?,
                            "fetchedAt": r.get::<_, String>(1)?,
                            "resetsAt": r.get::<_, Option<String>>(2)?,
                            "source": r.get::<_, String>(3)?,
                            "seatId": r.get::<_, Option<String>>(4)?,
                        }))
                    })
                    .unwrap()
                    .collect::<std::result::Result<_, _>>()
                    .unwrap();
                std::fs::write(format!("{out}/{name}.json"), serde_json::to_string(&rows).unwrap()).unwrap();
                println!("{name}: {} rows", rows.len());
            }
            let mut stmt = conn.prepare("SELECT ts, rate_limit_type FROM anchors ORDER BY ts ASC").unwrap();
            let rows: Vec<serde_json::Value> = stmt
                .query_map([], |r| Ok(serde_json::json!({ "ts": r.get::<_, String>(0)?, "kind": r.get::<_, Option<String>>(1)? })))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            std::fs::write(format!("{out}/anchors.json"), serde_json::to_string(&rows).unwrap()).unwrap();
            println!("anchors: {} rows", rows.len());
        }
        // v1.1.1（D69）：CUM_PROBE_SEAT=<seat id> 時多倒一份指定席位的歷史，驗「按 id 挑席位」。
        if let Ok(seat) = std::env::var("CUM_PROBE_SEAT") {
            let h = ledger.history(Some(&seat), "day", 0, super::HistKind::FiveHour).await.unwrap();
            std::fs::write(format!("{out}/history-seat.json"), serde_json::to_string_pretty(&h).unwrap()).unwrap();
            // v1.4.1：指定席位的今日頁 payload——驗 expired 旗標（切換器看別的座位）。
            let t = ledger.today_outlook(Some(&seat)).await.unwrap();
            std::fs::write(format!("{out}/today-seat.json"), serde_json::to_string_pretty(&t).unwrap()).unwrap();
        }
        // v1.5（D79）：三條額度 × 三個尺度都倒，檔名 history-{unit}-{kind}.json；
        // history-{unit}.json 仍是 5h（舊 fixture 名）。
        for kind in [super::HistKind::FiveHour, super::HistKind::SevenDay, super::HistKind::Fable] {
            for (unit, off) in [("day", -6), ("week", -1), ("month", -1)] {
                let h = ledger.history(None, unit, off, kind).await.unwrap();
                let s = serde_json::to_string_pretty(&h).unwrap();
                std::fs::write(format!("{out}/history-{unit}-{}.json", kind.key()), &s).unwrap();
                if kind == super::HistKind::FiveHour {
                    std::fs::write(format!("{out}/history-{unit}.json"), &s).unwrap();
                }
            }
        }
        // v1.8.12（D90）：窗口詳情——三條額度各拿本週（沒有就上週）的最後一個窗口倒一份，
        // 檔名 history-window-{kind}.json（前端 fixture 模式讀 get_history_window-{kind}.json）。
        for kind in [super::HistKind::FiveHour, super::HistKind::SevenDay, super::HistKind::Fable] {
            for off in [0, -1] {
                let h = ledger.history(None, "week", off, kind).await.unwrap();
                let Some(w) = h["windows"].as_array().and_then(|a| a.last()).cloned() else { continue };
                let seat = h["selected"]["id"].as_str().unwrap_or_default().to_string();
                let d = ledger
                    .history_window(&seat, kind, w["start"].as_str().unwrap(), w["end"].as_str().unwrap())
                    .await
                    .unwrap();
                std::fs::write(format!("{out}/history-window-{}.json", kind.key()), serde_json::to_string_pretty(&d).unwrap()).unwrap();
                println!("history-window-{}: {} samples, {} anchors, usd {}", kind.key(), d["samples"].as_array().map_or(0, |a| a.len()), d["anchors"].as_array().map_or(0, |a| a.len()), d["usage"]["usd"]);
                break;
            }
        }
        let paths = ledger
            .export_evidence(None, 180, std::path::Path::new(&format!("{out}/exports")), "probe")
            .await
            .unwrap();
        println!("exported: {paths:?}");
    }
}

/// v1.8.9（D88）：切帳號拖尾守門的行為測試——用真的 Ledger（temp 目錄）跑整條路。
#[cfg(test)]
mod carryover_tests {
    use crate::collector::model::{SeatConfidence, Source, TruthSample};

    fn sample(acct: &str, kind: &str, scope: Option<&str>, pct: f32, at: &str) -> TruthSample {
        TruthSample {
            account_uuid: Some(acct.to_string()),
            org_uuid: Some(format!("org-{acct}")),
            seat_confidence: SeatConfidence::Exact,
            limit_kind: kind.to_string(),
            scope: scope.map(str::to_string),
            percent: pct,
            resets_at: None,
            source: Source::Probe,
            fetched_at: chrono::DateTime::parse_from_rfc3339(at).unwrap().with_timezone(&chrono::Utc),
            raw_json: serde_json::json!({}),
        }
    }
    fn triple(acct: &str, s: f32, w: f32, f: f32, at: &str) -> Vec<TruthSample> {
        vec![
            sample(acct, "session", None, s, at),
            sample(acct, "weekly_all", None, w, at),
            sample(acct, "weekly_scoped", Some("Fable"), f, at),
        ]
    }
    async fn fresh_ledger(tag: &str) -> crate::test_support::TempLedger {
        crate::test_support::TempLedger::new(&format!("carryover-{tag}"))
    }

    #[tokio::test]
    async fn identical_weekly_and_fable_after_a_switch_is_carryover_until_they_move() {
        let l = fresh_ledger("a").await;
        // 座位 A 在用，最後一筆 1／56／100（就是 09-22 的真數字）
        l.insert_truth_samples(&triple("A", 1.0, 56.0, 100.0, "2026-09-22T08:14:00Z")).await.unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        // 切到 B：CLI 回的還是 1／56／100 → 拖尾
        let v = l.carryover_check(&triple("B", 1.0, 56.0, 100.0, "2026-09-22T12:02:00Z")).await.unwrap();
        assert!(matches!(v, Some((ref p, w, Some(f))) if p == &l.get_channel_state("current_seat_id").await.unwrap().unwrap() && w == 56.0 && f == 100.0));
        // 之後 mark_current_seat 換成 B；5h 自己重置成 0 也還是拖尾（只看 7d／Fable）
        l.mark_current_seat(Some("B"), Some("org-B"), None, None).await.unwrap();
        let v = l.carryover_check(&triple("B", 0.0, 56.0, 100.0, "2026-09-23T07:24:00Z")).await.unwrap();
        assert!(v.is_some(), "5h 歸零但 7d／Fable 沒動仍是拖尾");
        assert!(!l.get_channel_state("prev_seat_id").await.unwrap().unwrap().is_empty());
        // 週重置後變 0／0／0 → 數字動了 → 解除
        let v = l.carryover_check(&triple("B", 0.0, 0.0, 0.0, "2026-09-23T10:00:00Z")).await.unwrap();
        assert!(v.is_none());
        assert!(l.get_channel_state("prev_seat_id").await.unwrap().unwrap_or_default().is_empty());
    }

    #[tokio::test]
    async fn different_numbers_right_after_a_switch_are_booked_normally() {
        let l = fresh_ledger("b").await;
        l.insert_truth_samples(&triple("A", 20.0, 5.0, 8.0, "2026-09-20T09:30:00Z")).await.unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        // 09-20 那次切換：切完立刻是不同數字
        let v = l.carryover_check(&triple("B", 11.0, 3.0, 4.0, "2026-09-20T09:43:00Z")).await.unwrap();
        assert!(v.is_none());
    }

    #[tokio::test]
    async fn zero_weekly_on_the_previous_seat_never_counts_as_carryover() {
        let l = fresh_ledger("c").await;
        l.insert_truth_samples(&triple("A", 0.0, 0.0, 0.0, "2026-09-25T03:10:00Z")).await.unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let v = l.carryover_check(&triple("B", 0.0, 0.0, 0.0, "2026-09-25T03:20:00Z")).await.unwrap();
        assert!(v.is_none(), "兩邊都 0 分不出誰是誰，不能當拖尾");
    }

    /// v1.8.13（D91 T-06）：守門只查座位、不建座位——判定拖尾的那一輪不能先生出一列座位。
    #[tokio::test]
    async fn carryover_check_never_creates_a_seat() {
        let l = fresh_ledger("d").await;
        l.insert_truth_samples(&triple("A", 1.0, 56.0, 100.0, "2026-09-22T08:14:00Z")).await.unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let seats = |l: &super::Ledger| {
            let l = l.clone();
            async move {
                let c = l.conn.lock().await;
                c.query_row("SELECT COUNT(*) FROM seats", [], |r| r.get::<_, i64>(0)).unwrap()
            }
        };
        assert_eq!(seats(&l).await, 1);
        // 拖尾（同分）與非拖尾（不同分）兩種都不能多出座位。
        let mut ghost = triple("A", 1.0, 56.0, 100.0, "2026-09-22T11:55:40Z");
        for s in &mut ghost {
            s.org_uuid = Some("org-B".into()); // 舊帳號 × 新組織
        }
        assert!(l.carryover_check(&ghost).await.unwrap().is_some(), "配對不同＝座位不同，同分照樣當拖尾");
        assert_eq!(seats(&l).await, 1, "判拖尾那一輪不能先建出「舊帳號 × 新組織」");
        let v = l.carryover_check(&triple("B", 1.0, 56.0, 100.0, "2026-09-22T12:02:00Z")).await.unwrap();
        assert!(v.is_some());
        let v = l.carryover_check(&triple("C", 9.0, 9.0, 9.0, "2026-09-22T12:03:00Z")).await.unwrap();
        assert!(v.is_none());
        assert_eq!(seats(&l).await, 1, "守門不建座位");
    }
}


/// v1.8.12（D90）：即時面吃同座位 Desktop 樣本的行為測試——用真的 Ledger（temp 目錄）。
#[cfg(test)]
mod live_sources_tests {
    use crate::collector::model::{SeatConfidence, Source, TruthSample};

    fn sample(kind: &str, pct: f32, at: &str, source: Source, reset: Option<&str>) -> TruthSample {
        TruthSample {
            account_uuid: Some("A".to_string()),
            org_uuid: Some("org-A".to_string()),
            seat_confidence: if matches!(source, Source::DesktopHistory) { SeatConfidence::Inferred } else { SeatConfidence::Exact },
            limit_kind: kind.to_string(),
            scope: None,
            percent: pct,
            resets_at: reset.map(|r| chrono::DateTime::parse_from_rfc3339(r).unwrap().with_timezone(&chrono::Utc)),
            source,
            fetched_at: chrono::DateTime::parse_from_rfc3339(at).unwrap().with_timezone(&chrono::Utc),
            raw_json: serde_json::json!({}),
        }
    }
    async fn fresh_ledger(tag: &str) -> crate::test_support::TempLedger {
        crate::test_support::TempLedger::new(&format!("live-{tag}"))
    }
    fn iso(t: chrono::DateTime<chrono::Utc>) -> String {
        t.to_rfc3339()
    }

    /// 主人 2026-09-28 的畫面：CLI 03:05 說 0%、Desktop 03:22 說 9%——懸浮窗 9%、今日頁 0%。
    #[tokio::test]
    async fn newer_desktop_sample_of_the_same_seat_drives_today_outlook() {
        let l = fresh_ledger("a").await;
        let now = chrono::Utc::now();
        let t0 = now - chrono::Duration::minutes(31);
        let t1 = now - chrono::Duration::minutes(14);
        let reset = now + chrono::Duration::hours(3);
        l.insert_truth_samples(&[
            sample("session", 0.0, &iso(t0), Source::CliCache, Some(&iso(reset))),
            sample("five_hour", 9.0, &iso(t1), Source::DesktopHistory, None),
        ])
        .await
        .unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let o = l.today_outlook(None).await.unwrap();
        assert_eq!(o["session"]["percent"], 9.0, "今日頁要跟懸浮窗一樣拿 Desktop 那筆");
        assert_eq!(o["session"]["fetchedAt"], iso(t1));
        assert_eq!(o["session"]["resetsAt"], iso(reset), "Desktop 不帶重置時刻，借 CLI 那筆的");
        assert_eq!(o["session"]["expired"], false);
        assert_eq!(o["windowSamples"].as_array().unwrap().len(), 2, "階梯圖也要有 Desktop 那筆");
        // 切換器的最後已知值同理
        let seat = l.get_channel_state("current_seat_id").await.unwrap().unwrap();
        let snap = l.seat_snapshot(&seat).await.unwrap();
        assert_eq!(snap["currentSession"]["usedPercent"], 9.0);
    }

    /// 今日頁的模型分佈：只算這個 5h 窗內、按 API 計價排（D97）、帶顯示名（跟專案表同一個範圍）。
    #[tokio::test]
    async fn today_outlook_lists_models_in_this_window() {
        let l = fresh_ledger("models").await;
        let now = chrono::Utc::now();
        let reset = now + chrono::Duration::hours(3); // 窗口起點＝2 小時前
        l.insert_truth_samples(&[sample("session", 10.0, &iso(now - chrono::Duration::minutes(5)), Source::CliCache, Some(&iso(reset)))])
            .await
            .unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        {
            let conn = l.conn.lock().await;
            for (id, mins_ago, model, out) in [
                ("m1", 60, "claude-sonnet-4-5", 100),
                ("m2", 30, "claude-opus-4-1", 900),
                ("m3", 20, "claude-sonnet-4-5", 50),
                ("m4", 180, "claude-opus-4-1", 5000), // 窗口之前，不算
            ] {
                conn.execute(
                    "INSERT INTO usage_events (msg_id, ts, model, entrypoint, input_tokens, output_tokens) \
                     VALUES (?1, ?2, ?3, 'cli', 0, ?4)",
                    rusqlite::params![id, iso(now - chrono::Duration::minutes(mins_ago)), model, out],
                )
                .unwrap();
            }
        }
        let o = l.today_outlook(None).await.unwrap();
        let models = o["topModels"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["model"], "claude-opus-4-1");
        assert_eq!(models[0]["outputTokens"], 900, "窗口之前那筆不算");
        assert_eq!(models[1]["calls"], 2);
        assert_eq!(models[1]["outputTokens"], 150);
        assert_eq!(models[1]["label"], crate::stats::model_label("claude-sonnet-4-5"));
    }

    /// D97：分佈按 API 計價排——長對話（讀快取多、輸出少）的專案要排在只是話多的專案前面；
    /// 同一個專案混用兩個模型時各照各的牌價。
    #[tokio::test]
    async fn usage_mix_ranks_by_api_price_not_output() {
        let l = fresh_ledger("d97").await;
        let now = chrono::Utc::now();
        let reset = now + chrono::Duration::hours(3);
        l.insert_truth_samples(&[sample("session", 10.0, &iso(now - chrono::Duration::minutes(5)), Source::CliCache, Some(&iso(reset)))])
            .await
            .unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        {
            let conn = l.conn.lock().await;
            for (id, project, model, cache_read, out) in [
                // sonnet：讀快取 0.30／M、輸出 15／M → 3.00 + 0.03
                ("m1", "C--long", "claude-sonnet-4-5", 10_000_000i64, 2_000i64),
                // sonnet 輸出 1.50；opus 4.1 輸出 75／M（官方 $15／$75，v1.9.4 精確 id 表；家族後備是 25）→ 1.50
                ("m2", "C--chatty", "claude-sonnet-4-5", 0, 100_000),
                ("m3", "C--chatty", "claude-opus-4-1", 0, 20_000),
            ] {
                conn.execute(
                    "INSERT INTO usage_events (msg_id, ts, model, entrypoint, project_dir, input_tokens, \
                                               cache_read, output_tokens) \
                     VALUES (?1, ?2, ?3, 'cli', ?4, 0, ?5, ?6)",
                    rusqlite::params![id, iso(now - chrono::Duration::minutes(30)), model, project, cache_read, out],
                )
                .unwrap();
            }
        }
        let o = l.today_outlook(None).await.unwrap();
        let projects = o["topProjects"].as_array().unwrap();
        assert_eq!(projects[0]["project"], "C--long", "輸出少但讀快取多，花得比較多");
        assert_eq!(projects[0]["usd"], 3.03);
        assert_eq!(projects[0]["cacheReadTokens"], 10_000_000);
        assert_eq!(projects[1]["project"], "C--chatty");
        assert_eq!(projects[1]["usd"], 3.0, "兩個模型各照各的牌價");
        assert_eq!(projects[1]["calls"], 2);
        assert_eq!(projects[1]["outputTokens"], 120_000);

        let models = o["topModels"].as_array().unwrap();
        assert_eq!(models[0]["model"], "claude-sonnet-4-5");
        assert_eq!(models[0]["usd"], 4.53);

        let eps = o["byEntrypoint"].as_array().unwrap();
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0]["entrypoint"], "cli");
        assert_eq!(eps[0]["usd"], 6.03);
    }

    /// D93：7d 的近期速度看一整天——昨晚睡覺沒燒的那段要算進去，不能只看白天的衝刺。
    #[tokio::test]
    async fn weekly_short_window_spans_a_whole_day() {
        let l = fresh_ledger("d93").await;
        let now = chrono::Utc::now();
        let at = |h: i64| iso(now - chrono::Duration::hours(h));
        let reset = iso(now + chrono::Duration::hours(72));
        l.insert_truth_samples(&[
            sample("weekly_all", 20.0, &at(23), Source::CliCache, Some(&reset)), // 昨晚
            sample("weekly_all", 22.0, &at(10), Source::CliCache, Some(&reset)), // 睡醒
            sample("weekly_all", 40.0, &at(1), Source::CliCache, Some(&reset)),  // 白天做完
        ])
        .await
        .unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let o = l.today_outlook(None).await.unwrap();
        assert_eq!(o["weekly"]["shortWindowMinutes"], 1440.0);
        // 14 小時窗只看得到睡醒後：(40−22)/9 = 2.0 %/h；一整天：(40−20)/22 ≈ 0.9 %/h
        assert_eq!(o["weekly"]["burnPerHour"], 0.9);
        assert_eq!(o["session"]["shortWindowMinutes"], 60.0, "5h 不受影響");
    }

    /// 兩個來源同刻差 3 點不是重置：12（CLI）→ 9（Desktop）→ 12（CLI），本窗仍是三筆。
    #[tokio::test]
    async fn source_disagreement_does_not_cut_the_window() {
        let l = fresh_ledger("b").await;
        let now = chrono::Utc::now();
        let reset = now + chrono::Duration::hours(2);
        let mk = |m: i64| iso(now - chrono::Duration::minutes(m));
        l.insert_truth_samples(&[
            sample("session", 12.0, &mk(40), Source::CliCache, Some(&iso(reset))),
            sample("five_hour", 9.0, &mk(39), Source::DesktopHistory, None),
            sample("session", 12.0, &mk(30), Source::CliCache, Some(&iso(reset))),
            sample("session", 14.0, &mk(10), Source::CliCache, Some(&iso(reset))),
        ])
        .await
        .unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let o = l.burn_stats(None).await.unwrap();
        assert_eq!(o["samples"], 4, "孤立的低值不切窗");
        assert_eq!(o["percent"], 14.0);
        // 真的重置（40 → 3，之後半小時都低）還是要切
        l.insert_truth_samples(&[
            sample("session", 40.0, &mk(8), Source::CliCache, Some(&iso(reset))),
            sample("five_hour", 3.0, &mk(5), Source::DesktopHistory, None),
            sample("session", 4.0, &mk(1), Source::CliCache, Some(&iso(reset + chrono::Duration::hours(5)))),
        ])
        .await
        .unwrap();
        let o = l.burn_stats(None).await.unwrap();
        assert_eq!(o["samples"], 2, "確認過的下跌才是新窗");
        assert_eq!(o["percent"], 4.0);
    }

    /// 借來的重置時刻若比最後一筆樣本早（跨窗沒切到），當未知——不能把有新樣本的窗判 expired。
    #[tokio::test]
    async fn stale_borrowed_reset_is_ignored() {
        let l = fresh_ledger("c").await;
        let now = chrono::Utc::now();
        let mk = |m: i64| iso(now - chrono::Duration::minutes(m));
        let old_reset = now - chrono::Duration::minutes(20);
        l.insert_truth_samples(&[
            sample("session", 1.0, &mk(40), Source::CliCache, Some(&iso(old_reset))),
            sample("five_hour", 1.0, &mk(10), Source::DesktopHistory, None),
        ])
        .await
        .unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let o = l.burn_stats(None).await.unwrap();
        assert_eq!(o["expired"], false);
        assert!(o["resetsAt"].is_null());
        assert_eq!(o["percent"], 1.0);
    }

    /// v1.8.13（D91 T-04）：窗尾全是 Desktop、借不到重置時刻——用窗長推：這一窗第一筆有用量的
    /// 樣本在 5.5 小時前，重置最晚 30 分鐘前就發生了。以前 expired 永遠 false、今日頁掛著 25%。
    #[tokio::test]
    async fn desktop_only_tail_expires_by_window_length() {
        let l = fresh_ledger("t04a").await;
        let now = chrono::Utc::now();
        let mk = |m: i64| iso(now - chrono::Duration::minutes(m));
        l.insert_truth_samples(&[
            sample("five_hour", 10.0, &mk(330), Source::DesktopHistory, None),
            sample("five_hour", 20.0, &mk(270), Source::DesktopHistory, None),
            sample("five_hour", 25.0, &mk(180), Source::DesktopHistory, None),
        ])
        .await
        .unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let o = l.today_outlook(None).await.unwrap();
        assert_eq!(o["session"]["expired"], true);
        assert_eq!(o["session"]["lastKnownPercent"], 25.0);
        assert!(o["session"]["resetsAt"].is_null(), "推出來的只是上限，不冒充真的重置時刻");
        let latest = chrono::DateTime::parse_from_rfc3339(o["session"]["resetsAtLatest"].as_str().unwrap()).unwrap().with_timezone(&chrono::Utc);
        assert!((latest - (now - chrono::Duration::minutes(30))).num_seconds().abs() <= 1);
    }

    /// 反例：中間有 0（Desktop 開著跨過重置）＝換窗，新窗從 1 小時前才開始用，還沒到期。
    #[tokio::test]
    async fn a_zero_reading_starts_a_new_window_for_the_estimate() {
        let l = fresh_ledger("t04b").await;
        let now = chrono::Utc::now();
        let mk = |m: i64| iso(now - chrono::Duration::minutes(m));
        l.insert_truth_samples(&[
            sample("five_hour", 1.0, &mk(330), Source::DesktopHistory, None),
            sample("five_hour", 0.0, &mk(240), Source::DesktopHistory, None),
            sample("five_hour", 5.0, &mk(60), Source::DesktopHistory, None),
            sample("five_hour", 8.0, &mk(30), Source::DesktopHistory, None),
        ])
        .await
        .unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let o = l.burn_stats(None).await.unwrap();
        assert_eq!(o["expired"], false);
        assert_eq!(o["percent"], 8.0);
        let latest = chrono::DateTime::parse_from_rfc3339(o["resetsAtLatest"].as_str().unwrap()).unwrap().with_timezone(&chrono::Utc);
        assert!((latest - (now + chrono::Duration::hours(4))).num_seconds().abs() <= 1);
    }

    /// L9：Desktop 在重置後 0.6 秒寫的舊窗 100%——借 CLI 的重置時刻要給容差，判 expired。
    #[tokio::test]
    async fn desktop_write_just_after_reset_counts_as_the_old_window() {
        let l = fresh_ledger("t04c").await;
        let now = chrono::Utc::now();
        let reset = now - chrono::Duration::minutes(30);
        l.insert_truth_samples(&[
            sample("session", 90.0, &iso(reset - chrono::Duration::minutes(10)), Source::CliCache, Some(&iso(reset))),
            sample("five_hour", 100.0, &iso(reset + chrono::Duration::milliseconds(598)), Source::DesktopHistory, None),
        ])
        .await
        .unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let o = l.today_outlook(None).await.unwrap();
        assert_eq!(o["session"]["expired"], true, "那筆 100% 屬於剛結束的窗");
        assert_eq!(o["session"]["lastKnownPercent"], 100.0);
        assert_eq!(o["session"]["resetsAt"], iso(reset));
    }

    /// 窗口詳情：樣本、429、本機事件範圍。
    #[tokio::test]
    async fn history_window_lists_samples_and_anchors() {
        let l = fresh_ledger("d").await;
        let now = chrono::Utc::now();
        let mk = |m: i64| iso(now - chrono::Duration::minutes(m));
        let reset = now + chrono::Duration::hours(1);
        l.insert_truth_samples(&[
            sample("session", 5.0, &mk(200), Source::Probe, None),
            sample("five_hour", 20.0, &mk(150), Source::DesktopHistory, None),
            sample("session", 60.0, &mk(100), Source::CliCache, Some(&iso(reset))),
        ])
        .await
        .unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let seat = l.get_channel_state("current_seat_id").await.unwrap().unwrap();
        let d = l.history_window(&seat, super::HistKind::FiveHour, &mk(200), &mk(100)).await.unwrap();
        assert_eq!(d["samples"].as_array().unwrap().len(), 3);
        assert_eq!(d["peak"], 60.0);
        assert_eq!(d["reset"], iso(reset));
        assert_eq!(d["sourceCounts"]["desktop-history"], 1);
        assert_eq!(d["durationMinutes"], 100);
        // 事件範圍：有重置時刻 → [reset−5h, reset)
        assert_eq!(d["usage"]["from"], iso(reset - chrono::Duration::hours(5)));
        assert_eq!(d["usage"]["to"], iso(reset));
        assert!(d["avgBurnPerHour"].as_f64().unwrap() > 30.0, "55 點 ÷ 100 分 ≈ 33 %/h");
        assert!(d["usage"]["byModel"].as_array().unwrap().is_empty());
    }
}

/// v1.8.13（D91 T-02）：帳本壞掉時開檔要回錯、絕不 DROP——用真的 Ledger（temp 目錄）弄壞 meta 那一頁。
#[cfg(test)]
mod open_tests {
    use crate::collector::model::{SeatConfidence, Source, TruthSample};

    fn temp_dir(tag: &str) -> crate::test_support::TempDir {
        crate::test_support::TempDir::new(&format!("open-{tag}"))
    }
    fn sample() -> TruthSample {
        TruthSample {
            account_uuid: Some("A".to_string()),
            org_uuid: Some("org-A".to_string()),
            seat_confidence: SeatConfidence::Exact,
            limit_kind: "session".to_string(),
            scope: None,
            percent: 42.0,
            resets_at: None,
            source: Source::CliCache,
            fetched_at: chrono::Utc::now(),
            raw_json: serde_json::json!({}),
        }
    }
    /// 建一本有一筆樣本的帳本，關掉（最後一條連線關閉時 WAL 會併回主檔）。
    async fn ledger_with_one_sample(dir: &std::path::Path) {
        let l = super::Ledger::open(dir.to_path_buf()).unwrap();
        assert_eq!(l.insert_truth_samples(&[sample()]).await.unwrap(), 1);
    }
    fn tables(path: &std::path::Path) -> Vec<String> {
        let c = rusqlite::Connection::open(path).unwrap();
        let mut stmt = c.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name").unwrap();
        let v = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()).collect();
        v
    }
    const ALL_TABLES: [&str; 8] =
        ["anchors", "channel_state", "gaps", "ingest_files", "meta", "seats", "truth_samples", "usage_events"];

    /// 01-異常態 A2 的重演：只壞 meta 的 root page。以前會 DROP 其他七張表再閃退。
    #[tokio::test]
    async fn corrupted_meta_page_is_an_error_and_drops_nothing() {
        let dir = temp_dir("meta");
        ledger_with_one_sample(&dir).await;
        let path = dir.join("ledger.sqlite");
        assert!(!dir.join("ledger.sqlite-wal").exists() || std::fs::metadata(dir.join("ledger.sqlite-wal")).unwrap().len() == 0);
        let (root, page_size): (u64, u64) = {
            let c = rusqlite::Connection::open(&path).unwrap();
            let root: i64 = c.query_row("SELECT rootpage FROM sqlite_master WHERE name = 'meta'", [], |r| r.get(0)).unwrap();
            let ps: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap();
            (root as u64, ps as u64)
        };
        assert!(root > 1, "meta 不會在第 1 頁（那是 sqlite_master）");
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.seek(SeekFrom::Start((root - 1) * page_size)).unwrap();
            f.write_all(&vec![0xFFu8; page_size as usize]).unwrap();
        }
        let err = super::Ledger::open(dir.clone()).err().expect("meta 壞掉要回錯，不能當新檔");
        assert!(format!("{err:#}").contains("meta"), "錯誤要講是哪裡壞：{err:#}");
        // 七張表＋meta 都還在，樣本也還讀得到。
        let t = tables(&path);
        for name in ALL_TABLES {
            assert!(t.iter().any(|x| x == name), "{name} 被刪了：{t:?}");
        }
        let c = rusqlite::Connection::open(&path).unwrap();
        let n: i64 = c.query_row("SELECT COUNT(*) FROM truth_samples", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    /// 版本值讀得出來但看不懂——也是回錯、不動表。
    #[tokio::test]
    async fn unreadable_version_value_is_an_error_and_drops_nothing() {
        let dir = temp_dir("ver");
        ledger_with_one_sample(&dir).await;
        let path = dir.join("ledger.sqlite");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute("UPDATE meta SET value = 'x' WHERE key = 'schema_version'", []).unwrap();
        }
        assert!(super::Ledger::open(dir.clone()).is_err());
        let c = rusqlite::Connection::open(&path).unwrap();
        let n: i64 = c.query_row("SELECT COUNT(*) FROM truth_samples", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    fn count(path: &std::path::Path, table: &str) -> i64 {
        let c = rusqlite::Connection::open(path).unwrap();
        c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    /// 審查 R2 的重演：meta 的表頁和它的索引頁一起變成「合法的空葉頁」——查版本不報錯、回空，
    /// 其他表的資料都還在。以前這支回 0（當成建表中途被打斷）→ DROP 八張表。
    #[tokio::test]
    async fn meta_pages_blanked_is_an_error_and_drops_nothing() {
        let dir = temp_dir("blank");
        ledger_with_one_sample(&dir).await;
        let path = dir.join("ledger.sqlite");
        let (roots, page_size): (Vec<(u64, u8)>, u64) = {
            let c = rusqlite::Connection::open(&path).unwrap();
            let root = |name: &str| -> u64 {
                c.query_row("SELECT rootpage FROM sqlite_master WHERE name = ?1", [name], |r| r.get::<_, i64>(0))
                    .unwrap() as u64
            };
            let ps: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap();
            // 0x0D＝table b-tree 葉頁、0x0A＝index b-tree 葉頁。
            (vec![(root("meta"), 0x0D), (root("sqlite_autoindex_meta_1"), 0x0A)], ps as u64)
        };
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            for (root, kind) in roots {
                assert!(root > 1);
                let mut page = vec![0u8; page_size as usize];
                page[0] = kind;
                // 儲存格內容起點＝頁尾（沒有任何儲存格）；65536 在格式裡寫成 0。
                let start = if page_size >= 65536 { 0 } else { page_size as u16 };
                page[5..7].copy_from_slice(&start.to_be_bytes());
                f.seek(SeekFrom::Start((root - 1) * page_size)).unwrap();
                f.write_all(&page).unwrap();
            }
        }
        {
            // 前提：真的變成「meta 在、版本列不在、不報錯」這個形狀。
            let c = rusqlite::Connection::open(&path).unwrap();
            let v: Vec<String> = c
                .prepare("SELECT value FROM meta WHERE key = 'schema_version'")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert!(v.is_empty(), "重演沒成功：{v:?}");
        }
        let err = super::Ledger::open(dir.clone()).err().expect("版本列不見要回錯，不能當新檔");
        assert!(format!("{err:#}").contains("版本"), "{err:#}");
        let t = tables(&path);
        for name in ALL_TABLES {
            assert!(t.iter().any(|x| x == name), "{name} 被刪了：{t:?}");
        }
        assert_eq!(count(&path, "truth_samples"), 1);
    }

    /// 審查 R2 次要：沒有 meta、truth_samples 剛好是空的，但座位還有資料——以前只看 truth_samples，
    /// 會把 seats 等表一起 DROP。
    #[tokio::test]
    async fn no_meta_but_other_tables_have_rows_is_an_error() {
        let dir = temp_dir("nometa");
        ledger_with_one_sample(&dir).await;
        let path = dir.join("ledger.sqlite");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch("DROP TABLE meta; DELETE FROM truth_samples;").unwrap();
        }
        let seats = count(&path, "seats");
        assert!(seats > 0, "前提：寫樣本時建了座位");
        let err = super::Ledger::open(dir.clone()).err().expect("有資料就不能當新檔");
        assert!(format!("{err:#}").contains("seats"), "{err:#}");
        assert_eq!(count(&path, "seats"), seats);
    }

    /// 沒有 meta、我們的表都在但全空＝沒東西可丟，照新檔重建（不把空殘檔卡成打不開）。
    #[tokio::test]
    async fn no_meta_and_all_tables_empty_rebuilds() {
        let dir = temp_dir("emptyrest");
        ledger_with_one_sample(&dir).await;
        let path = dir.join("ledger.sqlite");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            let mut sql = "DROP TABLE meta;".to_string();
            // 先刪子表再刪 seats（外鍵）。
            for t in ["truth_samples", "usage_events", "anchors", "gaps", "ingest_files", "channel_state", "seats"] {
                sql.push_str(&format!("DELETE FROM {t};"));
            }
            c.execute_batch(&sql).unwrap();
        }
        drop(super::Ledger::open(dir.clone()).expect("全空要能重建"));
        let t = tables(&path);
        for name in ALL_TABLES {
            assert!(t.iter().any(|x| x == name), "{name} 沒建：{t:?}");
        }
    }

    /// DATA_TABLES 要跟 SCHEMA_V2 建出來的表對得上——加表忘了加，沒 meta 時的保護就會漏一張。
    #[tokio::test]
    async fn data_tables_match_schema() {
        let dir = temp_dir("tables");
        ledger_with_one_sample(&dir).await;
        let mut got: Vec<String> = tables(&dir.join("ledger.sqlite")).into_iter().filter(|t| t != "meta" && !t.starts_with("sqlite_")).collect();
        let mut want: Vec<String> = super::DATA_TABLES.iter().map(|s| s.to_string()).collect();
        got.sort();
        want.sort();
        assert_eq!(got, want);
    }

    /// 回歸：全新檔照常建表；再開一次資料還在（沒被當新檔重建）。
    #[tokio::test]
    async fn fresh_file_builds_schema_and_reopen_keeps_data() {
        let dir = temp_dir("fresh");
        ledger_with_one_sample(&dir).await;
        let t = tables(&dir.join("ledger.sqlite"));
        for name in ALL_TABLES {
            assert!(t.iter().any(|x| x == name), "{name} 沒建：{t:?}");
        }
        let l = super::Ledger::open(dir.clone()).unwrap();
        let conn = l.conn.lock().await;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM truth_samples", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "重開不能把資料當新檔清掉");
    }
}

/// v1.8.13（D91 T-05）：備份保留「每個本地日留最新一份、留 7 個日子」＋殘骸清理＋先 .tmp 再改名。
#[cfg(test)]
mod backup_tests {
    use chrono::TimeZone;

    fn temp_dir(tag: &str) -> crate::test_support::TempDir {
        crate::test_support::TempDir::new(&format!("backup-{tag}"))
    }
    /// 本地時刻 → 備份檔名（檔名是 UTC）。用本地時刻造檔，測試不管在哪個時區跑都成立。
    fn name_at_local(d: chrono::NaiveDate, h: u32, m: u32) -> String {
        let local = chrono::Local
            .from_local_datetime(&d.and_hms_opt(h, m, 0).unwrap())
            .earliest()
            .unwrap();
        format!("ledger-{}.sqlite", local.with_timezone(&chrono::Utc).format("%Y%m%d-%H%M%S"))
    }
    fn touch(dir: &std::path::Path, name: &str, age: std::time::Duration) {
        let p = dir.join(name);
        std::fs::write(&p, b"x").unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(std::time::SystemTime::now() - age).unwrap();
    }
    fn listing(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> =
            std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn keeps_the_newest_backup_of_each_local_day_for_seven_days() {
        let dir = temp_dir("prune");
        let today = chrono::Local::now().date_naive();
        let mut expected_kept = Vec::new();
        // 十個日子，每天三份：凌晨 00:30（台北＝前一個 UTC 日）、中午、晚上 23:10。
        for back in 0..10 {
            let d = today - chrono::Duration::days(back);
            for (h, m) in [(0, 30), (12, 0), (23, 10)] {
                touch(&dir, &name_at_local(d, h, m), std::time::Duration::ZERO);
            }
            if back < 7 {
                expected_kept.push(name_at_local(d, 23, 10));
            }
        }
        // 殘骸：兩小時前被打斷的 journal 與 tmp 要清；剛寫的 tmp 不能碰（可能正在寫）。
        let hour2 = std::time::Duration::from_secs(7200);
        touch(&dir, "ledger-20260921-102700.sqlite-journal", hour2);
        touch(&dir, "ledger-20260921-102700.sqlite.tmp", hour2);
        touch(&dir, "ledger-20260928-120000.sqlite.tmp", std::time::Duration::ZERO);
        touch(&dir, "notes.txt", hour2); // 不是我們的檔，不碰

        let removed = super::prune_backups(&dir, 7, std::time::SystemTime::now()).unwrap();
        assert_eq!(removed.len(), 30 - 7 + 2);
        let mut want = expected_kept.clone();
        want.push("ledger-20260928-120000.sqlite.tmp".to_string());
        want.push("notes.txt".to_string());
        want.sort();
        assert_eq!(listing(&dir), want);
    }

    /// App 關了很久再開：不是「今天往前 7 天」，所以舊備份不會一口氣全被清掉。
    #[test]
    fn a_long_pause_does_not_wipe_old_backups() {
        let dir = temp_dir("pause");
        let today = chrono::Local::now().date_naive();
        for back in [0, 30, 31, 32] {
            touch(&dir, &name_at_local(today - chrono::Duration::days(back), 9, 0), std::time::Duration::ZERO);
        }
        let removed = super::prune_backups(&dir, 7, std::time::SystemTime::now()).unwrap();
        assert!(removed.is_empty());
        assert_eq!(listing(&dir).len(), 4);
    }

    /// 真的跑一次 VACUUM INTO：留下正式檔名、沒有 .tmp，而且是開得起來的帳本。
    #[tokio::test]
    async fn backup_writes_via_tmp_and_leaves_a_real_ledger() {
        let data = temp_dir("run-data");
        let l = super::Ledger::open(data.clone()).unwrap();
        let backups = data.join("backups");
        let dest = l.backup_into(&backups, 7).await.unwrap();
        let names = listing(&backups);
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].ends_with(".sqlite") && !names[0].contains(".tmp"));
        assert_eq!(dest.file_name().unwrap().to_string_lossy(), names[0]);
        let c = rusqlite::Connection::open(&dest).unwrap();
        let v: String = c.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(v, super::SCHEMA_VERSION.to_string());
    }
}

/// v1.8.13（D91 T-13）：趨勢頁日條、見頂日、方案建議按本地日切——跟歷史頁熱圖（local_day_of）同口徑。
#[cfg(test)]
mod local_day_tests {
    use chrono::TimeZone;
    use crate::collector::model::{SeatConfidence, Source, TruthSample};

    async fn fresh_ledger(tag: &str) -> crate::test_support::TempLedger {
        crate::test_support::TempLedger::new(&format!("localday-{tag}"))
    }
    /// 某個本地日的本地時刻 → UTC（測試在哪個時區跑都成立；在台北＝UTC 前一天 16:00～24:00）。
    fn local(d: chrono::NaiveDate, h: u32, m: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::Local
            .from_local_datetime(&d.and_hms_opt(h, m, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    /// SQLite 的 date(x,'localtime') 要跟 Rust 的 local_day_of 一模一樣——帳本裡四種時間字串都試。
    #[tokio::test]
    async fn sqlite_localtime_matches_local_day_of_for_every_stored_format() {
        let l = fresh_ledger("fmt").await;
        let conn = l.conn.lock().await;
        let start = chrono::Utc.with_ymd_and_hms(2026, 9, 19, 0, 0, 0).unwrap();
        for i in 0..(48 * 2) {
            let t = start + chrono::Duration::minutes(30 * i) + chrono::Duration::milliseconds(123);
            let forms = [
                t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),  // usage_events：…123Z（24 字）
                t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),   // …+00:00（25 字）
                t.to_rfc3339_opts(chrono::SecondsFormat::Millis, false), // 29 字
                t.to_rfc3339_opts(chrono::SecondsFormat::Micros, false), // 32 字
                t.to_rfc3339_opts(chrono::SecondsFormat::Nanos, false),  // 35 字
            ];
            for f in forms {
                let sql: String = conn.query_row("SELECT date(?1,'localtime')", [&f], |r| r.get(0)).unwrap();
                assert_eq!(Some(sql.clone()), super::local_day_of(&f).map(|d| d.to_string()), "{f}");
            }
        }
    }

    /// 台北 00:00～08:00 的用量要算當天，不是前一天。
    #[tokio::test]
    async fn early_morning_usage_and_walls_land_on_the_local_day() {
        let l = fresh_ledger("dash").await;
        let today = chrono::Local::now().date_naive();
        let d = today - chrono::Duration::days(3);
        let early = local(d, 3, 0); // 台北 03:00＝UTC 前一天 19:00
        l.insert_truth_samples(&[TruthSample {
            account_uuid: Some("A".into()),
            org_uuid: Some("org-A".into()),
            seat_confidence: SeatConfidence::Exact,
            limit_kind: "session".into(),
            scope: None,
            percent: 100.0,
            resets_at: None,
            source: Source::CliCache,
            fetched_at: early,
            raw_json: serde_json::json!({}),
        }])
        .await
        .unwrap();
        {
            let conn = l.conn.lock().await;
            conn.execute(
                "INSERT INTO usage_events (msg_id, ts, model, entrypoint, input_tokens, output_tokens)                  VALUES ('m1', ?1, 'claude-sonnet-4-5', 'cli', 1000000, 0)",
                [early.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)],
            )
            .unwrap();
        }
        let dash = l.dashboard(Some(super::ALL_SEATS), 30).await.unwrap();
        let daily = dash["daily"].as_array().unwrap();
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0]["day"], d.to_string(), "用量歸本地日");
        assert_eq!(daily[0]["usd"], 3.0);
        assert_eq!(dash["saturatedDays"], serde_json::json!([d.to_string()]), "見頂日歸本地日");
        // 方案建議的每日峰值也是本地日
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let o = l.today_outlook(None).await.unwrap();
        assert_eq!(o["plan"]["dailyPeaks"], serde_json::json!([{ "day": d.to_string(), "peak": 100.0 }]));
    }
}

/// v1.8.13（D91 T-12）：方案建議——門檻看 60 天，見頂天數／P90／週峰值只看近 14 個本地日。
#[cfg(test)]
mod plan_window_tests {
    use chrono::TimeZone;
    use crate::collector::model::{SeatConfidence, Source, TruthSample};

    async fn fresh_ledger(tag: &str) -> crate::test_support::TempLedger {
        crate::test_support::TempLedger::new(&format!("plan-{tag}"))
    }
    fn at_local_noon(days_ago: i64) -> chrono::DateTime<chrono::Utc> {
        let d = chrono::Local::now().date_naive() - chrono::Duration::days(days_ago);
        chrono::Local
            .from_local_datetime(&d.and_hms_opt(12, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&chrono::Utc)
    }
    fn day(days_ago: i64, session: f32, weekly: f32) -> Vec<TruthSample> {
        let mk = |kind: &str, pct: f32| TruthSample {
            account_uuid: Some("A".into()),
            org_uuid: Some("org-A".into()),
            seat_confidence: SeatConfidence::Exact,
            limit_kind: kind.into(),
            scope: None,
            percent: pct,
            resets_at: None,
            source: Source::CliCache,
            fetched_at: at_local_noon(days_ago),
            raw_json: serde_json::json!({}),
        };
        vec![mk("session", session), mk("weekly_all", weekly)]
    }

    /// fcf0 的形狀：一個月前連續見頂、近兩週很平靜——以前 60 天一起算判「考慮升級」。
    #[tokio::test]
    async fn old_walls_outside_fourteen_days_do_not_drive_the_suggestion() {
        let l = fresh_ledger("old").await;
        let mut all = Vec::new();
        for back in 20..25 {
            all.extend(day(back, 100.0, 95.0));
        }
        for back in 0..8 {
            all.extend(day(back, 50.0, 50.0));
        }
        l.insert_truth_samples(&all).await.unwrap();
        let seat = l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        // v1.9.4：方案建議認方案——這個座位的方案很早以前就記下來了（統計窗不被方案起點縮短）
        l.conn
            .lock()
            .await
            .execute(
                "INSERT INTO seat_epochs (seat_id, tier, org_type, since, observed_at) \
                 VALUES (?1, 'default_claude_max_20x', 'claude_max', ?2, ?2)",
                rusqlite::params![seat, (chrono::Utc::now() - chrono::Duration::days(90)).to_rfc3339()],
            )
            .unwrap();
        let plan = &l.today_outlook(None).await.unwrap()["plan"];
        assert_eq!(plan["coverageDays"], 13, "門檻仍看 60 天");
        assert_eq!(plan["statsWindowDays"], 14);
        assert_eq!(plan["statsDays"], 8);
        assert_eq!(plan["sessionSatDays"], 0, "一個月前的見頂不算");
        assert_eq!(plan["weeklyPeak"], 50.0);
        assert_eq!(plan["dailyPeaks"].as_array().unwrap().len(), 8, "長條只畫近 14 天");
        assert_eq!(plan["suggestion"], "keep");
    }

    /// 門檻夠、但近 14 天一筆都沒有：不能把 0 當「用得很少」建議降級。
    #[tokio::test]
    async fn nothing_in_fourteen_days_is_calibrating_not_downgrade() {
        let l = fresh_ledger("empty").await;
        let mut all = Vec::new();
        for back in 20..28 {
            all.extend(day(back, 10.0, 10.0));
        }
        l.insert_truth_samples(&all).await.unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let plan = &l.today_outlook(None).await.unwrap()["plan"];
        assert_eq!(plan["coverageDays"], 8);
        assert_eq!(plan["statsDays"], 0);
        assert_eq!(plan["suggestion"], "calibrating");
    }
}

/// v1.8.13（D91 T-15）：快取只預熱最近有人讀過的配方；背景重算不算「讀」。
#[cfg(test)]
mod cache_tests {
    use crate::collector::model::{SeatConfidence, Source, TruthSample};

    async fn ledger_with_a_seat() -> (crate::test_support::TempLedger, String) {
        let l = crate::test_support::TempLedger::new("cache");
        l.insert_truth_samples(&[TruthSample {
            account_uuid: Some("A".into()),
            org_uuid: Some("org-A".into()),
            seat_confidence: SeatConfidence::Exact,
            limit_kind: "session".into(),
            scope: None,
            percent: 10.0,
            resets_at: None,
            source: Source::CliCache,
            fetched_at: chrono::Utc::now(),
            raw_json: serde_json::json!({}),
        }])
        .await
        .unwrap();
        let seat = l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        (l, seat)
    }
    fn keys(v: &[super::Recipe]) -> Vec<String> {
        let mut k: Vec<String> = v.iter().map(|r| r.key()).collect();
        k.sort();
        k
    }
    /// Windows 的 Instant 從開機起算，開機不到 `by` 時退不回去——回 false 讓測試跳過，
    /// 不在剛開機的機器上假失敗（審查 22r R4）。
    fn age(l: &super::Ledger, key: &str, by: std::time::Duration) -> bool {
        let Some(t) = std::time::Instant::now().checked_sub(by) else { return false };
        let mut c = l.cache.lock().unwrap();
        c.get_mut(key).unwrap().last_read_at = t;
        true
    }

    #[tokio::test]
    async fn only_recently_read_recipes_are_warmed() {
        let (l, seat) = ledger_with_a_seat().await;
        l.dashboard(Some(&seat), 180).await.unwrap();
        l.dashboard(None, 180).await.unwrap(); // 趨勢頁掛載時 seat=null 多打的那一份
        let seat_key = format!("dash|{seat}|180");
        let null_key = "dash||180".to_string();
        let (empty, r) = l.recipes_to_warm();
        assert!(!empty);
        assert_eq!(keys(&r), { let mut k = vec![seat_key.clone(), null_key.clone()]; k.sort(); k });

        // 31 分鐘沒人讀 → 不再每輪重算
        if !age(&l, &null_key, std::time::Duration::from_secs(31 * 60)) {
            return;
        }
        assert_eq!(keys(&l.recipes_to_warm().1), vec![seat_key.clone()]);

        // 背景重算寫回去不算「讀」——不能自己把自己續命
        let recipe = super::Recipe::Dashboard { seat: None, days: 180 };
        l.cache_store(recipe, "fp".into(), serde_json::json!({}), false);
        assert_eq!(keys(&l.recipes_to_warm().1), vec![seat_key.clone()]);

        // 真的有頁面再讀一次 → 回到預熱名單
        l.dashboard(None, 180).await.unwrap();
        assert_eq!(keys(&l.recipes_to_warm().1).len(), 2);
    }
}

/// v1.9.4（D99 B1／B2）：價目表精確比對、無牌價、長 prompt 檔、fast mode。
#[cfg(test)]
mod price_tests {
    use super::*;

    fn p(model: &str, speed: &str, input: i64, cr: i64, out: i64, long: bool) -> Option<f64> {
        price_usd(model, speed, input, 0, 0, cr, out, 0, long)
    }
    const M: i64 = 1_000_000;

    #[test]
    fn new_versions_use_their_own_official_prices_not_the_family_fallback() {
        // 快取讀 1M tokens：Opus 5.5 $0.20（家族後備會給 $0.50）、Opus 5 $0.50
        assert_eq!(p("claude-opus-5-5", "", 0, M, 0, false), Some(0.20));
        assert_eq!(p("claude-opus-5", "", 0, M, 0, false), Some(0.50));
        // Fable 5.1 快取讀 $0.25（Fable 5 是 $1）
        assert_eq!(p("claude-fable-5-1", "", 0, M, 0, false), Some(0.25));
        assert_eq!(p("claude-fable-5", "", 0, M, 0, false), Some(1.0));
        // Sonnet 5.5：$2 in／$10 out／快取讀 $0.10（Sonnet 5 是 $0.20）
        assert_eq!(p("claude-sonnet-5-5", "", M, 0, 0, false), Some(2.0));
        assert_eq!(p("claude-sonnet-5-5", "", 0, M, 0, false), Some(0.10));
        assert_eq!(p("claude-sonnet-5", "", 0, M, 0, false), Some(0.20));
        // Opus 5.5 輸出 $20（舊表 $25）
        assert_eq!(p("claude-opus-5-5", "", 0, 0, M, false), Some(20.0));
        // Opus 4.1 是 $15／$75
        assert_eq!(p("claude-opus-4-1", "", M, 0, 0, false), Some(15.0));
        assert_eq!(p("claude-opus-4-1", "", 0, 0, M, false), Some(75.0));
    }

    #[test]
    fn long_prompt_threshold_const_matches_the_sql() {
        assert!(LONG_PROMPT_SQL.contains(&LONG_PROMPT_TOKENS.to_string()));
    }

    #[test]
    fn dated_and_suffixed_ids_still_match_exactly() {
        assert_eq!(normalize_model_id("Claude-Opus-4-1-20250805"), "claude-opus-4-1");
        assert_eq!(normalize_model_id("claude-sonnet-5-5[1m]"), "claude-sonnet-5-5");
        assert_eq!(normalize_model_id("claude-haiku-4-5-20251001"), "claude-haiku-4-5");
        assert!(matches!(price_lookup("claude-opus-4-1-20250805"), Some((_, PriceBasis::Exact))));
        assert_eq!(p("claude-opus-4-1-20250805", "", 0, 0, M, false), Some(75.0));
    }

    #[test]
    fn unknown_version_falls_back_to_family_and_says_so() {
        let (row, basis) = price_lookup("claude-opus-9").unwrap();
        assert_eq!(basis, PriceBasis::Family);
        assert_eq!(row.std, OPUS_STD);
        assert!(matches!(price_lookup("claude-opus-5-5"), Some((_, PriceBasis::Exact))));
    }

    #[test]
    fn a_model_without_a_price_is_none_not_zero() {
        assert_eq!(p("claude-nova-1", "", 1000, 0, 1000, false), None);
        assert!(!has_price("claude-nova-1"));
        // <synthetic> 這類零用量列沒東西可估——$0 才是對的
        assert_eq!(p("<synthetic>", "", 0, 0, 0, false), Some(0.0));
        assert_eq!(p("<synthetic>", "", 5, 0, 0, false), None);
    }

    #[test]
    fn haiku_55_long_prompt_tier_and_opus_55_fast_mode() {
        assert_eq!(p("claude-haiku-5-5", "", M, 0, 0, false), Some(0.10));
        assert_eq!(p("claude-haiku-5-5", "", M, 0, 0, true), Some(0.50));
        assert_eq!(p("claude-haiku-5-5", "", 0, 0, M, true), Some(2.50));
        // Opus 5.5 fast $8／$40，快取讀疊 0.05×＝$0.40
        assert_eq!(p("claude-opus-5-5", "fast", M, 0, 0, false), Some(8.0));
        assert_eq!(p("claude-opus-5-5", "fast", 0, 0, M, false), Some(40.0));
        assert_eq!(p("claude-opus-5-5", "fast", 0, M, 0, false), Some(0.40));
        // 其他 Opus 的 fast 維持 $10／$50
        assert_eq!(p("claude-opus-5", "fast", M, 0, 0, false), Some(10.0));
        assert_eq!(p("claude-opus-9", "fast", 0, 0, M, false), Some(50.0));
        // 沒有 fast 價的模型 fast 欄位忽略
        assert_eq!(p("claude-sonnet-5-5", "fast", M, 0, 0, false), Some(2.0));
    }

    fn insert_event(conn: &Connection, id: &str, model: &str, input: i64, out: i64) {
        conn.execute(
            "INSERT INTO usage_events (msg_id, ts, model, entrypoint, project_dir, input_tokens, output_tokens) \
             VALUES (?1, ?2, ?3, 'cli', 'C--p', ?4, ?5)",
            params![id, chrono::Utc::now().to_rfc3339(), model, input, out],
        )
        .unwrap();
    }

    /// Haiku 5.5 的長 prompt 是**逐筆請求**判斷：同一個模型兩筆，一筆超過 10 萬、一筆沒有，各照各的檔。
    #[tokio::test]
    async fn long_prompt_tier_is_decided_per_request_in_sql() {
        let l = crate::test_support::TempLedger::new("price-long");
        let conn = l.conn.lock().await;
        insert_event(&conn, "a", "claude-haiku-5-5", 20_000_000, 0); // 長檔：20M × $0.50 = $10
        insert_event(&conn, "b", "claude-haiku-5-5", 40_000, 0); // 標準檔：$0.004
        let rows = usage_mix(&conn, "model", "model", "1", &[], 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["usd"], 10.0, "長檔 $10.00＋標準檔 $0.004；整組都當標準檔只會有 $2.00");
        assert_eq!(rows[0]["priceBasis"], "exact");
    }

    #[tokio::test]
    async fn unpriced_models_show_up_as_null_usd_and_in_the_health_list() {
        let l = crate::test_support::TempLedger::new("price-none");
        {
            let conn = l.conn.lock().await;
            insert_event(&conn, "a", "claude-nova-1", 1000, 1000);
            insert_event(&conn, "b", "claude-sonnet-5", 1000, 1000);
            insert_event(&conn, "c", "<synthetic>", 0, 0);
        }
        let h = l.health_snapshot().await.unwrap();
        let un = h["unpricedModels"].as_array().unwrap();
        assert_eq!(un.len(), 1, "<synthetic> 零用量不算：{un:?}");
        assert_eq!(un[0]["model"], "claude-nova-1");
        let conn = l.conn.lock().await;
        let rows = usage_mix(&conn, "model", "model", "1", &[], 0).unwrap();
        let nova = rows.iter().find(|r| r["model"] == "claude-nova-1").unwrap();
        assert!(nova["usd"].is_null(), "沒牌價是 null 不是 0：{nova}");
        assert_eq!(nova["priceBasis"], "none");
        assert_eq!(rows[0]["model"], "claude-sonnet-5", "有牌價的排在沒牌價的前面");
        // 混在同一個專案裡的分佈：usd 是有牌價部分的下限，並標出幾次沒算
        let proj = usage_mix(&conn, PROJECT_KEY_SQL, "project", "1", &[], 0).unwrap();
        assert_eq!(proj[0]["unpricedCalls"], 1);
        assert!(proj[0]["usd"].as_f64().unwrap() > 0.0);
    }
}

/// v1.9.4（D99 A1／A2／A3／A4／A5／A9／A10）：方案時間軸與「同方案才可比」。
#[cfg(test)]
mod plan_epoch_tests {
    use super::*;
    use crate::collector::model::{SeatConfidence, Source, TruthSample};
    use chrono::TimeZone;

    const T5X: &str = "default_claude_max_5x";
    const T20X: &str = "default_claude_max_20x";

    fn obs(tier: &str) -> PlanObservation {
        PlanObservation { tier: Some(tier.into()), org_type: Some("claude_max".into()), seat_tier: None }
    }
    fn dir(tag: &str) -> crate::test_support::TempDir {
        crate::test_support::TempDir::new(&format!("plan-{tag}"))
    }
    fn mk(kind: &str, pct: f32, at: chrono::DateTime<chrono::Utc>) -> TruthSample {
        TruthSample {
            account_uuid: Some("A".into()),
            org_uuid: Some("org-A".into()),
            seat_confidence: SeatConfidence::Exact,
            limit_kind: kind.into(),
            scope: None,
            percent: pct,
            resets_at: None,
            source: Source::CliCache,
            fetched_at: at,
            raw_json: serde_json::json!({}),
        }
    }
    /// 本地時間 y-m-d h:mm → UTC。
    fn local(y: i32, m: u32, d: u32, h: u32, mi: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::Local.with_ymd_and_hms(y, m, d, h, mi, 0).earliest().unwrap().with_timezone(&chrono::Utc)
    }
    /// 往回數 k 個月的 (年, 月)。
    fn month_back(k: u32) -> (i32, u32) {
        use chrono::Datelike;
        let today = chrono::Local::now().date_naive();
        let total = today.year() * 12 + today.month0() as i32 - k as i32;
        (total.div_euclid(12), total.rem_euclid(12) as u32 + 1)
    }
    fn key(k: u32) -> String {
        let (y, m) = month_back(k);
        format!("{y:04}-{m:02}")
    }

    /// 灌一個月：本地 10 日中午起每 10 分一筆、5h 與 7d 各 +2%、30 段，每段一筆 sonnet-5 用量
    /// （API 價 = rate × 2 美元，所以匯率恰好是 rate $/1%）。
    async fn seed_month(l: &Ledger, k: u32, rate: f64) {
        seed_at(l, k, 10, rate).await;
    }
    /// 同上，但指定本月第幾日（同一個月灌兩段不同方案時用）。
    async fn seed_at(l: &Ledger, k: u32, day: u32, rate: f64) {
        let (y, m) = month_back(k);
        let t0 = local(y, m, day, 12, 0);
        let mut samples = Vec::new();
        for i in 0..=30 {
            let t = t0 + chrono::Duration::minutes(10 * i);
            samples.push(mk("session", 2.0 * i as f32, t));
            samples.push(mk("weekly_all", 2.0 * i as f32, t));
        }
        l.insert_truth_samples(&samples).await.unwrap();
        let conn = l.conn.lock().await;
        for i in 0..30i64 {
            let ts = t0 + chrono::Duration::minutes(10 * i + 5);
            // sonnet-5 輸出 $10／M → out tokens = usd × 1e5
            let out = (rate * 2.0 * 1e5) as i64;
            conn.execute(
                "INSERT INTO usage_events (msg_id, ts, model, entrypoint, project_dir, input_tokens, output_tokens) \
                 VALUES (?1, ?2, 'claude-sonnet-5', 'cli', 'C--p', 0, ?3)",
                params![format!("m{k}-{day}-{i}"), ts.to_rfc3339(), out],
            )
            .unwrap();
        }
    }
    /// 同上，另外指定「最後一次確認」的時刻（`observed_at`）。
    async fn epoch_obs(
        l: &Ledger,
        seat: &str,
        tier: &str,
        since: chrono::DateTime<chrono::Utc>,
        until: Option<chrono::DateTime<chrono::Utc>>,
        observed: chrono::DateTime<chrono::Utc>,
    ) {
        let conn = l.conn.lock().await;
        conn.execute(
            "INSERT INTO seat_epochs (seat_id, tier, org_type, seat_tier, since, until, observed_at, source) \
             VALUES (?1, ?2, 'claude_max', NULL, ?3, ?4, ?5, 'observed')",
            params![seat, tier, since.to_rfc3339(), until.map(|u| u.to_rfc3339()), observed.to_rfc3339()],
        )
        .unwrap();
    }
    /// 直接寫一段方案時間軸（測試要能指定 since／until，不能用 now）。
    async fn epoch(l: &Ledger, seat: &str, tier: &str, since: chrono::DateTime<chrono::Utc>, until: Option<chrono::DateTime<chrono::Utc>>) {
        let conn = l.conn.lock().await;
        conn.execute(
            "INSERT INTO seat_epochs (seat_id, tier, org_type, seat_tier, since, until, observed_at, source) \
             VALUES (?1, ?2, 'claude_max', NULL, ?3, ?4, ?3, 'observed')",
            params![seat, tier, since.to_rfc3339(), until.map(|u| u.to_rfc3339())],
        )
        .unwrap();
    }
    async fn mark_with(l: &Ledger, t: Option<&PlanObservation>) -> SeatMark {
        l.mark_current_seat_with_plan(Some("A"), Some("org-A"), None, None, t).await.unwrap()
    }
    async fn seat_of(l: &Ledger) -> String {
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap()
    }
    async fn dash(l: &Ledger, seat: &str, days: u32) -> serde_json::Value {
        let conn = l.conn.lock().await;
        Ledger::dashboard_sync(&conn, Some(seat), days).unwrap()
    }
    fn first_of(k: u32) -> chrono::DateTime<chrono::Utc> {
        let (y, m) = month_back(k);
        local(y, m, 1, 0, 0)
    }

    // ---------------------------------------------------------------- 時間軸本身

    #[tokio::test]
    async fn epoch_opens_once_and_only_a_change_cuts_a_new_one() {
        let d = dir("cut");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        assert_eq!(mark_with(&l, None).await.plan, PlanNote::NoInfo);
        let a = obs(T5X);
        assert_eq!(mark_with(&l, Some(&a)).await.plan, PlanNote::First);
        let seat = mark_with(&l, Some(&a)).await;
        assert_eq!(seat.plan, PlanNote::Same);
        // 這一輪沒讀到任何欄位：不當作「方案變了」，時間軸不動
        let empty = PlanObservation::default();
        assert_eq!(mark_with(&l, Some(&empty)).await.plan, PlanNote::NoInfo);
        assert_eq!(l.plan_timeline(&seat.seat_id).await.unwrap().len(), 1);
        // 換方案 → 關舊段、開新段
        let b = obs(T20X);
        match mark_with(&l, Some(&b)).await.plan {
            PlanNote::Changed { from_label, to_label } => {
                assert_eq!(from_label, "Claude Max 5x");
                assert_eq!(to_label, "Claude Max 20x");
            }
            other => panic!("要偵測到換方案：{other:?}"),
        }
        let tl = l.plan_timeline(&seat.seat_id).await.unwrap();
        assert_eq!(tl.len(), 2);
        assert!(tl[0]["until"].is_string() && tl[1]["until"].is_null());
        // 21r1 M1：舊段停在「最後一次確認」的時刻，不是偵測時刻（中間是「方案未知」的洞）
        assert_eq!(tl[0]["until"], tl[0]["observedAt"], "舊段的 until＝它最後一次被確認的時刻");
        assert!(tl[0]["until"].as_str().unwrap() <= tl[1]["since"].as_str().unwrap());
        // 同一個方案再看一次不會多開段；換回去才多一段
        assert_eq!(mark_with(&l, Some(&b)).await.plan, PlanNote::Same);
        assert!(matches!(mark_with(&l, Some(&a)).await.plan, PlanNote::Changed { .. }));
        assert_eq!(l.plan_timeline(&seat.seat_id).await.unwrap().len(), 3);
    }

    // ---------------------------------------------------------------- 遷移

    /// 紅線：加 seat_epochs 是純新增——舊帳本（沒有這張表、版本 3）開起來不能重建、不能動任何既有資料。
    #[tokio::test]
    async fn opening_an_old_ledger_adds_the_table_and_touches_nothing_else() {
        let d = dir("migrate");
        {
            let l = Ledger::open(d.to_path_buf()).unwrap();
            l.insert_truth_samples(&[mk("session", 42.0, chrono::Utc::now())]).await.unwrap();
            let seat = seat_of(&l).await;
            let conn = l.conn.lock().await;
            conn.execute(
                "INSERT INTO usage_events (msg_id, ts, model, entrypoint, input_tokens, output_tokens) \
                 VALUES ('old', ?1, 'claude-sonnet-5', 'cli', 1, 2)",
                params![chrono::Utc::now().to_rfc3339()],
            )
            .unwrap();
            // 還原成 v1.9.3 的形狀：沒有 seat_epochs
            conn.execute_batch("DROP TABLE seat_epochs;").unwrap();
            conn.execute("INSERT INTO channel_state (key, value) VALUES ('keep', ?1)", params![seat]).unwrap();
        }
        let path = d.join("ledger.sqlite");
        let snapshot = |c: &Connection| -> (i64, i64, i64, String, String) {
            (
                c.query_row("SELECT COUNT(*) FROM truth_samples", [], |r| r.get(0)).unwrap(),
                c.query_row("SELECT COUNT(*) FROM usage_events", [], |r| r.get(0)).unwrap(),
                c.query_row("SELECT COUNT(*) FROM seats", [], |r| r.get(0)).unwrap(),
                c.query_row("SELECT id FROM truth_samples", [], |r| r.get(0)).unwrap(),
                c.query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0)).unwrap(),
            )
        };
        let before = snapshot(&Connection::open(&path).unwrap());
        assert_eq!(before.4, "3");
        let l = Ledger::open(d.to_path_buf()).expect("舊帳本要能直接開");
        drop(l);
        let c = Connection::open(&path).unwrap();
        assert_eq!(snapshot(&c), before, "既有資料與 schema_version 一個字都不能變（沒重建、沒升版）");
        let n: i64 = c.query_row("SELECT COUNT(*) FROM seat_epochs", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "新表是空的——舊資料就是「方案未知」，不回溯");
        let keep: String = c.query_row("SELECT value FROM channel_state WHERE key='keep'", [], |r| r.get(0)).unwrap();
        assert!(!keep.is_empty());
        // 再開一次（表已存在）也不報錯、不重複建
        drop(Ledger::open(d.to_path_buf()).unwrap());
    }

    // ---------------------------------------------------------------- 縮水判定的同方案閘

    /// 同一個方案等級的兩個月：可比、四道閘照舊，匯率翻倍 → 宣告；plan 閘列在最前面。
    #[tokio::test]
    async fn two_months_on_the_same_plan_are_compared() {
        let d = dir("same");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 2, 4.0).await;
        seed_month(&l, 1, 8.0).await;
        let seat = seat_of(&l).await;
        epoch(&l, &seat, T20X, first_of(3), None).await;
        let v = dash(&l, &seat, 180).await["verdict"].clone();
        assert_eq!(v["comparable"], true, "{v}");
        assert_eq!(v["prevMonth"], key(2));
        assert_eq!(v["curMonth"], key(1));
        assert_eq!(v["monthsApart"], 1);
        assert_eq!(v["gates"][0]["key"], "plan");
        assert_eq!(v["gates"][0]["pass"], true);
        assert!(v["momPct"].is_number());
        assert_eq!(v["declare"], true, "匯率 4→8 四閘全過：{v}");
    }

    /// 整月換方案：D99 Q2——不宣告，句子是「這期間換過方案（X→Y，MM-DD），不能跨比」，沒有 momPct。
    #[tokio::test]
    async fn months_on_different_plans_are_not_compared() {
        let d = dir("diff");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 2, 4.0).await;
        seed_month(&l, 1, 16.0).await; // 池從 5x 變 20x → 每 1% 的美元多 4 倍，要是跨比就是「放寬」誤判
        let seat = seat_of(&l).await;
        epoch(&l, &seat, T5X, first_of(3), Some(first_of(1))).await;
        epoch(&l, &seat, T20X, first_of(1), None).await;
        let dj = dash(&l, &seat, 180).await;
        let v = &dj["verdict"];
        assert_eq!(v["declare"], false);
        assert_eq!(v["comparable"], false);
        assert_eq!(v["reason"], "plan_changed");
        assert!(v["momPct"].is_null(), "跨方案不出「較上月」：{v}");
        assert_eq!(v["planCross"]["fromLabel"], "Claude Max 5x");
        assert_eq!(v["planCross"]["toLabel"], "Claude Max 20x");
        let (_, m) = month_back(1);
        assert_eq!(v["planCross"]["switchOn"], format!("{m:02}-01"));
        assert_eq!(
            v["sentence"],
            format!("這期間換過方案（Claude Max 5x→Claude Max 20x，{m:02}-01），不能跨比。")
        );
        // 月匯率各自按自己的方案算：兩列、各標方案
        let fx = dj["fx5"].as_array().unwrap();
        assert_eq!(fx.len(), 2);
        assert_eq!(fx[0]["planLabel"], "Claude Max 5x");
        assert_eq!(fx[1]["planLabel"], "Claude Max 20x");
        assert!((fx[0]["rate"].as_f64().unwrap() - 4.0).abs() < 0.01);
        assert!((fx[1]["rate"].as_f64().unwrap() - 16.0).abs() < 0.01);
        // 這段期間跨了方案（A13）
        assert_eq!(dj["plan"]["crossesKnownPlans"], true);
        assert_eq!(dj["plan"]["changes"].as_array().unwrap().len(), 1);
        // A5：模型倍率只用最近的 20x 那 30 段，5x 那 30 段排除
        assert_eq!(dj["byModel"]["scope"]["planLabel"], "Claude Max 20x");
        assert_eq!(dj["byModel"]["scope"]["intervals"], 30);
        assert_eq!(dj["byModel"]["scope"]["excludedIntervals"], 30);
        // 審查 23r：排除的 30 段全是「換方案前」（另一個已知方案），沒有方案未知的
        assert_eq!(dj["byModel"]["scope"]["excludedOtherPlan"], 30);
        assert_eq!(dj["byModel"]["scope"]["excludedUnknownPlan"], 0);
        // 審查 23r：planScoped 帶 Δ 合計＝只算目前方案那 30 段（前端守門要同時看 minN 與 minDeltaSum）
        let ps = &dj["reconcile"]["planScoped"];
        assert_eq!(ps["intervals"], 30);
        let ds = ps["deltaSum"].as_f64().unwrap();
        let usd = ps["rateRaw"].as_f64().unwrap() * ds;
        assert!(ds > 0.0 && (usd / ds - 16.0).abs() < 0.01, "deltaSum 要是 20x 那批的 Δ 合計：{ps}");
        // A4：每個方案各一個基期；縮到 7 天（期間裡一筆都沒有）基期也不變
        assert_eq!(dj["shrink"]["bases"].as_array().unwrap().len(), 2);
        let short = dash(&l, &seat, 7).await;
        assert_eq!(short["shrink"]["bases"], dj["shrink"]["bases"], "基期不受期間選擇影響");
        assert_eq!(short["shrink"]["bases"][0]["baseMonth"], key(2));
        assert_eq!(short["shrink"]["bases"][1]["baseMonth"], key(1));
        // 證據包要帶出方案時間軸（A9）——見 export 測試
    }

    /// 工程判斷：5x→20x→5x 切換——同一個方案等級的不同時段**可以互比**（隔了一個月也行）。
    #[tokio::test]
    async fn same_plan_in_separate_stretches_is_comparable_across_the_other_plan() {
        let d = dir("aba");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 3, 4.0).await;
        seed_month(&l, 2, 16.0).await;
        seed_month(&l, 1, 8.0).await;
        let seat = seat_of(&l).await;
        epoch(&l, &seat, T20X, first_of(4), Some(first_of(2))).await;
        epoch(&l, &seat, T5X, first_of(2), Some(first_of(1))).await;
        epoch(&l, &seat, T20X, first_of(1), None).await;
        let v = dash(&l, &seat, 180).await["verdict"].clone();
        assert_eq!(v["comparable"], true, "{v}");
        assert_eq!(v["prevMonth"], key(3), "跳過中間 5x 的那個月，跟上一段 20x 比");
        assert_eq!(v["curMonth"], key(1));
        assert_eq!(v["monthsApart"], 2, "前端不能再寫死「較上月」");
    }

    /// 方案從沒被觀測過（舊資料）＝「方案未知」，不跟任何東西比（D99 Q1）。
    #[tokio::test]
    async fn unknown_plan_never_declares() {
        let d = dir("unknown");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 2, 4.0).await;
        seed_month(&l, 1, 8.0).await;
        let seat = seat_of(&l).await;
        let dj = dash(&l, &seat, 180).await;
        let v = &dj["verdict"];
        assert_eq!(v["declare"], false);
        assert_eq!(v["comparable"], false);
        assert_eq!(v["reason"], "plan_unknown");
        assert!(v["momPct"].is_null());
        assert_eq!(dj["fx5"][0]["planKnown"], false);
        assert_eq!(dj["fx5"][0]["planLabel"], "方案未知");
        assert_eq!(dj["selected"]["plan"]["known"], false);
        // 圖還是畫得出來：未知段自成一個基期
        assert_eq!(dj["shrink"]["monthly"].as_array().unwrap().len(), 2);
    }

    /// 升版後第一輪：App 剛開始記方案（今天），過去所有月份都是「方案未知」——不宣告，句子講「從哪天才開始記」，
    /// 不能說成「這個帳號的方案還沒被記錄過」（它其實記到了，只是歷史補不回來）。
    #[tokio::test]
    async fn first_run_after_upgrade_explains_that_history_predates_plan_tracking() {
        let d = dir("firstrun");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 2, 4.0).await;
        seed_month(&l, 1, 8.0).await;
        let seat = mark_with(&l, Some(&obs(T20X))).await.seat_id;
        let dj = dash(&l, &seat, 180).await;
        assert_eq!(dj["verdict"]["declare"], false);
        assert_eq!(dj["verdict"]["reason"], "plan_unknown_history", "{}", dj["verdict"]);
        assert!(dj["verdict"]["sentence"].as_str().unwrap().contains("才開始記錄方案"));
        assert_eq!(dj["selected"]["plan"]["label"], "Claude Max 20x");
        // 時間軸上看得到兩段（過去未知、現在已知），但新的已知段還沒有任何區間——不算「跨了方案」（21r1 nit 8）
        assert_eq!(dj["plan"]["segments"].as_array().unwrap().len(), 2);
        assert_eq!(dj["plan"]["crossesPlan"], false);
    }

    /// 方案剛開始記（今天）：之前的月份是「方案未知」，最近的月份是已知——不能跨比，句子寫明從哪天開始記。
    #[tokio::test]
    async fn plan_first_seen_after_unknown_history_does_not_compare_across() {
        let d = dir("firstseen");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 2, 4.0).await;
        seed_month(&l, 1, 8.0).await;
        let seat = seat_of(&l).await;
        epoch(&l, &seat, T20X, first_of(1), None).await;
        let dj = dash(&l, &seat, 180).await;
        let v = dj["verdict"].clone();
        assert_eq!(v["declare"], false);
        assert_eq!(v["reason"], "plan_unknown_before", "{v}");
        let (_, m) = month_back(1);
        assert_eq!(v["sentence"], format!("方案從 {m:02}-01 才開始有記錄，{m:02}-01 以前的方案未知，不能跨比。"));
        // 審查 23r：模型倍率排除的全是「方案未知」那段，不是「換方案前」——前端才不會說成另一個池
        let sc = &dj["byModel"]["scope"];
        assert!(sc["excludedIntervals"].as_u64().unwrap() > 0, "{sc}");
        assert_eq!(sc["excludedUnknownPlan"], sc["excludedIntervals"], "{sc}");
        assert_eq!(sc["excludedOtherPlan"], 0, "{sc}");
        assert_eq!(dj["plan"]["crossesKnownPlans"], false);
    }

    /// 審查 23r（線 F 交辦）：使用者看得到的句子裡「方案不明」一律寫成「方案未知」（跟畫面、圖卡同一個說法）。
    #[tokio::test]
    async fn plan_sentences_say_unknown_not_unclear() {
        let d = dir("wording");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 2, 4.0).await;
        seed_month(&l, 1, 8.0).await;
        let seat = mark_with(&l, Some(&obs(T20X))).await.seat_id;
        let dj = dash(&l, &seat, 180).await;
        let s = dj["verdict"]["sentence"].as_str().unwrap();
        assert!(s.contains("方案未知") && !s.contains("方案不明"), "{s}");
        let t = l.today_outlook(Some(&seat)).await.unwrap();
        let r = t["plan"]["reason"].as_str().unwrap();
        assert!(!r.contains("方案不明"), "{r}");
    }

    /// A10：全量視角不合匯率，期間消耗照合計。
    #[tokio::test]
    async fn all_seats_view_does_not_pool_the_rate() {
        let d = dir("all");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 2, 4.0).await;
        seed_month(&l, 1, 8.0).await;
        let seat = seat_of(&l).await;
        epoch(&l, &seat, T20X, first_of(3), None).await;
        let dj = dash(&l, ALL_SEATS, 180).await;
        assert_eq!(dj["fx5"].as_array().unwrap().len(), 0);
        assert_eq!(dj["fxMode"], "per_seat");
        assert_eq!(dj["verdict"]["reason"], "all_seats");
        assert!(dj["reconcile"]["rateTrim"].is_null());
        assert_eq!(dj["perSeat"].as_array().unwrap().len(), 1);
        assert!(dj["perSeat"][0]["rateTrim"].as_f64().unwrap() > 0.0);
        assert!(dj["daily"].as_array().unwrap().len() >= 2, "期間消耗照樣合計");
    }

    // ---------------------------------------------------------------- 審查後修正（21r1／21r2）

    /// 21r1 M1：舊段最後一次確認是兩個月前，這兩個月裡其實已經換到 20x 只是沒被看到。偵測到的那一刻，
    /// 舊段停在「最後一次確認」，中間是「方案未知」的洞——洞裡的月份不能被標成舊方案去跟舊月份比。
    /// （修之前：09 月被標 5x，4→16 的 +300% 四道閘全過、宣告。）
    #[tokio::test]
    async fn an_unobserved_change_leaves_a_hole_not_a_mislabelled_month() {
        let d = dir("hole");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 3, 4.0).await; // 5x 時期，有確認
        seed_month(&l, 2, 4.0).await; // 其實已經 20x，但沒被看到
        seed_month(&l, 1, 16.0).await;
        let seat = seat_of(&l).await;
        // 5x 這一段最後一次被確認是 m3 結束（= m2 開始）；之後 App 沒開／座位沒登入，沒人看到方案
        epoch_obs(&l, &seat, T5X, first_of(4), None, first_of(2)).await;
        // 現在才看到 20x
        let m = mark_with(&l, Some(&obs(T20X))).await;
        assert!(matches!(m.plan, PlanNote::Changed { .. }));
        let tl = l.plan_timeline(&seat).await.unwrap();
        assert_eq!(tl.len(), 2);
        assert_eq!(tl[0]["until"], tl[0]["observedAt"], "舊段停在最後確認的時刻，不是偵測時刻");
        assert_eq!(tl[0]["until"].as_str().unwrap(), first_of(2).to_rfc3339());
        let dj = dash(&l, &seat, 180).await;
        let fx = dj["fx5"].as_array().unwrap();
        let plan_of = |month: String| -> Vec<(bool, String)> {
            fx.iter().filter(|r| r["month"] == month).map(|r| (r["planKnown"].as_bool().unwrap(), r["planLabel"].as_str().unwrap().to_string())).collect()
        };
        assert_eq!(plan_of(key(3)), vec![(true, "Claude Max 5x".to_string())]);
        assert_eq!(plan_of(key(2)), vec![(false, "方案未知".to_string())], "洞裡的月份是方案未知，不是 5x");
        assert_eq!(plan_of(key(1)), vec![(false, "方案未知".to_string())]);
        let v = &dj["verdict"];
        assert_eq!(v["declare"], false, "{v}");
        assert_eq!(v["comparable"], false);
        assert!(v["momPct"].is_null());
        // 洞也出現在方案分段裡（前端看得到「這段時間不知道是什麼方案」）
        let segs: Vec<String> = dj["plan"]["segments"].as_array().unwrap().iter().map(|s| s["planLabel"].as_str().unwrap().to_string()).collect();
        assert!(segs.contains(&"方案未知".to_string()), "{segs:?}");
    }

    /// 洞的起點也是歷史頁的切點：洞裡的樣本可能已是新池的 %，不能留在舊窗裡。
    #[tokio::test]
    async fn history_windows_cut_at_the_start_of_the_hole() {
        let d = dir("holecut");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        let now = chrono::Utc::now();
        let h = |m: i64| now - chrono::Duration::minutes(m);
        // 舊池最後一次確認在 −300 分；−200 分的樣本在洞裡（已是新池的 % 階躍）；−100 分才偵測到新方案
        l.insert_truth_samples(&[mk("session", 20.0, h(320)), mk("session", 25.0, h(280)), mk("session", 80.0, h(200)), mk("session", 85.0, h(60))])
            .await
            .unwrap();
        let seat = seat_of(&l).await;
        epoch_obs(&l, &seat, T5X, h(1000), Some(h(300)), h(300)).await;
        epoch(&l, &seat, T20X, h(100), None).await;
        let conn = l.conn.lock().await;
        let (_, wins, _) = Ledger::windows_sync(
            &conn,
            &seat,
            HistKind::FiveHour,
            &h(2000).to_rfc3339(),
            &(now + chrono::Duration::minutes(1)).to_rfc3339(),
            false,
        )
        .unwrap();
        assert_eq!(wins.len(), 2, "{wins:?}");
        assert!(wins[1].plan_change && !wins[0].plan_change);
        assert_eq!(wins[0].peak, 20.0, "洞裡的樣本（25、80）不能留在舊窗：洞的起點才是切點");
        assert_eq!(wins[1].peak, 85.0);
    }

    /// 21r1 M2：同一個月有兩個方案組時，「最近的合格月」與輸出列序要照時間排，不是照方案字串的字母序
    /// （`max_20x` < `max_5x` < `unknown`）。月內先 5x 後 20x、現在是 20x：該比的是 20x 對 20x。
    #[tokio::test]
    async fn two_plan_groups_in_one_month_are_ordered_by_time() {
        let d = dir("samemonth");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 3, 4.0).await; // 5x
        seed_month(&l, 2, 16.0).await; // 20x
        seed_at(&l, 1, 5, 4.0).await; // 本月前半 5x
        seed_at(&l, 1, 20, 16.0).await; // 本月後半 20x（現在）
        let seat = seat_of(&l).await;
        let at = |k: u32, day: u32| {
            let (y, m) = month_back(k);
            local(y, m, day, 0, 0)
        };
        epoch(&l, &seat, T5X, first_of(4), Some(first_of(2))).await;
        epoch(&l, &seat, T20X, first_of(2), Some(first_of(1))).await;
        epoch(&l, &seat, T5X, first_of(1), Some(at(1, 12))).await;
        epoch(&l, &seat, T20X, at(1, 12), None).await;
        let dj = dash(&l, &seat, 180).await;
        let order: Vec<String> = dj["fx5"].as_array().unwrap().iter().map(|r| format!("{} {}", r["month"].as_str().unwrap(), r["planKey"].as_str().unwrap())).collect();
        assert_eq!(
            order,
            vec![format!("{} max_5x", key(3)), format!("{} max_20x", key(2)), format!("{} max_5x", key(1)), format!("{} max_20x", key(1))],
            "列序＝時間順序"
        );
        let v = &dj["verdict"];
        assert_eq!(v["comparable"], true, "{v}");
        assert_eq!(v["planLabel"], "Claude Max 20x", "現在是 20x");
        assert_eq!(v["curMonth"], key(1));
        assert_eq!(v["prevMonth"], key(2), "拿本月後半的 20x 對上月的 20x");
        // 圖上的線在方案不同的相鄰列斷開，斷在對的地方
        let brk: Vec<bool> = dj["shrink"]["monthly"].as_array().unwrap().iter().map(|r| r["planBreak"].as_bool().unwrap()).collect();
        assert_eq!(brk, vec![false, true, true, true]);
        // 舊欄位 baseMonth 是**目前方案（20x）**的基期，不是 5x 的
        assert_eq!(dj["shrink"]["baseMonth"], key(2));
    }

    /// 21r1 M2：已知→（同月）未知→已知。後面那段已知的 20x 可以直接跟前一個月的 20x 比。
    #[tokio::test]
    async fn known_unknown_known_in_one_month_compares_the_two_known_stretches() {
        let d = dir("kuk");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 2, 16.0).await;
        seed_at(&l, 1, 5, 16.0).await; // 本月前半：App 沒開，方案未知（洞）
        seed_at(&l, 1, 20, 16.0).await; // 本月後半：又看到了
        let seat = seat_of(&l).await;
        let (y, m) = month_back(1);
        epoch_obs(&l, &seat, T20X, first_of(3), Some(first_of(1)), first_of(1)).await;
        epoch(&l, &seat, T20X, local(y, m, 12, 0, 0), None).await;
        let dj = dash(&l, &seat, 180).await;
        let v = &dj["verdict"];
        assert_eq!(v["comparable"], true, "{v}");
        assert_eq!(v["curMonth"], key(1));
        assert_eq!(v["prevMonth"], key(2));
        // 本月的「方案未知」那一組排在已知那一組前面（時間先後）
        let order: Vec<bool> = dj["fx5"].as_array().unwrap().iter().filter(|r| r["month"] == key(1)).map(|r| r["planKnown"].as_bool().unwrap()).collect();
        assert_eq!(order, vec![false, true]);
    }

    /// 21r1 S1／21r2 F1：方案身分只看等級。欄位暫時讀不到、seatTier 新出現都不切段、不觸發「換方案」（也就不會多一次刷新）。
    #[tokio::test]
    async fn partial_reads_and_a_new_seat_tier_do_not_cut_a_segment() {
        let d = dir("partial");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        let o = |tier: Option<&str>, org: Option<&str>, seat: Option<&str>| PlanObservation {
            tier: tier.map(Into::into),
            org_type: org.map(Into::into),
            seat_tier: seat.map(Into::into),
        };
        let first = mark_with(&l, Some(&o(Some(T20X), Some("claude_max"), None))).await;
        assert_eq!(first.plan, PlanNote::First);
        let seat = first.seat_id;
        // tier 這一輪讀不到（只剩 organizationType）
        assert_eq!(mark_with(&l, Some(&o(None, Some("claude_max"), None))).await.plan, PlanNote::NoInfo);
        // tier 回來
        assert_eq!(mark_with(&l, Some(&o(Some(T20X), Some("claude_max"), None))).await.plan, PlanNote::Same);
        // seatTier 新冒出來
        assert_eq!(mark_with(&l, Some(&o(Some(T20X), Some("claude_max"), Some("standard")))).await.plan, PlanNote::Same);
        // seatTier 換字串也不算
        assert_eq!(mark_with(&l, Some(&o(Some(T20X), Some("claude_max"), Some("premium")))).await.plan, PlanNote::Same);
        // 只剩 seatTier（tier 與 organizationType 都沒讀到）＝沒有方案資訊
        assert_eq!(mark_with(&l, Some(&o(None, None, Some("premium")))).await.plan, PlanNote::NoInfo);
        // 同一個等級、tier 字串細節不同（大小寫）
        assert_eq!(mark_with(&l, Some(&o(Some("Default_Claude_Max_20X"), Some("claude_max"), None))).await.plan, PlanNote::Same);
        let tl = l.plan_timeline(&seat).await.unwrap();
        assert_eq!(tl.len(), 1, "從頭到尾只有一段：{tl:?}");
        assert_eq!(tl[0]["seatTier"], "standard", "新冒出來的 seatTier 補寫進目前這段，不覆寫");
        assert!(tl[0]["until"].is_null());
        // 真的換了才切：換等級、或 organizationType 矛盾
        assert!(matches!(mark_with(&l, Some(&o(Some(T5X), Some("claude_max"), None))).await.plan, PlanNote::Changed { .. }));
        assert!(matches!(
            mark_with(&l, Some(&o(Some(T5X), Some("claude_team"), None))).await.plan,
            PlanNote::Changed { .. }
        ));
        assert_eq!(l.plan_timeline(&seat).await.unwrap().len(), 3);
    }

    /// 第一次讀到的是不完整的（只有 organizationType）→ 開一段「認不得等級」的段；之後讀全了是精修，不是換方案。
    #[tokio::test]
    async fn a_first_partial_read_is_refined_in_place() {
        let d = dir("refine");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        let half = PlanObservation { tier: None, org_type: Some("claude_max".into()), seat_tier: None };
        let seat = mark_with(&l, Some(&half)).await.seat_id;
        assert_eq!(mark_with(&l, Some(&obs(T20X))).await.plan, PlanNote::Same);
        let tl = l.plan_timeline(&seat).await.unwrap();
        assert_eq!(tl.len(), 1);
        assert_eq!(tl[0]["key"], "max_20x");
        assert_eq!(tl[0]["label"], "Claude Max 20x");
    }

    /// 21r1 S4：期間混了方案時，「每 1%」類的數字只算目前方案；全量視角不給。
    #[tokio::test]
    async fn pool_dependent_numbers_follow_the_current_plan_or_disappear() {
        let d = dir("s4");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 2, 4.0).await;
        seed_month(&l, 1, 16.0).await;
        let seat = seat_of(&l).await;
        epoch(&l, &seat, T5X, first_of(3), Some(first_of(1))).await;
        epoch(&l, &seat, T20X, first_of(1), None).await;
        let dj = dash(&l, &seat, 180).await;
        assert_eq!(dj["reconcile"]["mixedPlans"], true);
        // workRate／bands／unexplainedShare 吃目前方案那 30 段（byModel 同一批），不是 60 段
        assert_eq!(dj["byModel"]["scope"]["intervals"], 30);
        let all = dash(&l, ALL_SEATS, 180).await;
        assert!(all["workRate"]["turnsPerPercent"].is_null(), "{}", all["workRate"]);
        assert!(all["workRate"]["outputTokensPerPercent"].is_null());
        assert!(all["reconcile"]["unexplainedShare"].is_null());
        assert!(all["reconcile"]["zeroEventDeltaShare"].is_null());
        assert!(all["heat"]["bands"].as_array().unwrap().iter().all(|b| b["rate"].is_null()));
        // 21r1 nit 1：全量視角的 byModel.scope 不宣稱任何方案
        assert!(all["byModel"]["scope"]["plan"].is_null() && all["byModel"]["scope"]["planLabel"].is_null());
    }

    /// 21r1 S3：可比但相隔不只一個月、又沒過檢定時，報告不能寫「較上月」。
    #[tokio::test]
    async fn evidence_report_does_not_say_last_month_when_the_months_are_apart() {
        let d = dir("s3");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 3, 4.0).await;
        seed_month(&l, 2, 16.0).await;
        seed_month(&l, 1, 4.0).await; // 與 m3 同匯率：可比、沒過檢定
        let seat = seat_of(&l).await;
        epoch(&l, &seat, T20X, first_of(4), Some(first_of(2))).await;
        epoch(&l, &seat, T5X, first_of(2), Some(first_of(1))).await;
        epoch(&l, &seat, T20X, first_of(1), None).await;
        let out = dir("s3-out");
        let paths = l.export_evidence(Some(&seat), 180, &out, "test").await.unwrap();
        let md = std::fs::read_to_string(paths.iter().find(|p| p.extension().is_some_and(|e| e == "md")).unwrap()).unwrap();
        assert!(md.contains("資料本身"), "{md}");
        assert!(!md.contains("較上月"), "隔了一個月，不能寫較上月：{md}");
        assert!(md.contains(&format!("較 {}", key(3))), "{md}");
        assert!(!md.contains("。。"), "句尾不能有兩個句號");
    }

    /// 21r2 F5：monthly.csv 與 intervals.csv 的 plan 欄是同一個口徑（方案鍵），對得起來。
    #[tokio::test]
    async fn csv_plan_columns_use_the_same_key() {
        let d = dir("csvkey");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 2, 4.0).await;
        seed_month(&l, 1, 8.0).await;
        let seat = seat_of(&l).await;
        epoch(&l, &seat, T20X, first_of(3), None).await;
        let out = dir("csvkey-out");
        let paths = l.export_evidence(Some(&seat), 180, &out, "test").await.unwrap();
        let read = |suffix: &str| std::fs::read_to_string(paths.iter().find(|p| p.to_string_lossy().ends_with(suffix)).unwrap()).unwrap();
        let monthly = read("-monthly.csv");
        let row = monthly.lines().nth(1).unwrap();
        assert!(row.ends_with(",max_20x,Claude Max 20x"), "{row}");
        let intervals = read("-intervals.csv");
        assert!(intervals.lines().nth(1).unwrap().contains(",max_20x,"), "{intervals}");
    }

    /// 有牌價、但不屬於任何家族欄的模型（例如 Mythos）：美元算進匯率，但這段區間不進分模型回歸。
    #[tokio::test]
    async fn intervals_with_a_family_less_model_stay_out_of_the_regression() {
        let d = dir("nofamily");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 1, 8.0).await;
        let seat = seat_of(&l).await;
        epoch(&l, &seat, T20X, first_of(2), None).await;
        {
            let (y, m) = month_back(1);
            let conn = l.conn.lock().await;
            conn.execute(
                "INSERT INTO usage_events (msg_id, ts, model, entrypoint, input_tokens, output_tokens) \
                 VALUES ('mythos', ?1, 'claude-mythos-5-1', 'cli', 1000, 1000)",
                params![local(y, m, 10, 12, 5).to_rfc3339()],
            )
            .unwrap();
        }
        let dj = dash(&l, &seat, 180).await;
        assert_eq!(dj["fx5"][0]["samples"], 30, "有牌價，區間照算進匯率");
        assert_eq!(dj["byModel"]["scope"]["excludedNoFamily"], 1);
        assert_eq!(dj["byModel"]["scope"]["intervals"], 29);
    }

    /// B2：含無牌價用量的區間不進匯率（usd 只是下限），另外報出來。
    #[tokio::test]
    async fn intervals_with_unpriced_usage_leave_the_rate_and_are_reported() {
        let d = dir("unpriced");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 1, 8.0).await;
        let seat = seat_of(&l).await;
        epoch(&l, &seat, T20X, first_of(2), None).await;
        {
            let (y, m) = month_back(1);
            let ts = local(y, m, 10, 12, 5);
            let conn = l.conn.lock().await;
            conn.execute(
                "INSERT INTO usage_events (msg_id, ts, model, entrypoint, input_tokens, output_tokens) \
                 VALUES ('x', ?1, 'claude-nova-1', 'cli', 1000, 1000)",
                params![ts.to_rfc3339()],
            )
            .unwrap();
        }
        let dj = dash(&l, &seat, 180).await;
        assert_eq!(dj["fx5"][0]["samples"], 29, "30 段裡有 1 段含無牌價用量，剔除");
        assert_eq!(dj["unpriced"]["intervals"], 1);
        assert_eq!(dj["unpriced"]["models"][0]["model"], "claude-nova-1");
    }

    /// A9：證據包附方案時間軸，不同方案的月份不寫結論句。
    #[tokio::test]
    async fn evidence_pack_carries_the_plan_timeline_and_no_cross_plan_verdict() {
        let d = dir("evidence");
        let l = Ledger::open(d.to_path_buf()).unwrap();
        seed_month(&l, 2, 4.0).await;
        seed_month(&l, 1, 16.0).await;
        let seat = seat_of(&l).await;
        epoch(&l, &seat, T5X, first_of(3), Some(first_of(1))).await;
        epoch(&l, &seat, T20X, first_of(1), None).await;
        let out = dir("evidence-out");
        let paths = l.export_evidence(Some(&seat), 180, &out, "test").await.unwrap();
        let json_path = paths.iter().find(|p| p.extension().is_some_and(|e| e == "json")).unwrap();
        let j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(json_path).unwrap()).unwrap();
        assert_eq!(j["planTimeline"][0]["epochs"].as_array().unwrap().len(), 2);
        assert_eq!(j["verdict"]["comparable"], false);
        assert!(j["intervals"][0]["plan"].is_string());
        // 新欄一律放最後，舊的重放腳本按欄位順序讀才不會錯位
        let head = |suffix: &str| -> String {
            let p = paths.iter().find(|p| p.to_string_lossy().ends_with(suffix)).unwrap();
            std::fs::read_to_string(p).unwrap().lines().next().unwrap().to_string()
        };
        assert_eq!(head("-monthly.csv"), "window,month,samples,deltaSum,usdSum,rateRaw,rateTrim,ciLo,ciHi,qualified,plan,planLabel");
        assert!(head("-intervals.csv").starts_with("t0,t1,deltaPct,usd,fable,opus,sonnet5,sonnet,haiku,"));
        assert!(head("-intervals.csv").ends_with(",plan,unpriced"));
        let md_path = paths.iter().find(|p| p.extension().is_some_and(|e| e == "md")).unwrap();
        let md = std::fs::read_to_string(md_path).unwrap();
        assert!(md.contains("## 方案時間軸"));
        assert!(md.contains("不宣告縮水或放寬"), "跨方案不能有結論句");
        assert!(md.contains("這期間換過方案（Claude Max 5x→Claude Max 20x"), "{md}");
        assert!(!md.contains("較上月"), "跨方案不能寫「較上月」");
    }
}

/// v1.9.4（D99 A6／A7／A11／D2／E4）：今日頁的預測、方案建議、歷史窗口、429 錨點歸類。
#[cfg(test)]
mod plan_outlook_tests {
    use super::*;
    use crate::collector::model::{SeatConfidence, Source, TruthSample};

    fn mk(kind: &str, pct: f32, at: chrono::DateTime<chrono::Utc>, reset: Option<chrono::DateTime<chrono::Utc>>) -> TruthSample {
        TruthSample {
            account_uuid: Some("A".into()),
            org_uuid: Some("org-A".into()),
            seat_confidence: SeatConfidence::Exact,
            limit_kind: kind.into(),
            scope: None,
            percent: pct,
            resets_at: reset,
            source: Source::CliCache,
            fetched_at: at,
            raw_json: serde_json::json!({}),
        }
    }
    fn ep(tier: &str, org_type: &str, since: chrono::DateTime<chrono::Utc>) -> PlanEpoch {
        PlanEpoch {
            id: 0,
            seat_id: "s".into(),
            tier: Some(tier.into()),
            org_type: Some(org_type.into()),
            seat_tier: None,
            since: since.to_rfc3339(),
            until: None,
            observed_at: since.to_rfc3339(),
            source: "observed".into(),
            since_t: since,
            until_t: None,
        }
    }
    async fn epoch(l: &Ledger, seat: &str, tier: &str, since: chrono::DateTime<chrono::Utc>, until: Option<chrono::DateTime<chrono::Utc>>) {
        let conn = l.conn.lock().await;
        conn.execute(
            "INSERT INTO seat_epochs (seat_id, tier, org_type, seat_tier, since, until, observed_at, source) \
             VALUES (?1, ?2, 'claude_max', NULL, ?3, ?4, ?3, 'observed')",
            params![seat, tier, since.to_rfc3339(), until.map(|u| u.to_rfc3339())],
        )
        .unwrap();
    }
    const T5X: &str = "default_claude_max_5x";
    const T20X: &str = "default_claude_max_20x";
    fn min(m: i64) -> chrono::Duration {
        chrono::Duration::minutes(m)
    }

    // ------------------------------------------------------------------ D2

    #[tokio::test]
    async fn a_last_sample_older_than_the_short_window_gives_no_eta() {
        let l = crate::test_support::TempLedger::new("d2-stale");
        let now = chrono::Utc::now();
        let reset = now + min(72);
        l.insert_truth_samples(&[
            mk("session", 10.0, now - min(240), Some(reset)),
            mk("session", 40.0, now - min(180), Some(reset)),
            mk("session", 60.0, now - min(150), Some(reset)),
        ])
        .await
        .unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let o = l.today_outlook(None).await.unwrap();
        let s = &o["session"];
        assert_eq!(s["stale"], true, "{s}");
        assert!(s["etaAt"].is_null() && s["etaShort"].is_null() && s["etaLong"].is_null(), "過去的見底時刻一個都不能留：{s}");
        assert_eq!(s["percent"], 60.0, "百分比與時間照回，畫面才說得出「上次是多少」");
    }

    #[tokio::test]
    async fn a_fresh_fast_burn_still_predicts() {
        let l = crate::test_support::TempLedger::new("d2-fresh");
        let now = chrono::Utc::now();
        let reset = now + min(180);
        l.insert_truth_samples(&[
            mk("session", 30.0, now - min(65), Some(reset)),
            mk("session", 50.0, now - min(35), Some(reset)),
            mk("session", 70.0, now - min(5), Some(reset)),
        ])
        .await
        .unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let s = l.today_outlook(None).await.unwrap()["session"].clone();
        assert_eq!(s["stale"], false, "{s}");
        let eta = chrono::DateTime::parse_from_rfc3339(s["etaAt"].as_str().expect("新鮮的快速燃燒要有見底時刻")).unwrap();
        assert!(eta > now, "見底時刻在未來");
    }

    // ------------------------------------------------------------------ A11

    // ---- 審查後修正（21r2 F4）：D2 兩個條件各自要能單獨被鎖住

    /// 緩燒、沒有任何見底時刻——只有「年齡超過短窗」這一條會讓它變 stale。
    /// 5h 條門檻 1 小時：59 分鐘不算、61 分鐘算。
    async fn slow_session(tag: &str, last_age_min: i64) -> serde_json::Value {
        let l = crate::test_support::TempLedger::new(tag);
        let now = chrono::Utc::now();
        let reset = now + min(120);
        l.insert_truth_samples(&[
            mk("session", 10.0, now - min(180), Some(reset)),
            mk("session", 10.5, now - min(last_age_min), Some(reset)),
        ])
        .await
        .unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        l.today_outlook(None).await.unwrap()["session"].clone()
    }

    #[tokio::test]
    async fn age_alone_decides_staleness_with_a_one_hour_threshold_on_the_five_hour_bar() {
        let fresh = slow_session("d2-59", 59).await;
        assert_eq!(fresh["stale"], false, "59 分鐘還新鮮：{fresh}");
        let old = slow_session("d2-61", 61).await;
        assert_eq!(old["stale"], true, "61 分鐘超過短窗：{old}");
        assert!(old["etaAt"].is_null(), "緩燒本來就沒有 ETA——stale 完全來自年齡");
    }

    /// 週條門檻 24 小時：5 小時前的樣本不是 stale，25 小時前才是。
    #[tokio::test]
    async fn weekly_bar_threshold_is_a_whole_day() {
        let run = |tag: &'static str, first_h: i64, last_h: i64| async move {
            let l = crate::test_support::TempLedger::new(tag);
            let now = chrono::Utc::now();
            let reset = now + chrono::Duration::hours(72);
            l.insert_truth_samples(&[
                mk("weekly_all", 10.0, now - chrono::Duration::hours(first_h), Some(reset)),
                mk("weekly_all", 10.5, now - chrono::Duration::hours(last_h), Some(reset)),
            ])
            .await
            .unwrap();
            l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
            l.today_outlook(None).await.unwrap()["weekly"].clone()
        };
        assert_eq!(run("d2-w5", 30, 5).await["stale"], false);
        assert_eq!(run("d2-w25", 50, 25).await["stale"], true);
    }

    /// 樣本很新（5 分鐘前）但燒得太快：外推出的見底時刻已經在過去——只有「ETA 在過去」這一條會讓它 stale。
    #[tokio::test]
    async fn an_eta_that_already_passed_makes_a_fresh_sample_stale() {
        let l = crate::test_support::TempLedger::new("d2-past");
        let now = chrono::Utc::now();
        let reset = now + min(180);
        l.insert_truth_samples(&[
            mk("session", 55.0, now - min(35), Some(reset)),
            mk("session", 95.0, now - min(5), Some(reset)),
        ])
        .await
        .unwrap();
        l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let s = l.today_outlook(None).await.unwrap()["session"].clone();
        assert_eq!(s["stale"], true, "{s}");
        assert!(s["etaAt"].is_null() && s["etaShort"].is_null() && s["etaLong"].is_null(), "{s}");
    }

    /// 21r1 S2：換方案之後又發生一次真的重置——本窗已是乾淨的新窗，不再掛 planChange／insufficient。
    #[tokio::test]
    async fn a_real_reset_after_a_plan_change_clears_the_plan_flag() {
        let l = crate::test_support::TempLedger::new("s2-reset");
        let now = chrono::Utc::now();
        l.insert_truth_samples(&[
            mk("session", 60.0, now - min(300), None),
            mk("session", 70.0, now - min(240), None), // 換方案之後
            mk("session", 2.0, now - min(200), None),  // 真的重置
            mk("session", 10.0, now - min(100), None),
            mk("session", 20.0, now - min(10), None),
        ])
        .await
        .unwrap();
        let seat = l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        epoch(&l, &seat, T5X, now - min(2000), Some(now - min(260))).await;
        epoch(&l, &seat, T20X, now - min(260), None).await;
        let s = l.today_outlook(None).await.unwrap()["session"].clone();
        assert_eq!(s["samples"], 3, "尾端是重置之後的三筆：{s}");
        assert!(s["planChange"].is_null(), "重置在切點之後，本窗跟換方案無關：{s}");
        assert_eq!(s["awaitingAfterPlanChange"], false);
    }

    /// 21r2 F4（E4）：趨勢頁 walls[] 要剔除伺服器限流、Fable 牆標 fable——拿掉 SQL 裡的剔除這支會失敗。
    #[tokio::test]
    async fn dashboard_walls_drop_server_throttling_and_class_the_rest() {
        let l = crate::test_support::TempLedger::new("e4-walls");
        let now = chrono::Utc::now();
        l.insert_truth_samples(&[mk("session", 10.0, now - min(180), None), mk("session", 12.0, now - min(170), None)])
            .await
            .unwrap();
        let seat = l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        {
            let conn = l.conn.lock().await;
            for (sid, text) in [
                ("a", "You've hit your session limit · resets 3pm"),
                ("b", "You've reached your Fable limit. Switch to another model"),
                ("c", "API Error: Server is temporarily limiting requests (not your usage limit) · Rate limited"),
            ] {
                conn.execute(
                    "INSERT INTO anchors (id, ts, session_id, rate_limit_type, content_text) VALUES (?1, ?2, ?1, NULL, ?3)",
                    params![sid, (now - min(60)).to_rfc3339(), text],
                )
                .unwrap();
            }
        }
        let conn = l.conn.lock().await;
        let dj = Ledger::dashboard_sync(&conn, Some(&seat), 180).unwrap();
        let classes: Vec<String> = dj["walls"].as_array().unwrap().iter().map(|w| w["class"].as_str().unwrap().to_string()).collect();
        assert_eq!(classes.len(), 2, "伺服器限流不是額度牆：{classes:?}");
        assert!(classes.contains(&"five_hour".to_string()) && classes.contains(&"fable".to_string()));
    }

    /// 21r2 F2：窗口詳情的總計——整段都沒牌價是 null、部分沒牌價是下限並帶 unpricedCalls、沒有事件才是 $0。
    #[tokio::test]
    async fn history_window_total_never_turns_unpriced_into_zero() {
        let l = crate::test_support::TempLedger::new("f2-window");
        let now = chrono::Utc::now();
        let mkm = |m: i64| (now - min(m)).to_rfc3339();
        let reset = now + chrono::Duration::hours(1);
        l.insert_truth_samples(&[mk("session", 5.0, now - min(200), Some(reset)), mk("session", 60.0, now - min(100), Some(reset))])
            .await
            .unwrap();
        let seat = l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        let ev = |id: &str, model: &str, m: i64| {
            let conn = l.conn.try_lock().unwrap();
            conn.execute(
                "INSERT INTO usage_events (msg_id, ts, model, entrypoint, input_tokens, output_tokens) \
                 VALUES (?1, ?2, ?3, 'cli', 1000, 1000)",
                params![id, mkm(m), model],
            )
            .unwrap();
        };
        let win = || async { l.history_window(&seat, HistKind::FiveHour, &mkm(200), &mkm(100)).await.unwrap() };
        // 窗內沒有任何事件 → $0
        let d = win().await;
        assert_eq!(d["usage"]["usd"], 0.0);
        assert_eq!(d["usage"]["unpricedCalls"], 0);
        // 只有沒牌價的模型 → null，不是 0
        ev("u1", "claude-nova-1", 150);
        let d = win().await;
        assert!(d["usage"]["usd"].is_null(), "{}", d["usage"]);
        assert_eq!(d["usage"]["unpricedCalls"], 1);
        // 混著有牌價的 → 下限，並說明有幾次沒算
        ev("p1", "claude-sonnet-5", 140);
        let d = win().await;
        assert!(d["usage"]["usd"].as_f64().unwrap() > 0.0);
        assert_eq!(d["usage"]["unpricedCalls"], 1);
    }

    /// 降級（20x→5x）：同樣的用量 % 一下子跳到 4 倍。如果不切，12%→48% 會被短窗斜率吃進去，
    /// 出一個假的「會提早見底」。
    #[tokio::test]
    async fn a_downgrade_step_is_cut_out_of_the_short_window_slope() {
        let l = crate::test_support::TempLedger::new("a11-down");
        let now = chrono::Utc::now();
        let reset = now + min(150);
        let changed = now - min(30);
        l.insert_truth_samples(&[
            mk("session", 10.0, now - min(180), Some(reset)),
            mk("session", 12.0, now - min(60), Some(reset)),
            mk("session", 48.0, now - min(20), Some(reset)),
            mk("session", 50.0, now - min(5), Some(reset)),
        ])
        .await
        .unwrap();
        let seat = l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        epoch(&l, &seat, T20X, now - min(600), Some(changed)).await;
        epoch(&l, &seat, T5X, changed, None).await;
        let s = l.today_outlook(None).await.unwrap()["session"].clone();
        assert_eq!(s["samples"], 2, "切點之前的樣本是另一個池：{s}");
        assert_eq!(s["planChange"]["fromLabel"], "Claude Max 20x");
        assert_eq!(s["planChange"]["toLabel"], "Claude Max 5x");
        assert_eq!(s["insufficient"], true, "剛換方案，重新觀察");
        assert!(s["etaAt"].is_null());
        assert!(s["burnPerHour"].as_f64().unwrap() < 20.0, "沒有把 12→48 的階躍當燃燒速度：{s}");
    }

    /// 升級（5x→20x）：% 掉下去，不能被當成重置——切點之前一律不看；切完空的就是「等新方案第一筆」。
    #[tokio::test]
    async fn an_upgrade_step_is_not_a_reset_and_waits_for_the_first_new_sample() {
        let l = crate::test_support::TempLedger::new("a11-up");
        let now = chrono::Utc::now();
        let reset = now + min(150);
        l.insert_truth_samples(&[
            mk("session", 80.0, now - min(120), Some(reset)),
            mk("session", 90.0, now - min(10), Some(reset)),
        ])
        .await
        .unwrap();
        let seat = l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        epoch(&l, &seat, T5X, now - min(600), Some(now - min(1))).await;
        epoch(&l, &seat, T20X, now - min(1), None).await;
        let s = l.today_outlook(None).await.unwrap()["session"].clone();
        assert_eq!(s["awaitingAfterPlanChange"], true, "{s}");
        assert_eq!(s["expired"], false, "不是「已重置」");
        assert!(s["percent"].is_null());
        assert_eq!(s["lastKnownPercent"], 90.0);
    }

    fn row(p: f64, at: chrono::DateTime<chrono::Utc>) -> SampleRow {
        (p, at.to_rfc3339(), None, "cli-cache".to_string())
    }

    #[test]
    fn history_windows_split_at_a_plan_change_and_are_flagged() {
        let t0 = chrono::DateTime::parse_from_rfc3339("2026-10-01T00:00:00+00:00").unwrap().with_timezone(&chrono::Utc);
        // 5x 的 30%、40% → 升級到 20x 後 % 重算成 8%、10%（階躍下降 >2、往後 30 分鐘也都低）
        let rows = vec![row(30.0, t0), row(40.0, t0 + min(20)), row(8.0, t0 + min(40)), row(10.0, t0 + min(60))];
        let cut = t0 + min(30);
        let plain = segment_windows(&rows, false, HistKind::FiveHour, &[]);
        assert_eq!(plain.len(), 2, "沒有方案資訊時，階躍下降被當成重置");
        assert!(plain.iter().all(|w| !w.plan_change));
        let with = segment_windows(&rows, false, HistKind::FiveHour, &[cut]);
        assert_eq!(with.len(), 2);
        assert!(!with[0].plan_change && with[1].plan_change, "第二個窗是換方案切出來的，不是重置");
        // 降級：% 跳高（沒有下降就不會被當重置，切點也要切開，兩個池的 % 不接成一個窗）
        let up = vec![row(10.0, t0), row(12.0, t0 + min(20)), row(48.0, t0 + min(40)), row(50.0, t0 + min(60))];
        assert_eq!(segment_windows(&up, false, HistKind::FiveHour, &[]).len(), 1);
        let cut_up = segment_windows(&up, false, HistKind::FiveHour, &[cut]);
        assert_eq!(cut_up.len(), 2);
        assert!(cut_up[1].plan_change);
        assert_eq!(cut_up[0].peak, 12.0);
    }

    // ------------------------------------------------------------------ A6／A7

    fn advice(tier: &str, org_type: &str, stats_days: usize, sat: usize, p90: f64, weekly: f64, fable: f64) -> PlanAdvice {
        let e = ep(tier, org_type, chrono::Utc::now() - chrono::Duration::days(60));
        plan_advice(Some(&e), 30, stats_days, sat, Some(p90), weekly, fable, false, false)
    }

    #[test]
    fn upgrade_follows_the_ladder_and_max20x_suggests_another_account() {
        let a = advice(T5X, "claude_max", 14, 5, 80.0, 60.0, 0.0);
        assert_eq!((a.suggestion, a.target_label, a.step_factor), ("upgrade", Some("Claude Max 20x"), Some(4.0)));
        let a = advice("default_claude_ai", "claude_pro", 14, 0, 50.0, 95.0, 0.0);
        assert_eq!((a.suggestion, a.target_label, a.step_factor), ("upgrade", Some("Claude Max 5x"), Some(5.0)));
        let a = advice(T20X, "claude_max", 14, 6, 100.0, 70.0, 0.0);
        assert_eq!(a.suggestion, "add_account", "Max 20x 沒有上一級，不能再說升級");
        assert!(a.reason.contains("多訂一個帳號"), "{}", a.reason);
        // Fable 週週見頂也算（以前被判「剛好」）
        let a = advice(T5X, "claude_max", 14, 0, 20.0, 30.0, 93.0);
        assert_eq!(a.suggestion, "upgrade");
    }

    #[test]
    fn downgrade_threshold_scales_with_the_pool_ratio() {
        // Max 20x→5x 池縮 4 倍：p90／週峰 15% 換算成 60%，可以降；25% 換算成 100%，不行（舊規則 <40% 會推人進牆）
        let a = advice(T20X, "claude_max", 14, 0, 15.0, 15.0, 0.0);
        assert_eq!((a.suggestion, a.target_label, a.step_factor), ("downgrade", Some("Claude Max 5x"), Some(4.0)));
        assert_eq!(a.projected_peak, Some(60.0));
        assert_eq!(advice(T20X, "claude_max", 14, 0, 25.0, 25.0, 0.0).suggestion, "keep");
        assert_eq!(advice(T20X, "claude_max", 14, 0, 39.0, 39.0, 0.0).suggestion, "keep", "舊規則在這裡說「可考慮降級」");
        // Fable 峰值也要換算得過
        assert_eq!(advice(T20X, "claude_max", 14, 0, 10.0, 10.0, 30.0).suggestion, "keep");
        // Max 5x→Pro 池縮 5 倍：15% 換算 75% 可降、17% 換算 85% 不行
        assert_eq!(advice(T5X, "claude_max", 14, 0, 15.0, 15.0, 0.0).suggestion, "downgrade");
        assert_eq!(advice(T5X, "claude_max", 14, 0, 17.0, 17.0, 0.0).suggestion, "keep");
        // Pro 下面沒有一級
        assert_eq!(advice("default_claude_ai", "claude_pro", 14, 0, 1.0, 1.0, 0.0).suggestion, "keep");
    }

    #[test]
    fn team_and_unrecognised_plans_get_no_advice_and_unknown_waits() {
        assert_eq!(advice("default_claude_ai", "claude_team", 14, 9, 100.0, 99.0, 0.0).suggestion, "none");
        assert_eq!(advice("default_claude_max_20x", "claude_enterprise", 14, 9, 100.0, 99.0, 0.0).suggestion, "none");
        assert_eq!(advice("default_claude_max", "claude_max", 14, 9, 100.0, 99.0, 0.0).suggestion, "none");
        let unknown = plan_advice(None, 30, 14, 9, Some(100.0), 99.0, 0.0, false, false);
        assert_eq!(unknown.suggestion, "calibrating");
    }

    /// T-12 沒修乾淨的最少天數：放假兩週回來第一天，5h 峰值 3% 不能直接「可考慮降級」。
    #[test]
    fn needs_five_days_of_samples_in_the_window() {
        let a = advice(T20X, "claude_max", 1, 0, 3.0, 3.0, 0.0);
        assert_eq!(a.suggestion, "calibrating");
        assert!(a.reason.contains("只有 1 天"), "{}", a.reason);
        let e = ep(T5X, "claude_max", chrono::Utc::now() - chrono::Duration::days(2));
        let a = plan_advice(Some(&e), 30, 2, 0, Some(3.0), 3.0, 0.0, true, true);
        assert_eq!(a.suggestion, "calibrating");
        assert!(a.reason.contains("換方案前的記錄是另一個池"), "{}", a.reason);
        // 21r1 nit 5：只是第一次記到方案（沒有上一段）——更早的記錄不是「另一個池」，是「方案未知」
        let first = plan_advice(Some(&e), 30, 2, 0, Some(3.0), 3.0, 0.0, true, false);
        assert!(first.reason.contains("才開始記錄方案") && !first.reason.contains("另一個池"), "{}", first.reason);
        // Free 認得，但沒有階梯；Other 才是「認不得」
        let free = ep("default_claude_free", "claude_free", chrono::Utc::now() - chrono::Duration::days(60));
        let a = plan_advice(Some(&free), 30, 14, 0, Some(3.0), 3.0, 0.0, false, false);
        assert_eq!(a.suggestion, "none");
        assert!(a.reason.contains("沒有可以升降的階梯") && !a.reason.contains("認不得"), "{}", a.reason);
        assert_eq!(advice(T20X, "claude_max", 5, 0, 3.0, 3.0, 0.0).suggestion, "downgrade", "滿 5 天就給");
    }

    /// 統計窗不跨方案段：升級前的 95% 峰值留在窗內就會在升級後還喊「考慮升級」。
    #[tokio::test]
    async fn plan_stats_window_starts_at_the_current_plan() {
        let l = crate::test_support::TempLedger::new("a6-window");
        let now = chrono::Utc::now();
        let mut s = Vec::new();
        // 前 10 天 5x 時期天天 95%（用滿），最近 6 天 20x 時期每天 15%
        for d in 0..14i64 {
            let pct = if d >= 6 { 95.0 } else { 15.0 };
            s.push(mk("session", pct, now - chrono::Duration::days(d) - min(30), None));
            s.push(mk("weekly_all", pct / 3.0, now - chrono::Duration::days(d) - min(30), None));
        }
        l.insert_truth_samples(&s).await.unwrap();
        let seat = l.mark_current_seat(Some("A"), Some("org-A"), None, None).await.unwrap();
        epoch(&l, &seat, T5X, now - chrono::Duration::days(20), Some(now - chrono::Duration::days(6) + min(60))).await;
        epoch(&l, &seat, T20X, now - chrono::Duration::days(6) + min(60), None).await;
        let p = l.today_outlook(None).await.unwrap()["plan"].clone();
        assert_eq!(p["limitedByPlan"], true, "{p}");
        assert_eq!(p["planLabel"], "Claude Max 20x");
        assert_ne!(p["suggestion"], "upgrade", "5x 時期的 95% 不能算進 20x 的建議：{p}");
        assert!(p["statsDays"].as_u64().unwrap() <= 6, "{p}");
    }

    // ------------------------------------------------------------------ A8

    #[tokio::test]
    async fn snapshot_and_outlook_carry_the_viewed_seats_own_plan() {
        let l = crate::test_support::TempLedger::new("a8-plan");
        let now = chrono::Utc::now();
        l.insert_truth_samples(&[mk("session", 10.0, now - min(5), None)]).await.unwrap();
        let seat = l
            .mark_current_seat_with_plan(
                Some("A"),
                Some("org-A"),
                None,
                None,
                Some(&PlanObservation { tier: Some(T20X.into()), org_type: Some("claude_max".into()), seat_tier: None }),
            )
            .await
            .unwrap()
            .seat_id;
        let snap = l.seat_snapshot(&seat).await.unwrap();
        assert_eq!(snap["planName"], "Claude Max 20x");
        assert_eq!(snap["plan"]["key"], "max_20x");
        assert!(snap["plan"]["observedAt"].is_string());
        // 另一個從沒被觀測過方案的座位：null，不退回別人的
        l.insert_truth_samples(&[TruthSample {
            account_uuid: Some("B".into()),
            org_uuid: Some("org-B".into()),
            ..mk("session", 5.0, now - min(5), None)
        }])
        .await
        .unwrap();
        let other = l.list_seats().await.unwrap();
        let b = other["seats"].as_array().unwrap().iter().find(|s| s["accountUuid"] == "B").unwrap().clone();
        assert_eq!(b["plan"]["known"], false);
        let snap_b = l.seat_snapshot(b["id"].as_str().unwrap()).await.unwrap();
        assert!(snap_b["planName"].is_null());
        let o = l.today_outlook(Some(&seat)).await.unwrap();
        assert_eq!(o["seatPlan"]["label"], "Claude Max 20x");
    }

    // ------------------------------------------------------------------ E4

    #[test]
    fn null_typed_walls_are_classified_by_their_text() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE anchors (id TEXT PRIMARY KEY, ts TEXT NOT NULL, session_id TEXT, rate_limit_type TEXT, \
             resets_at TEXT, overage_status TEXT, overage_disabled_reason TEXT, content_text TEXT, raw_json TEXT);",
        )
        .unwrap();
        let cases: [(&str, Option<&str>, Option<&str>, &str); 7] = [
            ("typed", Some("five_hour"), Some("You've hit your session limit · resets 3pm"), "five_hour"),
            ("session", None, Some("You've hit your session limit · resets 4:50am (Asia/Taipei)"), "five_hour"),
            ("fable5", None, Some("You've reached your Fable 5 limit. Switch to another model, or manage usage credits"), "fable"),
            ("fable", None, Some("You've reached your Fable limit. Switch to another model"), "fable"),
            ("credits", None, Some("You're out of usage credits. Run /usage-credits to keep using Fable 5 or /model"), "five_hour"),
            ("overload", None, Some("API Error: Server is temporarily limiting requests (not your usage limit) · Rate limited"), "server"),
            ("blank", None, None, "five_hour"),
        ];
        for (id, t, text, _) in cases {
            conn.execute(
                "INSERT INTO anchors (id, ts, session_id, rate_limit_type, content_text) VALUES (?1, '2026-10-06T11:00:00Z', ?1, ?2, ?3)",
                params![id, t, text],
            )
            .unwrap();
        }
        let ids = |kind: HistKind| -> Vec<String> {
            let mut st = conn.prepare(&format!("SELECT id FROM anchors WHERE {} ORDER BY id", kind.anchor_sql())).unwrap();
            st.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()).collect()
        };
        assert_eq!(ids(HistKind::FiveHour), ["blank", "credits", "session", "typed"], "Fable 牆與伺服器限流都不在 5h 頁");
        assert_eq!(ids(HistKind::Fable), ["fable", "fable5"]);
        // Rust 版分類（趨勢頁 walls[].class）要跟 SQL 一致
        for (id, t, text, class) in cases {
            let want = if class == "server" { "five_hour" } else { class }; // 伺服器限流由查詢剔除，分類函式不管
            assert_eq!(wall_class(t, text), want, "{id}");
        }
    }
}

/// v1.9.1：worktree 裡的對話歸回主專案——用真的 SQLite 跑 PROJECT_KEY_SQL。
#[cfg(test)]
mod project_key_tests {
    #[test]
    fn worktree_dirs_fold_into_their_project() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_events (project_dir TEXT, output_tokens INTEGER);
             INSERT INTO usage_events VALUES
               ('C--dev-NextStop', 10),
               ('C--dev-NextStop--claude-worktrees-funny-swartz-5045f1', 20),
               ('C--dev-NextStop--claude-worktrees-v1-1-6', 30),
               ('C--dev-Media-OmniAPI--claude-worktrees-generate-wall-design', 5),
               ('C--Users-User--claude', 1),
               (NULL, 2);",
        )
        .unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT COALESCE({},'?'), SUM(output_tokens) FROM usage_events GROUP BY 1 ORDER BY 1",
                super::PROJECT_KEY_SQL
            ))
            .unwrap();
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            rows,
            vec![
                ("?".to_string(), 2),
                ("C--Users-User--claude".to_string(), 1),
                ("C--dev-Media-OmniAPI".to_string(), 5),
                ("C--dev-NextStop".to_string(), 60),
            ]
        );
    }
}
