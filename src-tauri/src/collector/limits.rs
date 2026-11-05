//! v1.9.5（D100 E1／E2／F1）：額度種類。
//!
//! 資料層本來就是開放的（`limits[]` 逐條入庫），消費層卻寫死三條（5h／週·全模型／週·Fable）。
//! 這個檔案管「伺服器現在說有哪些額度」：
//! - **認得的三條**與 Fable 的 scope 判斷（以前散在 collector／ledger 各處，收到這裡）；
//!   「存在嗎」跟「畫哪一筆」用同一個判斷 [`KnownLimit::of`]（審查 32r S2）；
//! - **簽名**（E1）：`limits[]` 的（kind, scope）集合、`weekly_scoped_shares` 的 allowance、週窗長、
//!   extra_usage——跟上一份比，有變就寫 log `limits-changed`；不認得的額度只要還在清單裡就出 notice；
//! - **「此方案沒有這條」**（E2）：認得的三條連續 [`ABSENT_AFTER`] 次、而且跨過 [`MIN_ABSENT_SPAN_MS`]
//!   沒出現在新鮮的快取裡。清單缺／空、或三條**同時**不見＝格式漂移，不算（審查 32r M1）；
//!   同一家族多了一條認不得的＝可能改名，先不算（32r S1）；
//! - **Extra usage**（F1）：`is_enabled && !spend_limit_reached`，再保守地要求沒有 `disabled_reason`、
//!   沒被使用者關掉（32r S5）。
//!
//! 只吃「帳號對得上、`fetchedAtMs` 是新的」那份 CLI 快取（T-06：快取帳號 ≠ 登入帳號時整批作廢）。
//! 狀態整包存在 `channel_state[STATE_KEY]`（一個 JSON 鍵）。判斷全是純函式，測試直接打。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::types::{ExtraUsageInfo, UnknownLimit};

/// 週·Fable 那條的 scope（`scope.model.display_name`）。
pub const FABLE_SCOPE: &str = "Fable";
/// SQL 比對用（`scope LIKE ?`）：以 Fable 開頭都算（「Fable 5」這種改名照樣接得上，32r S1）。
/// SQLite 的 LIKE 對 ASCII 不分大小寫，跟 [`is_fable_scope`] 同一個口徑。
pub const FABLE_SCOPE_LIKE: &str = "Fable%";

/// scope 是 Fable 嗎：不分大小寫、以「fable」開頭（記憶體端；SQL 端用 [`FABLE_SCOPE_LIKE`]）。
pub fn is_fable_scope(scope: Option<&str>) -> bool {
    // .get：前 5 個位元組不是字元邊界（例如「Fablé」）就不算，不會 panic
    norm(scope).is_some_and(|s| s.get(..FABLE_SCOPE.len()).is_some_and(|p| p.eq_ignore_ascii_case(FABLE_SCOPE)))
}

/// 這一筆是「週·Fable」那條嗎。
pub fn is_fable_limit(kind: &str, scope: Option<&str>) -> bool {
    kind == "weekly_scoped" && is_fable_scope(scope)
}

/// 空字串的 scope 當成沒有（32r N2：`Some("")` 不能讓 5h 變成認不得的）。
fn norm(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// 連續這麼多次（新鮮的快取）沒出現 → 「此方案沒有這條」。跟 E3「連續 3 次一致才採用」同一把尺：
/// 一次沒出現可能只是 API 偶發。
pub const ABSENT_AFTER: u32 = 3;
/// 而且第一次缺席到最近一份至少隔這麼久（32r N6：主人手動連跑 3 次 /usage 也會生出 3 份新快取，
/// 伺服器短暫回殘缺清單不該因此判死）。
pub const MIN_ABSENT_SPAN_MS: i64 = 15 * 60 * 1000;
/// 週窗名目長度與容差（小時）。
pub const WEEK_HOURS: f64 = 168.0;
pub const WEEK_TOLERANCE_HOURS: f64 = 1.0;
/// 週窗長要連續這麼多次偏離才出 notice（重置那一刻兩個欄位可能不同步，一次不算）。
const WEEK_OFF_AFTER: u32 = 2;
/// channel_state 的鍵。
pub const STATE_KEY: &str = "cli_limits_state";
/// 提示句的共同結尾。
const TRUST_CLAUDE_AI: &str = "——數字以 claude.ai 的用量頁為準。";

/// App 認得、畫得出來的三條。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownLimit {
    Session,
    WeeklyAll,
    Fable,
}

impl KnownLimit {
    pub const ALL: [KnownLimit; 3] = [KnownLimit::Session, KnownLimit::WeeklyAll, KnownLimit::Fable];

    /// （kind, scope.model）→ 認得的哪一條。**存在判定與顯示選樣本共用這一個**（32r S2）；
    /// surface 兩邊都不看（顯示端的樣本本來就沒有 surface——都看會把現有數字清掉）。
    pub fn of(kind: &str, model: Option<&str>) -> Option<Self> {
        let model = norm(model);
        match kind {
            "session" | "five_hour" if model.is_none() => Some(KnownLimit::Session),
            "weekly_all" | "seven_day" if model.is_none() => Some(KnownLimit::WeeklyAll),
            "weekly_scoped" if is_fable_scope(model) => Some(KnownLimit::Fable),
            _ => None,
        }
    }

    /// 同一家族（改名候選）：同一種 kind、scope 不同。
    fn family(self, kind: &str) -> bool {
        match self {
            KnownLimit::Session => matches!(kind, "session" | "five_hour"),
            KnownLimit::WeeklyAll => matches!(kind, "weekly_all" | "seven_day"),
            KnownLimit::Fable => kind == "weekly_scoped",
        }
    }

    /// 計數表的鍵（存檔用，別改）。
    pub fn key(self) -> &'static str {
        match self {
            KnownLimit::Session => "session",
            KnownLimit::WeeklyAll => "weekly_all",
            KnownLimit::Fable => "fable",
        }
    }

    /// 畫面上的名字。
    pub fn label(self) -> &'static str {
        match self {
            KnownLimit::Session => "5 小時額度",
            KnownLimit::WeeklyAll => "週 · 全模型",
            KnownLimit::Fable => "週 · Fable",
        }
    }
}

/// kind 的白話（32r N3：notice 不露 `weekly_scoped` 這種內部名稱）。
fn kind_label(kind: &str) -> String {
    let k = kind.to_ascii_lowercase();
    if k.starts_with("session") || k.starts_with("five_hour") {
        "5 小時額度".into()
    } else if k.starts_with("weekly") || k.starts_with("seven_day") {
        "週額度".into()
    } else if k.contains("month") {
        "月額度".into()
    } else if k.contains("daily") || k.starts_with("day") {
        "每日額度".into()
    } else {
        format!("額度「{kind}」")
    }
}

/// `limits[]` 的一條（只留判斷要用的欄位）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitEntry {
    pub kind: String,
    /// `scope.model.display_name`
    #[serde(default)]
    pub model: Option<String>,
    /// `scope.surface`（字串，或物件的 display_name／name／id）。只進簽名與顯示字，不參與「認不認得」。
    #[serde(default)]
    pub surface: Option<String>,
    #[serde(default)]
    pub percent: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<String>,
}

impl LimitEntry {
    /// scope 的顯示字：「Fable」「Opus · claude_code」；沒有 scope 是 None。
    pub fn scope_label(&self) -> Option<String> {
        match (norm(self.model.as_deref()), norm(self.surface.as_deref())) {
            (Some(m), Some(s)) => Some(format!("{m} · {s}")),
            (Some(m), None) => Some(m.to_string()),
            (None, Some(s)) => Some(s.to_string()),
            (None, None) => None,
        }
    }
    /// 簽名用的鍵：「kind」或「kind／scope」。
    pub fn key(&self) -> String {
        match self.scope_label() {
            Some(s) => format!("{}／{s}", self.kind),
            None => self.kind.clone(),
        }
    }
    pub fn known(&self) -> Option<KnownLimit> {
        KnownLimit::of(&self.kind, self.model.as_deref())
    }
    /// 白話名：「週額度 · Opus」。
    fn label(&self) -> String {
        match self.scope_label() {
            Some(s) => format!("{} · {s}", kind_label(&self.kind)),
            None => kind_label(&self.kind),
        }
    }
    fn label_with_percent(&self) -> String {
        match self.percent {
            Some(p) => format!("{}（目前 {p:.0}%）", self.label()),
            None => self.label(),
        }
    }
}

/// 一份快取讀出來的額度種類（E1 簽名的原料＋F1）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LimitsReading {
    /// 空＝`limits` 缺、空、或不是陣列——這份對「存不存在」沒有資訊（32r M1）。
    pub entries: Vec<LimitEntry>,
    /// `weekly_scoped_shares[].allowance_percent_of_weekly`，鍵＝`limits[limit_index]` 的 [`LimitEntry::key`]。
    pub allowances: BTreeMap<String, f64>,
    /// 週窗長（小時）：`seven_day_breakdown.window_started_at` → `seven_day.resets_at`。
    pub week_hours: Option<f64>,
    pub extra_usage: Option<ExtraUsageInfo>,
}

fn surface_of(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        serde_json::Value::Object(o) => ["display_name", "name", "id"]
            .iter()
            .find_map(|k| o.get(*k).and_then(|x| x.as_str()).filter(|s| !s.trim().is_empty()).map(str::to_string)),
        _ => None,
    }
}

fn parse_ts(v: Option<&serde_json::Value>) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    v.and_then(|x| x.as_str()).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
}

/// F1：超額付費成立嗎。D100 的字面是 `is_enabled && !spend_limit_reached`；32r S5 再保守一點——
/// 伺服器同時寫了停用原因（例如 `out_of_credits`）或使用者自己關掉，都不算（U5 沒驗，寧可少說）。
fn extra_usage_of(x: &serde_json::Value) -> ExtraUsageInfo {
    let flag = |k: &str| x.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let enabled = flag("is_enabled");
    let spend_limit_reached = flag("spend_limit_reached");
    let disabled_reason = x
        .get("disabled_reason")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    ExtraUsageInfo {
        enabled,
        // Q7：不用 oauthAccount.hasExtraUsageEnabled（主人那個旗標是 true，但 429 全是 overage rejected）
        active: enabled && !spend_limit_reached && disabled_reason.is_none() && !flag("user_disabled"),
        spend_limit_reached,
        disabled_reason,
    }
}

/// 從 `cachedUsageUtilization.utilization` 解析。缺欄位＝資料，不是錯誤。
pub fn parse_reading(util: &serde_json::Value) -> LimitsReading {
    let raw: &[serde_json::Value] = util.get("limits").and_then(|v| v.as_array()).map(Vec::as_slice).unwrap_or(&[]);
    // 跟 limits[] 對齊（limit_index 指的是原陣列的位置），沒有 kind 的位置留 None
    let aligned: Vec<Option<LimitEntry>> = raw
        .iter()
        .map(|e| {
            let kind = e.get("kind").and_then(|v| v.as_str()).filter(|k| !k.trim().is_empty())?.to_string();
            Some(LimitEntry {
                kind,
                model: norm(e.pointer("/scope/model/display_name").and_then(|v| v.as_str())).map(str::to_string),
                surface: e.pointer("/scope/surface").and_then(surface_of),
                percent: e.get("percent").and_then(|v| v.as_f64()),
                resets_at: e.get("resets_at").and_then(|v| v.as_str()).map(str::to_string),
            })
        })
        .collect();
    let mut allowances = BTreeMap::new();
    for share in util.get("weekly_scoped_shares").and_then(|v| v.as_array()).map(Vec::as_slice).unwrap_or(&[]) {
        let idx = share.get("limit_index").and_then(|v| v.as_u64());
        let allowance = share.get("allowance_percent_of_weekly").and_then(|v| v.as_f64());
        if let (Some(i), Some(a)) = (idx, allowance) {
            if let Some(Some(e)) = aligned.get(i as usize) {
                allowances.insert(e.key(), a);
            }
        }
    }
    let entries: Vec<LimitEntry> = aligned.into_iter().flatten().collect();
    let week_end = parse_ts(util.pointer("/seven_day/resets_at")).or_else(|| {
        entries
            .iter()
            .find(|e| e.known() == Some(KnownLimit::WeeklyAll))
            .and_then(|e| e.resets_at.as_deref())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    });
    let week_start = parse_ts(util.pointer("/seven_day_breakdown/window_started_at"));
    let week_hours = match (week_start, week_end) {
        (Some(s), Some(e)) => Some(((e - s).num_seconds() as f64 / 3600.0 * 100.0).round() / 100.0),
        _ => None,
    }
    // 32r N1：起點晚於終點（零或負的窗長）是讀錯了，不是窗長變了
    .filter(|h| *h > 0.0);
    let extra_usage = util.get("extra_usage").filter(|v| v.is_object()).map(extra_usage_of);
    LimitsReading { entries, allowances, week_hours, extra_usage }
}

/// 簽名的一處變化（寫 log 用）。
#[derive(Debug, Clone, PartialEq)]
pub enum LimitChange {
    Added(String),
    Removed(String),
    Renamed { from: String, to: String },
    Allowance { key: String, from: Option<f64>, to: Option<f64> },
    WeekLength { from: f64, to: f64 },
    /// 32r N8：extra_usage 翻了（之後驗 U5 要靠這行 log）。
    ExtraUsage { from: Option<ExtraUsageInfo>, to: Option<ExtraUsageInfo> },
}

impl LimitChange {
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            LimitChange::Added(k) => json!({ "change": "added", "limit": k }),
            LimitChange::Removed(k) => json!({ "change": "removed", "limit": k }),
            LimitChange::Renamed { from, to } => json!({ "change": "renamed", "from": from, "to": to }),
            LimitChange::Allowance { key, from, to } => json!({ "change": "allowance", "limit": key, "from": from, "to": to }),
            LimitChange::WeekLength { from, to } => json!({ "change": "week-length", "fromHours": from, "toHours": to }),
            LimitChange::ExtraUsage { from, to } => json!({ "change": "extra-usage", "from": from, "to": to }),
        }
    }
}

/// E1 簽名比對：（kind, scope）集合的增減（一增一減＝改名）、allowance、週窗長（差超過 1 分鐘）、extra_usage。
pub fn diff(prev: &LimitsReading, next: &LimitsReading) -> Vec<LimitChange> {
    let keys = |r: &LimitsReading| r.entries.iter().map(LimitEntry::key).collect::<std::collections::BTreeSet<_>>();
    let (a, b) = (keys(prev), keys(next));
    let removed: Vec<String> = a.difference(&b).cloned().collect();
    let added: Vec<String> = b.difference(&a).cloned().collect();
    let mut out = Vec::new();
    if removed.len() == 1 && added.len() == 1 {
        out.push(LimitChange::Renamed { from: removed[0].clone(), to: added[0].clone() });
    } else {
        out.extend(added.into_iter().map(LimitChange::Added));
        out.extend(removed.into_iter().map(LimitChange::Removed));
    }
    let all: std::collections::BTreeSet<&String> = prev.allowances.keys().chain(next.allowances.keys()).collect();
    for k in all {
        let (f, t) = (prev.allowances.get(k).copied(), next.allowances.get(k).copied());
        if f != t {
            out.push(LimitChange::Allowance { key: k.clone(), from: f, to: t });
        }
    }
    if let (Some(f), Some(t)) = (prev.week_hours, next.week_hours) {
        if (f - t).abs() > 1.0 / 60.0 {
            out.push(LimitChange::WeekLength { from: f, to: t });
        }
    }
    if prev.extra_usage != next.extra_usage {
        out.push(LimitChange::ExtraUsage { from: prev.extra_usage.clone(), to: next.extra_usage.clone() });
    }
    out
}

/// 存在 `channel_state` 的整包狀態（按座位；換座位重來）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LimitsState {
    /// [`seat_key`]：`帳號uuid|組織uuid`。
    pub seat: String,
    /// 最後採用的那份快取的 `fetchedAtMs`。
    pub fetched_ms: i64,
    /// 最後一份**讀得懂**的清單（格式漂移那幾份不覆蓋 entries／allowances，只更新窗長與 extra_usage）。
    pub reading: LimitsReading,
    /// 認得的三條各自連續幾次（新鮮的快取）沒出現，鍵＝[`KnownLimit::key`]。
    pub misses: BTreeMap<String, u32>,
    /// 認得的三條各自「這一串缺席」的第一份快取的 `fetchedAtMs`。
    pub first_miss_ms: BTreeMap<String, i64>,
    /// 週窗長連續幾次偏離 168 小時超過 1 小時。
    pub week_off: u32,
    /// 最近一份是格式漂移：`limits` 缺／空，或認得的三條同時不見（32r M1）。這時不判「此方案沒有」。
    pub drift: bool,
}

pub fn seat_key(account: &str, org: Option<&str>) -> String {
    format!("{account}|{}", org.unwrap_or(""))
}

/// 這一輪採用的結果。
#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    pub state: LimitsState,
    /// 跟同一個座位的上一份比出來的變化（`baseline` 時一律空）。
    pub changes: Vec<LimitChange>,
    /// 第一次記錄、或換了座位：這份當基準，不比。
    pub baseline: bool,
    /// 這一份是新的（`fetchedAtMs` 前進了），狀態有更新、要存檔。
    pub updated: bool,
    /// 這一份起進入格式漂移（上一份不是）。
    pub drift_started: bool,
    /// 這一份起判成「此方案沒有」的。
    pub newly_not_applicable: Vec<KnownLimit>,
}

/// 吃一份帳號對得上的快取。`fetchedAtMs` 沒有前進＝同一份（或更舊的），不重算、不計次。
pub fn observe(prev: Option<&LimitsState>, seat: &str, fetched_ms: i64, reading: LimitsReading) -> Observed {
    let same_seat = prev.filter(|p| p.seat == seat);
    if let Some(p) = same_seat {
        if fetched_ms <= p.fetched_ms {
            return Observed {
                state: p.clone(),
                changes: Vec::new(),
                baseline: false,
                updated: false,
                drift_started: false,
                newly_not_applicable: Vec::new(),
            };
        }
    }
    // 32r M1：清單缺／空＝沒有資訊；三條同時不見＝格式漂移。兩種都不加不減。
    let listed = !reading.entries.is_empty();
    let present = |r: &LimitsReading, k: KnownLimit| r.entries.iter().any(|e| e.known() == Some(k));
    let drift = !listed || KnownLimit::ALL.iter().all(|k| !present(&reading, *k));
    // 讀不懂的那份不覆蓋上一份的清單（簽名比對與「還在清單裡」的提示都看最後一份讀得懂的）
    let reading = if listed {
        reading
    } else {
        let base = same_seat.map(|p| p.reading.clone()).unwrap_or_default();
        LimitsReading { entries: base.entries, allowances: base.allowances, ..reading }
    };
    let mut misses = BTreeMap::new();
    let mut first_miss_ms = BTreeMap::new();
    for k in KnownLimit::ALL {
        let before = same_seat.and_then(|p| p.misses.get(k.key()).copied()).unwrap_or(0);
        let first = same_seat.and_then(|p| p.first_miss_ms.get(k.key()).copied());
        let (n, f) = if drift || (!present(&reading, k) && rename_candidate(&reading, k).is_some()) {
            // 漂移、或同一家族多了一條認不得的（可能改名，32r S1）：先不算，維持原狀
            (before, first)
        } else if present(&reading, k) {
            (0, None)
        } else {
            (before + 1, first.or(Some(fetched_ms)))
        };
        misses.insert(k.key().to_string(), n);
        if let Some(f) = f {
            first_miss_ms.insert(k.key().to_string(), f);
        }
    }
    let off = reading.week_hours.is_some_and(|h| (h - WEEK_HOURS).abs() > WEEK_TOLERANCE_HOURS);
    let week_off = if off { same_seat.map(|p| p.week_off).unwrap_or(0) + 1 } else { 0 };
    let changes = same_seat.map(|p| diff(&p.reading, &reading)).unwrap_or_default();
    let state = LimitsState { seat: seat.to_string(), fetched_ms, reading, misses, first_miss_ms, week_off, drift };
    let was = |f: &dyn Fn(&LimitsState) -> bool| same_seat.is_some_and(f);
    let drift_started = state.drift && !was(&|p| p.drift);
    let newly_not_applicable = KnownLimit::ALL
        .into_iter()
        .filter(|k| state.not_applicable(*k) && !was(&|p| p.not_applicable(*k)))
        .collect();
    Observed { state, changes, baseline: same_seat.is_none(), updated: true, drift_started, newly_not_applicable }
}

/// k 不在清單裡、但同一家族有一條認不得的——八成是 k 改了名字（32r S1）。
fn rename_candidate(r: &LimitsReading, k: KnownLimit) -> Option<&LimitEntry> {
    if r.entries.iter().any(|e| e.known() == Some(k)) {
        return None;
    }
    r.entries.iter().find(|e| e.known().is_none() && k.family(&e.kind))
}

/// 一輪的黏合結果（`collector::observe_limits` 照做：存檔、寫 log、回傳狀態）。純函式，測試直接打。
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// 目前登入座位的狀態（別的座位的舊狀態不回傳）。
    pub state: Option<LimitsState>,
    pub save: bool,
    pub logs: Vec<(&'static str, serde_json::Value)>,
}

/// `fresh`＝這一輪可以吃的快取（帳號對得上、不是切帳號拖尾）；None＝這輪沒得吃，沿用存著的。
pub fn step(prev: Option<LimitsState>, seat: &str, fresh: Option<(LimitsReading, i64)>) -> Step {
    let Some((reading, fetched_ms)) = fresh else {
        return Step { state: prev.filter(|p| p.seat == seat), save: false, logs: Vec::new() };
    };
    let obs = observe(prev.as_ref(), seat, fetched_ms, reading);
    if !obs.updated {
        return Step { state: Some(obs.state), save: false, logs: Vec::new() };
    }
    let st = &obs.state;
    let mut logs = Vec::new();
    if obs.baseline {
        logs.push(("limits-seen", st.summary_json()));
    } else if !obs.changes.is_empty() {
        logs.push((
            "limits-changed",
            serde_json::json!({
                "changes": obs.changes.iter().map(LimitChange::to_json).collect::<Vec<_>>(),
                "now": st.summary_json(),
            }),
        ));
    }
    if obs.drift_started {
        logs.push(("limits-format-drift", st.summary_json()));
    }
    if !obs.newly_not_applicable.is_empty() {
        logs.push((
            "limits-not-applicable",
            serde_json::json!({ "limits": obs.newly_not_applicable.iter().map(|k| k.key()).collect::<Vec<_>>() }),
        ));
    }
    Step { state: Some(obs.state), save: true, logs }
}

impl LimitsState {
    fn absent_long_enough(&self, k: KnownLimit) -> bool {
        self.misses.get(k.key()).is_some_and(|n| *n >= ABSENT_AFTER)
            && self
                .first_miss_ms
                .get(k.key())
                .is_some_and(|f| self.fetched_ms - f >= MIN_ABSENT_SPAN_MS)
    }

    /// E2：這條在此方案不存在（連續 [`ABSENT_AFTER`] 次、跨過 15 分鐘沒出現）。格式漂移時一律 false；
    /// 三條同時成立也當格式漂移（真的方案不會連 5h 都沒有，32r M1 第二道）。
    pub fn not_applicable(&self, k: KnownLimit) -> bool {
        !self.drift
            && self.absent_long_enough(k)
            && !KnownLimit::ALL.iter().all(|x| self.absent_long_enough(*x))
    }

    /// E1：還在清單裡、但 App 不認得的額度（含改名候選——對 App 來說一樣是認不得）。
    pub fn unknown_limits(&self) -> Vec<UnknownLimit> {
        self.reading
            .entries
            .iter()
            .filter(|e| e.known().is_none())
            .map(|e| UnknownLimit {
                kind: e.kind.clone(),
                scope: e.scope_label(),
                percent: e.percent,
                resets_at: e.resets_at.clone(),
            })
            .collect()
    }

    /// 週窗長偏離（連續 2 次以上才算）→ 這一窗的小時數。
    pub fn week_hours_off(&self) -> Option<f64> {
        self.reading.week_hours.filter(|_| self.week_off >= WEEK_OFF_AFTER)
    }

    /// 畫面上那句提示（格式漂移／可能改名／不認得的額度＋週窗長偏離）；都沒有＝None。
    pub fn notice(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if self.drift {
            parts.push("額度資料的格式看起來變了，App 暫時讀不懂".into());
        } else {
            let mut claimed: Vec<&LimitEntry> = Vec::new();
            for k in KnownLimit::ALL {
                if let Some(e) = rename_candidate(&self.reading, k) {
                    claimed.push(e);
                    parts.push(format!(
                        "「{}」這條在伺服器的清單上可能改了名字，現在叫「{}」，App 還認不得",
                        k.label(),
                        e.label_with_percent()
                    ));
                }
            }
            let rest: Vec<&LimitEntry> = self
                .reading
                .entries
                .iter()
                .filter(|e| e.known().is_none() && !claimed.iter().any(|c| std::ptr::eq(*c, *e)))
                .collect();
            if !rest.is_empty() {
                let list = rest.iter().map(|e| e.label_with_percent()).collect::<Vec<_>>().join("、");
                let n = if rest.len() == 1 { "一".to_string() } else { rest.len().to_string() };
                parts.push(format!("伺服器有{n}條 App 還不會畫的額度：{list}"));
            }
        }
        if let Some(h) = self.week_hours_off() {
            parts.push(format!("週額度這一窗是 {h:.0} 小時（原本 168 小時），App 的配速與預測還照 7 天算，可能不準"));
        }
        (!parts.is_empty()).then(|| format!("{}{TRUST_CLAUDE_AI}", parts.join("；")))
    }

    /// 給 log 看的簽名摘要。
    pub fn summary_json(&self) -> serde_json::Value {
        serde_json::json!({
            "limits": self.reading.entries.iter().map(LimitEntry::key).collect::<Vec<_>>(),
            "allowances": self.reading.allowances,
            "weekHours": self.reading.week_hours,
            "misses": self.misses,
            "drift": self.drift,
            "extraUsage": self.reading.extra_usage,
        })
    }
}

/// E2：今日頁／MCP 的一條額度（`limit_outlook_sync` 的輸出）掛上 `notApplicable`。
/// v1.9.5 審查（32r S3）：拿掉「有更新的樣本就不掛」的例外——探針樣本蓋的是探針**開始**時刻，同一輪
/// 重讀的快取一定更晚，那個例外在正常路徑永遠不成立，只會給人「探針值能救場」的錯覺。誤判改靠
/// 格式漂移與三條同時缺席兩道擋（見 [`LimitsState::not_applicable`]）。
pub fn apply_to_outlook(o: &mut serde_json::Value, st: &LimitsState, k: KnownLimit) {
    if st.not_applicable(k) {
        o["notApplicable"] = serde_json::json!(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 主人 2026-10-11 的快取（只留判斷用的欄位）。
    fn owner_util() -> serde_json::Value {
        json!({
            "seven_day": { "utilization": 23, "resets_at": "2026-10-14T08:00:00.008402+00:00" },
            "limits": [
                { "kind": "session", "group": "session", "percent": 3, "resets_at": "2026-10-11T09:00:00.008381+00:00", "scope": null, "is_active": false },
                { "kind": "weekly_all", "group": "weekly", "percent": 23, "resets_at": "2026-10-14T08:00:00.008402+00:00", "scope": null, "is_active": false },
                { "kind": "weekly_scoped", "group": "weekly", "percent": 24, "resets_at": "2026-10-14T08:00:00.008562+00:00",
                  "scope": { "model": { "id": null, "display_name": "Fable" }, "surface": null }, "is_active": true }
            ],
            "extra_usage": { "is_enabled": false, "monthly_limit": 500, "used_credits": 0, "utilization": 0, "currency": "USD",
                             "disabled_reason": "out_of_credits", "user_disabled": false, "spend_limit_reached": false, "credits_ever_enabled": true },
            "weekly_scoped_shares": [{ "limit_index": 2, "used_percent_of_weekly": 12, "allowance_percent_of_weekly": 50 }],
            "seven_day_breakdown": { "as_of": "2026-10-11T04:51:39.031476+00:00", "window_started_at": "2026-10-07T08:00:00.008402+00:00", "rows": [] },
            "tangelo": null, "iguana_necktie": null
        })
    }

    /// 分鐘 → 毫秒（fetchedAtMs 用）。
    fn m(n: i64) -> i64 {
        n * 60_000
    }

    #[test]
    fn parses_the_owner_cache() {
        let r = parse_reading(&owner_util());
        assert_eq!(r.entries.iter().map(LimitEntry::key).collect::<Vec<_>>(), ["session", "weekly_all", "weekly_scoped／Fable"]);
        assert!(r.entries.iter().all(|e| e.known().is_some()), "三條都認得");
        assert_eq!(r.allowances.get("weekly_scoped／Fable"), Some(&50.0));
        assert_eq!(r.week_hours, Some(168.0));
        // F1：主人現況＝不成立（is_enabled=false），代號鍵全 null 不當額度
        let x = r.extra_usage.unwrap();
        assert!(!x.enabled && !x.active && !x.spend_limit_reached);
        assert_eq!(x.disabled_reason.as_deref(), Some("out_of_credits"));
        let st = observe(None, "a|o", 1, parse_reading(&owner_util())).state;
        assert!(st.unknown_limits().is_empty());
        assert_eq!(st.notice(), None);
        assert!(!st.drift);
    }

    /// 32r S5：主人現況不成立；`is_enabled=true` 但伺服器同時寫了停用原因（out_of_credits）也不成立；
    /// 使用者自己關掉不成立；花到上限不成立；乾淨地開著才成立。
    #[test]
    fn extra_usage_active_is_conservative() {
        let mk = |enabled: bool, reached: bool, reason: Option<&str>, user_off: bool| {
            let mut u = owner_util();
            u["extra_usage"]["is_enabled"] = json!(enabled);
            u["extra_usage"]["spend_limit_reached"] = json!(reached);
            u["extra_usage"]["disabled_reason"] = json!(reason);
            u["extra_usage"]["user_disabled"] = json!(user_off);
            parse_reading(&u).extra_usage.unwrap()
        };
        assert!(!mk(false, false, Some("out_of_credits"), false).active, "主人現況");
        assert!(!mk(true, false, Some("out_of_credits"), false).active, "開著但寫了停用原因");
        assert!(!mk(true, false, None, true).active, "使用者自己關掉");
        assert!(!mk(true, true, None, false).active, "花到上限＝不再超額付費");
        assert!(mk(true, false, None, false).active);
        assert!(mk(true, false, Some(""), false).active, "空字串的原因當沒有");
        let mut u = owner_util();
        u["extra_usage"] = json!(null);
        assert_eq!(parse_reading(&u).extra_usage, None);
    }

    /// S2：存在判定不看 surface（跟顯示端一樣）；Fable 以開頭比（S1）；空字串 scope 當沒有（N2）。
    #[test]
    fn known_limits_and_the_fable_scope() {
        assert_eq!(KnownLimit::of("five_hour", None), Some(KnownLimit::Session));
        assert_eq!(KnownLimit::of("session", Some("")), Some(KnownLimit::Session), "空字串 scope＝沒有");
        assert_eq!(KnownLimit::of("weekly_scoped", Some("fable")), Some(KnownLimit::Fable));
        assert_eq!(KnownLimit::of("weekly_scoped", Some("Fable 5")), Some(KnownLimit::Fable), "改名成 Fable 5 照樣認得");
        assert_eq!(KnownLimit::of("weekly_scoped", Some("Opus")), None);
        assert_eq!(KnownLimit::of("weekly_scoped", Some("Fab")), None);
        assert_eq!(KnownLimit::of("weekly_scoped", Some("Fabl\u{e9}")), None, "非 ASCII 邊界不 panic");
        assert_eq!(KnownLimit::of("session", Some("Fable")), None);
        assert!(is_fable_limit("weekly_scoped", Some("FABLE")));
        assert!(!is_fable_limit("weekly_all", Some("Fable")));
        // surface 有值的 Fable 仍是 Fable（顯示端的樣本本來就沒有 surface，都看會把現有數字清掉）
        let mut u = owner_util();
        u["limits"][2]["scope"]["surface"] = json!("claude_code");
        let r = parse_reading(&u);
        assert_eq!(r.entries[2].known(), Some(KnownLimit::Fable));
        assert_eq!(r.entries[2].key(), "weekly_scoped／Fable · claude_code");
        let st = observe(None, "a|o", 1, r).state;
        assert!(st.unknown_limits().is_empty() && st.notice().is_none());
    }

    fn reading(entries: &[(&str, Option<&str>)], allowance: Option<f64>, week: Option<f64>) -> LimitsReading {
        let entries: Vec<LimitEntry> = entries
            .iter()
            .map(|(k, m)| LimitEntry { kind: k.to_string(), model: m.map(str::to_string), surface: None, percent: Some(10.0), resets_at: None })
            .collect();
        let mut allowances = BTreeMap::new();
        if let Some(a) = allowance {
            allowances.insert("weekly_scoped／Fable".to_string(), a);
        }
        LimitsReading { entries, allowances, week_hours: week, extra_usage: None }
    }
    const THREE: [(&str, Option<&str>); 3] = [("session", None), ("weekly_all", None), ("weekly_scoped", Some("Fable"))];

    #[test]
    fn signature_diff_added_removed_renamed_allowance_week_extra() {
        let base = reading(&THREE, Some(50.0), Some(168.0));
        assert!(diff(&base, &base.clone()).is_empty());
        // 新增
        let more = reading(&[THREE[0], THREE[1], THREE[2], ("weekly_scoped", Some("Opus"))], Some(50.0), Some(168.0));
        assert_eq!(diff(&base, &more), [LimitChange::Added("weekly_scoped／Opus".into())]);
        // 消失
        let fewer = reading(&THREE[..2], None, Some(168.0));
        assert_eq!(
            diff(&base, &fewer),
            [LimitChange::Removed("weekly_scoped／Fable".into()), LimitChange::Allowance { key: "weekly_scoped／Fable".into(), from: Some(50.0), to: None }]
        );
        // 改名（一減一增）
        let renamed = reading(&[THREE[0], THREE[1], ("weekly_scoped", Some("Fable 5"))], None, Some(168.0));
        assert!(matches!(&diff(&base, &renamed)[0], LimitChange::Renamed { from, to } if from == "weekly_scoped／Fable" && to == "weekly_scoped／Fable 5"));
        // allowance 變
        let tighter = reading(&THREE, Some(40.0), Some(168.0));
        assert_eq!(diff(&base, &tighter), [LimitChange::Allowance { key: "weekly_scoped／Fable".into(), from: Some(50.0), to: Some(40.0) }]);
        // 週窗長偏離
        let longer = reading(&THREE, Some(50.0), Some(192.0));
        assert_eq!(diff(&base, &longer), [LimitChange::WeekLength { from: 168.0, to: 192.0 }]);
        // 秒級抖動、或讀不到窗長：不算變
        assert!(diff(&base, &reading(&THREE, Some(50.0), Some(168.004))).is_empty());
        assert!(diff(&base, &reading(&THREE, Some(50.0), None)).is_empty());
        // extra_usage 翻了（N8）
        let mut on = base.clone();
        on.extra_usage = Some(ExtraUsageInfo { enabled: true, active: true, ..Default::default() });
        assert!(matches!(&diff(&base, &on)[0], LimitChange::ExtraUsage { from: None, to: Some(x) } if x.active));
    }

    /// N1：窗長零或負（起點晚於終點）是讀錯了，丟掉。
    #[test]
    fn a_negative_week_length_is_dropped() {
        let mut u = owner_util();
        u["seven_day_breakdown"]["window_started_at"] = json!("2026-10-21T08:00:00Z");
        assert_eq!(parse_reading(&u).week_hours, None);
    }

    #[test]
    fn unknown_limit_makes_a_plain_language_notice_while_it_is_listed() {
        let mut r = reading(&[THREE[0], THREE[1], THREE[2], ("weekly_scoped", Some("Opus"))], None, Some(168.0));
        r.entries[3].percent = Some(37.0);
        let st = observe(None, "a|o", 1, r).state;
        let u = st.unknown_limits();
        assert_eq!(u.len(), 1);
        assert_eq!((u[0].kind.as_str(), u[0].scope.as_deref(), u[0].percent), ("weekly_scoped", Some("Opus"), Some(37.0)));
        let n = st.notice().unwrap();
        assert_eq!(n, "伺服器有一條 App 還不會畫的額度：週額度 · Opus（目前 37%）——數字以 claude.ai 的用量頁為準。");
        assert!(!n.contains("weekly_scoped"), "不露內部名稱（N3）");
        // 那條不在了 → 提示跟著消失（看狀態不看事件）
        let st = observe(Some(&st), "a|o", 2, reading(&THREE, None, Some(168.0))).state;
        assert_eq!(st.notice(), None);
    }

    #[test]
    fn week_length_notice_needs_two_readings_in_a_row() {
        let r = || reading(&THREE, None, Some(192.0));
        let s1 = observe(None, "a|o", 1, r()).state;
        assert_eq!(s1.notice(), None, "一次偏離不算（重置那一刻兩個欄位可能不同步）");
        let s2 = observe(Some(&s1), "a|o", 2, r()).state;
        let n = s2.notice().unwrap();
        assert!(n.contains("192 小時") && n.contains("168"), "{n}");
        let s3 = observe(Some(&s2), "a|o", 3, reading(&THREE, None, Some(168.0))).state;
        assert_eq!(s3.notice(), None);
        // 偏離不到 1 小時不算
        let s = observe(None, "a|o", 1, reading(&THREE, None, Some(168.9))).state;
        assert_eq!(observe(Some(&s), "a|o", 2, reading(&THREE, None, Some(168.9))).state.week_off, 0);
    }

    /// E2：連續 3 次（新鮮的）、跨過 15 分鐘沒出現才算「此方案沒有這條」；出現一次就清零；
    /// 同一份快取不重複計；換座位歸零。
    #[test]
    fn three_misses_in_a_row_mean_not_applicable() {
        let without = || reading(&THREE[..2], None, Some(168.0));
        let o1 = observe(None, "a|o", m(0), without());
        assert!(o1.baseline && o1.updated);
        assert!(!o1.state.not_applicable(KnownLimit::Fable));
        // 同一份快取（fetchedAtMs 沒前進）不計次
        let same = observe(Some(&o1.state), "a|o", m(0), without());
        assert!(!same.updated);
        assert_eq!(same.state.misses["fable"], 1);
        let o2 = observe(Some(&o1.state), "a|o", m(10), without());
        assert!(!o2.state.not_applicable(KnownLimit::Fable));
        let o3 = observe(Some(&o2.state), "a|o", m(20), without());
        assert!(o3.state.not_applicable(KnownLimit::Fable));
        assert_eq!(o3.newly_not_applicable, [KnownLimit::Fable]);
        assert!(!o3.state.not_applicable(KnownLimit::Session) && !o3.state.not_applicable(KnownLimit::WeeklyAll));
        // 已經是了：不再算「新判成」
        let o4 = observe(Some(&o3.state), "a|o", m(30), without());
        assert!(o4.state.not_applicable(KnownLimit::Fable) && o4.newly_not_applicable.is_empty());
        // 出現一次就清零
        let back = observe(Some(&o3.state), "a|o", m(40), reading(&THREE, None, Some(168.0)));
        assert!(!back.state.not_applicable(KnownLimit::Fable));
        assert_eq!(back.state.misses["fable"], 0);
        assert!(!back.state.first_miss_ms.contains_key("fable"));
        // 換座位：重新算（之前的 3 次不帶過去），而且是基準、不比簽名
        let other = observe(Some(&o3.state), "b|o", m(90), without());
        assert!(other.baseline && other.changes.is_empty());
        assert_eq!(other.state.misses["fable"], 1);
        assert!(!other.state.not_applicable(KnownLimit::Fable));
        // 更舊的快取（fetchedAtMs 倒退）也不計
        assert!(!observe(Some(&o3.state), "a|o", m(5), without()).updated);
    }

    /// N6：3 份快取擠在 15 分鐘內（主人手動連跑 /usage）不算；等跨過 15 分鐘的那一份才算。
    #[test]
    fn three_quick_misses_are_not_enough() {
        let without = || reading(&THREE[..2], None, None);
        let s = observe(None, "a|o", m(0), without()).state;
        let s = observe(Some(&s), "a|o", m(1), without()).state;
        let s = observe(Some(&s), "a|o", m(2), without()).state;
        assert_eq!(s.misses["fable"], 3);
        assert!(!s.not_applicable(KnownLimit::Fable), "2 分鐘內三份不算");
        let s = observe(Some(&s), "a|o", m(15), without()).state;
        assert!(s.not_applicable(KnownLimit::Fable));
    }

    /// 32r M1：`limits` 缺、空、不是陣列——不加不減、出格式漂移提示；三條同時不見也一樣。
    /// 漂移的那幾份不覆蓋上一份讀得懂的清單。
    #[test]
    fn a_missing_or_empty_list_is_drift_not_not_applicable() {
        let mut u = owner_util();
        for bad in [json!(null), json!([]), json!({}), json!("x"), json!([{ "percent": 3 }])] {
            u["limits"] = bad.clone();
            assert!(parse_reading(&u).entries.is_empty(), "{bad}");
        }
        let empty = || {
            let mut u = owner_util();
            u.as_object_mut().unwrap().remove("limits");
            parse_reading(&u)
        };
        let base = observe(None, "a|o", m(0), parse_reading(&owner_util())).state;
        let mut st = base.clone();
        let mut started = Vec::new();
        for i in 1..=5 {
            let o = observe(Some(&st), "a|o", m(i * 20), empty());
            started.push(o.drift_started);
            st = o.state;
            for k in KnownLimit::ALL {
                assert!(!st.not_applicable(k), "第 {i} 份：{k:?} 不能判成此方案沒有");
            }
        }
        assert_eq!(started, [true, false, false, false, false], "只在進入漂移那一份記 log");
        assert!(st.drift);
        assert_eq!(st.misses.values().copied().collect::<Vec<_>>(), [0, 0, 0], "不加不減");
        assert_eq!(st.reading.entries, base.reading.entries, "清單維持上一份讀得懂的");
        assert_eq!(st.reading.extra_usage, base.reading.extra_usage);
        let n = st.notice().unwrap();
        assert!(n.contains("格式看起來變了") && n.contains("claude.ai"), "{n}");
        // 三條同時不見（清單在、但全是認不得的）＝也是漂移
        let alien = reading(&[("five_hour_v2", None), ("weekly_v2", None)], None, None);
        let mut s = observe(None, "a|o", m(0), alien.clone()).state;
        for i in 1..=4 {
            s = observe(Some(&s), "a|o", m(i * 20), alien.clone()).state;
        }
        assert!(s.drift && KnownLimit::ALL.iter().all(|k| !s.not_applicable(*k)));
        assert_eq!(s.unknown_limits().len(), 2, "MCP 仍列出認不得的那幾條");
        assert!(s.notice().unwrap().contains("格式看起來變了"));
        // 恢復正常：漂移解除
        let s = observe(Some(&s), "a|o", m(200), reading(&THREE, None, None)).state;
        assert!(!s.drift && s.notice().is_none());
    }

    /// 第二道：就算計數被弄成三條都到了（例如舊版狀態），三條同時「沒有」一律不成立。
    #[test]
    fn all_three_absent_never_count() {
        let mut st = observe(None, "a|o", m(60), reading(&THREE, None, None)).state;
        for k in KnownLimit::ALL {
            st.misses.insert(k.key().into(), 9);
            st.first_miss_ms.insert(k.key().into(), 0);
        }
        assert!(KnownLimit::ALL.iter().all(|k| !st.not_applicable(*k)));
    }

    /// 32r S1：Fable 改名——「Fable 5」直接認得（同一條，log 記改名）；改成完全不像的名字
    /// （同家族一減一增）＝可能改名：不計缺席，只出一句「可能改了名字」，不同時說「多了一條」。
    #[test]
    fn a_renamed_fable_is_one_limit_not_two_messages() {
        let base = observe(None, "a|o", m(0), reading(&THREE, None, None)).state;
        // Fable 5：認得
        let f5 = || reading(&[THREE[0], THREE[1], ("weekly_scoped", Some("Fable 5"))], None, None);
        let o = observe(Some(&base), "a|o", m(20), f5());
        assert!(matches!(&o.changes[0], LimitChange::Renamed { to, .. } if to == "weekly_scoped／Fable 5"));
        assert!(o.state.unknown_limits().is_empty() && o.state.notice().is_none());
        assert_eq!(o.state.misses["fable"], 0);
        // 改成「Mythos」：不認得，但同家族——不計缺席、一句話
        let mythos = || {
            let mut r = reading(&[THREE[0], THREE[1], ("weekly_scoped", Some("Mythos"))], None, None);
            r.entries[2].percent = Some(24.0);
            r
        };
        let mut st = base.clone();
        for i in 1..=4 {
            st = observe(Some(&st), "a|o", m(i * 20), mythos()).state;
        }
        assert_eq!(st.misses["fable"], 0);
        assert!(!st.not_applicable(KnownLimit::Fable));
        let n = st.notice().unwrap();
        assert!(n.contains("「週 · Fable」這條在伺服器的清單上可能改了名字") && n.contains("週額度 · Mythos（目前 24%）"), "{n}");
        assert!(!n.contains("還不會畫的額度"), "不能同時說「多了一條」：{n}");
        assert_eq!(st.unknown_limits().len(), 1, "MCP 仍把它列為認不得");
    }

    #[test]
    fn outlook_gets_the_flag_only_when_not_applicable() {
        let without = || reading(&THREE[..2], None, None);
        let mut st = observe(None, "a|o", m(0), without()).state;
        for i in 1..=2 {
            st = observe(Some(&st), "a|o", m(i * 10), without()).state;
        }
        let mut o = json!({ "percent": null, "expired": true, "lastKnownPercent": 88.0, "fetchedAt": "1970-01-01T00:10:00Z" });
        apply_to_outlook(&mut o, &st, KnownLimit::Fable);
        assert_eq!(o["notApplicable"], true);
        // S3：有「更新」的樣本也照掛（例外已拿掉）
        let mut newer = json!({ "percent": 5.0, "fetchedAt": "2999-01-01T00:00:00Z" });
        apply_to_outlook(&mut newer, &st, KnownLimit::Fable);
        assert_eq!(newer["notApplicable"], true);
        // 5h 那條在清單裡 → 不動
        let mut s = json!({ "percent": 3.0 });
        apply_to_outlook(&mut s, &st, KnownLimit::Session);
        assert!(s.get("notApplicable").is_none());
    }

    /// N7：observe_limits 的黏合——沒得吃就沿用（別的座位的舊狀態不回傳）、同一份不存檔、
    /// 基準／變化／漂移／新判成各寫哪一行 log。
    #[test]
    fn step_glue_saves_and_logs_the_right_things() {
        let names = |s: &Step| s.logs.iter().map(|(n, _)| *n).collect::<Vec<_>>();
        let first = step(None, "a|o", Some((reading(&THREE, None, None), m(0))));
        assert!(first.save);
        assert_eq!(names(&first), ["limits-seen"]);
        let st = first.state.clone().unwrap();
        // 這輪沒得吃（拖尾／快取對不上）：沿用、不存、不記
        let idle = step(Some(st.clone()), "a|o", None);
        assert_eq!((idle.state.as_ref(), idle.save, idle.logs.len()), (Some(&st), false, 0));
        // 換了座位又沒得吃：別人的狀態不能拿來用
        assert_eq!(step(Some(st.clone()), "b|o", None).state, None);
        // 同一份：不存、不記
        let same = step(Some(st.clone()), "a|o", Some((reading(&THREE, None, None), m(0))));
        assert!(!same.save && same.logs.is_empty());
        // 一樣的清單、新的一份：存、但沒有變化就不記
        let quiet = step(Some(st.clone()), "a|o", Some((reading(&THREE, None, None), m(5))));
        assert!(quiet.save && quiet.logs.is_empty());
        // 多一條：limits-changed
        let more = step(Some(st.clone()), "a|o", Some((reading(&[THREE[0], THREE[1], THREE[2], ("weekly_scoped", Some("Opus"))], None, None), m(5))));
        assert_eq!(names(&more), ["limits-changed"]);
        // 漂移：limits-format-drift（清單沒換，所以沒有 limits-changed）
        let drift = step(Some(st.clone()), "a|o", Some((LimitsReading::default(), m(5))));
        assert_eq!(names(&drift), ["limits-format-drift"]);
        // 新判成此方案沒有：limits-not-applicable
        let mut s = st;
        let mut last = Vec::new();
        for i in 1..=3 {
            let r = step(Some(s), "a|o", Some((reading(&THREE[..2], None, None), m(i * 10))));
            last = names(&r);
            s = r.state.unwrap();
        }
        assert_eq!(last, ["limits-not-applicable"]);
    }

    /// 真資料 probe（不是單元測試）：`CUM_PROBE_CLI=1 cargo test --lib limits::tests::probe_real_cache -- --nocapture`
    /// 唯讀 `~/.claude.json`，只印額度種類摘要（kind／scope、allowance、週窗長、extra_usage），不印帳號。
    #[test]
    fn probe_real_cache() {
        if std::env::var("CUM_PROBE_CLI").is_err() {
            return;
        }
        // 不走 cli_cache::read()：它碰 tauri::AppHandle，會把 Tauri 的 Windows 匯入拉進測試 exe
        // （STATUS_ENTRYPOINT_NOT_FOUND）。這裡自己讀、自己比帳號。
        let home = std::env::var("USERPROFILE").unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(std::path::Path::new(&home).join(".claude.json")).unwrap()).unwrap();
        let c = &v["cachedUsageUtilization"];
        let matched = c["accountUuid"].as_str() == v["oauthAccount"]["accountUuid"].as_str();
        println!("cache account matches login: {matched}; fetchedAtMs={}", c["fetchedAtMs"]);
        let st = observe(None, "probe", c["fetchedAtMs"].as_i64().unwrap_or(0), parse_reading(&c["utilization"])).state;
        println!("{}", serde_json::to_string_pretty(&st.summary_json()).unwrap());
        println!("unknown={:?} notice={:?}", st.unknown_limits(), st.notice());
    }

    #[test]
    fn state_round_trips_through_json() {
        let st = observe(None, "a|o", 7, parse_reading(&owner_util())).state;
        let back: LimitsState = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert_eq!(back, st);
        // 舊／壞的 JSON：預設值，不會炸
        let empty: LimitsState = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.fetched_ms, 0);
    }
}
