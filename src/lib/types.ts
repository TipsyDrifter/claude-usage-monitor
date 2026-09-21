export type ScrapeStatus =
  | "idle"
  | "loading"
  | "ok"
  | "needs-login"
  | "error";

export interface UsageItem {
  usedPercent: number | null;
  resetAt: string | null; // ISO 8601
  note: string | null;
}

export interface UsageSnapshot {
  planName: string | null;
  currentSession: UsageItem;
  weeklyAllModels: UsageItem;
  /** M0: weekly Fable-scoped limit from the collection layer (channel A/C).
   *  Optional so pre-M0 mocks / demo payloads stay valid. */
  weeklyFable?: UsageItem;
  scrapedAt: string;
}

export interface UsageState {
  status: ScrapeStatus;
  data: UsageSnapshot | null;
  error: string | null;
  /** M1/D51: non-fatal advisory (e.g. Desktop/CLI 帳號不一致提醒). */
  notice?: string | null;
  lastSuccessAt: string | null;
  nextRefreshAt: string | null;
}

// =============================================================================
// AppSettings — nested groups (v0.3 audit #7 refactor)
//
// Why nested? v0.2 was flat. v0.3 adds a Statistics window with its own
// settings, so the flat namespace was about to balloon. Grouping by concern
// keeps related toggles together and gives each surface area room to grow.
//
// Migration is handled in Rust (`src-tauri/src/settings.rs`): legacy flat
// settings.json files are read once and rewritten in this nested shape, so
// existing users keep all of their tuned values.
// =============================================================================

export interface GeneralSettings {
  pollIntervalMinutes: number;
  autoStart: boolean;
  /** D54 低頻心跳：閒置時探針最久這麼多分鐘一發（0 = 關）。 */
  idleHeartbeatMinutes: number;
  /** D61 全 app 未來時刻顯示："relative"（幾小時後）| "absolute"（週幾幾點）。 */
  timeFormat: string;
}

export interface WidgetSettings {
  /** Show the floating desktop widget. False keeps the app tray-only. */
  show: boolean;
  /** Last-saved widget position so it reopens where the user left it. */
  position: { x: number; y: number } | null;
  /** M2/W5 三段密度: "micro" | "card" | "panel". */
  density: string;
  /** v1.8.1（D82）主題商店: "w5" 深色儀器（預設）| "w1" 毛玻璃 | "w4" 環形。 */
  theme: string;
}

/** v1.2 兩級告警（D67-Q16／Q17）。兩個數是全 app 的唯一來源，見 `lib/tone.ts`。 */
export interface NotificationsSettings {
  /** 留意值（%）：懸浮窗象牙針位置、轉琥珀、第一級通知。預設 70。 */
  noticePct: number;
  /** 撞牆值（%）：轉珊瑚、第二級通知。預設 90。 */
  wallPct: number;
  /** 懸浮窗額度條隨門檻變色（預設開）。 */
  widgetTint: boolean;
  /** Windows 系統通知（預設關）。無聲音。 */
  systemToast: boolean;
}

export interface WebhookSettings {
  port: number;
  /** Bearer token for the local /refresh webhook. Generated on first launch. */
  token: string;
}

export interface StatisticsWindowSettings {
  /** Last page viewed in the statistics window. Restored on reopen. */
  lastPage: string;
}

/** v1.3（D75）：座位別名，key＝`帳號uuid|組織uuid`（見 lib/seatLabel.ts）。 */
export interface AccountsSettings {
  aliases: Record<string, string>;
}

export interface AppSettings {
  general: GeneralSettings;
  widget: WidgetSettings;
  notifications: NotificationsSettings;
  webhook: WebhookSettings;
  statisticsWindow: StatisticsWindowSettings;
  accounts: AccountsSettings;
}

/** v1.3：切換器用的座位資訊（後端 `list_seats`）。 */
export interface SeatInfo {
  id: string;
  accountUuid: string | null;
  orgUuid: string | null;
  email: string | null;
  orgName: string | null;
  lastSeenAt: string;
  /** probe／cli-cache 樣本數——0 代表這個座位沒有即時面可看。 */
  liveSamples: number;
  lastSampleAt: string | null;
}

export const DEFAULT_SETTINGS: AppSettings = {
  general: {
    pollIntervalMinutes: 5,
    autoStart: false,
    idleHeartbeatMinutes: 45,
    timeFormat: "absolute",
  },
  widget: {
    show: true,
    position: null,
    density: "card",
    theme: "w5",
  },
  notifications: {
    noticePct: 70,
    wallPct: 90,
    widgetTint: true,
    systemToast: false,
  },
  webhook: {
    port: 17819,
    token: "",
  },
  statisticsWindow: {
    lastPage: "overview",
  },
  accounts: {
    aliases: {},
  },
};

export const EMPTY_ITEM: UsageItem = {
  usedPercent: null,
  resetAt: null,
  note: null,
};

