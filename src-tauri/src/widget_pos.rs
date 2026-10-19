//! 懸浮窗的位置：開機還原、存檔把關、跑丟了救回來。
//!
//! 懸浮窗沒有工作列按鈕（skipTaskbar），所以只要被最小化（Win+D／Win+M）或位置落在
//! 所有螢幕外，主人就叫不回來——托盤點了是「顯示」，視窗卻待在 (-32000,-32000)。
//! 10-13、10-14 兩次都是這樣：最小化時 Windows 把視窗移到 (-32000,-32000)，onMoved 把這個
//! 座標存進 settings.json，連視窗的「還原位置」也變成 -32000，還原了還是在畫面外。
//! 所以三道關：存檔時不收螢幕外的座標、被最小化就馬上還原、每次顯示前檢查一次在不在螢幕上。
//!
//! 懸浮窗**永遠不拿焦點**（D104），兩層：
//! 1. tauri.conf.json 的 `focusable: false`＝WS_EX_NOACTIVATE：點它不會啟用它，前景視窗被縮小或
//!    關掉時系統也不會挑它當下一個前景。電腦操作工具每次點擊前把不在允許清單的視窗全縮小、
//!    再看前景是誰（不是允許的 App 就再縮，最多五次）；懸浮窗剛被我們還原、又是置頂，桌面其他
//!    視窗都被縮掉時就輪到它當前景，工具的檢查撞到它就放棄這次點擊——10-16 一場代驗 40 秒內
//!    打了 66 次乒乓。
//! 2. 我們自己的還原與顯示也不啟用：`unminimize()` 是 SW_RESTORE、`show()` 是 SW_SHOW，都會
//!    順便啟用視窗，所以還原改走 Win32 的 SW_SHOWNOACTIVATE，顯示靠 config 的 `focus: false`
//!    （見 [`show_without_focus`]）。事後用 [`is_foreground`] 當絆線，萬一又當上前景就記一行 log。

use tauri::WebviewWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, ShowWindowAsync, SW_SHOWNOACTIVATE,
};

use crate::types::WidgetPosition;

/// 一顆螢幕的實體像素矩形（左上角＋寬高）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct MonitorRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// 這個左上角還落在某一顆螢幕上嗎？往視窗裡面多看一點（右 40、下 16 px）——
/// 只露出一條邊的視窗等於找不到，當作不在。
pub(crate) fn position_on_some_monitor(x: i32, y: i32, monitors: &[MonitorRect]) -> bool {
    let (px, py) = (x.saturating_add(40), y.saturating_add(16));
    monitors
        .iter()
        .any(|m| px >= m.x && px < m.x + m.w && py >= m.y && py < m.y + m.h)
}

pub(crate) fn monitor_rects(window: &WebviewWindow) -> Vec<MonitorRect> {
    window
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| MonitorRect {
            x: m.position().x,
            y: m.position().y,
            w: m.size().width as i32,
            h: m.size().height as i32,
        })
        .collect()
}

pub(crate) fn describe(rects: &[MonitorRect]) -> Vec<String> {
    rects
        .iter()
        .map(|r| format!("({},{})+{}x{}", r.x, r.y, r.w, r.h))
        .collect()
}

/// 存下來的位置可以拿來用嗎？列不出螢幕時無從判斷，照用。
pub(crate) fn usable(saved: Option<WidgetPosition>, rects: &[MonitorRect]) -> Option<WidgetPosition> {
    saved.filter(|p| rects.is_empty() || position_on_some_monitor(p.x, p.y, rects))
}

/// 把被最小化的懸浮窗還原，但不啟用它——前景留在主人（或自動化工具）正在用的視窗。
/// SW_SHOWNOACTIVATE＝「用上次的大小位置顯示，跟 SW_SHOWNORMAL 一樣會解除最小化，只是不啟用」。
/// 用 Async 版：這裡是 tokio 執行緒，不在視窗自己的執行緒上等它處理完。
fn restore_without_focus(widget: &WebviewWindow) {
    match widget.hwnd() {
        Ok(hwnd) => unsafe {
            ShowWindowAsync(hwnd.0 as _, SW_SHOWNOACTIVATE);
        },
        // 拿不到 HWND 幾乎不可能；寧可搶一次焦點，也不要讓懸浮窗卡在最小化回不來。
        Err(_) => {
            let _ = widget.unminimize();
        }
    }
}

/// 顯示懸浮窗而不拿焦點。tauri.conf.json 給懸浮窗設了 `"focus": false`，tao（0.34）會在
/// 視窗旗標裡留一個「別給焦點」的記號，之後每次 `show()` 都用 SW_SHOWNOACTIVATE。
/// 這是 tao 的實作細節不是文件保證——升級 tao 後若 log 出現 `widget-took-focus`，先看這裡。
pub(crate) fn show_without_focus(widget: &WebviewWindow) {
    let _ = widget.show();
}

/// 懸浮窗現在是不是前景視窗（絆線用）。
pub(crate) fn is_foreground(widget: &WebviewWindow) -> bool {
    match widget.hwnd() {
        Ok(hwnd) => std::ptr::eq(unsafe { GetForegroundWindow() }, hwnd.0 as _),
        Err(_) => false,
    }
}

/// 主螢幕右下角（留工作列的高度）。
pub(crate) fn place_default(widget: &WebviewWindow) {
    if let Ok(Some(monitor)) = widget.primary_monitor() {
        let size = monitor.size();
        let scale = monitor.scale_factor();
        let widget_w = 302.0; // card 密度含簷廊
        let widget_h = 250.0;
        let margin = 16.0;
        let logical_w = size.width as f64 / scale;
        let logical_h = size.height as f64 / scale;
        let x = monitor.position().x as f64 / scale + logical_w - widget_w - margin;
        let y = monitor.position().y as f64 / scale + logical_h - widget_h - margin - 48.0;
        let _ = widget.set_position(tauri::LogicalPosition::new(x.max(0.0), y.max(0.0)));
    }
}

/// 懸浮窗要看得到：被最小化就還原，還原後（或本來就）在所有螢幕外，就搬到存檔位置、
/// 存檔位置也不行就搬到主螢幕右下。真的動了才回傳要寫進 collector.log 的內容。
pub(crate) async fn ensure_on_screen(
    widget: &WebviewWindow,
    saved: Option<WidgetPosition>,
) -> Option<serde_json::Value> {
    let minimized = widget.is_minimized().unwrap_or(false);
    if minimized {
        restore_without_focus(widget);
        // 還原是送給視窗執行緒的訊息，等它做完再量位置，不然量到的還是 -32000。
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let rects = monitor_rects(widget);
    let pos = widget.outer_position().ok();
    let on_screen = match pos {
        Some(p) => rects.is_empty() || position_on_some_monitor(p.x, p.y, &rects),
        None => true,
    };
    if on_screen && !minimized {
        return None;
    }
    let moved_to = if on_screen {
        "stayed"
    } else if let Some(p) = usable(saved, &rects) {
        let _ = widget.set_position(tauri::PhysicalPosition::new(p.x, p.y));
        "saved"
    } else {
        place_default(widget);
        "default"
    };
    Some(serde_json::json!({
        "minimized": minimized,
        "was": pos.map(|p| [p.x, p.y]),
        "movedTo": moved_to,
        "saved": saved.map(|p| [p.x, p.y]),
        "monitors": describe(&rects),
    }))
}

#[cfg(test)]
mod tests {
    use super::{position_on_some_monitor, usable, MonitorRect};
    use crate::types::WidgetPosition;

    /// 主人這台實量：主 (0,0)-(1920,1080)、副 (1920,0)-(4480,1600)。
    const TWO: [MonitorRect; 2] = [
        MonitorRect { x: 0, y: 0, w: 1920, h: 1080 },
        MonitorRect { x: 1920, y: 0, w: 2560, h: 1600 },
    ];

    #[test]
    fn a_widget_on_the_secondary_monitor_stays_there() {
        assert!(position_on_some_monitor(3957, 1028, &TWO)); // 09-28 實量位置
        assert!(position_on_some_monitor(4019, 1258, &TWO)); // 09-21 診斷包位置
        assert!(position_on_some_monitor(1600, 900, &TWO)); // 主螢幕右下
    }

    #[test]
    fn a_position_on_an_unplugged_monitor_falls_back() {
        let only_primary = &TWO[..1];
        assert!(!position_on_some_monitor(3957, 1028, only_primary));
        // 主螢幕下方的空白（副螢幕比主螢幕高，但 x 在主螢幕範圍內）
        assert!(!position_on_some_monitor(500, 1300, &TWO));
        assert!(!position_on_some_monitor(-500, 100, &TWO));
    }

    /// 10-13、10-14 實際存進 settings.json 的值：最小化時的座標。
    #[test]
    fn the_minimized_parking_spot_is_never_usable() {
        let parked = Some(WidgetPosition { x: -32000, y: -32000 });
        assert!(!position_on_some_monitor(-32000, -32000, &TWO));
        assert!(usable(parked, &TWO).is_none());
        let fine = Some(WidgetPosition { x: 1676, y: 966 });
        assert!(usable(fine, &TWO).is_some());
        // 列不出螢幕時無從判斷，照用（不要因為查不到螢幕就把好位置丟掉）
        assert!(usable(fine, &[]).is_some());
    }

    /// D104：懸浮窗置頂但永遠不拿焦點，靠的是 config 這三個欄位一起在。
    #[test]
    fn the_widget_is_on_top_but_never_takes_focus() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let w = &conf["app"]["windows"][0];
        assert_eq!(w["label"], "widget");
        assert_eq!(w["alwaysOnTop"], true);
        assert_eq!(w["focus"], false, "focus: false 讓 show() 走 SW_SHOWNOACTIVATE");
        assert_eq!(w["focusable"], false, "focusable: false＝WS_EX_NOACTIVATE，系統不會挑它當前景");
    }
}
