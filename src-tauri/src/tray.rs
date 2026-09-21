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
            "toggle-widget" => {
                if let Some(w) = app.get_webview_window("widget") {
                    let visible = w.is_visible().unwrap_or(false);
                    if visible {
                        let _ = w.hide();
                    } else {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
            }
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

fn handle_tray_event(tray: &TrayIcon, event: TrayIconEvent) {
    if let TrayIconEvent::Click {
        button: MouseButton::Left,
        button_state: MouseButtonState::Up,
        ..
    } = event
    {
        let app = tray.app_handle();
        if let Some(w) = app.get_webview_window("widget") {
            let visible = w.is_visible().unwrap_or(false);
            if visible {
                let _ = w.hide();
            } else {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }
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

    let headline = state
        .data
        .as_ref()
        .and_then(|d| d.current_session.used_percent);

    let mood = icon::mood_from_state(state.status, headline, settings.notifications.thresholds());
    let img = icon::render(headline, mood);
    let png = icon::to_png_bytes(&img);
    if let Ok(image) = Image::from_bytes(&png) {
        let _ = tray.set_icon(Some(image));
    }

    let tooltip = build_tooltip(state, &time_format);
    let _ = tray.set_tooltip(Some(&tooltip));
}

fn fmt_pct(v: Option<f32>) -> String {
    match v {
        Some(p) => format!("{:.0}%", p),
        None => "—".into(),
    }
}

/// One tooltip line: "  5h 窗口 40% · 週二 16:00 重置 · cli · 3 分前".
/// D61 自查補洞：tooltip 原本完全沒有重置時刻——fable 撞牆時看不到哪天解禁。
/// 時刻格式跟前端同一顆設定（相對/絕對），不再各表面各自為政。
fn tooltip_line(label: &str, item: &crate::types::UsageItem, time_format: &str) -> String {
    let stamp = item.note.as_deref().unwrap_or("來源不明");
    let reset = item
        .reset_at
        .as_deref()
        .and_then(|iso| chrono::DateTime::parse_from_rfc3339(iso).ok())
        .map(|t| {
            if time_format == "relative" {
                let mins = (t.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_minutes();
                let rel = if mins <= 0 {
                    "已到".to_string()
                } else if mins < 60 {
                    format!("{mins}m")
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
        .unwrap_or_default();
    format!("  {label} {}{reset} · {stamp}", fmt_pct(item.used_percent))
}

fn build_tooltip(state: &UsageState, time_format: &str) -> String {
    match state.status {
        ScrapeStatus::Idle => "Claude Usage — 尚未取得資料".to_string(),
        ScrapeStatus::Loading => "Claude Usage — 更新中…".to_string(),
        // Legacy variant — the collector never produces it (kept only for
        // serialization compatibility until the M1 data-model pass).
        ScrapeStatus::NeedsLogin => "Claude Usage — 需要登入".to_string(),
        ScrapeStatus::Error => format!(
            "Claude Usage — 錯誤：{}",
            state.error.as_deref().unwrap_or("未知錯誤"),
        ),
        ScrapeStatus::Ok => {
            let Some(d) = state.data.as_ref() else {
                return "Claude Usage".to_string();
            };
            let plan = d.plan_name.as_deref().unwrap_or("Claude");
            let mut tip = format!(
                "Claude · {plan}\n{}\n{}\n{}",
                tooltip_line("5h 窗口:  ", &d.current_session, time_format),
                tooltip_line("週·全模型:", &d.weekly_all_models, time_format),
                tooltip_line("週·Fable: ", &d.weekly_fable, time_format),
            );
            if let Some(n) = state.notice.as_deref() {
                tip.push_str("\n⚠ ");
                tip.push_str(n);
            }
            tip
        }
    }
}
