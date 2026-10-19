//! v1.10.4（D105）：每小時在 collector.log 記一次 WebView2 整家子用了多少記憶體（`webview-memory`）。
//!
//! 2026-10-19 主人看到一個 renderer 長到 1.8 GB，那個行程重開就沒了，事後只能推論。
//! 有這一行之後，下次再長大就看得出：從哪個小時開始長、長的是哪一種行程、
//! 當時統計／設定視窗開著沒有、有沒有縮小。
//!
//! 量法跟 `tools/measure-webview-memory.ps1` 一樣：從自己的 pid 沿 ParentProcessId 往下走，
//! 收集所有 `msedgewebview2.exe` 後代，讀每個的 PrivateUsage（工作管理員的「記憶體」欄）。

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tauri::{AppHandle, Manager};
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

/// 開機後多久量第一次（讓懸浮窗、第一次刷新先跑完），之後每隔多久量一次。
const FIRST_AFTER: Duration = Duration::from_secs(10 * 60);
const EVERY: Duration = Duration::from_secs(60 * 60);

struct Proc {
    pid: u32,
    parent: u32,
    exe: String,
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

/// 一個行程的私有記憶體（MB）；開不了（已經結束、權限不夠）就是 None。
fn private_mb(pid: u32) -> Option<f64> {
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
        (ok != 0).then(|| c.PrivateUsage as f64 / 1_048_576.0)
    }
}

/// App 本體＋所有 WebView2 後代的私有記憶體。後代裡不是 WebView2 的（例如正在跑的 CLI 探針）不算，
/// 跟 measure-webview-memory.ps1 的 `other:*` 一樣。
fn measure() -> serde_json::Value {
    let me = std::process::id();
    let procs = snapshot();
    let edges: Vec<(u32, u32)> = procs.iter().map(|p| (p.pid, p.parent)).collect();
    let names: HashMap<u32, &str> = procs.iter().map(|p| (p.pid, p.exe.as_str())).collect();
    let mut webview: Vec<f64> = descendants(&edges, me)
        .into_iter()
        .filter(|pid| names.get(pid).is_some_and(|n| n.eq_ignore_ascii_case("msedgewebview2.exe")))
        .filter_map(private_mb)
        .collect();
    webview.sort_by(|a, b| b.total_cmp(a));
    let round = |v: f64| (v * 10.0).round() / 10.0;
    serde_json::json!({
        "app_mb": private_mb(me).map(round),
        "webview_mb": round(webview.iter().sum()),
        "webview_procs": webview.len(),
        "largest_mb": webview.iter().take(3).map(|v| round(*v)).collect::<Vec<_>>(),
    })
}

/// 設定／統計視窗現在開著沒有、有沒有縮小（記憶體長大時第一個要問的就是這個）。
fn panels(app: &AppHandle) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for label in ["statistics", "settings"] {
        if let Some(w) = app.get_webview_window(label) {
            let state = if w.is_minimized().unwrap_or(false) { "minimized" } else { "open" };
            m.insert(label.into(), state.into());
        }
    }
    m.into()
}

pub async fn run(app: AppHandle) {
    tokio::time::sleep(FIRST_AFTER).await;
    loop {
        if let Ok(mut info) = tokio::task::spawn_blocking(measure).await {
            info["panels"] = panels(&app);
            crate::applog::write_log(&app, "webview-memory", info).await;
        }
        tokio::time::sleep(EVERY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{descendants, measure};

    /// 真的呼叫 Win32：測試行程自己的記憶體讀得到，而且沒有 WebView2 後代。
    #[test]
    fn measures_our_own_process() {
        let m = measure();
        assert!(m["app_mb"].as_f64().is_some_and(|v| v > 0.0), "{m}");
        assert_eq!(m["webview_procs"], 0);
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
}
