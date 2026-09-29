use anyhow::Result;
use tauri::{
    image::Image,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Wry,
};

use crate::icon::{self, Mood};
use crate::types::{ScrapeStatus, UsageState};

const TRAY_LABEL: &str = "main-tray";

pub fn build_tray(app: &AppHandle) -> Result<TrayIcon> {
    let menu = build_menu(app)?;

    let icon = icon::render(None, Mood::Idle);
    let png = icon::to_png_bytes(&icon);

    let tray = TrayIconBuilder::with_id(TRAY_LABEL)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .icon(Image::from_bytes(&png)?)
        .icon_as_template(false)
        .tooltip("Claude Usage — initializing…")
        .on_menu_event(handle_menu_event)
        .on_tray_icon_event(handle_tray_event)
        .build(app)?;

    Ok(tray)
}

fn build_menu(app: &AppHandle) -> Result<Menu<Wry>> {
    let toggle_widget = MenuItem::with_id(app, "toggle-widget", "顯示/隱藏浮動視窗", true, None::<&str>)?;
    let refresh = MenuItem::with_id(app, "refresh", "立即刷新", true, None::<&str>)?;
    let statistics = MenuItem::with_id(app, "statistics", "打開統計視窗", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "設定…", true, None::<&str>)?;
    let separator1 = PredefinedMenuItem::separator(app)?;
    let separator2 = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "結束", true, None::<&str>)?;

    let about = MenuItem::with_id(
        app,
        "about",
        "Claude Usage Monitor — 非官方工具",
        false,
        None::<&str>,
    )?;

    let menu = Menu::with_items(
        app,
        &[
            &about,
            &separator1,
            &toggle_widget,
            &refresh,
            &statistics,
            &settings,
            &separator2,
            &quit,
        ],
    )?;
    Ok(menu)
}

fn handle_menu_event(app: &AppHandle, event: MenuEvent) {
    let app = app.clone();
    let id = event.id.0.clone();
    tauri::async_runtime::spawn(async move {
        match id.as_str() {
            "toggle-widget" => toggle_widget(&app).await,
            "refresh" => {
                let _ = crate::collector::refresh_manual(&app).await;
            }
            "settings" => {
                if let Some(w) = app.get_webview_window("settings") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            "statistics" => {
                if let Some(w) = app.get_webview_window("statistics") {
                    let _ = w.show();
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                }
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        }
    });
}

/// v1.8.7（D86 蟲 1）：托盤的「顯示／隱藏」以前只動視窗、不動 `settings.widget.show`。
/// 主人在設定頁關掉懸浮窗、再從托盤點開——設定裡還是 false，下一筆任何存檔
/// （拖曳位置、切密度）走 `settings::apply` 就把它藏回去。現在切換＝改設定、
/// 存檔、廣播，三個視窗與設定頁的開關都跟著同一個真相。
async fn toggle_widget(app: &AppHandle) {
    let Some(w) = app.get_webview_window("widget") else {
        return;
    };
    let visible = w.is_visible().unwrap_or(false);
    crate::settings::set_widget_visible(app, !visible).await;
    if !visible {
        let _ = w.set_focus();
    }
}

fn handle_tray_event(tray: &TrayIcon, event: TrayIconEvent) {
    if let TrayIconEvent::Click {
        button: MouseButton::Left,
        button_state: MouseButtonState::Up,
        ..
    } = event
    {
        let app = tray.app_handle().clone();
        tauri::async_runtime::spawn(async move {
            toggle_widget(&app).await;
        });
    }
}

pub async fn refresh_tray(app: &AppHandle, state: &UsageState) {
    let Some(tray) = app.tray_by_id(TRAY_LABEL) else {
        return;
    };
    let app_state: tauri::State<crate::state::AppState> = {
        use tauri::Manager as _;
        app.state()
    };
    let settings = app_state.get_settings().await;
    let time_format = settings.general.time_format;

    // v1.8.13（D91 T-04）：5h 窗的重置時刻已過、數字是重置前量的＝那是上一個窗的值——
    // 圖示不再拿它定顏色、畫數字（以前停用前在警戒值以上就整晚紅色）。
    let now = chrono::Utc::now();
    let headline = state
        .data
        .as_ref()
        .map(|d| &d.current_session)
        .filter(|s| !s.is_expired_at(now))
        .and_then(|s| s.used_percent);

    let mood = icon::mood_from_state(state.status, headline, settings.notifications.thresholds());
    let img = icon::render(headline, mood);
    let png = icon::to_png_bytes(&img);
    if let Ok(image) = Image::from_bytes(&png) {
        let _ = tray.set_icon(Some(image));
    }

    let tooltip = build_tooltip(state, &time_format);
    let _ = tray.set_tooltip(Some(&tooltip));

    schedule_expiry_redraw(app, next_reset_after(state, now));
}

/// v1.8.13（D91 T-04 審查 R-2）：三條額度裡最早還沒到的重置時刻。過了這一刻，那條的數字
/// 就是上一個窗的（`is_expired_at` 翻成 true），托盤該改寫「已重置」、圖示該退成灰。
fn next_reset_after(
    state: &UsageState,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let d = state.data.as_ref()?;
    [&d.current_session, &d.weekly_all_models, &d.weekly_fable]
        .into_iter()
        .filter_map(|it| it.reset_at.as_deref())
        .filter_map(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc))
        .filter(|t| *t >= now)
        .min()
}

/// 等待中的那一個重畫計時（新的一次 `refresh_tray` 會取消舊的、排新的）。
static EXPIRY_TIMER: std::sync::Mutex<Option<(u64, tauri::async_runtime::JoinHandle<()>)>> =
    std::sync::Mutex::new(None);
static EXPIRY_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// v1.8.13（D91 T-04 審查 R-2）：托盤的「已重置」與灰圖示以前只在刷新時算——閒置時只有
/// 排程器會刷新（主人設 30 分一輪、心跳關），重置後最多 30 分鐘托盤還是紅的、還寫舊百分比，
/// 懸浮窗卻一分鐘內就改成「已重置」。現在排一個一次性計時：到了最早那個重置時刻（多 1 秒），
/// 拿 AppState 當下的 usage 只重畫托盤，不跑整輪刷新。
fn schedule_expiry_redraw(app: &AppHandle, at: Option<chrono::DateTime<chrono::Utc>>) {
    use std::sync::atomic::Ordering;
    let gen = EXPIRY_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    let Ok(mut slot) = EXPIRY_TIMER.lock() else {
        return;
    };
    if let Some((_, old)) = slot.take() {
        old.abort();
    }
    let Some(at) = at else {
        return;
    };
    let wait = (at - chrono::Utc::now())
        .to_std()
        .unwrap_or_default()
        + std::time::Duration::from_secs(1);
    let app = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        tokio::time::sleep(wait).await;
        // 還是最新排的那一個才動手；先把自己從槽裡拿掉，免得重畫時把自己取消掉。
        if EXPIRY_GEN.load(Ordering::Relaxed) != gen {
            return;
        }
        if let Ok(mut slot) = EXPIRY_TIMER.lock() {
            slot.take();
        }
        let usage = app.state::<crate::state::AppState>().get_usage().await;
        Box::pin(refresh_tray(&app, &usage)).await;
    });
    *slot = Some((gen, handle));
}

fn fmt_pct(v: Option<f32>) -> String {
    match v {
        Some(p) => format!("{:.0}%", p),
        None => "—".into(),
    }
}

/// v1.8.13（D91 T-10）：tray-icon 0.21.3 把 tooltip 截在 128 個 UTF-16 單位（含結尾 0），
/// 超過的部分直接消失——以前正常一份就 135～160 字，接在最後的 ⚠ 那行永遠看不到。
const TOOLTIP_MAX_UNITS: usize = 127;

fn units(s: &str) -> usize {
    s.encode_utf16().count()
}

/// 截到最多 `max` 個 UTF-16 單位，截到就補「…」。
fn clip(s: &str, max: usize) -> String {
    if units(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut n = 0;
    for ch in s.chars() {
        let w = ch.len_utf16();
        if n + w > max.saturating_sub(1) {
            break;
        }
        out.push(ch);
        n += w;
    }
    out.push('…');
    out
}

/// 重置欄：" · 週二 16:00 重置"／" · 2h14m 後重置"。
/// D61 自查補洞：tooltip 原本完全沒有重置時刻——fable 撞牆時看不到哪天解禁。
/// 時刻格式跟前端同一顆設定（相對/絕對），不再各表面各自為政。
fn reset_part(item: &crate::types::UsageItem, time_format: &str) -> String {
    item.reset_at
        .as_deref()
        .and_then(|iso| chrono::DateTime::parse_from_rfc3339(iso).ok())
        .map(|t| {
            if time_format == "relative" {
                let mins = (t.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_minutes();
                let rel = if mins < 60 {
                    format!("{}m", mins.max(0))
                } else if mins < 48 * 60 {
                    format!("{}h{:02}m", mins / 60, mins % 60)
                } else {
                    format!("{}d{:02}h", mins / (24 * 60), (mins / 60) % 24)
                };
                return format!(" · {rel} 後重置");
            }
            let local = t.with_timezone(&chrono::Local);
            let days = (local.date_naive() - chrono::Local::now().date_naive()).num_days();
            let hm = local.format("%H:%M");
            let day = match days {
                0 => String::new(),
                1 => "明日 ".to_string(),
                2..=7 => {
                    let wd = ["日", "一", "二", "三", "四", "五", "六"]
                        [local.format("%w").to_string().parse::<usize>().unwrap_or(0)];
                    format!("週{wd} ")
                }
                _ => local.format("%m-%d ").to_string(),
            };
            format!(" · {day}{hm} 重置")
        })
        .unwrap_or_default()
}

/// One tooltip line: "5h 窗口 40% · 週二 16:00 重置 · cli · 3 分前"（`stamp=false` 不帶來源戳）。
/// v1.8.13（D91 T-04）：重置時刻已過、數字是重置前量的 → "5h 窗口 — · 已重置"，不再顯示
/// 上個窗的百分比與「已到後重置」。
fn tooltip_line(
    label: &str,
    item: &crate::types::UsageItem,
    time_format: &str,
    now: chrono::DateTime<chrono::Utc>,
    stamp: bool,
) -> String {
    if item.is_expired_at(now) {
        return format!("{label} — · 已重置");
    }
    let stamp = if stamp {
        format!(" · {}", item.note.as_deref().unwrap_or("來源不明"))
    } else {
        String::new()
    };
    format!(
        "{label} {}{}{stamp}",
        fmt_pct(item.used_percent),
        reset_part(item, time_format)
    )
}

fn build_tooltip(state: &UsageState, time_format: &str) -> String {
    build_tooltip_at(state, time_format, chrono::Utc::now())
}

fn build_tooltip_at(
    state: &UsageState,
    time_format: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let tip = match state.status {
        ScrapeStatus::Idle => "Claude Usage — 尚未取得資料".to_string(),
        ScrapeStatus::Loading => "Claude Usage — 更新中…".to_string(),
        // Legacy variant — the collector never produces it (kept only for
        // serialization compatibility until the M1 data-model pass).
        ScrapeStatus::NeedsLogin => "Claude Usage — 需要登入".to_string(),
        ScrapeStatus::Error => format!(
            "Claude Usage — {}",
            state.error.as_deref().unwrap_or("發生錯誤"),
        ),
        ScrapeStatus::Ok => {
            let Some(d) = state.data.as_ref() else {
                return "Claude Usage".to_string();
            };
            let plan = d.plan_name.as_deref().unwrap_or("Claude");
            let rows = [
                ("5h 窗口", &d.current_session),
                ("週·全模型", &d.weekly_all_models),
                ("週·Fable", &d.weekly_fable),
            ];
            let lines = |stamp: bool| {
                rows.iter()
                    .map(|(l, it)| tooltip_line(l, it, time_format, now, stamp))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            // v1.8.13（D91 T-10）：先試完整版（每行帶來源戳）；放不下就改成來源戳只寫一次
            // （5h 那條的，寫在標題行）；⚠ 那行最後接上、用剩下的額度截，保證看得到開頭。
            let full = format!("Claude · {plan}\n{}", lines(true));
            let compact = format!(
                "Claude · {plan} · {}\n{}",
                d.current_session.note.as_deref().unwrap_or("來源不明"),
                lines(false)
            );
            match state.notice.as_deref() {
                None if units(&full) <= TOOLTIP_MAX_UNITS => full,
                None => compact,
                Some(n) => {
                    let warn = format!("⚠ {n}");
                    let with_full = format!("{full}\n{warn}");
                    if units(&with_full) <= TOOLTIP_MAX_UNITS {
                        with_full
                    } else {
                        // 換行佔 1；至少留 24 個單位給 ⚠ 那行（放不下就連標題的來源戳也拿掉）。
                        let body = if units(&compact) + 1 + 24 <= TOOLTIP_MAX_UNITS {
                            compact
                        } else {
                            format!("Claude · {plan}\n{}", lines(false))
                        };
                        let room = TOOLTIP_MAX_UNITS.saturating_sub(units(&body) + 1);
                        format!("{body}\n{}", clip(&warn, room))
                    }
                }
            }
        }
    };
    clip(&tip, TOOLTIP_MAX_UNITS)
}

#[cfg(test)]
mod tooltip_tests {
    //! v1.8.13（D91 T-04／T-10）：托盤 tooltip 要放得進 127 個 UTF-16 單位、⚠ 那行要看得到；
    //! 重置已過的舊值寫「已重置」。
    use super::*;
    use crate::types::{UsageItem, UsageSnapshot};

    fn item(pct: f32, reset: &str, sampled: &str, note: &str) -> UsageItem {
        UsageItem {
            used_percent: Some(pct),
            reset_at: Some(reset.into()),
            note: Some(note.into()),
            sampled_at: Some(sampled.into()),
        }
    }
    fn state(notice: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> UsageState {
        let fut = |h: i64| (now + chrono::Duration::hours(h)).to_rfc3339();
        let past = |m: i64| (now - chrono::Duration::minutes(m)).to_rfc3339();
        UsageState {
            status: ScrapeStatus::Ok,
            data: Some(UsageSnapshot {
                plan_name: Some("Claude Max 20x".into()),
                current_session: item(40.0, &fut(3), &past(3), "cli · 3 分前"),
                weekly_all_models: item(56.0, &fut(100), &past(3), "cli · 3 分前"),
                weekly_fable: item(100.0, &fut(120), &past(3), "cli · 3 分前"),
                scraped_at: now.to_rfc3339(),
            }),
            notice: notice.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn fits_127_units_and_keeps_the_warning_line() {
        let now = chrono::Utc::now();
        let long = "額度查詢已連續 3 次只拿回 CLI 手上的舊數字（12 分前的），沒有拿到伺服器的新值——畫面停在那個時刻。下次重試 14:05。";
        for tf in ["absolute", "relative"] {
            let plain = build_tooltip_at(&state(None, now), tf, now);
            assert!(units(&plain) <= TOOLTIP_MAX_UNITS, "{tf}: {} units", units(&plain));
            let t = build_tooltip_at(&state(Some(long), now), tf, now);
            assert!(units(&t) <= TOOLTIP_MAX_UNITS, "{tf}: {} units", units(&t));
            let last = t.lines().last().unwrap();
            assert!(last.starts_with("⚠ 額度查詢"), "{tf}: ⚠ 那行要在：{t}");
            assert!(t.contains("5h 窗口 40%"), "{t}");
            assert!(t.contains("週·Fable 100%"), "{t}");
        }
    }

    #[test]
    fn a_reset_that_already_passed_reads_as_reset_not_the_old_number() {
        let now = chrono::Utc::now();
        let mut s = state(None, now);
        // 5h 窗 20 分鐘前就重置了，最後一筆是 2 小時前（重置前）量的 32%
        s.data.as_mut().unwrap().current_session = item(
            32.0,
            &(now - chrono::Duration::minutes(20)).to_rfc3339(),
            &(now - chrono::Duration::hours(2)).to_rfc3339(),
            "cli · 2 小時前",
        );
        let t = build_tooltip_at(&s, "absolute", now);
        assert!(t.contains("5h 窗口 — · 已重置"), "{t}");
        assert!(!t.contains("32%"), "{t}");
        assert!(s.data.as_ref().unwrap().current_session.is_expired_at(now));
        // 重置後 0.6 秒寫下的樣本仍當上一個窗（容差 60 秒）
        let r = now - chrono::Duration::minutes(5);
        let it = item(100.0, &r.to_rfc3339(), &(r + chrono::Duration::milliseconds(600)).to_rfc3339(), "desktop");
        assert!(it.is_expired_at(now));
        // 重置後 2 分鐘量的＝新窗的值，不算過期
        let it = item(3.0, &r.to_rfc3339(), &(r + chrono::Duration::minutes(2)).to_rfc3339(), "desktop");
        assert!(!it.is_expired_at(now));
    }

    /// 審查 R-2：托盤重畫計時要排在「三條裡最早還沒到的重置時刻」，一過那一刻那條就翻成已重置。
    #[test]
    fn redraw_timer_targets_the_earliest_upcoming_reset() {
        let now = chrono::Utc::now();
        let s = state(None, now);
        let at = next_reset_after(&s, now).expect("有還沒到的重置");
        // 5h 那條 3 小時後重置，比兩條週額度（100／120 小時後）早
        assert_eq!((at - now).num_minutes(), 180);
        let later = at + chrono::Duration::seconds(1);
        let d = s.data.as_ref().unwrap();
        assert!(!d.current_session.is_expired_at(now));
        assert!(d.current_session.is_expired_at(later), "計時到點時那條要已經算過期");
        assert!(build_tooltip_at(&s, "absolute", later).contains("5h 窗口 — · 已重置"));
        // 已過的重置不再排（它已經是「已重置」了），往下一個找；全部沒有就不排
        let mut s2 = state(None, now);
        s2.data.as_mut().unwrap().current_session.reset_at =
            Some((now - chrono::Duration::minutes(5)).to_rfc3339());
        assert_eq!((next_reset_after(&s2, now).unwrap() - now).num_hours(), 100);
        assert_eq!(next_reset_after(&UsageState::default(), now), None);
    }
}
