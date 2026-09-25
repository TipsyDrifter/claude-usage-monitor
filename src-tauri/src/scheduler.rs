use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::AppHandle;
use tokio::sync::Notify;

pub struct Scheduler {
    interval_minutes: AtomicU32,
    notify: Notify,
}

impl Scheduler {
    pub fn new(initial_minutes: u32) -> Arc<Self> {
        Arc::new(Self {
            interval_minutes: AtomicU32::new(initial_minutes),
            notify: Notify::new(),
        })
    }

    pub fn set_interval(&self, minutes: u32) {
        self.interval_minutes
            .store(minutes.max(1), Ordering::Relaxed);
        self.notify.notify_one();
    }

    pub fn run(self: Arc<Self>, app: AppHandle) {
        tauri::async_runtime::spawn(async move {
            // Initial delay so the app finishes booting before the first scrape.
            tokio::time::sleep(Duration::from_secs(3)).await;
            loop {
                let mins = self.interval_minutes.load(Ordering::Relaxed);
                let dur = Duration::from_secs((mins as u64) * 60);

                tokio::select! {
                    _ = tokio::time::sleep(dur) => {
                        let _ = crate::collector::refresh(&app).await;
                    }
                    _ = self.notify.notified() => {
                        // Settings changed (e.g. interval slider moved) —
                        // restart the loop to re-read the cadence.
                    }
                }
            }
        });
    }
}
