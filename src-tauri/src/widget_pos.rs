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
//!
//! 懸浮窗**藏起來時 WebView 也看不見**（D107）：顯示／藏起來各只有一個入口
//! （[`show_without_focus`]／[`hide`]），視窗和 WebView 一起切。
//!
//! 懸浮窗**一直在最上層**（D114）：每次顯示都重新放回置頂層（[`reassert_topmost`]），
//! 全螢幕閃避那個每 3 秒的迴圈也順便看一眼（[`topmost_problem`]），置頂掉了就放回去、記一行
//! `widget-topmost-lost`。

use std::time::{Duration, Instant};

use tauri::{Listener, Manager, WebviewWindow};
use tokio::sync::Mutex;
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

/// 懸浮窗的 WebView 現在是不是被設成看不見（D107）。鎖把「視窗＋WebView」整段包成一步：
/// 全螢幕閃避、托盤、設定、按 X 可能同時來，不包起來會交錯成「視窗開著、WebView 卻是藏的」。
/// 顯示時要等頁面畫好（跨 await），所以用 tokio 的鎖。
static WEBVIEW_HIDDEN: Mutex<bool> = Mutex::const_new(false);

/// 前端（`src/widget/painted.ts`）在頁面從 hidden 變回 visible、畫完一格後送出的事件。
pub(crate) const PAINTED_EVENT: &str = "widget://painted";

/// 等頁面畫好新的一格最多等多久；等不到（前端沒載好、事件丟了）也照樣顯示，寧可閃一格舊的
/// 也不能讓懸浮窗叫不出來。
const PAINT_WAIT: Duration = Duration::from_millis(500);

/// WebView 這次從看不見切回看得見：頁面有沒有在時限內回報畫好、等了多久（寫 log 用）。
pub(crate) struct WebviewShown {
    pub painted: bool,
    pub waited_ms: u64,
}

/// 顯示懸浮窗而不拿焦點，WebView 也一起設回看得見（D107）。所有顯示的路徑都經過這裡
/// （`settings::show_widget`：托盤／設定、最小化救回、全螢幕閃避結束）。
///
/// 先 WebView、等頁面回報畫好、才顯示視窗。只排順序不夠：WebView 設回看得見之後，新的一格
/// 要十幾毫秒才畫出來，視窗馬上出來的話，第一格是藏起來前的舊畫面——A／B 截圖 8 輪有 7 輪
/// 閃了一格（`research/2026-10-21-懸浮窗藏起來時停止繪製.md`）。
///
/// 不拿焦點：tauri.conf.json 給懸浮窗設了 `"focus": false`，tao（0.34）會在視窗旗標裡留一個
/// 「別給焦點」的記號，之後每次 `show()` 都用 SW_SHOWNOACTIVATE。這是 tao 的實作細節不是文件
/// 保證——升級 tao 後若 log 出現 `widget-took-focus`，先看這裡。WebView 那一步（wry
/// `set_visible`）是對子視窗 ShowWindow＋`SetIsVisible`，子視窗不會被啟用。
pub(crate) async fn show_without_focus(widget: &WebviewWindow) -> Option<WebviewShown> {
    let mut hidden = WEBVIEW_HIDDEN.lock().await;
    let mut shown = None;
    if *hidden {
        // 先掛好接收端再叫 WebView 回來，事件才不會在掛上之前就送到。
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let app = widget.app_handle();
        let id = app.once(PAINTED_EVENT, move |_| {
            let _ = tx.send(());
        });
        let webview: &tauri::Webview = widget.as_ref();
        if webview.show().is_ok() {
            *hidden = false;
            let t0 = Instant::now();
            let painted = tokio::time::timeout(PAINT_WAIT, rx).await.is_ok_and(|r| r.is_ok());
            shown = Some(WebviewShown {
                painted,
                waited_ms: t0.elapsed().as_millis() as u64,
            });
        }
        app.unlisten(id);
    }
    let _ = widget.show();
    // D114：每次顯示都重新放回置頂層，托盤／設定的「顯示」才是救得回來的出口。
    if let Some(hwnd) = hwnd_of(widget) {
        reassert_topmost(hwnd);
    }
    shown
}

/// 把懸浮窗藏起來，WebView 也設成看不見（D107）。所有藏起來的路徑都經過這裡
/// （`settings::hide_widget`：全螢幕閃避、按 X、設定關掉懸浮窗）。
///
/// 只 `hide()` 視窗的話，WebView2 不知道自己沒人看（它是子視窗，收不到頂層被藏的訊息），
/// 頁面的動畫照樣每秒畫 60 張——W1／W4 警戒時的閃爍、W5 刷新時的轉圈。主人看影片、玩遊戲
/// 時懸浮窗正好被全螢幕閃避藏著，背景不該有任何繪製。跟 D105 面板最小化同一招：
/// `Webview::hide()`＝`SetIsVisible(false)`，頁面變 hidden，Chromium 停掉動畫與 rAF。
///
/// 回傳 true＝WebView 這次從看得見切成看不見（呼叫端寫 log 用）。
pub(crate) async fn hide(widget: &WebviewWindow) -> bool {
    let mut hidden = WEBVIEW_HIDDEN.lock().await;
    let _ = widget.hide();
    if *hidden {
        return false;
    }
    let webview: &tauri::Webview = widget.as_ref();
    if webview.hide().is_err() {
        return false;
    }
    *hidden = true;
    true
}

/// 懸浮窗現在是不是前景視窗（絆線用）。
pub(crate) fn is_foreground(widget: &WebviewWindow) -> bool {
    match widget.hwnd() {
        Ok(hwnd) => std::ptr::eq(unsafe { GetForegroundWindow() }, hwnd.0 as _),
        Err(_) => false,
    }
}

/// 懸浮窗的 HWND 換成整數，好丟進 `spawn_blocking`（指標不是 Send）。
pub(crate) fn hwnd_of(widget: &WebviewWindow) -> Option<isize> {
    widget.hwnd().ok().map(|h| h.0 as isize)
}

/// 把懸浮窗放回置頂那一層的最上面（D114）。tao 只在 alwaysOnTop 設定**改變**時才呼叫
/// SetWindowPos(HWND_TOPMOST)，藏起來再顯示、改 exstyle 都不會重新排 z-order，所以置頂一旦被
/// 弄丟，從設定開關懸浮窗也救不回來——11-07 主人就是這樣，要點一下懸浮窗才回到上面。
/// 用 ASYNC：從 tokio 執行緒發，不在 App 主執行緒上等它（D106 托盤卡主執行緒的教訓）。
pub(crate) fn reassert_topmost(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };
    unsafe {
        SetWindowPos(
            hwnd as _,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_ASYNCWINDOWPOS | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 懸浮窗看得到、置頂卻出了問題嗎（D114）？兩種：
/// - `bitLost`：WS_EX_TOPMOST 被拿掉了（別的程式對它下了 HWND_NOTOPMOST 之類）；
/// - `coveredBy`：旗標還在，z-order 裡卻有一個看得到、不是置頂、跟懸浮窗重疊的視窗排在它前面。
///   正常的 Windows 不會這樣（置頂的一律排在普通視窗前面），出現就是 z-order 亂了。
///
/// 沒問題回 None；有問題回要寫進 collector.log 的內容（誰壓住它、當時前景是誰），
/// 根因還沒抓到，下次發生時靠這行分辨是哪一種。只讀視窗屬性、不送訊息，可以在任何執行緒跑。
pub(crate) fn topmost_problem(hwnd: isize) -> Option<serde_json::Value> {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowLongW, GetWindowRect, IsIconic, IsWindowVisible, GWL_EXSTYLE,
        WS_EX_TOPMOST,
    };

    struct Scan {
        target: HWND,
        rect: RECT,
        covered_by: Option<HWND>,
    }

    unsafe extern "system" fn visit(h: HWND, lparam: LPARAM) -> i32 {
        let scan = &mut *(lparam as *mut Scan);
        if h == scan.target {
            return 0; // 走到懸浮窗自己了，前面都看完
        }
        if IsWindowVisible(h) == 0 || IsIconic(h) != 0 {
            return 1;
        }
        if GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0 {
            return 1;
        }
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if GetWindowRect(h, &mut r) == 0 || r.right <= r.left || r.bottom <= r.top {
            return 1;
        }
        let w = scan.rect;
        if r.left < w.right && r.right > w.left && r.top < w.bottom && r.bottom > w.top {
            scan.covered_by = Some(h);
            return 0;
        }
        1
    }

    unsafe {
        let target = hwnd as HWND;
        if IsWindowVisible(target) == 0 {
            return None;
        }
        let bit_lost = GetWindowLongW(target, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST == 0;
        let mut scan = Scan {
            target,
            rect: RECT { left: 0, top: 0, right: 0, bottom: 0 },
            covered_by: None,
        };
        if GetWindowRect(target, &mut scan.rect) == 0 {
            return None;
        }
        if !bit_lost {
            EnumWindows(Some(visit), &mut scan as *mut Scan as LPARAM);
            scan.covered_by?;
        }
        Some(serde_json::json!({
            "bitLost": bit_lost,
            "coveredBy": scan.covered_by.map(describe_window),
            "foreground": describe_window(GetForegroundWindow()),
        }))
    }
}

/// log 用：視窗的 class 和所屬程式的 exe 名。不記標題——檔案總管的標題是資料夾名稱，
/// collector.log 會進診斷包。
fn describe_window(h: windows_sys::Win32::Foundation::HWND) -> serde_json::Value {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindowThreadProcessId};
    if h.is_null() {
        return serde_json::Value::Null;
    }
    unsafe {
        let mut buf = [0u16; 260];
        let n = GetClassNameW(h, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
        let class = String::from_utf16_lossy(&buf[..n]);
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, &mut pid);
        let mut exe = String::new();
        let ph = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !ph.is_null() {
            let mut len = buf.len() as u32;
            if QueryFullProcessImageNameW(ph, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0 {
                let path = String::from_utf16_lossy(&buf[..len as usize]);
                exe = path.rsplit('\\').next().unwrap_or("").to_string();
            }
            CloseHandle(ph);
        }
        serde_json::json!({ "class": class, "exe": exe })
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

    /// D114 手動驗：對正在跑的懸浮窗量一次置頂狀況、有問題就放回去。
    /// `$env:WIDGET_HWND=0x30272; cargo test --lib live_widget_topmost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_widget_topmost() {
        let raw = std::env::var("WIDGET_HWND").expect("WIDGET_HWND");
        let hwnd = isize::from_str_radix(raw.trim_start_matches("0x"), 16).unwrap();
        let problem = super::topmost_problem(hwnd);
        println!("problem: {problem:?}");
        if problem.is_some() {
            super::reassert_topmost(hwnd);
            std::thread::sleep(std::time::Duration::from_millis(300));
            println!("after reassert: {:?}", super::topmost_problem(hwnd));
        }
    }

    /// D107：懸浮窗的顯示／藏起來只准走 `show_without_focus`／`hide`（視窗和 WebView 一起切）。
    /// 別的檔案直接 `.hide()`／`.show()`，藏起來時 WebView 就還看得見、動畫照跑——
    /// 以前全螢幕閃避、按 X、設定關掉三處各自 `hide()`，就是這樣漏掉的。
    /// 例外：windows.rs 管的是設定／統計視窗（D105 自己切 WebView），notifier 的 `.show()` 是通知。
    #[test]
    fn widget_show_and_hide_go_through_one_door() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut hits = Vec::new();
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                if !name.ends_with(".rs") || matches!(name.as_str(), "widget_pos.rs" | "windows.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                for (i, line) in text.lines().enumerate() {
                    let code = line.split("//").next().unwrap_or("");
                    let shows = code.contains(".show()") && name != "notifier.rs";
                    if code.contains(".hide()") || shows {
                        hits.push(format!("{}:{}: {}", name, i + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            hits.is_empty(),
            "懸浮窗請用 settings::show_widget／hide_widget：\n{}",
            hits.join("\n")
        );
    }

    /// D107：「畫好了」的事件名前後端各寫一次，對不上的話顯示時每次都乾等 500 ms、還是閃舊畫面。
    #[test]
    fn the_painted_event_name_matches_the_frontend() {
        let ts = include_str!("../../src/widget/painted.ts");
        let needle = format!("PAINTED_EVENT = \"{}\"", super::PAINTED_EVENT);
        assert!(ts.contains(&needle), "src/widget/painted.ts 要有 {needle}");
    }

    /// D107：螢幕保護事件名前後端各寫一次，對不上的話螢幕保護時動畫照跑。
    #[test]
    fn the_screensaver_event_name_matches_the_frontend() {
        let ts = include_str!("../../src/lib/motion.ts");
        let needle = format!("SCREENSAVER_EVENT = \"{}\"", crate::fullscreen::SCREENSAVER_EVENT);
        assert!(ts.contains(&needle), "src/lib/motion.ts 要有 {needle}");
    }

    /// D106：前端不准再用 CSS 的 app-region（含 React 行內樣式的 WebkitAppRegion）。
    /// wry 對每個 WebView2 都開了 IsNonClientRegionSupportEnabled，拖曳區裡只要有東西在動，
    /// Blink 每一幀都送一次 DraggableRegionsChanged，App 主執行緒每秒重排幾百次子視窗——
    /// 2026-10-21 WebView2 主程序因此排了幾十萬則訊息、長到 3.9 GB。拖曳走 widget/drag.ts。
    #[test]
    fn the_frontend_never_uses_css_drag_regions() {
        fn walk(dir: &std::path::Path, hits: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, hits);
                    continue;
                }
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !matches!(ext, "css" | "ts" | "tsx" | "html") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                for (i, line) in text.lines().enumerate() {
                    let squashed: String = line.chars().filter(|c| !c.is_whitespace()).collect();
                    let l = squashed.to_ascii_lowercase();
                    if l.contains("app-region:") || l.contains("appregion:") {
                        hits.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                    }
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../src");
        let mut hits = Vec::new();
        walk(&src, &mut hits);
        assert!(hits.is_empty(), "拖曳改用 widget/drag.ts，不要用 app-region：\n{}", hits.join("\n"));
        // 前端的 html 入口也一起看（不在 src 底下）
        for page in ["widget.html", "settings.html", "statistics.html", "index.html"] {
            let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(page);
            if let Ok(text) = std::fs::read_to_string(&p) {
                let l: String = text.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
                assert!(!l.contains("app-region:"), "{} 用了 app-region", p.display());
            }
        }
    }
}
