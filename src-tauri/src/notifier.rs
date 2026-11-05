//! Two-level threshold alerts (v1.2 · D67-Q16/Q17 · D74).
//!
//! Called from `collector::refresh` whenever a fresh snapshot lands. Each of
//! the three official limits (5h / 7d / Fable) has two levels — 留意值
//! (`notice`) and 撞牆值 (`wall`), both read from settings, the same two
//! numbers that position the widget's ivory needle and drive amber / coral.
//!
//! Debounce (D74 決策點 6–8): per limit we remember "which level already
//! fired in this window". A level fires at most once per window; the memory
//! resets when the limit's `reset_at` jumps forward (new window) and lives
//! only in RAM, so a relaunch that finds the account already over a line
//! warns exactly once（v1.8.13：只算還沒重置掉的窗——過期值整條跳過）. Dropping back under a line does NOT re-arm it — noise
//! between sources must not turn into a second toast.
//!
//! The decision logic is pure (`AlarmMemory::observe`) so it is unit-tested
//! without an AppHandle; only `fire` touches the OS.

use chrono::{DateTime, Duration, Utc};
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

use crate::state::AppState;
use crate::types::{AlertThresholds, AppSettings, UsageItem, UsageSnapshot};

/// A `reset_at` that moves forward by more than this is a new window. The
/// three limits all take reset_at from the CLI cache (ISO), so real jitter
/// is seconds; 30 min is generous without ever swallowing a real 5h roll.
const NEW_WINDOW_GAP: Duration = Duration::minutes(30);
/// Fallback when reset_at is missing: a drop this large is a window roll.
const NEW_WINDOW_DROP_PCT: f32 = 25.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Level {
    #[default]
    None,
    Notice,
    Wall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitId {
    Session,
    WeeklyAll,
    WeeklyFable,
}

impl LimitId {
    fn label(self) -> &'static str {
        match self {
            LimitId::Session => "5h 額度",
            LimitId::WeeklyAll => "7d 額度",
            LimitId::WeeklyFable => "Fable 週額度",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Alert {
    pub limit: LimitId,
    pub level: Level,
    pub pct: f32,
}

#[derive(Debug, Clone, Default)]
struct LimitAlarm {
    window_reset_at: Option<DateTime<Utc>>,
    last_pct: Option<f32>,
    fired: Level,
}

/// Per-limit "already told the user" memory. Lives in `AppState`.
#[derive(Debug, Clone, Default)]
pub struct AlarmMemory {
    session: LimitAlarm,
    weekly_all: LimitAlarm,
    weekly_fable: LimitAlarm,
}

pub fn classify(pct: f32, th: AlertThresholds) -> Level {
    if pct >= th.wall {
        Level::Wall
    } else if pct >= th.notice {
        Level::Notice
    } else {
        Level::None
    }
}

fn parse_reset(item: &UsageItem) -> Option<DateTime<Utc>> {
    item.reset_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

/// Observe one limit. Returns the level that should fire now (if any).
fn observe_one(alarm: &mut LimitAlarm, item: &UsageItem, th: AlertThresholds) -> Option<Level> {
    let pct = item.used_percent?;
    let reset = parse_reset(item);

    let new_window = match (alarm.window_reset_at, reset) {
        (Some(old), Some(new)) => new - old > NEW_WINDOW_GAP,
        // No usable reset_at on this frame: a big drop is the only tell.
        _ => alarm
            .last_pct
            .is_some_and(|prev| pct < prev - NEW_WINDOW_DROP_PCT),
    };
    if new_window {
        alarm.fired = Level::None;
    }
    if reset.is_some() {
        alarm.window_reset_at = reset;
    }
    alarm.last_pct = Some(pct);

    let level = classify(pct, th);
    if level > alarm.fired {
        // Jumping straight past notice to wall fires wall only (決策點 7);
        // recording the higher level also marks notice as done.
        alarm.fired = level;
        Some(level)
    } else {
        None
    }
}

impl AlarmMemory {
    /// Pure decision step: which alerts does this snapshot produce?
    ///
    /// v1.8.13（D91 T-04 審查 R-3）：重置時刻已過、數字是重置前量的那一條＝上一個窗的值，
    /// 整條跳過（不告警、也不動記憶）。告警記憶只在記憶體裡，重開 App 後第一輪常拿到昨晚那筆
    /// CLI 快取——以前會對早就重置掉的 95% 跳「已達警戒值」，同一時間懸浮窗寫「已重置」。
    pub fn observe_at(
        &mut self,
        next: &UsageSnapshot,
        th: AlertThresholds,
        now: DateTime<Utc>,
    ) -> Vec<Alert> {
        let mut out = Vec::new();
        let mut step = |alarm: &mut LimitAlarm, item: &UsageItem, limit: LimitId| {
            if item.is_expired_at(now) {
                return;
            }
            if let Some(level) = observe_one(alarm, item, th) {
                out.push(Alert {
                    limit,
                    level,
                    pct: item.used_percent.unwrap_or(0.0),
                });
            }
        };
        step(&mut self.session, &next.current_session, LimitId::Session);
        step(&mut self.weekly_all, &next.weekly_all_models, LimitId::WeeklyAll);
        step(&mut self.weekly_fable, &next.weekly_fable, LimitId::WeeklyFable);
        out
    }
}

/// Entry point from the collector: run the debounced decision against the
/// shared memory, log every crossing, and toast only when the user opted in.
pub async fn check_threshold_crossings(
    app: &AppHandle,
    state: &AppState,
    settings: &AppSettings,
    next: &UsageSnapshot,
) {
    let th = settings.notifications.thresholds();
    let alerts = state.observe_alerts(next, th).await;
    for a in alerts {
        let (title, line) = match a.level {
            // v1.7（D81 追加）：使用者面的名字是「警戒值」，程式內仍叫 wall。
            Level::Wall => ("Claude 用量 · 警戒", format!("越過警戒值 {:.0}%", th.wall)),
            Level::Notice => ("Claude 用量 · 留意", format!("越過留意值 {:.0}%", th.notice)),
            Level::None => continue,
        };
        let body = alert_body(&a, &line, next.extra_usage_active());
        let toast = settings.notifications.system_toast;
        log::info!("alert: {title} — {body}");
        // Auditable trail in collector.log (same file as `startup` / `refresh-ok`),
        // so "did it fire, and was the toast on?" is answerable after the fact.
        crate::applog::write_log(
            app,
            "alert",
            serde_json::json!({
                "limit": a.limit.label(),
                "level": match a.level { Level::Wall => "wall", _ => "notice" },
                "pct": a.pct,
                "notice": th.notice,
                "wall": th.wall,
                "toast": toast,
                "extraUsageActive": next.extra_usage_active(),
            }),
        )
        .await;
        if toast {
            fire(app, title, &body);
        }
    }
}

/// 通知本文。v1.9.5（D100 F1）：開了超額付費（`extra_usage.is_enabled && !spend_limit_reached`）時同步改說法——
/// 滿格「已用完方案額度 · 超額付費中」；警戒（還沒滿）補一句「已開超額付費」。沒開（主人現況）照舊。
/// 只換字，不動告警時機（U5 沒驗：開了超額 % 會不會超過 100）。
fn alert_body(a: &Alert, line: &str, extra_active: bool) -> String {
    let label = a.limit.label();
    if extra_active && a.pct >= 100.0 {
        return format!("{label} 已用完方案額度 · 超額付費中（{line}）");
    }
    let base = format!("{label} 已達 {:.0}%（{line}）", a.pct);
    if extra_active && a.level == Level::Wall {
        format!("{base} · 已開超額付費")
    } else {
        base
    }
}

// =============================================================================
// v1.10（D98／D101）：自己的預算的提醒。
// - 7d／Fable：目前的 % 越過今天的線 → 每個座位、每種額度、每個「一天」最多一次（D101 決策點 7）。
// - 5h 超限預警：今日頁「會提早見底」成立（etaAt 有值＝雙窗都超速且早於重置）→ 每個 5h 窗最多一次（決策點 6）。
// 只看目前登入的座位；記憶只在 RAM（跟兩級告警同一個做法：重開 App 後已越線的會再說一次）。
// =============================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum BudgetAlert {
    OverLine { limit: LimitId, pct: f64, line: f64 },
    SessionEarly { pct: f64, eta: DateTime<Utc>, reset: DateTime<Utc> },
}

#[derive(Debug, Clone, Default)]
pub struct BudgetMemory {
    /// `座位|額度|今天的起點`——同一天同一條只響一次。
    lines: std::collections::HashSet<String>,
    /// 座位 → 上次響過的那一窗的重置時刻（差 30 分以內算同一窗，同 NEW_WINDOW_GAP）。
    session: std::collections::HashMap<String, DateTime<Utc>>,
}

fn parse_utc(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

impl BudgetMemory {
    /// 純判斷：`lines` 是 `Ledger::budget_lines` 的輸出、`session` 是同一個座位的 5h outlook（`burn_stats`）。
    pub fn observe(&mut self, seat: &str, lines: &serde_json::Value, session: &serde_json::Value) -> Vec<BudgetAlert> {
        let mut out = Vec::new();
        for (key, limit) in [("weekly", LimitId::WeeklyAll), ("fable", LimitId::WeeklyFable)] {
            let l = &lines[key];
            if !l["over"].as_bool().unwrap_or(false) {
                continue;
            }
            let (Some(pct), Some(line), Some(day)) = (l["percent"].as_f64(), l["line"].as_f64(), l["dayStart"].as_str()) else {
                continue;
            };
            if self.lines.len() > 512 {
                self.lines.clear();
            }
            if self.lines.insert(format!("{seat}|{key}|{day}")) {
                out.push(BudgetAlert::OverLine { limit, pct, line });
            }
        }
        if let (Some(eta), Some(reset), Some(pct)) =
            (parse_utc(&session["etaAt"]), parse_utc(&session["resetsAt"]), session["percent"].as_f64())
        {
            let same_window = self.session.get(seat).is_some_and(|prev| (reset - *prev).abs() <= NEW_WINDOW_GAP);
            if eta < reset && !same_window {
                self.session.insert(seat.to_string(), reset);
                out.push(BudgetAlert::SessionEarly { pct, eta, reset });
            }
        }
        out
    }
}

/// 一位小數，整數就不帶 .0（60 → "60"、65.7 → "65.7"）。
fn trim1(x: f64) -> String {
    let s = format!("{x:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

fn budget_text(a: &BudgetAlert) -> (&'static str, String) {
    let hm = |t: DateTime<Utc>| t.with_timezone(&chrono::Local).format("%H:%M").to_string();
    match a {
        // 審查 Y1：% 多是整數、線帶一位小數，第一次越線正好是 ceil(線)——線也四捨五入會寫成「66%，超過 66%」。
        BudgetAlert::OverLine { limit, pct, line } => (
            "Claude 用量 · 今天的份",
            format!("{} 已到 {:.0}%，超過今天的線 {}%", limit.label(), pct, trim1(*line)),
        ),
        BudgetAlert::SessionEarly { pct, eta, reset } => {
            let mins = (*reset - *eta).num_minutes().max(0);
            let early = if mins >= 60 { format!("{} 小時 {} 分", mins / 60, mins % 60) } else { format!("{mins} 分") };
            (
                "Claude 用量 · 超限預警",
                format!("5h 額度 {:.0}%，照現在的速度約 {} 用完，比重置（{}）早 {early}", pct, hm(*eta), hm(*reset)),
            )
        }
    }
}

/// 刷新後呼叫：預算開著才算；每筆都寫 `budget` log，系統通知跟現有開關走（D101 決策點 8）。
pub async fn check_budget(app: &AppHandle, state: &AppState, settings: &AppSettings, ledger: &crate::ledger::Ledger) {
    if !settings.budget.enabled {
        return;
    }
    let (lines, session) = match (
        ledger.budget_lines(None, settings.budget.day_start_hour).await,
        ledger.burn_stats(None).await,
    ) {
        (Ok(l), Ok(s)) => (l, s),
        (Err(e), _) | (_, Err(e)) => {
            log::warn!("budget: {e:#}");
            return;
        }
    };
    let seat = lines["seatId"].as_str().unwrap_or_default().to_string();
    if seat.is_empty() {
        return;
    }
    let alerts = state.observe_budget(&seat, &lines, &session).await;
    for a in alerts {
        let (title, body) = budget_text(&a);
        let toast = settings.notifications.system_toast;
        log::info!("budget: {title} — {body}");
        let detail = match &a {
            BudgetAlert::OverLine { limit, pct, line } => serde_json::json!({
                "kind": "over-line", "limit": limit.label(), "pct": pct, "line": line,
                "dayStart": lines[if *limit == LimitId::WeeklyAll { "weekly" } else { "fable" }]["dayStart"],
            }),
            BudgetAlert::SessionEarly { pct, eta, reset } => serde_json::json!({
                "kind": "session-early", "pct": pct, "eta": eta.to_rfc3339(), "reset": reset.to_rfc3339(),
            }),
        };
        let mut detail = detail;
        detail["toast"] = serde_json::json!(toast);
        crate::applog::write_log(app, "budget", detail).await;
        if toast {
            fire(app, title, &body);
        }
    }
}

fn fire(app: &AppHandle, title: &str, body: &str) {
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        log::warn!("Failed to show notification: {e:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TH: AlertThresholds = AlertThresholds { notice: 70.0, wall: 90.0 };

    fn item(pct: Option<f32>, reset: Option<&str>) -> UsageItem {
        UsageItem {
            used_percent: pct,
            reset_at: reset.map(str::to_string),
            note: None,
            sampled_at: None,
            source_label: None,
            not_applicable: false,
        }
    }

    /// v1.9.5（D100 F1）：超額付費成立才換字；主人現況（沒開）一字不變。
    #[test]
    fn alert_text_mentions_extra_usage_only_when_active() {
        let wall = |pct: f32| Alert { limit: LimitId::Session, level: Level::Wall, pct };
        assert_eq!(alert_body(&wall(100.0), "越過警戒值 90%", false), "5h 額度 已達 100%（越過警戒值 90%）");
        assert_eq!(alert_body(&wall(100.0), "越過警戒值 90%", true), "5h 額度 已用完方案額度 · 超額付費中（越過警戒值 90%）");
        assert_eq!(alert_body(&wall(92.0), "越過警戒值 90%", true), "5h 額度 已達 92%（越過警戒值 90%） · 已開超額付費");
        let notice = Alert { limit: LimitId::WeeklyAll, level: Level::Notice, pct: 72.0 };
        assert_eq!(alert_body(&notice, "越過留意值 70%", true), "7d 額度 已達 72%（越過留意值 70%）");
    }

    fn snap(session: UsageItem) -> UsageSnapshot {
        UsageSnapshot {
            current_session: session,
            ..Default::default()
        }
    }

    const W1: &str = "2026-09-15T10:00:00+00:00";
    const W2: &str = "2026-09-15T15:00:00+00:00";
    /// 測試的「現在」：W1 之前（所有窗都還沒重置）。
    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-15T09:00:00+00:00").unwrap().with_timezone(&Utc)
    }

    fn levels(alerts: &[Alert]) -> Vec<Level> {
        alerts.iter().map(|a| a.level).collect()
    }

    #[test]
    fn crossing_fires_each_level_once_per_window() {
        let mut m = AlarmMemory::default();
        assert!(m.observe_at(&snap(item(Some(40.0), Some(W1))), TH, t0()).is_empty());
        assert_eq!(levels(&m.observe_at(&snap(item(Some(72.0), Some(W1))), TH, t0())), [Level::Notice]);
        // Stays over notice: silent.
        assert!(m.observe_at(&snap(item(Some(80.0), Some(W1))), TH, t0()).is_empty());
        assert_eq!(levels(&m.observe_at(&snap(item(Some(91.0), Some(W1))), TH, t0())), [Level::Wall]);
        assert!(m.observe_at(&snap(item(Some(99.0), Some(W1))), TH, t0()).is_empty());
    }

    #[test]
    fn dip_below_does_not_rearm_within_window() {
        let mut m = AlarmMemory::default();
        m.observe_at(&snap(item(Some(75.0), Some(W1))), TH, t0());
        // Source noise: 75 → 68 → 76 in the same window must stay quiet.
        assert!(m.observe_at(&snap(item(Some(68.0), Some(W1))), TH, t0()).is_empty());
        assert!(m.observe_at(&snap(item(Some(76.0), Some(W1))), TH, t0()).is_empty());
    }

    #[test]
    fn new_window_resets_memory() {
        let mut m = AlarmMemory::default();
        m.observe_at(&snap(item(Some(95.0), Some(W1))), TH, t0());
        // Window rolled: 5% in the new window, then climbs back over notice.
        assert!(m.observe_at(&snap(item(Some(5.0), Some(W2))), TH, t0()).is_empty());
        assert_eq!(levels(&m.observe_at(&snap(item(Some(71.0), Some(W2))), TH, t0())), [Level::Notice]);
    }

    #[test]
    fn seconds_of_reset_jitter_is_same_window() {
        let mut m = AlarmMemory::default();
        m.observe_at(&snap(item(Some(75.0), Some(W1))), TH, t0());
        let jitter = "2026-09-15T10:00:40+00:00";
        assert!(m.observe_at(&snap(item(Some(78.0), Some(jitter))), TH, t0()).is_empty());
    }

    #[test]
    fn startup_already_over_line_fires_once() {
        let mut m = AlarmMemory::default();
        assert_eq!(levels(&m.observe_at(&snap(item(Some(93.0), Some(W1))), TH, t0())), [Level::Wall]);
        assert!(m.observe_at(&snap(item(Some(94.0), Some(W1))), TH, t0()).is_empty());
    }

    #[test]
    fn jump_past_both_levels_fires_wall_only() {
        let mut m = AlarmMemory::default();
        m.observe_at(&snap(item(Some(10.0), Some(W1))), TH, t0());
        assert_eq!(levels(&m.observe_at(&snap(item(Some(92.0), Some(W1))), TH, t0())), [Level::Wall]);
        // Notice is considered done too — no late notice toast.
        assert!(m.observe_at(&snap(item(Some(80.0), Some(W1))), TH, t0()).is_empty());
    }

    #[test]
    fn missing_reset_at_uses_drop_as_window_roll() {
        let mut m = AlarmMemory::default();
        m.observe_at(&snap(item(Some(88.0), None)), TH, t0());
        assert!(m.observe_at(&snap(item(Some(3.0), None)), TH, t0()).is_empty());
        assert_eq!(levels(&m.observe_at(&snap(item(Some(70.0), None)), TH, t0())), [Level::Notice]);
    }

    /// v1.8.13（D91 T-04 審查 R-3）：重開 App 後第一輪拿到昨晚的 CLI 快取——95%、重置已過。
    /// 不能跳「已達警戒值」；也不能把這筆當成記憶，之後新窗真的越線照常告警。
    #[test]
    fn a_value_from_a_window_that_already_reset_does_not_alert() {
        let mut m = AlarmMemory::default();
        let morning = DateTime::parse_from_rfc3339("2026-09-15T11:30:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let last_night = UsageItem {
            sampled_at: Some("2026-09-15T08:40:00+00:00".into()),
            ..item(Some(95.0), Some(W1))
        };
        assert!(last_night.is_expired_at(morning));
        assert!(m.observe_at(&snap(last_night), TH, morning).is_empty(), "過期值不告警");
        // 探到新窗：5% → 不響；爬到 92% → 警戒照常響一次
        assert!(m.observe_at(&snap(item(Some(5.0), Some(W2))), TH, morning).is_empty());
        assert_eq!(
            levels(&m.observe_at(&snap(item(Some(92.0), Some(W2))), TH, morning)),
            [Level::Wall]
        );
    }

    // ---- v1.10（D101）：預算提醒 ----

    fn lines(over: bool, day: &str) -> serde_json::Value {
        serde_json::json!({
            "weekly": { "over": over, "percent": 64.0, "line": 60.0, "dayStart": day },
            "fable": { "over": false, "percent": 30.0, "line": 45.0, "dayStart": day },
        })
    }
    fn session(eta: Option<&str>, reset: &str) -> serde_json::Value {
        serde_json::json!({ "percent": 55.0, "etaAt": eta, "resetsAt": reset })
    }
    const D1: &str = "2026-10-11T22:00:00+00:00";
    const D2: &str = "2026-10-12T22:00:00+00:00";

    #[test]
    fn over_the_line_fires_once_per_day_per_seat() {
        let mut m = BudgetMemory::default();
        let quiet = session(None, W1);
        assert_eq!(
            m.observe("A", &lines(true, D1), &quiet),
            [BudgetAlert::OverLine { limit: LimitId::WeeklyAll, pct: 64.0, line: 60.0 }]
        );
        assert!(m.observe("A", &lines(true, D1), &quiet).is_empty(), "同一天不再響");
        assert_eq!(m.observe("B", &lines(true, D1), &quiet).len(), 1, "另一個帳號各算各的");
        assert_eq!(m.observe("A", &lines(true, D2), &quiet).len(), 1, "隔天還超線就再說一次");
        assert!(m.observe("A", &lines(false, "2026-10-13T22:00:00+00:00"), &quiet).is_empty(), "線下不響");
    }

    #[test]
    fn session_early_fires_once_per_window() {
        let mut m = BudgetMemory::default();
        let none = lines(false, D1);
        let eta = "2026-09-15T08:40:00+00:00";
        let alerts = m.observe("A", &none, &session(Some(eta), W1));
        assert!(matches!(alerts.as_slice(), [BudgetAlert::SessionEarly { .. }]));
        assert!(m.observe("A", &none, &session(Some(eta), W1)).is_empty());
        assert!(m.observe("A", &none, &session(Some(eta), "2026-09-15T10:00:40+00:00")).is_empty(), "重置時刻抖幾秒還是同一窗");
        assert!(m.observe("A", &none, &session(None, W2)).is_empty(), "新窗沒超速不響");
        assert_eq!(m.observe("A", &none, &session(Some("2026-09-15T13:00:00+00:00"), W2)).len(), 1, "新窗超速再響一次");
    }

    #[test]
    fn budget_texts_read_naturally() {
        let (_, body) = budget_text(&BudgetAlert::OverLine { limit: LimitId::WeeklyFable, pct: 64.4, line: 60.0 });
        assert_eq!(body, "Fable 週額度 已到 64%，超過今天的線 60%");
        let (_, body) = budget_text(&BudgetAlert::OverLine { limit: LimitId::WeeklyAll, pct: 66.0, line: 65.7 });
        assert_eq!(body, "7d 額度 已到 66%，超過今天的線 65.7%", "不能寫成「66%，超過 66%」");
        let reset = DateTime::parse_from_rfc3339(W1).unwrap().with_timezone(&Utc);
        let (title, body) = budget_text(&BudgetAlert::SessionEarly { pct: 55.0, eta: reset - Duration::minutes(80), reset });
        assert_eq!(title, "Claude 用量 · 超限預警");
        assert!(body.ends_with("早 1 小時 20 分"), "{body}");
    }

    #[test]
    fn no_percent_is_ignored() {
        let mut m = AlarmMemory::default();
        assert!(m.observe_at(&snap(item(None, Some(W1))), TH, t0()).is_empty());
    }

    #[test]
    fn limits_are_tracked_independently() {
        let mut m = AlarmMemory::default();
        let s = UsageSnapshot {
            current_session: item(Some(75.0), Some(W1)),
            weekly_all_models: item(Some(95.0), Some(W1)),
            weekly_fable: item(Some(10.0), Some(W1)),
            ..Default::default()
        };
        let alerts = m.observe_at(&s, TH, t0());
        assert_eq!(alerts.len(), 2);
        assert_eq!(alerts[0], Alert { limit: LimitId::Session, level: Level::Notice, pct: 75.0 });
        assert_eq!(alerts[1], Alert { limit: LimitId::WeeklyAll, level: Level::Wall, pct: 95.0 });
    }

    #[test]
    fn normalize_keeps_notice_below_wall() {
        use crate::types::NotificationsSettings;
        let mut n = NotificationsSettings { notice_pct: 95, wall_pct: 90, ..Default::default() };
        n.normalize();
        assert_eq!((n.notice_pct, n.wall_pct), (89, 90));
        let mut n = NotificationsSettings { notice_pct: 0, wall_pct: 150, ..Default::default() };
        n.normalize();
        assert_eq!((n.notice_pct, n.wall_pct), (1, 99));
    }
}
