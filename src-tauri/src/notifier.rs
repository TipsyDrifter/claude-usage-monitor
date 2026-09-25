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
//! warns exactly once. Dropping back under a line does NOT re-arm it — noise
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
    pub fn observe(&mut self, next: &UsageSnapshot, th: AlertThresholds) -> Vec<Alert> {
        let mut out = Vec::new();
        let mut step = |alarm: &mut LimitAlarm, item: &UsageItem, limit: LimitId| {
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
        let body = format!("{} 已達 {:.0}%（{line}）", a.limit.label(), a.pct);
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
            }),
        )
        .await;
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
        }
    }

    fn snap(session: UsageItem) -> UsageSnapshot {
        UsageSnapshot {
            current_session: session,
            ..Default::default()
        }
    }

    const W1: &str = "2026-09-15T10:00:00+00:00";
    const W2: &str = "2026-09-15T15:00:00+00:00";

    fn levels(alerts: &[Alert]) -> Vec<Level> {
        alerts.iter().map(|a| a.level).collect()
    }

    #[test]
    fn crossing_fires_each_level_once_per_window() {
        let mut m = AlarmMemory::default();
        assert!(m.observe(&snap(item(Some(40.0), Some(W1))), TH).is_empty());
        assert_eq!(levels(&m.observe(&snap(item(Some(72.0), Some(W1))), TH)), [Level::Notice]);
        // Stays over notice: silent.
        assert!(m.observe(&snap(item(Some(80.0), Some(W1))), TH).is_empty());
        assert_eq!(levels(&m.observe(&snap(item(Some(91.0), Some(W1))), TH)), [Level::Wall]);
        assert!(m.observe(&snap(item(Some(99.0), Some(W1))), TH).is_empty());
    }

    #[test]
    fn dip_below_does_not_rearm_within_window() {
        let mut m = AlarmMemory::default();
        m.observe(&snap(item(Some(75.0), Some(W1))), TH);
        // Source noise: 75 → 68 → 76 in the same window must stay quiet.
        assert!(m.observe(&snap(item(Some(68.0), Some(W1))), TH).is_empty());
        assert!(m.observe(&snap(item(Some(76.0), Some(W1))), TH).is_empty());
    }

    #[test]
    fn new_window_resets_memory() {
        let mut m = AlarmMemory::default();
        m.observe(&snap(item(Some(95.0), Some(W1))), TH);
        // Window rolled: 5% in the new window, then climbs back over notice.
        assert!(m.observe(&snap(item(Some(5.0), Some(W2))), TH).is_empty());
        assert_eq!(levels(&m.observe(&snap(item(Some(71.0), Some(W2))), TH)), [Level::Notice]);
    }

    #[test]
    fn seconds_of_reset_jitter_is_same_window() {
        let mut m = AlarmMemory::default();
        m.observe(&snap(item(Some(75.0), Some(W1))), TH);
        let jitter = "2026-09-15T10:00:40+00:00";
        assert!(m.observe(&snap(item(Some(78.0), Some(jitter))), TH).is_empty());
    }

    #[test]
    fn startup_already_over_line_fires_once() {
        let mut m = AlarmMemory::default();
        assert_eq!(levels(&m.observe(&snap(item(Some(93.0), Some(W1))), TH)), [Level::Wall]);
        assert!(m.observe(&snap(item(Some(94.0), Some(W1))), TH).is_empty());
    }

    #[test]
    fn jump_past_both_levels_fires_wall_only() {
        let mut m = AlarmMemory::default();
        m.observe(&snap(item(Some(10.0), Some(W1))), TH);
        assert_eq!(levels(&m.observe(&snap(item(Some(92.0), Some(W1))), TH)), [Level::Wall]);
        // Notice is considered done too — no late notice toast.
        assert!(m.observe(&snap(item(Some(80.0), Some(W1))), TH).is_empty());
    }

    #[test]
    fn missing_reset_at_uses_drop_as_window_roll() {
        let mut m = AlarmMemory::default();
        m.observe(&snap(item(Some(88.0), None)), TH);
        assert!(m.observe(&snap(item(Some(3.0), None)), TH).is_empty());
        assert_eq!(levels(&m.observe(&snap(item(Some(70.0), None)), TH)), [Level::Notice]);
    }

    #[test]
    fn no_percent_is_ignored() {
        let mut m = AlarmMemory::default();
        assert!(m.observe(&snap(item(None, Some(W1))), TH).is_empty());
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
        let alerts = m.observe(&s, TH);
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
