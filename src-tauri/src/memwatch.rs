//! v1.10.4（D105）：在 collector.log 記 WebView2 整家子用了多少記憶體（`webview-memory`）。
//!
//! 2026-10-19 主人看到一個 renderer 長到 1.8 GB，那個行程重開就沒了，事後只能推論。
//! 有這一行之後，下次再長大就看得出：從哪個小時開始長、長的是哪一種行程、
//! 當時統計／設定視窗開著沒有、有沒有縮小。
//!
//! 量法跟 `tools/measure-webview-memory.ps1` 一樣：從自己的 pid 沿 ParentProcessId 往下走，
//! 收集所有 `msedgewebview2.exe` 後代，讀每個的 PrivateUsage（私有記憶體＝工作管理員的「記憶體」欄）。
//!
//! v1.10.5（D106）：2026-10-21 的現場證明「每小時一行」太慢、也不夠：
//! - 長大的是 WebView2 **主程序**（browser，App 的直接子行程），不是 renderer。現在單獨記
//!   `browser_mb`，另外記整家子的工作集 `webview_ws_mb`——系統缺記憶體時工作集會被修剪，
//!   私有記憶體照樣在漲，兩個數字並排才不會看錯。
//! - 那次的根本是 App 主執行緒被塞住（托盤、視窗查詢都要等它）。現在每次都量一次主執行緒回應
//!   要多久（`main_thread_ms`，逾時記 null），塞住時也不再去查視窗狀態——以前這裡會跟著卡死。
//! - 每 5 分鐘看一次；照舊每小時記一行，另外 WebView2 合計比上一行多了 150 MB 以上、或主執行緒
//!   超過 2 秒才回應，就馬上記（`reason`），事情發生時有每 5 分鐘一行的時間線。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

/// 開機後多久量第一次（讓懸浮窗、第一次刷新先跑完）。
const FIRST_AFTER: Duration = Duration::from_secs(10 * 60);
/// 多久看一次（D106）。看了不一定記，見 [`reason_to_log`]。
const CHECK_EVERY: Duration = Duration::from_secs(5 * 60);
/// 沒有異狀時多久記一行。
const LOG_EVERY: Duration = Duration::from_secs(60 * 60);
/// WebView2 合計比上一行多出這麼多，不等整點就記（D106）。
const GROWTH_MB: f64 = 150.0;
/// 主執行緒回應比這慢就算異狀（D106）。平常是幾毫秒。
const MAIN_THREAD_SLOW_MS: u64 = 2_000;
/// 等主執行緒最多等多久；等不到記 null。
const MAIN_THREAD_TIMEOUT: Duration = Duration::from_secs(10);

const WEBVIEW_EXE: &str = "msedgewebview2.exe";

struct Proc {
    pid: u32,
    parent: u32,
    exe: String,
}

/// 一次量測（MB）。
struct Sample {
    app_mb: Option<f64>,
    /// 整家子 WebView2 的私有記憶體合計。
    webview_mb: f64,
    /// 整家子 WebView2 的工作集合計（D106：跟私有記憶體並排看）。
    webview_ws_mb: f64,
    webview_procs: usize,
    /// 最大的三個（私有記憶體）。
    largest_mb: Vec<f64>,
    /// WebView2 主程序（App 的直接子行程）的私有記憶體（D106）。
    browser_mb: Option<f64>,
}

/// `root` 底下所有後代的 pid（不含 root）。pid 會被重複使用，所以只沿「父 pid 還活著」的邊走，
/// 而且每個 pid 只走一次（不會因為環狀的父子關係卡住）。
fn descendants(procs: &[(u32, u32)], root: u32) -> Vec<u32> {
    let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(pid, parent) in procs {
        if pid != parent {
            kids.entry(parent).or_default().push(pid);
        }
    }
    let mut seen: HashSet<u32> = HashSet::from([root]);
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        for &k in kids.get(&p).map(Vec::as_slice).unwrap_or(&[]) {
            if seen.insert(k) {
                out.push(k);
                stack.push(k);
            }
        }
    }
    out
}

/// WebView2 主程序：App 的**直接**子行程裡的 msedgewebview2.exe（renderer／gpu／utility 都是它的子行程）。
fn browser_pids(procs: &[(u32, u32, &str)], root: u32) -> Vec<u32> {
    procs
        .iter()
        .filter(|(pid, parent, exe)| *parent == root && *pid != root && exe.eq_ignore_ascii_case(WEBVIEW_EXE))
        .map(|(pid, _, _)| *pid)
        .collect()
}

fn snapshot() -> Vec<Proc> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap, &mut e);
        while ok != 0 {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            out.push(Proc {
                pid: e.th32ProcessID,
                parent: e.th32ParentProcessID,
                exe: String::from_utf16_lossy(&e.szExeFile[..len]),
            });
            ok = Process32NextW(snap, &mut e);
        }
        CloseHandle(snap);
    }
    out
}

/// 一個行程的（私有記憶體, 工作集），MB；開不了（已經結束、權限不夠）就是 None。
fn memory_mb(pid: u32) -> Option<(f64, f64)> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut c = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..std::mem::zeroed()
        };
        let ok = GetProcessMemoryInfo(h, &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb);
        CloseHandle(h);
        (ok != 0).then(|| (c.PrivateUsage as f64 / 1_048_576.0, c.WorkingSetSize as f64 / 1_048_576.0))
    }
}

/// App 本體＋所有 WebView2 後代。後代裡不是 WebView2 的（例如正在跑的 CLI 探針）不算，
/// 跟 measure-webview-memory.ps1 的 `other:*` 一樣。
fn measure() -> Sample {
    let me = std::process::id();
    let procs = snapshot();
    let edges: Vec<(u32, u32)> = procs.iter().map(|p| (p.pid, p.parent)).collect();
    let names: HashMap<u32, &str> = procs.iter().map(|p| (p.pid, p.exe.as_str())).collect();
    let mut private = Vec::new();
    let mut ws_total = 0.0;
    for pid in descendants(&edges, me) {
        if !names.get(&pid).is_some_and(|n| n.eq_ignore_ascii_case(WEBVIEW_EXE)) {
            continue;
        }
        if let Some((p, ws)) = memory_mb(pid) {
            private.push(p);
            ws_total += ws;
        }
    }
    let triples: Vec<(u32, u32, &str)> = procs.iter().map(|p| (p.pid, p.parent, p.exe.as_str())).collect();
    let browser: Vec<f64> = browser_pids(&triples, me)
        .into_iter()
        .filter_map(|pid| memory_mb(pid).map(|(p, _)| p))
        .collect();
    private.sort_by(|a, b| b.total_cmp(a));
    Sample {
        app_mb: memory_mb(me).map(|(p, _)| p),
        webview_mb: private.iter().sum(),
        webview_ws_mb: ws_total,
        webview_procs: private.len(),
        largest_mb: private.iter().take(3).copied().collect(),
        browser_mb: (!browser.is_empty()).then(|| browser.iter().sum()),
    }
}

/// 這一次要不要記、記的理由（D106）。`last_logged`＝上一行記下的 WebView2 合計與距今多久；
/// 開機第一次（`None`）一定記。`main_thread_ms` 是 None＝等主執行緒逾時。
fn reason_to_log(
    webview_mb: f64,
    main_thread_ms: Option<u64>,
    last_logged: Option<(f64, Duration)>,
) -> Option<&'static str> {
    if main_thread_ms.is_none_or(|ms| ms >= MAIN_THREAD_SLOW_MS) {
        return Some("main-thread");
    }
    let Some((last_mb, since)) = last_logged else {
        return Some("first");
    };
    if webview_mb - last_mb >= GROWTH_MB {
        return Some("growth");
    }
    (since >= LOG_EVERY).then_some("hourly")
}

/// 主執行緒多久才回應一個空工作（毫秒）；等超過 [`MAIN_THREAD_TIMEOUT`] 就是 None。
/// 托盤 setter、視窗查詢、建視窗都要等它——它塞住時這些全部跟著卡（2026-10-21 現場）。
async fn main_thread_ms(app: &AppHandle) -> Option<u64> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let start = Instant::now();
    app.run_on_main_thread(move || {
        let _ = tx.send(());
    })
    .ok()?;
    match tokio::time::timeout(MAIN_THREAD_TIMEOUT, rx).await {
        Ok(Ok(())) => Some(start.elapsed().as_millis() as u64),
        _ => None,
    }
}

/// 設定／統計視窗現在開著沒有、有沒有縮小（記憶體長大時第一個要問的就是這個）。
/// 查「有沒有縮小」要等主執行緒：放到 blocking 執行緒、加逾時，主執行緒塞住時不拖住這裡（D106）。
async fn panels(app: &AppHandle) -> serde_json::Value {
    let app = app.clone();
    let task = tokio::task::spawn_blocking(move || {
        let mut m = serde_json::Map::new();
        for label in ["statistics", "settings"] {
            if let Some(w) = app.get_webview_window(label) {
                let state = if w.is_minimized().unwrap_or(false) { "minimized" } else { "open" };
                m.insert(label.into(), state.into());
            }
        }
        serde_json::Value::from(m)
    });
    match tokio::time::timeout(MAIN_THREAD_TIMEOUT, task).await {
        Ok(Ok(v)) => v,
        _ => "main-thread-timeout".into(),
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

pub async fn run(app: AppHandle) {
    tokio::time::sleep(FIRST_AFTER).await;
    let mut last_logged: Option<(f64, Instant)> = None;
    loop {
        if let Ok(s) = tokio::task::spawn_blocking(measure).await {
            let mt = main_thread_ms(&app).await;
            let last = last_logged.map(|(mb, at)| (mb, at.elapsed()));
            if let Some(reason) = reason_to_log(s.webview_mb, mt, last) {
                let mut info = serde_json::json!({
                    "reason": reason,
                    "app_mb": s.app_mb.map(round1),
                    "webview_mb": round1(s.webview_mb),
                    "webview_ws_mb": round1(s.webview_ws_mb),
                    "browser_mb": s.browser_mb.map(round1),
                    "webview_procs": s.webview_procs,
                    "largest_mb": s.largest_mb.iter().map(|v| round1(*v)).collect::<Vec<_>>(),
                    "main_thread_ms": mt,
                });
                // 主執行緒沒回應時不去查視窗——那正是會卡住的地方。
                info["panels"] = if mt.is_some() { panels(&app).await } else { "skipped".into() };
                crate::applog::write_log(&app, "webview-memory", info).await;
                last_logged = Some((s.webview_mb, Instant::now()));
            }
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{browser_pids, descendants, measure, reason_to_log, Duration, LOG_EVERY};

    /// 真的呼叫 Win32：測試行程自己的記憶體讀得到，而且沒有 WebView2 後代。
    #[test]
    fn measures_our_own_process() {
        let s = measure();
        assert!(s.app_mb.is_some_and(|v| v > 0.0));
        assert_eq!(s.webview_procs, 0);
        assert!(s.browser_mb.is_none());
    }

    #[test]
    fn walks_the_whole_family_but_not_siblings_or_the_parent() {
        // 10 = 我們；20 = WebView2 browser；30／31 = renderer／gpu；40 = 別的程式；5 = 我們的父行程
        let procs = [(10, 5), (20, 10), (30, 20), (31, 20), (40, 5), (5, 1)];
        let mut d = descendants(&procs, 10);
        d.sort();
        assert_eq!(d, vec![20, 30, 31]);
    }

    #[test]
    fn a_parent_cycle_does_not_loop_forever() {
        // pid 重複使用時可能出現「互為父子」的假邊
        let procs = [(10, 5), (20, 10), (10, 20)];
        assert_eq!(descendants(&procs, 10), vec![20]);
    }

    /// D106：主程序＝我們的直接子行程裡的 msedgewebview2.exe；孫行程（renderer）、別的子行程（探針）不算。
    #[test]
    fn the_browser_is_our_direct_webview2_child() {
        let procs = [
            (20, 10, "msedgewebview2.exe"),
            (30, 20, "msedgewebview2.exe"),
            (40, 10, "claude.exe"),
            (50, 7, "msedgewebview2.exe"),
        ];
        assert_eq!(browser_pids(&procs, 10), vec![20]);
    }

    /// D106：開機第一次記；之後沒異狀就等整點；長大 150 MB 或主執行緒慢／不回應就馬上記。
    #[test]
    fn logs_hourly_and_right_away_when_something_is_off() {
        let min = |m: u64| Duration::from_secs(m * 60);
        assert_eq!(reason_to_log(120.0, Some(3), None), Some("first"));
        assert_eq!(reason_to_log(125.0, Some(3), Some((120.0, min(5)))), None);
        assert_eq!(reason_to_log(125.0, Some(3), Some((120.0, LOG_EVERY))), Some("hourly"));
        assert_eq!(reason_to_log(300.0, Some(3), Some((120.0, min(5)))), Some("growth"));
        assert_eq!(reason_to_log(125.0, Some(2_500), Some((120.0, min(5)))), Some("main-thread"));
        assert_eq!(reason_to_log(125.0, None, Some((120.0, min(5)))), Some("main-thread"));
        // 變小不算長大
        assert_eq!(reason_to_log(40.0, Some(3), Some((900.0, min(5)))), None);
    }
}
