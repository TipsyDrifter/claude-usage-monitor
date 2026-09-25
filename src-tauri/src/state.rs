use std::sync::Arc;
use tokio::sync::{watch, RwLock};

use crate::notifier::{Alert, AlarmMemory};
use crate::types::{AlertThresholds, AppSettings, UsageSnapshot, UsageState};

#[derive(Default)]
pub struct AppStateInner {
    pub usage: UsageState,
    pub settings: AppSettings,
    /// v1.2: per-limit "already notified this window" memory (RAM only, D74).
    pub alerts: AlarmMemory,
}

#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<RwLock<AppStateInner>>,
    /// v1.8.7（D86）：「設定已從磁碟載入」的旗標。三個 webview 在 `setup` 之前就建好，
    /// 早到的 `get_settings` 以前直接拿到 `AppSettings::default()`（主題 w5、密碼牌空），
    /// 靠 3 秒重試＋一次廣播補救——開機自啟時磁碟慢就補不到，懸浮窗就穿回預設臉。
    /// 現在 `get_settings` 會等這面旗子，載入前不再發預設值。
    loaded: Arc<watch::Sender<bool>>,
}

impl Default for AppState {
    fn default() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self {
            inner: Arc::new(RwLock::new(AppStateInner::default())),
            loaded: Arc::new(tx),
        }
    }
}

impl AppState {
    pub async fn get_usage(&self) -> UsageState {
        self.inner.read().await.usage.clone()
    }

    pub async fn get_settings(&self) -> AppSettings {
        self.inner.read().await.settings.clone()
    }

    /// Flip the "settings loaded from disk" flag (called once by `setup`).
    /// `send_replace`, not `send`: `watch::Sender::send` refuses to store the
    /// value while no receiver exists (the initial one is dropped in
    /// `Default`), and the first 1.8.7 build shipped exactly that — every
    /// window waited the full 20 s before giving up (`settings-waited
    /// loaded:false`). `send_replace` always stores; late subscribers see it.
    pub fn mark_loaded(&self) {
        self.loaded.send_replace(true);
    }

    pub fn is_loaded(&self) -> bool {
        *self.loaded.borrow()
    }

    /// Wait until settings are loaded, or until `timeout` passes. Returns
    /// whether the flag is set (false = gave up, caller gets whatever is in
    /// state — the same defaults it would have gotten before v1.8.7).
    pub async fn wait_loaded(&self, timeout: std::time::Duration) -> bool {
        if self.is_loaded() {
            return true;
        }
        let mut rx = self.loaded.subscribe();
        tokio::time::timeout(timeout, rx.wait_for(|v| *v))
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false)
    }

    pub async fn set_settings(&self, settings: AppSettings) {
        self.inner.write().await.settings = settings;
    }

    pub async fn set_usage(&self, usage: UsageState) {
        self.inner.write().await.usage = usage;
    }

    /// Run the debounced alert decision against the shared memory.
    pub async fn observe_alerts(&self, next: &UsageSnapshot, th: AlertThresholds) -> Vec<Alert> {
        self.inner.write().await.alerts.observe(next, th)
    }
}
