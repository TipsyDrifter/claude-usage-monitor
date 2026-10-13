//! v1.8.14：設定／統計視窗按需建立、關閉就真的銷毀。
//!
//! 以前三個視窗都寫在 `tauri.conf.json`、開機就建好藏起來，按 X 也只是 `hide()`——
//! 每個視窗一個 WebView2 renderer 永遠活著。2026-09-30 記憶體健康檢查量到整個 App
//! 約 400 MB Private，本體只佔 15 MB，其餘全是 WebView2（browser／gpu／三個 renderer…）。
//! 現在只有懸浮窗常駐；設定與統計由這裡「找得到就叫到前面、找不到就建一個」，
//! 按 X 走 Tauri 預設的關閉流程，renderer 跟著收掉。
//!
//! 所有入口（托盤選單、`show_settings`／`show_statistics` IPC、第二次啟動）都只准走
//! [`show`]，不要再直接 `get_webview_window("settings"|"statistics")` 假設它存在。
//!
//! ⚠️ Windows 上在主執行緒的事件回呼（選單、視窗事件、single-instance）裡同步建
//! webview 會死鎖（WebView2 的限制，Tauri 文件有寫）——那些地方要先 spawn 再呼叫。

use std::sync::Mutex;

use tauri::{
    AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, Window,
};

/// v1.8.14（D92 決策點 1，主人看過畫面後拍板）：WebView2 關掉硬體加速、改軟體繪製。
/// GPU 行程 88～165 MB → 13～19 MB；代價是閒置 CPU 從一顆核心的 2% 變 4%。
///
/// ⚠️ 兩個約束：
/// 1. 自訂參數會**整串取代** wry 的預設（`--disable-features=…` 與 autoplay），所以預設的兩段要抄過來。
/// 2. 同一個 App 的所有 webview 共用一個 WebView2 環境，參數必須一字不差——
///    懸浮窗寫在 tauri.conf.json 的 `additionalBrowserArgs`，下面的測試會對這兩處。
pub const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --autoplay-policy=no-user-gesture-required --disable-gpu";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Settings,
    Statistics,
}

struct Spec {
    url: &'static str,
    title: &'static str,
    size: (f64, f64),
    min_size: Option<(f64, f64)>,
    resizable: bool,
}

impl Panel {
    pub const fn label(self) -> &'static str {
        match self {
            Panel::Settings => "settings",
            Panel::Statistics => "statistics",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "settings" => Some(Panel::Settings),
            "statistics" => Some(Panel::Statistics),
            _ => None,
        }
    }

    const fn index(self) -> usize {
        match self {
            Panel::Settings => 0,
            Panel::Statistics => 1,
        }
    }

    /// 以前寫在 tauri.conf.json 的那兩段，原樣搬過來。
    const fn spec(self) -> Spec {
        match self {
            Panel::Settings => Spec {
                url: "settings.html",
                title: "Claude Usage Monitor — Settings",
                size: (480.0, 580.0),
                min_size: None,
                resizable: false,
            },
            Panel::Statistics => Spec {
                url: "statistics.html",
                title: "Claude Usage Monitor — 統計",
                size: (1300.0, 840.0),
                min_size: Some((800.0, 560.0)),
                resizable: true,
            },
        }
    }
}

/// 關掉前的位置與大小（實體像素），只記在記憶體裡。以前視窗只是藏起來，
/// 重開時自然停在原處、維持拖過的大小——改成銷毀後靠這個接回同樣的手感。
/// App 重啟就忘（跟以前一樣：以前每次啟動也是置中、預設大小）。
#[derive(Clone, Copy)]
struct Bounds {
    pos: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
    maximized: bool,
}

static LAST_BOUNDS: Mutex<[Option<Bounds>; 2]> = Mutex::new([None, None]);

/// 同一時間只准一條路在建視窗：托盤連點兩下時，第二下要看到第一下建好的那個，
/// 而不是也看到 None、再建一次撞「label 已存在」。
static CREATE_LOCK: Mutex<()> = Mutex::new(());

/// 找得到就叫到前面，找不到就建一個再叫到前面。回傳 `true`＝這次是新建的。
pub fn show(app: &AppHandle, panel: Panel) -> tauri::Result<bool> {
    let (window, created) = {
        let _guard = CREATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        match app.get_webview_window(panel.label()) {
            Some(w) => (w, false),
            None => (build(app, panel)?, true),
        }
    };
    window.show()?;
    let _ = window.unminimize();
    let _ = window.set_focus();
    Ok(created)
}

fn build(app: &AppHandle, panel: Panel) -> tauri::Result<WebviewWindow> {
    let spec = panel.spec();
    let saved = LAST_BOUNDS.lock().unwrap_or_else(|e| e.into_inner())[panel.index()];

    let mut builder =
        WebviewWindowBuilder::new(app, panel.label(), WebviewUrl::App(spec.url.into()))
            .title(spec.title)
            .inner_size(spec.size.0, spec.size.1)
            .resizable(spec.resizable)
            .additional_browser_args(BROWSER_ARGS)
            .visible(false);
    if let Some((w, h)) = spec.min_size {
        builder = builder.min_inner_size(w, h);
    }
    if saved.is_none() {
        builder = builder.center();
    }
    let window = builder.build()?;

    if let Some(b) = saved {
        let _ = window.set_size(b.size);
        let _ = window.set_position(b.pos);
        if b.maximized {
            let _ = window.maximize();
        }
    }
    Ok(window)
}

/// 在 `CloseRequested` 時呼叫（視窗還在，讀得到位置）。最小化中被關（工作列右鍵關閉）
/// 時位置是 (-32000,-32000)，記下來下次會開在畫面外——那種情況保留上一筆。
pub fn remember_bounds(window: &Window, panel: Panel) {
    if window.is_minimized().unwrap_or(false) {
        return;
    }
    let (Ok(pos), Ok(size)) = (window.outer_position(), window.inner_size()) else {
        return;
    };
    let maximized = window.is_maximized().unwrap_or(false);
    let mut slots = LAST_BOUNDS.lock().unwrap_or_else(|e| e.into_inner());
    if maximized {
        // 最大化時的位置／大小就是整個螢幕；保留上一筆「還原後」的框，只記得要再最大化。
        match slots[panel.index()].as_mut() {
            Some(prev) => prev.maximized = true,
            None => slots[panel.index()] = Some(Bounds { pos, size, maximized }),
        }
        return;
    }
    slots[panel.index()] = Some(Bounds {
        pos,
        size,
        maximized: false,
    });
}

/// 給需要從 async 情境打開面板的地方用：建視窗＋在 collector.log 留一行 `window-open`
///（`created` 看得出這次有沒有真的新建 WebView——驗「關閉＝銷毀」就看它）。
pub async fn show_logged(app: &AppHandle, panel: Panel, via: &str) -> tauri::Result<()> {
    let created = show(app, panel)?;
    crate::applog::write_log(
        app,
        "window-open",
        serde_json::json!({ "window": panel.label(), "created": created, "via": via }),
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Panel;

    /// on_window_event 靠 from_label 分辨「只藏起來」（懸浮窗）和「真的關掉」（設定／統計）。
    #[test]
    fn labels_round_trip_and_the_widget_is_not_a_panel() {
        for p in [Panel::Settings, Panel::Statistics] {
            assert_eq!(Panel::from_label(p.label()), Some(p));
        }
        assert_eq!(Panel::from_label("widget"), None);
    }

    /// 懸浮窗（config）與動態建的視窗參數不一致，WebView2 會拒絕建第二個視窗。
    #[test]
    fn widget_config_uses_the_same_browser_args() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let windows = conf["app"]["windows"].as_array().unwrap();
        assert_eq!(windows.len(), 1, "config 只該留懸浮窗");
        assert_eq!(windows[0]["label"], "widget");
        assert_eq!(windows[0]["additionalBrowserArgs"], super::BROWSER_ARGS);
    }
}
