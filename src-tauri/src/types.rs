use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum ScrapeStatus {
    #[default]
    Idle,
    Loading,
    Ok,
    NeedsLogin,
    Error,
}


#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageItem {
    pub used_percent: Option<f32>,
    pub reset_at: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    pub plan_name: Option<String>,
    pub current_session: UsageItem,
    pub weekly_all_models: UsageItem,
    /// M0: the weekly Fable-scoped limit (channel A/C only). Additive field —
    /// pre-M0 frontends simply ignore it.
    #[serde(default)]
    pub weekly_fable: UsageItem,
    pub scraped_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageState {
    pub status: ScrapeStatus,
    pub data: Option<UsageSnapshot>,
    pub error: Option<String>,
    /// M1/D51: non-fatal advisory shown alongside the numbers — e.g.
    /// "Desktop 與 CLI 可能登入不同帳號". None when all is well.
    #[serde(default)]
    pub notice: Option<String>,
    pub last_success_at: Option<String>,
    pub next_refresh_at: Option<String>,
}

// =============================================================================
// AppSettings — nested groups (v0.3 audit #7 refactor)
//
// Why nested? v0.2 was flat (10 sibling fields). v0.3 adds a Statistics window
// with its own settings, so the flat namespace was about to balloon. Grouping
// by concern (general / widget / notifications / webhook / settingsUi /
// statisticsWindow) keeps related toggles together and gives each new
// surface area its own slot to grow into.
//
// Migration: `settings::load` reads the legacy flat shape as a fallback
// (LegacySettingsV1) and upgrades it on the first launch. Users keep their
// previously-tuned values — no need to re-toggle anything.
// =============================================================================

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WidgetPosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneralSettings {
    pub poll_interval_minutes: u32,
    pub auto_start: bool,
    /// D54 低頻心跳：閒置時探針不完全停火，最久這麼多分鐘一發（0 = 關）。
    /// 治「網頁端活動盲區」——瀏覽器用 claude.ai 時 JSONL 門鈴不響，
    /// 心跳保證數字最舊不超過一拍。
    #[serde(default = "default_idle_heartbeat")]
    pub idle_heartbeat_minutes: u32,
    /// D61 全 app 統一的未來時刻顯示模式："relative"（幾小時後）或
    /// "absolute"（週幾幾點）——不再各表面各自為政。
    #[serde(default = "default_time_format")]
    pub time_format: String,
}

fn default_time_format() -> String {
    "absolute".to_string()
}

fn default_idle_heartbeat() -> u32 {
    45
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            poll_interval_minutes: 5,
            auto_start: false,
            idle_heartbeat_minutes: default_idle_heartbeat(),
            time_format: default_time_format(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WidgetSettings {
    pub show: bool,
    pub position: Option<WidgetPosition>,
    /// M2/W5 三段密度: "micro" (220×32) / "card" (280×160) / "panel" (320×280).
    #[serde(default = "default_widget_density")]
    pub density: String,
    /// v1.8.1（D82）主題商店: "w5" 深色儀器（預設）/ "w1" 毛玻璃 / "w4" 環形。
    /// 開放字串，前端 normalizeTheme 不認得就當 w5；舊 settings.json 沒這欄＝w5。
    #[serde(default = "default_widget_theme")]
    pub theme: String,
}

fn default_widget_density() -> String {
    "card".to_string()
}

fn default_widget_theme() -> String {
    "w5".to_string()
}

impl Default for WidgetSettings {
    fn default() -> Self {
        Self {
            show: true,
            position: None,
            density: default_widget_density(),
            theme: default_widget_theme(),
        }
    }
}

/// v1.2 兩級告警（D67-Q16／Q17、D74）：兩個數驅動全 app——留意值＝懸浮窗象牙針
/// 位置＋轉琥珀＋第一級通知；撞牆值＝轉珊瑚＋第二級通知。托盤圖示同一來源。
/// v1.1 以前的 `notifyOnLow`／`lowThreshold` 讀到就丟（serde 忽略未知 key）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationsSettings {
    /// 留意值（%）。預設 70。
    #[serde(default = "default_notice_pct")]
    pub notice_pct: u32,
    /// 撞牆值（%）。預設 90。
    #[serde(default = "default_wall_pct")]
    pub wall_pct: u32,
    /// 懸浮窗額度條隨門檻轉琥珀／珊瑚（預設開）。關掉時三列全程象牙色，針仍畫在留意值。
    #[serde(default = "default_true")]
    pub widget_tint: bool,
    /// Windows 系統通知（預設關，D67-Q7：會打斷工作）。無聲音。
    #[serde(default)]
    pub system_toast: bool,
}

fn default_notice_pct() -> u32 {
    70
}
fn default_wall_pct() -> u32 {
    90
}
fn default_true() -> bool {
    true
}

impl Default for NotificationsSettings {
    fn default() -> Self {
        Self {
            notice_pct: default_notice_pct(),
            wall_pct: default_wall_pct(),
            widget_tint: true,
            system_toast: false,
        }
    }
}

/// 兩級門檻，給 notifier 與托盤圖示用的純數值形。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlertThresholds {
    pub notice: f32,
    pub wall: f32,
}

impl NotificationsSettings {
    /// 夾進合理範圍並保證留意值 < 撞牆值（防手改 settings.json）。
    /// 設定頁自己維持「留意值 ≤ 撞牆值 − 5」，這裡只守底線。
    pub fn normalize(&mut self) {
        self.wall_pct = self.wall_pct.clamp(2, 99);
        self.notice_pct = self.notice_pct.clamp(1, 98);
        if self.notice_pct >= self.wall_pct {
            self.notice_pct = self.wall_pct - 1;
        }
    }

    pub fn thresholds(&self) -> AlertThresholds {
        AlertThresholds {
            notice: self.notice_pct as f32,
            wall: self.wall_pct as f32,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookSettings {
    pub port: u16,
    /// Bearer token required by `POST /refresh`. Empty on first launch;
    /// `settings::load` provisions a fresh random token before save.
    #[serde(default)]
    pub token: String,
}

impl Default for WebhookSettings {
    fn default() -> Self {
        Self {
            port: 17819,
            token: String::new(),
        }
    }
}

/// v1.3（D75 決策點 1）：座位別名。key＝`帳號uuid|組織uuid`（不是 seat id——
/// seat id 是 uuid v4，ledger 重建會換一輪；這一對才跨重建穩定）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountsSettings {
    #[serde(default)]
    pub aliases: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatisticsWindowSettings {
    /// Last page the user was viewing in the statistics window. Restored on
    /// reopen. v0.3 will add more fields here (thresholds, burn-rate window,
    /// chart preferences, etc.).
    #[serde(default = "default_stats_page")]
    pub last_page: String,
}

fn default_stats_page() -> String {
    "overview".to_string()
}

impl Default for StatisticsWindowSettings {
    fn default() -> Self {
        Self {
            last_page: default_stats_page(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    #[serde(default)]
    pub general: GeneralSettings,
    #[serde(default)]
    pub widget: WidgetSettings,
    #[serde(default)]
    pub notifications: NotificationsSettings,
    #[serde(default)]
    pub webhook: WebhookSettings,
    #[serde(default)]
    pub statistics_window: StatisticsWindowSettings,
    #[serde(default)]
    pub accounts: AccountsSettings,
}

// -----------------------------------------------------------------------------
// Legacy flat shape (v0.1 / v0.2 settings.json). Used only for one-shot
// migration in `settings::load`. After the first successful launch on v0.3+
// the on-disk file is rewritten in the nested shape and this is never read
// again.
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacySettingsV1 {
    #[serde(default = "legacy_default_poll_interval")]
    pub poll_interval_minutes: u32,
    #[serde(default = "legacy_default_show_widget")]
    pub show_floating_widget: bool,
    #[serde(default)]
    pub auto_start: bool,
    // v0 的 notify_on_low／low_threshold 不再搬——v1.2 告警是兩級新設定（D74 決策點 2）。
    #[serde(default = "legacy_default_port")]
    pub webhook_port: u16,
    #[serde(default)]
    pub widget_position: Option<WidgetPosition>,
    #[serde(default)]
    pub webhook_token: String,
}

fn legacy_default_poll_interval() -> u32 {
    5
}
fn legacy_default_show_widget() -> bool {
    true
}
fn legacy_default_port() -> u16 {
    17819
}

impl From<LegacySettingsV1> for AppSettings {
    fn from(old: LegacySettingsV1) -> Self {
        Self {
            general: GeneralSettings {
                poll_interval_minutes: old.poll_interval_minutes,
                auto_start: old.auto_start,
                idle_heartbeat_minutes: default_idle_heartbeat(),
                time_format: default_time_format(),
            },
            widget: WidgetSettings {
                show: old.show_floating_widget,
                position: old.widget_position,
                density: default_widget_density(),
                theme: default_widget_theme(),
            },
            notifications: NotificationsSettings::default(),
            webhook: WebhookSettings {
                port: old.webhook_port,
                token: old.webhook_token,
            },
            statistics_window: StatisticsWindowSettings::default(),
            accounts: AccountsSettings::default(),
        }
    }
}
