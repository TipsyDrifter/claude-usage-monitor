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
    /// v1.8.13（D91 T-04）：這個數字的樣本時刻（RFC 3339）。拿來判斷「重置時刻已過、
    /// 而數字是重置前量的」——那就是上一個窗的值，畫面要寫「已重置」而不是照舊顯示。
    #[serde(default)]
    pub sampled_at: Option<String>,
    /// v1.9.4（線 W 交辦）：這個數字來自哪個管道的短標籤（`"probe"`／`"cli"`／`"desktop"`，
    /// 同 `Source::label()`）。以前前端得從 `note`（"cli · 16 分前"）字串尾端拆出來；
    /// 現在直接讀這個欄位。`note` 照舊保留、不改名。
    #[serde(default)]
    pub source_label: Option<String>,
    /// v1.9.5（D100 E2）：這條額度在目前方案不存在——伺服器的額度清單（CLI 快取 `limits[]`）連續 3 次
    /// 都沒有它。懸浮窗／托盤寫「—」＋「此方案沒有這條」；這時其他欄位都是空的。舊前端忽略也不壞。
    #[serde(default)]
    pub not_applicable: bool,
}

/// v1.9.5（D100 F1）：Extra usage（超額付費）。判斷用 `extra_usage.is_enabled && !spend_limit_reached`——
/// **不用** `oauthAccount.hasExtraUsageEnabled`（主人那個旗標是 true，但 429 全是 overage rejected）。
/// 只改措辭與旗標，不動任何裁決／預測（U5 沒驗：開了超額 % 會不會超過 100）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtraUsageInfo {
    /// `extra_usage.is_enabled`
    pub enabled: bool,
    /// 用完方案額度後會轉超額付費＝`enabled && !spend_limit_reached`
    pub active: bool,
    pub spend_limit_reached: bool,
    /// 例如 `out_of_credits`
    #[serde(default)]
    pub disabled_reason: Option<String>,
}

/// v1.9.5（D100 E1）：伺服器有、App 還不會畫的那條額度（`limits[]` 裡認不得的）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnknownLimit {
    pub kind: String,
    /// scope 的顯示字（模型名，或「模型 · surface」）；沒有 scope 是 None。
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub percent: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<String>,
}

/// v1.8.13（D91 T-04）：重置後這麼短時間內寫下的樣本仍當上一個窗的——Desktop 會在重置後
/// 0.6 秒寫一筆舊窗的 100%（最終驗收 L9 機制實證）。
pub const RESET_GRACE_SECS: i64 = 60;

impl UsageItem {
    /// v1.8.13（D91 T-04）：這個數字屬於一個已經重置掉的窗嗎？條件＝重置時刻已過，且
    /// （沒有樣本時刻，或樣本早於重置時刻＋容差）。前端 `shared.tsx isExpired` 同一個口徑。
    pub fn is_expired_at(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        let parse = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|t| t.with_timezone(&chrono::Utc))
        };
        let Some(reset) = self.reset_at.as_deref().and_then(parse) else {
            return false;
        };
        if reset >= now {
            return false;
        }
        match self.sampled_at.as_deref().and_then(parse) {
            Some(s) => s < reset + chrono::Duration::seconds(RESET_GRACE_SECS),
            None => true,
        }
    }
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
    /// v1.9.5（D100 F1）：目前帳號的 Extra usage；讀不到 CLI 快取（或快取是別的帳號的）是 None。
    #[serde(default)]
    pub extra_usage: Option<ExtraUsageInfo>,
    /// v1.9.5（D100 E1）：伺服器有、App 還不會畫的額度；沒有就空陣列。
    #[serde(default)]
    pub unknown_limits: Vec<UnknownLimit>,
}

impl UsageSnapshot {
    /// v1.9.5（D100 F1）：開了超額付費、而且還沒花到上限。
    pub fn extra_usage_active(&self) -> bool {
        self.extra_usage.as_ref().is_some_and(|x| x.active)
    }
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
    /// v1.8.9（D88，主人）：查額度的最少間隔（分鐘）。有在用（本機新紀錄／門鈴）時，
    /// CLI 快取比這個舊、且距上次探針超過這麼久才再查；額度 ≥90% 放寬到 2 分。
    /// 以前是寫死的 10 分（快取過期）＋3 分（探針地板）。
    #[serde(default = "default_probe_min")]
    pub probe_min_minutes: u32,
    /// v1.8.10（D88 追加，主人）：5h 額度到警戒值以上時改用的更短間隔（分鐘）。
    /// 以前寫死「≥90% → 2 分」；門檻現在跟告警的警戒值連動，間隔由這裡調。
    #[serde(default = "default_probe_hot")]
    pub probe_hot_minutes: u32,
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

fn default_probe_min() -> u32 {
    10
}

fn default_probe_hot() -> u32 {
    2
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            poll_interval_minutes: 5,
            // v1.8.4（主人 2026-09-21）：App 要常駐帳本才會累積，新安裝預設就開；
            // 既有 settings.json 的值照舊。
            auto_start: true,
            idle_heartbeat_minutes: default_idle_heartbeat(),
            probe_min_minutes: default_probe_min(),
            probe_hot_minutes: default_probe_hot(),
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

/// v1.10（D98／D101）：自己的預算。兩個帳號共用一份規則，數字各算各的。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetSettings {
    /// 關掉＝今天的線不畫、預算提醒不響、MCP 不回 todayLine。預設開。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 一天從幾點開始算（本地時間 0～23，預設 6——主人常用到凌晨三點後）。
    #[serde(default = "default_day_start_hour")]
    pub day_start_hour: u32,
}

fn default_day_start_hour() -> u32 {
    6
}

impl Default for BudgetSettings {
    fn default() -> Self {
        Self { enabled: true, day_start_hour: default_day_start_hour() }
    }
}

impl BudgetSettings {
    pub fn normalize(&mut self) {
        self.day_start_hour = self.day_start_hour.min(23);
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
    #[serde(default)]
    pub budget: BudgetSettings,
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

// =============================================================================
// v1.9.4（D99）方案（plan）— 唯一定義。
//
// 座位的額度池大小由「方案等級」決定（Max 20x 的池是 Max 5x 的 4 倍）。App 自己算的數字
// （匯率、縮水判定、固定籃、模型倍率、方案建議）都默認「同一座位的池不變」，所以帳本要記
// 座位在什麼時候是什麼方案（`seat_epochs`），統計才知道哪些時段可以互比。
// 方案名稱、倍數、可比性的判斷都收在這裡；ledger／collector／mcp 一律引用，不另寫第二份。
// =============================================================================

/// 方案類別（方案階梯、顯示名、可比性）。可比性的鍵 [`plan_identity`] 就是從它來的：
/// 認得的等級用等級本身，認不得的（`Other`）才退回 tier／organizationType 的原字串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind {
    Free,
    Pro,
    Max5x,
    Max20x,
    Team,
    Enterprise,
    /// 認不得的字串（含只寫「max」沒寫倍數的）：不給方案建議，也不猜倍數。
    Other,
}

impl PlanKind {
    /// 從 `oauthAccount` 的 `organizationRateLimitTier`＋`organizationType` 判斷**方案等級**。
    /// `seatTier` 不參與——它出現與否、換不換都不代表池的大小變了（審查 21r1 S1）。
    /// 逐「詞」比對（以非英數字切開）而不是子字串：`max_25x`、`max_120x` 不會被誤判成 5x／20x。
    /// Team／Enterprise 先判（Team 席次的 tier 字串可能長得像 Max）；字串實況未驗（U1／G4）——
    /// 認不得就 `Other`，絕不猜。
    pub fn classify(tier: Option<&str>, org_type: Option<&str>) -> Self {
        let hay = [tier, org_type].iter().flatten().map(|s| s.to_lowercase()).collect::<Vec<_>>().join(" ");
        let tokens: Vec<&str> = hay.split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| !t.is_empty()).collect();
        let has = |w: &str| tokens.contains(&w);
        if tokens.iter().any(|t| t.starts_with("enterprise")) {
            PlanKind::Enterprise
        } else if has("team") || has("teams") {
            PlanKind::Team
        } else if has("20x") {
            PlanKind::Max20x
        } else if has("5x") {
            PlanKind::Max5x
        } else if has("free") {
            PlanKind::Free
        } else if has("pro") && !has("max") {
            PlanKind::Pro
        } else {
            PlanKind::Other
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            PlanKind::Free => "free",
            PlanKind::Pro => "pro",
            PlanKind::Max5x => "max_5x",
            PlanKind::Max20x => "max_20x",
            PlanKind::Team => "team",
            PlanKind::Enterprise => "enterprise",
            PlanKind::Other => "other",
        }
    }

    /// 階梯上的方案顯示名（方案建議的目標用；跟 `plan_label` 對 tier 字串的輸出一致）。
    pub fn display(self) -> &'static str {
        match self {
            PlanKind::Free => "Claude Free",
            PlanKind::Pro => "Claude Pro",
            PlanKind::Max5x => "Claude Max 5x",
            PlanKind::Max20x => "Claude Max 20x",
            PlanKind::Team => "Claude Team",
            PlanKind::Enterprise => "Claude Enterprise",
            PlanKind::Other => "Claude",
        }
    }

    /// 池相對於 Pro 的倍數（Pro＝1、Max 5x＝5、Max 20x＝20）。其他方案沒有可靠倍數。
    pub fn pool_multiple(self) -> Option<f64> {
        match self {
            PlanKind::Pro => Some(1.0),
            PlanKind::Max5x => Some(5.0),
            PlanKind::Max20x => Some(20.0),
            _ => None,
        }
    }

    /// 下一級（升級目標）。Max 20x 沒有上一級——上面只有 extra usage 或多訂一個帳號。
    pub fn next_up(self) -> Option<PlanKind> {
        match self {
            PlanKind::Pro => Some(PlanKind::Max5x),
            PlanKind::Max5x => Some(PlanKind::Max20x),
            _ => None,
        }
    }

    /// 下一級（降級目標）。Pro 下面只有 Free，不算建議。
    pub fn next_down(self) -> Option<PlanKind> {
        match self {
            PlanKind::Max20x => Some(PlanKind::Max5x),
            PlanKind::Max5x => Some(PlanKind::Pro),
            _ => None,
        }
    }
}

/// 兩個方案之間池的倍數（`from` 的池 ÷ `to` 的池，兩邊都要有倍數）：Max 20x→Max 5x＝4、
/// Max 5x→Pro＝5。降級後同樣的用量在新方案的百分比＝原百分比 × 這個倍數。
pub fn plan_step_factor(from: PlanKind, to: PlanKind) -> Option<f64> {
    Some(from.pool_multiple()? / to.pool_multiple()?)
}

/// 可比性的鍵：**只看方案等級**。認得的等級（Pro／Max 5x／Max 20x／Team…）用 [`PlanKind::key`]；
/// 認不得的（`Other`）退回 `other|tier|organizationType` 的小寫原字串。`seatTier` 不參與。
/// tier 與 organizationType 都沒讀到＝這一輪沒有方案資訊 → None（不當成「方案變了」，也不開新段）。
/// 兩段時間「同一個方案等級」＝這個鍵相同。
pub fn plan_identity(tier: Option<&str>, org_type: Option<&str>) -> Option<String> {
    let norm = |s: Option<&str>| s.map(|v| v.trim().to_lowercase()).unwrap_or_default();
    let (t, o) = (norm(tier), norm(org_type));
    if t.is_empty() && o.is_empty() {
        return None;
    }
    Some(match PlanKind::classify(Some(&t), Some(&o)) {
        PlanKind::Other => format!("other|{t}|{o}"),
        k => k.key().to_string(),
    })
}

/// 顯示用的方案名。`default_claude_max_20x` → "Claude Max 20x"（把 tier 去掉 `default_` 前綴、
/// 底線換空白、每個字首大寫）；沒有 tier 就看 organizationType，再沒有就用組織名。
pub fn plan_label(tier: Option<&str>, org_type: Option<&str>, org_name: Option<&str>) -> String {
    // 階梯上認得的方案一律用標準名（Pro 的 tier 字串可能是 default_claude_ai，機械轉出來會是「Claude Ai」）
    if tier.is_some_and(|t| !t.is_empty()) || org_type.is_some_and(|t| !t.is_empty()) {
        if let k @ (PlanKind::Pro | PlanKind::Max5x | PlanKind::Max20x) = PlanKind::classify(tier, org_type) {
            return k.display().to_string();
        }
    }
    if let Some(tier) = tier.filter(|t| !t.is_empty()) {
        let trimmed = tier.trim_start_matches("default_");
        return trimmed
            .split('_')
            .map(|w| {
                let mut chars = w.chars();
                match chars.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
    }
    if let Some(t) = org_type.filter(|t| !t.is_empty()) {
        return match t {
            "claude_max" => "Claude Max".to_string(),
            "claude_pro" => "Claude Pro".to_string(),
            other => other.to_string(),
        };
    }
    org_name.filter(|n| !n.is_empty()).map(str::to_string).unwrap_or_else(|| "Claude".to_string())
}

impl From<LegacySettingsV1> for AppSettings {
    fn from(old: LegacySettingsV1) -> Self {
        Self {
            general: GeneralSettings {
                poll_interval_minutes: old.poll_interval_minutes,
                auto_start: old.auto_start,
                idle_heartbeat_minutes: default_idle_heartbeat(),
                probe_min_minutes: default_probe_min(),
                probe_hot_minutes: default_probe_hot(),
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
            budget: BudgetSettings::default(),
        }
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    #[test]
    fn classify_known_tiers() {
        let k = |t: &str, o: &str| PlanKind::classify(Some(t), Some(o));
        assert_eq!(k("default_claude_max_20x", "claude_max"), PlanKind::Max20x);
        assert_eq!(k("default_claude_max_5x", "claude_max"), PlanKind::Max5x);
        assert_eq!(PlanKind::classify(None, Some("claude_pro")), PlanKind::Pro);
        assert_eq!(k("default_claude_ai", "claude_team"), PlanKind::Team);
        assert_eq!(k("default_claude_max_5x", "claude_enterprise"), PlanKind::Enterprise);
        assert_eq!(k("default_claude_max", "claude_max"), PlanKind::Other, "只寫 max 沒寫倍數＝不猜");
        assert_eq!(PlanKind::classify(None, None), PlanKind::Other);
    }

    /// 21r1 nit 2：逐詞比對，不是子字串——`max_25x`／`max_120x`／`max_15x` 不是 5x／20x。
    #[test]
    fn classify_matches_whole_words_not_substrings() {
        let k = |t: &str| PlanKind::classify(Some(t), Some("claude_max"));
        assert_eq!(k("default_claude_max_25x"), PlanKind::Other);
        assert_eq!(k("default_claude_max_15x"), PlanKind::Other);
        assert_eq!(k("default_claude_max_120x"), PlanKind::Other);
        assert_eq!(k("default_claude_max_20x"), PlanKind::Max20x);
        assert_eq!(k("default-claude-max-5x"), PlanKind::Max5x, "分隔符不限底線");
    }

    #[test]
    fn step_factors_follow_the_ladder() {
        assert_eq!(plan_step_factor(PlanKind::Max20x, PlanKind::Max5x), Some(4.0));
        assert_eq!(plan_step_factor(PlanKind::Max5x, PlanKind::Pro), Some(5.0));
        assert_eq!(plan_step_factor(PlanKind::Pro, PlanKind::Max5x), Some(0.2));
        assert_eq!(plan_step_factor(PlanKind::Team, PlanKind::Pro), None);
        assert_eq!(PlanKind::Max20x.next_up(), None, "Max 20x 沒有上一級");
    }

    #[test]
    fn identity_is_the_plan_level_not_the_raw_strings() {
        assert_eq!(plan_identity(None, None), None);
        assert_eq!(plan_identity(Some(""), Some(" ")), None);
        // 大小寫、tier 字串的細節差異不算換方案；等級相同就同一個鍵
        assert_eq!(
            plan_identity(Some("Default_Claude_Max_20x"), Some("claude_max")),
            plan_identity(Some("default_claude_max_20x"), Some("claude_max"))
        );
        assert_eq!(plan_identity(Some("default_claude_max_20x"), Some("claude_max")).as_deref(), Some("max_20x"));
        assert_ne!(
            plan_identity(Some("default_claude_max_5x"), Some("claude_max")),
            plan_identity(Some("default_claude_max_20x"), Some("claude_max"))
        );
        // 認不得的等級才退回原字串
        assert_eq!(plan_identity(Some("zzz"), Some("claude_max")).as_deref(), Some("other|zzz|claude_max"));
    }

    #[test]
    fn label_matches_the_old_collector_wording() {
        assert_eq!(plan_label(Some("default_claude_max_20x"), None, None), "Claude Max 20x");
        assert_eq!(plan_label(None, Some("claude_pro"), None), "Claude Pro");
        assert_eq!(plan_label(None, None, Some("Home Org")), "Home Org");
        assert_eq!(plan_label(None, None, None), "Claude");
        // 21r1 nit 4：Pro 的 tier 字串（猜測 default_claude_ai）不能機械轉成「Claude Ai」
        assert_eq!(plan_label(Some("default_claude_ai"), Some("claude_pro"), None), "Claude Pro");
        assert_eq!(plan_label(Some("default_claude_max_5x"), Some("claude_max"), None), "Claude Max 5x");
    }
}
