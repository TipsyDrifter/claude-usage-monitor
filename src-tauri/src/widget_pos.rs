//! 懸浮窗的位置：開機還原、存檔把關、跑丟了救回來。
//!
//! 懸浮窗沒有工作列按鈕（skipTaskbar），所以只要被最小化（Win+D／Win+M）或位置落在
//! 所有螢幕外，主人就叫不回來——托盤點了是「顯示」，視窗卻待在 (-32000,-32000)。
//! 10-13、10-14 兩次都是這樣：最小化時 Windows 把視窗移到 (-32000,-32000)，onMoved 把這個
//! 座標存進 settings.json，連視窗的「還原位置」也變成 -32000，還原了還是在畫面外。
//! 所以三道關：存檔時不收螢幕外的座標、被最小化就馬上還原、每次顯示前檢查一次在不在螢幕上。

use tauri::WebviewWindow;

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
        let _ = widget.unminimize();
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
}
