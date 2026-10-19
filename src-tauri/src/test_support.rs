//! 測試用暫存資料夾：跑完自己收掉，不在 %TEMP% 留一堆 `cum-*-<pid>`。

use std::path::PathBuf;

use crate::ledger::Ledger;

/// `%TEMP%\cum-<tag>-<pid>`，離開作用域就整個刪掉。
/// Deref 到 PathBuf，原本拿 PathBuf 用的地方（`&dir`、`dir.join`、`dir.clone()`）照寫。
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cum-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl std::ops::Deref for TempDir {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for TempDir {
    /// 帳本的背景重算（tauri 全域 runtime 上）可能比測試多抱著連線幾毫秒，
    /// Windows 上檔案還開著就刪不掉——重試一小段時間等它放手。
    fn drop(&mut self) {
        for _ in 0..100 {
            if std::fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// 開在 TempDir 裡的帳本，Deref 到 Ledger。
/// 欄位順序＝drop 順序：先關帳本連線，再刪資料夾（Windows 上開著的 sqlite 檔刪不掉）。
/// 別的 Ledger clone（背景重算）還沒放手時，交給 TempDir 的重試等。
pub struct TempLedger {
    ledger: Ledger,
    _dir: TempDir,
}

impl TempLedger {
    pub fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let ledger = Ledger::open(dir.to_path_buf()).unwrap();
        Self { ledger, _dir: dir }
    }
}

impl std::ops::Deref for TempLedger {
    type Target = Ledger;
    fn deref(&self) -> &Ledger {
        &self.ledger
    }
}
