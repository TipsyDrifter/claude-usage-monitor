mod applog;
mod collector;
mod commands;
mod fullscreen;
mod hook_installer;
mod icon;
mod ledger;
mod mcp;
mod memwatch;
mod notifier;
mod scheduler;
mod server;
mod settings;
mod state;
mod stats;
#[cfg(test)]
mod test_support;
mod tray;
mod types;
mod widget_pos;
mod windows;

use tauri::Manager;
use tauri_plugin_autostart::MacosLauncher;

use crate::scheduler::Scheduler;
use crate::state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // v1.8.13（D91 T-08）：warn 以上的 log 也寫進 collector.log（發行版沒有主控台）。
    applog::init_logger();

    tauri::Builder::default()
        // v1.8.13（D91 T-08）：同一時間只准一份。以前開機自啟後再從開始選單點一次就是雙托盤、
        // 雙排程、兩份告警，第二份的門鈴綁不到 port 還沒人知道。第二次啟動＝把統計視窗叫到前面。
        // 官方文件：這個 plugin 必須第一個註冊。
        // v1.8.14：統計視窗改成按需建立——這個回呼跑在主執行緒，建 webview 要先 spawn 出去（見 windows.rs）。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                applog::write_log(&app, "second-instance", serde_json::json!({})).await;
                if let Err(e) =
                    windows::show_logged(&app, windows::Panel::Statistics, "second-instance").await
                {
                    log::error!("second-instance: open statistics failed: {e}");
                }
            });
        }))
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
            commands::get_budget_lines,
            commands::get_today_outlook,
            commands::get_dashboard,
            // v1.3 (D75): account switcher
            commands::list_seats,
            commands::get_seat_snapshot,
            // M6 (D65): history page + evidence export
            commands::get_history,
            commands::get_history_window,
            commands::export_evidence,
            commands::save_export_file,
            commands::reveal_export_folder,
        ])
        .setup(|app| {
            // v1.8.13（D91 T-08）：log 橋要知道 collector.log 在哪——越早越好，settings::load 的 warn 才接得到。
            if let Ok(dir) = app.path().app_data_dir() {
                applog::set_log_dir(dir);
            }
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
            // v1.8.13（D91 T-02）：開不了帳本以前是從 setup 回 Err → tauri panic；release 版
            // 沒有主控台，主人看到的只是「App 不見了」，collector.log 一行都沒有。現在先寫 log、
            // 跳一個看得到的對話框說怎麼從備份還原，再乾淨結束（Ledger::open 已保證不動任何表）。
            let ledger = match ledger::Ledger::open(db_dir.clone()) {
                Ok(l) => l,
                Err(e) => {
                    let detail = format!("{e:#}");
                    tauri::async_runtime::block_on(applog::write_log(
                        app.handle(),
                        "ledger-open-failed",
                        serde_json::json!({
                            "error": detail,
                            "path": db_dir.join("ledger.sqlite").to_string_lossy(),
                        }),
                    ));
                    ledger::show_open_failed_dialog(&db_dir, &detail);
                    std::process::exit(1);
                }
            };
            app.manage(ledger);
            // v1.8.13（D91 T-06）：CLI 快取讀到「快取帳號≠登入帳號」時要能自己寫 collector.log。
            collector::cli_cache::init_log(app.handle());

            // Build the system tray
            let _tray = tray::build_tray(app.handle())?;

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
            // v1.8.7（D86 蟲 2）：放行早到的 `get_settings`——它們現在會等這面旗子，
            // 不再拿到預設值（主題 w5、密碼牌空）。
            state.mark_loaded();
            // v1.8.7（D86 蟲 5）：密碼牌若換過（空牌補鑄、重新產生），已裝的 Stop hook
            // 手上是舊牌、每次都 401——啟動時對一次，對不上就用現在的牌重寫那一條。
            {
                let app_for_hook = app.handle().clone();
                let token = loaded_settings.webhook.token.clone();
                let port = loaded_settings.webhook.port;
                tauri::async_runtime::spawn(async move {
                    hook_installer::repair_if_installed(&app_for_hook, &token, port).await;
                });
            }
            // v1.8.5（D85 追加）：config 裡的視窗在 setup 之前就建好了，webview 早於
            // 這裡的 load 呼叫 get_settings 會拿到 AppSettings::default()（密碼牌空、
            // autoStart 預設…），之後沒有人通知它——設定視窗裡按的每一下都是「預設值＋
            // 改動」。載完就廣播一次，早到的 store 用它蓋掉預設。
            // （v1.8.14 起 config 只剩懸浮窗；設定／統計在 load 之後才按需建立，不會早到。）
            {
                use tauri::Emitter as _;
                let _ = app.handle().emit("settings://update", &loaded_settings);
            }

            // v1.1 (D67／O12 結案的教訓): one `startup` line per process so a
            // later reader can tell "no probe fired" from "the app was not
            // running" without inferring it from missing refresh-ok rows.
            {
                let app_for_log = app.handle().clone();
                let version = app.package_info().version.to_string();
                let poll = loaded_settings.general.poll_interval_minutes;
                let heartbeat = loaded_settings.general.idle_heartbeat_minutes;
                let probe_min = loaded_settings.general.probe_min_minutes;
                tauri::async_runtime::spawn(async move {
                    applog::write_log(
                        &app_for_log,
                        "startup",
                        serde_json::json!({
                            "version": version,
                            "pollIntervalMinutes": poll,
                            "idleHeartbeatMinutes": heartbeat,
                            "probeMinMinutes": probe_min,
                        }),
                    )
                    .await;
                });
            }

            // D61: restore the widget to its last saved position (physical
            // px, saved on every drag by the frontend). Clamped to the
            // current monitor layout — a stale位置 from an unplugged monitor
            // falls back to the bottom-right default.
            // v1.8.13（D91 T-07）：以前只跟 `primary_monitor()` 的寬高比——主人把懸浮窗放在副螢幕
            //（主 (0,0)-(1920,1080)、副 (1920,0)-(4480,1600)，視窗在 (3957,1028)），3957 超過
            // 1920+200 就判「不在螢幕上」，每次開機自啟都跳回主螢幕右下。現在逐一看每顆螢幕。
            if let Some(widget) = app.get_webview_window("widget") {
                let rects = widget_pos::monitor_rects(&widget);
                let saved = loaded_settings.widget.position;
                if let Some(pos) = widget_pos::usable(saved, &rects) {
                    let _ = widget.set_position(tauri::PhysicalPosition::new(pos.x, pos.y));
                } else {
                    if let Some(pos) = saved {
                        let app_for_log = app.handle().clone();
                        let monitors = widget_pos::describe(&rects);
                        tauri::async_runtime::spawn(async move {
                            applog::write_log(
                                &app_for_log,
                                "widget-position-reset",
                                serde_json::json!({ "saved": [pos.x, pos.y], "monitors": monitors }),
                            )
                            .await;
                        });
                    }
                    widget_pos::place_default(&widget);
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
                // v1.8.13（D91 T-08）：綁不到 port 的情況 server::run 自己寫 log、記 channel_state；
                // 這裡的 error 經 log 橋也會進 collector.log（以前只到 stderr，發行版看不到）。
                if let Err(e) = server::run(app_handle_for_server, webhook_port).await {
                    log::error!("Webhook server stopped: {e:?}");
                }
            });

            // Trigger first collection pass (after small delay)
            let app_handle_for_first = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                {
                    let ledger: tauri::State<ledger::Ledger> = app_handle_for_first.state();
                    let _ = ledger.set_channel_state("pending_refresh_cause", "startup").await;
                }
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
                                // v1.8.7（D86 蟲 5）：活動不只是「准不准探」的依據，
                                // 也是門鈴——C 快取過期、節奏允許就直接刷新。
                                let _ = collector::refresh_on_activity(&app_handle_for_jsonl).await;
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
                        let s = state.get_settings().await;
                        if s.widget.show {
                            settings::show_widget(&app_handle_for_fs, &w, s.widget.position, "fullscreen-restore").await;
                        }
                    }
                }
            });

            // v1.9.5（D100 D4）：App 心跳——每 5 分鐘把「我還活著」寫進帳本。距上次超過 15 分
            // （App 沒在跑、或電腦休眠到睡醒）就記一段 `source='app'` 的觀測缺口；開機第一次
            // 也走同一個判斷（上次關機到現在多久）。判斷邏輯是 Ledger::app_heartbeat／heartbeat_gap。
            let app_handle_for_heartbeat = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    let ledger: tauri::State<ledger::Ledger> = app_handle_for_heartbeat.state();
                    let ledger = ledger.inner().clone();
                    if let Err(e) = ledger.app_heartbeat(chrono::Utc::now()).await {
                        applog::write_log(
                            &app_handle_for_heartbeat,
                            "heartbeat-error",
                            serde_json::json!({ "error": format!("{e:#}") }),
                        )
                        .await;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(ledger::APP_HEARTBEAT_SECS)).await;
                }
            });

            // M1: ledger backup (VACUUM INTO). First backup two minutes in so it
            // lands after the boot-time scans settle, then every 24h.
            // v1.8.13（D91 T-05）：保留規則改成「每個本地日留最新一份、留最近 7 個日子」
            // （以前按檔數留 7 份，重裝日一天就吃掉好幾份）；細節見 Ledger::backup_into。
            const BACKUP_KEEP_DAYS: usize = 7;
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
                        match ledger.backup_into(&dir, BACKUP_KEEP_DAYS).await {
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

            // v1.10.4（D105）／v1.10.5（D106）：每 5 分鐘量一次 WebView2 整家子的記憶體與主執行緒回應，
            // 每小時或有異狀時寫一行 collector.log `webview-memory`。
            tauri::async_runtime::spawn(memwatch::run(app.handle().clone()));

            Ok(())
        })
        .on_window_event(|window, event| {
            // 懸浮窗沒有工作列按鈕，被最小化（Win+D／Win+M、電腦操作工具截圖前）就等於不見了
            // ——馬上還原，而且不拿焦點（D104）。最小化時 Windows 會把它移到 (-32000,-32000)，
            // Moved 一定會來。
            if let tauri::WindowEvent::Moved(_) = event {
                if window.label() == "widget" && window.is_minimized().unwrap_or(false) {
                    let app = window.app_handle().clone();
                    tauri::async_runtime::spawn(async move {
                        let Some(w) = app.get_webview_window("widget") else {
                            return;
                        };
                        let state: tauri::State<AppState> = app.state();
                        let saved = state.get_settings().await.widget.position;
                        settings::show_widget(&app, &w, saved, "minimized").await;
                    });
                }
            }
            // v1.10.4（D105）：設定／統計最小化時 WebView 設成看不見（停掉動畫、清快取），還原再設回來。
            if let tauri::WindowEvent::Resized(_) = event {
                if let Some(panel) = windows::Panel::from_label(window.label()) {
                    let app = window.app_handle().clone();
                    tauri::async_runtime::spawn(async move {
                        if let Some(visible) = windows::sync_webview_visibility(&app, panel) {
                            crate::applog::write_log(
                                &app,
                                "panel-webview",
                                serde_json::json!({ "window": panel.label(), "visible": visible }),
                            )
                            .await;
                        }
                    });
                }
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let label = window.label();
                if label == "widget" {
                    // 懸浮窗常駐：按 X 只藏起來（它的 renderer 本來就要一直活著）。
                    api.prevent_close();
                    let _ = window.hide();
                } else if let Some(panel) = windows::Panel::from_label(label) {
                    // v1.8.14：設定／統計讓它真的關掉、renderer 跟著收掉；下次由
                    // windows::show 重建（以前 hide 不銷毀，三個 renderer 永遠活著）。
                    windows::remember_bounds(window, panel);
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            // 保險：懸浮窗理論上永遠在，但萬一所有視窗都關了，也不准因為
            // 「最後一個視窗關了」就結束——這是托盤常駐的 App。
            // 托盤「結束」走 app.exit(0)，code 是 Some(0)，照常放行。
            if let tauri::RunEvent::ExitRequested { code: None, api, .. } = event {
                api.prevent_exit();
            }
        });
}
