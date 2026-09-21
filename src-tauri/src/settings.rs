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

pub async fn apply(app: &AppHandle, settings: AppSettings) {
    let state: tauri::State<AppState> = app.state();
    state.set_settings(settings.clone()).await;

    // Apply widget visibility
    if let Some(widget) = app.get_webview_window("widget") {
        if settings.widget.show {
            let _ = widget.show();
        } else {
            let _ = widget.hide();
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

