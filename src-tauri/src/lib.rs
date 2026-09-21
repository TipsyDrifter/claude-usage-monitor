mod applog;
mod collector;
mod commands;
mod fullscreen;
mod hook_installer;
mod icon;
mod ledger;
mod notifier;
mod scheduler;
mod server;
mod settings;
mod state;
mod stats;
mod tray;
mod types;

use tauri::Manager;
use tauri_plugin_autostart::MacosLauncher;

use crate::scheduler::Scheduler;
use crate::state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = env_logger::try_init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .invoke_handler(tauri::generate_handler![
            commands::get_usage_state,
            commands::get_settings,
            commands::save_settings,
            commands::refresh_now,
            commands::show_settings,
            commands::show_statistics,
            commands::quit_app,
            commands::reveal_log_folder,
            // v1.8.2: diagnostics bundle
            commands::pack_diagnostics,
            // v0.2: webhook token + hook installer
            commands::install_claude_code_hook,
            commands::regenerate_webhook_token,
            // M1–M4: ledger-backed pages
            commands::get_data_health,
            commands::get_burn_stats,
            commands::get_today_outlook,
            commands::get_dashboard,
            // v1.3 (D75): account switcher
            commands::list_seats,
            commands::get_seat_snapshot,
            // M6 (D65): history page + evidence export
            commands::get_history,
            commands::export_evidence,
            commands::save_export_file,
            commands::reveal_export_folder,
        ])
        .setup(|app| {
            // Initialize app state
            let state = AppState::default();
            app.manage(state.clone());

            // M0: the truth ledger (ledger.sqlite). DB failure is fatal during
            // setup: better loud than silently not recording. (v1.1: the v0.3
            // history.sqlite layer is gone; an old file on disk is left alone.)
            let db_dir = app
                .path()
                .app_data_dir()
                .map_err(|e| Box::<dyn std::error::Error>::from(format!("app_data_dir: {e}")))?;
            let ledger = ledger::Ledger::open(db_dir).map_err(|e| {
                Box::<dyn std::error::Error>::from(format!("open ledger.sqlite: {e}"))
            })?;
            app.manage(ledger);

            // Build the system tray
            let _tray = tray::build_tray(app.handle())?;

            // Hide the settings window initially in case config didn't take
            if let Some(w) = app.get_webview_window("settings") {
                let _ = w.hide();
            }

            // Load settings (sync via async runtime) — BEFORE widget
            // positioning so the saved position can win (D61).
            let app_handle = app.handle().clone();
            let state_for_load = state.clone();
            let loaded_settings = tauri::async_runtime::block_on(async {
                let loaded = settings::load(&app_handle).await;
                state_for_load.set_settings(loaded.clone()).await;
                settings::apply(&app_handle, loaded.clone()).await;
                loaded
            });

            // v1.1 (D67／O12 結案的教訓): one `startup` line per process so a
            // later reader can tell "no probe fired" from "the app was not
            // running" without inferring it from missing refresh-ok rows.
            {
                let app_for_log = app.handle().clone();
                let version = app.package_info().version.to_string();
                let poll = loaded_settings.general.poll_interval_minutes;
                let heartbeat = loaded_settings.general.idle_heartbeat_minutes;
                tauri::async_runtime::spawn(async move {
                    applog::write_log(
                        &app_for_log,
                        "startup",
                        serde_json::json!({
                            "version": version,
                            "pollIntervalMinutes": poll,
                            "idleHeartbeatMinutes": heartbeat,
                        }),
                    )
                    .await;
                });
            }

            // D61: restore the widget to its last saved position (physical
            // px, saved on every drag by the frontend). Clamped to the
            // current monitor layout — a stale位置 from an unplugged monitor
            // falls back to the bottom-right default.
            if let Some(widget) = app.get_webview_window("widget") {
                let mut restored = false;
                if let Some(pos) = loaded_settings.widget.position {
                    if let Ok(Some(monitor)) = widget.primary_monitor() {
                        let m = monitor.size();
                        let inside = pos.x > -200
                            && pos.y > -50
                            && (pos.x as u32) < m.width.saturating_add(200)
                            && (pos.y as u32) < m.height;
                        if inside {
                            let _ = widget.set_position(tauri::PhysicalPosition::new(
                                pos.x, pos.y,
                            ));
                            restored = true;
                        }
                    }
                }
                if !restored {
                    if let Ok(Some(monitor)) = widget.primary_monitor() {
                        let size = monitor.size();
                        let scale = monitor.scale_factor();
                        let widget_w = 302.0; // card 密度含簷廊
                        let widget_h = 250.0;
                        let margin = 16.0;
                        let logical_w = size.width as f64 / scale;
                        let logical_h = size.height as f64 / scale;
                        // Reserve ~48px for the taskbar at the bottom
                        let x = logical_w - widget_w - margin;
                        let y = logical_h - widget_h - margin - 48.0;
                        let _ = widget.set_position(tauri::LogicalPosition::new(
                            x.max(0.0),
                            y.max(0.0),
                        ));
                    }
                }
            }

            // Spawn scheduler with current interval
            let initial_interval = tauri::async_runtime::block_on(state.get_settings())
                .general
                .poll_interval_minutes;
            let scheduler = Scheduler::new(initial_interval);
            app.manage(scheduler.clone());
            scheduler.clone().run(app.handle().clone());

            // Spawn local HTTP webhook server
            let webhook_port = tauri::async_runtime::block_on(state.get_settings())
                .webhook
                .port;
            let app_handle_for_server = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = server::run(app_handle_for_server, webhook_port).await {
                    log::error!("Webhook server stopped: {e:?}");
                }
            });

            // Trigger first collection pass (after small delay)
            let app_handle_for_first = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let _ = collector::refresh(&app_handle_for_first).await;
            });

            // M1: seed the seats table from Desktop's bridge-state.json —
            // free org→account learning, no user round-trip (D48/D51).
            {
                let ledger: tauri::State<ledger::Ledger> = app.handle().state();
                let ledger = ledger.inner().clone();
                tauri::async_runtime::spawn(async move {
                    if let Ok(pairs) = collector::bridge::read_pairs() {
                        let _ = ledger.seed_seats(&pairs).await;
                    }
                });
            }

            // M0/M1: JSONL transcript reader. First full scan 15s after
            // startup, then every 60s — the mtime+size+offset index makes
            // quiet rounds near-free, and the scan doubles as the activity
            // detector for the probe's idle pause (last_activity_at).
            let app_handle_for_jsonl = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                let mut first_round = true;
                loop {
                    let ledger: tauri::State<ledger::Ledger> =
                        app_handle_for_jsonl.state();
                    let ledger = ledger.inner().clone();
                    match collector::jsonl::scan(&ledger).await {
                        Ok(stats) => {
                            // Only log rounds that actually read something —
                            // at a 60s cadence the quiet ones are pure noise.
                            if stats.files_read > 0 {
                                applog::write_log(
                                    &app_handle_for_jsonl,
                                    "jsonl-scan",
                                    serde_json::json!({
                                        "files_seen": stats.files_seen,
                                        "files_read": stats.files_read,
                                        "events_upserted": stats.events_upserted,
                                        "anchors_inserted": stats.anchors_inserted,
                                    }),
                                )
                                .await;
                            }
                            // New usage rows = the user is actively burning
                            // quota. The first round after boot replays
                            // history, which says nothing about "now".
                            if stats.events_upserted > 0 && !first_round {
                                let _ = ledger
                                    .set_channel_state(
                                        "last_activity_at",
                                        &chrono::Utc::now().to_rfc3339(),
                                    )
                                    .await;
                            }
                        }
                        Err(e) => {
                            applog::write_log(
                                &app_handle_for_jsonl,
                                "jsonl-scan-error",
                                serde_json::json!({ "error": format!("{e:#}") }),
                            )
                            .await;
                        }
                    }
                    first_round = false;
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
            });

            // M2: 全螢幕自動閃避 — hide the always-on-top widget while a
            // fullscreen app holds the foreground, restore it after. Only
            // windows WE auto-hid get auto-restored: a user-hidden widget
            // (settings.widget.show == false) stays hidden.
            let app_handle_for_fs = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                use std::sync::atomic::{AtomicBool, Ordering};
                static AUTO_HIDDEN: AtomicBool = AtomicBool::new(false);
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    let is_fs = tokio::task::spawn_blocking(fullscreen::foreground_is_fullscreen)
                        .await
                        .unwrap_or(false);
                    let Some(w) = app_handle_for_fs.get_webview_window("widget") else {
                        continue;
                    };
                    if is_fs {
                        if w.is_visible().unwrap_or(false) {
                            let _ = w.hide();
                            AUTO_HIDDEN.store(true, Ordering::Relaxed);
                        }
                    } else if AUTO_HIDDEN.swap(false, Ordering::Relaxed) {
                        let state: tauri::State<AppState> = app_handle_for_fs.state();
                        if state.get_settings().await.widget.show {
                            let _ = w.show();
                        }
                    }
                }
            });

            // M1: daily ledger backup (VACUUM INTO, keep 7). First backup
            // two minutes in so it lands after the boot-time scans settle.
            let app_handle_for_backup = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                loop {
                    let ledger: tauri::State<ledger::Ledger> =
                        app_handle_for_backup.state();
                    let ledger = ledger.inner().clone();
                    let dir = app_handle_for_backup
                        .path()
                        .app_data_dir()
                        .map(|d| d.join("backups"));
                    if let Ok(dir) = dir {
                        match ledger.backup_into(&dir, 7).await {
                            Ok(dest) => {
                                applog::write_log(
                                    &app_handle_for_backup,
                                    "ledger-backup",
                                    serde_json::json!({
                                        "dest": dest.to_string_lossy(),
                                    }),
                                )
                                .await;
                            }
                            Err(e) => {
                                applog::write_log(
                                    &app_handle_for_backup,
                                    "ledger-backup-error",
                                    serde_json::json!({ "error": format!("{e:#}") }),
                                )
                                .await;
                            }
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(86_400)).await;
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Don't actually close — hide instead, so the app keeps running
                // in tray. All three of our managed windows need this: without
                // it, clicking the X destroys the webview and a subsequent
                // `app.get_webview_window(label)` returns None, leaving the
                // tray menu / IPC entry-points silently broken.
                let label = window.label();
                if label == "widget" || label == "settings" || label == "statistics" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
