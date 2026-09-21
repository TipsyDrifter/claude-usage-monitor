use std::sync::Arc;
use tokio::sync::RwLock;

use crate::notifier::{Alert, AlarmMemory};
use crate::types::{AlertThresholds, AppSettings, UsageSnapshot, UsageState};

#[derive(Default)]
pub struct AppStateInner {
    pub usage: UsageState,
    pub settings: AppSettings,
    /// v1.2: per-limit "already notified this window" memory (RAM only, D74).
    pub alerts: AlarmMemory,
}

#[derive(Clone, Default)]
pub struct AppState(pub Arc<RwLock<AppStateInner>>);

impl AppState {
    pub async fn get_usage(&self) -> UsageState {
        self.0.read().await.usage.clone()
    }

    pub async fn get_settings(&self) -> AppSettings {
        self.0.read().await.settings.clone()
    }

    pub async fn set_settings(&self, settings: AppSettings) {
        self.0.write().await.settings = settings;
    }

    pub async fn set_usage(&self, usage: UsageState) {
        self.0.write().await.usage = usage;
    }

    /// Run the debounced alert decision against the shared memory.
    pub async fn observe_alerts(&self, next: &UsageSnapshot, th: AlertThresholds) -> Vec<Alert> {
        self.0.write().await.alerts.observe(next, th)
    }
}
