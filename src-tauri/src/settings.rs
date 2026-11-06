use anyhow::Result;
use base64::Engine;
use tauri::{AppHandle, Manager};
use tauri_plugin_store::StoreExt;

use crate::state::AppState;
use crate::types::{AppSettings, LegacySettingsV1};

const SETTINGS_FILE: &str = "settings.json";
const SETTINGS_KEY: &str = "settings";

/// 24 random bytes (192 bits) → URL-safe base64 ≈ 32 chars. Plenty of entropy
/// for a local-only bearer token; stored plain-text in settings.json because
/// the AppData directory is already a trusted boundary.
///
/// Goes straight to the OS CSPRNG (BCryptGenRandom on Windows) via the
/// `getrandom` crate — no thread-local RNG seeding state to worry about.
pub fn generate_webhook_token() -> String {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).expect("OS RNG must be available to mint webhook token");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Two-stage parser: try the v0.3 nested shape first, then fall back to the
/// v0.1/v0.2 flat shape and upgrade in place. Returns `(settings, upgraded)`
/// where `upgraded == true` means the caller should re-save immediately so
/// the on-disk file is rewritten in the new nested shape.
fn parse_or_migrate(raw: serde_json::Value) -> (AppSettings, bool) {
    // Heuristic: nested payload has at least one of the group keys
    // ("general", "widget", "notifications", "webhook", ...). Flat legacy
    // payload has none of them. We try nested first so a partial nested
    // payload (e.g. only "general" populated) still parses cleanly via
    // `#[serde(default)]` on each group.
    let looks_nested = raw.as_object().is_some_and(|obj| {
        obj.contains_key("general")
            || obj.contains_key("widget")
            || obj.contains_key("notifications")
            || obj.contains_key("webhook")
            || obj.contains_key("settingsUi")
            || obj.contains_key("statisticsWindow")
    });

    if looks_nested {
        match serde_json::from_value::<AppSettings>(raw.clone()) {
            Ok(s) => return (s, false),
            Err(e) => {
                log::warn!(
                    "settings: nested payload failed to parse ({e}); trying legacy fallback"
                );
            }
        }
    }

    match serde_json::from_value::<LegacySettingsV1>(raw) {
        Ok(legacy) => {
            log::info!("settings: migrating legacy flat schema to nested (v0.3)");
            (AppSettings::from(legacy), true)
        }
        Err(e) => {
            log::warn!("settings: payload unreadable in either shape, using defaults: {e}");
            (AppSettings::default(), false)
        }
    }
}

pub async fn load(app: &AppHandle) -> AppSettings {
    let store = app.store(SETTINGS_FILE);
    let value = match store {
        Ok(s) => s.get(SETTINGS_KEY),
        Err(e) => {
            log::warn!("settings: failed to open store, using defaults: {e}");
            None
        }
    };

    let (mut settings, migrated) = match value {
        Some(v) => parse_or_migrate(v),
        None => (AppSettings::default(), false),
    };
    // v1.2: hand-edited settings.json must not leave 留意值 ≥ 撞牆值.
    settings.notifications.normalize();
    settings.budget.normalize();

    // If we just upgraded from the legacy flat shape, persist immediately so
    // the on-disk JSON matches what we hold in memory. Failure here isn't
    // fatal — next save() will fix it — but it's worth logging.
    if migrated {
        if let Err(e) = save(app, &settings).await {
            log::warn!("settings: failed to persist migrated nested payload: {e}");
        } else {
            log::info!("settings: persisted nested payload after migration");
        }
    }

    // Provision a webhook bearer token on first run, then persist immediately
    // so the same token survives across launches (browser extension and
    // `~/.claude/settings.json` hook only need to learn it once).
    if settings.webhook.token.is_empty() {
        settings.webhook.token = generate_webhook_token();
        if let Err(e) = save(app, &settings).await {
            log::warn!("settings: failed to persist generated webhook token: {e}");
        } else {
            log::info!("settings: minted new webhook token");
        }
    }

    settings
}

pub async fn save(app: &AppHandle, settings: &AppSettings) -> Result<()> {
    let store = app.store(SETTINGS_FILE)?;
    store.set(SETTINGS_KEY, serde_json::to_value(settings)?);
    store.save()?;
    Ok(())
}

/// v1.8.7（D86 蟲 1）：托盤切換懸浮窗的唯一入口——改 `widget.show`、落盤、套用、
/// 廣播 `settings://update`，設定頁的開關才會跟著動，之後的存檔也不會把它藏回去。
pub async fn set_widget_visible(app: &AppHandle, show: bool) {
    use tauri::Emitter as _;
    let state: tauri::State<AppState> = app.state();
    let mut s = state.get_settings().await;
    s.widget.show = show;
    if let Err(e) = save(app, &s).await {
        log::warn!("settings: failed to persist widget.show={show}: {e}");
    }
    apply(app, s.clone()).await;
    let _ = app.emit("settings://update", &s);
}

/// 讓懸浮窗看得到，而且不拿焦點（D104）：沒顯示就顯示、被最小化或跑到所有螢幕外就叫回來，
/// 叫回來的話在 collector.log 留一行 `widget-rescued`（`via` 是誰發現的：
/// show／minimized／fullscreen-restore）。本來就在螢幕上就什麼都不做。
///
/// 絆線：真的動了視窗、而且動之前前景不是懸浮窗，300 ms 後它卻變成前景＝又搶焦點了，
/// 記一行 `widget-took-focus`。正常情況永遠不會出現這行。
pub async fn show_widget(
    app: &AppHandle,
    widget: &tauri::WebviewWindow,
    saved: Option<crate::types::WidgetPosition>,
    via: &str,
) {
    use crate::widget_pos::{ensure_on_screen, is_foreground, show_without_focus};
    let was_visible = widget.is_visible().unwrap_or(false);
    let was_foreground = is_foreground(widget);
    if let Some(shown) = show_without_focus(widget).await {
        log_webview(
            app,
            serde_json::json!({
                "visible": true, "via": via, "painted": shown.painted, "waitedMs": shown.waited_ms,
            }),
        )
        .await;
    }
    let rescued = ensure_on_screen(widget, saved).await;
    let acted = !was_visible || rescued.is_some();
    if let Some(mut info) = rescued {
        info["via"] = serde_json::json!(via);
        crate::applog::write_log(app, "widget-rescued", info).await;
    }
    if acted && !was_foreground {
        let (app, widget, via) = (app.clone(), widget.clone(), via.to_string());
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            if is_foreground(&widget) {
                crate::applog::write_log(
                    &app,
                    "widget-took-focus",
                    serde_json::json!({ "via": via, "wasVisible": was_visible }),
                )
                .await;
            }
        });
    }
}

/// 把懸浮窗藏起來的唯一入口（D107）：視窗和 WebView 一起藏，WebView 真的切了就在
/// collector.log 留一行 `widget-webview`（`via`：fullscreen／close／hide）。顯示那一行另外帶
/// `painted`／`waitedMs`：頁面有沒有在時限內畫好新的一格、視窗等了多久才出來。
/// 「藏」跟「設定裡的 widget.show」是兩回事：全螢幕閃避只藏不改設定，托盤／設定頁走
/// [`set_widget_visible`] → [`apply`] 才會走到這裡。
pub async fn hide_widget(app: &AppHandle, widget: &tauri::WebviewWindow, via: &str) {
    if crate::widget_pos::hide(widget).await {
        log_webview(app, serde_json::json!({ "visible": false, "via": via })).await;
    }
}

async fn log_webview(app: &AppHandle, info: serde_json::Value) {
    crate::applog::write_log(app, "widget-webview", info).await;
}

pub async fn apply(app: &AppHandle, settings: AppSettings) {
    let state: tauri::State<AppState> = app.state();
    state.set_settings(settings.clone()).await;

    // Apply widget visibility
    if let Some(widget) = app.get_webview_window("widget") {
        if settings.widget.show {
            // 托盤「顯示」也走這裡：顯示了卻在畫面外＝主人還是看不到。
            show_widget(app, &widget, settings.widget.position, "show").await;
        } else {
            hide_widget(app, &widget, "hide").await;
        }
    }

    // Apply autostart. Surface failures via log::warn so a UI claim of
    // "auto-start enabled" doesn't silently diverge from the Windows
    // registry state (e.g. when AV / policy blocks writing the Run key).
    use tauri_plugin_autostart::ManagerExt;
    let autostart = app.autolaunch();
    let already = match autostart.is_enabled() {
        Ok(v) => v,
        Err(e) => {
            log::warn!("autostart: is_enabled probe failed: {e}");
            false
        }
    };
    if settings.general.auto_start && !already {
        if let Err(e) = autostart.enable() {
            log::warn!("autostart: enable() failed — UI may report enabled while Windows isn't: {e}");
        }
    } else if !settings.general.auto_start && already {
        if let Err(e) = autostart.disable() {
            log::warn!("autostart: disable() failed — UI may report disabled while Windows isn't: {e}");
        }
    }
}

