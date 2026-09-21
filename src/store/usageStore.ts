import { create } from "zustand";
import type { UsageState, AppSettings, UsageSnapshot, SeatInfo } from "@/lib/types";
import { DEFAULT_SETTINGS } from "@/lib/types";
import { cmd, events } from "@/lib/tauri";
import { MOCK_STATE, MOCK_SEATS, MOCK_SEAT_SNAPSHOT } from "@/lib/mock";

type UnlistenFn = () => void;

interface Store {
  usage: UsageState;
  settings: AppSettings;
  initialized: boolean;
  /** v1.8.6（D85）：後端的設定真的進來了嗎（判準＝密碼牌非空）。init 失敗或搶在後端
   *  載入前拿到預設值時是 false——這時任何 updateSettings 都不准存，等 settings://update。 */
  settingsLoaded: boolean;
  demoMode: boolean;
  /** Set when the last `cmd.saveSettings` rejected. UI surfaces this as
   *  an inline warning; cleared on the next successful save. */
  settingsError: string | null;

  // ---- v1.4 E15（D77）：存檔成功的回執 ----
  /** 上一次成功寫入設定的時刻（Date.now()）。設定頁拿它當「跳一次小字」的觸發。
   *  settingsError 是失敗的紅字，這兩個是一對。 */
  settingsSavedAt: number | null;
  /** 那一次存了哪幾個欄位（`"<group>.<key>"`）。設定頁靠它只讓「被改到的那張卡」
   *  跳小字——不是六張卡一起閃。 */
  settingsSavedFields: string[];

  // ---- v1.3 兩個帳號（D75）----
  /** 切換器的座位清單（只有 exact 座位）。 */
  seats: SeatInfo[];
  /** CLI 目前登入的座位；即時面永遠是它。 */
  currentSeatId: string | null;
  /** 使用者切去看的座位；null＝目前帳號。不持久化（決策點 6）。 */
  viewSeatId: string | null;
  /** viewSeatId 座位的最後已知值（UsageSnapshot 形狀，scrapedAt＝最後樣本時刻）。 */
  seatSnapshot: UsageSnapshot | null;
  loadSeats: () => Promise<void>;
  setViewSeat: (id: string | null) => Promise<void>;
  /** 懸浮窗點標籤：依清單順序循環。 */
  cycleSeat: () => Promise<void>;

  init: () => Promise<void>;
  /** Detach all Tauri event listeners and reset to a re-initable state.
   *  Should be called from a component unmount path if the host ever
   *  starts tearing down + recreating the store at runtime. */
  dispose: () => void;
  loadDemo: () => void;
  refresh: () => Promise<void>;
  /** Patch a single settings group. The store does the spread internally so
   *  callers don't have to repeat `{...prev.group, field: v}` boilerplate.
   *
   *  Example: `updateSettings("widget", { show: false })`
   *
   *  Saves to disk via Tauri IPC and rolls back the optimistic update on
   *  failure (setting `settingsError` so the UI can surface it). */
  updateSettings: <K extends keyof AppSettings>(
    group: K,
    patch: Partial<AppSettings[K]>,
  ) => Promise<void>;
  clearSettingsError: () => void;
}

// Module-scoped unlisten registry. Kept outside the store state so that
// React's strict-mode double-render of `init` re-uses (and overwrites) the
// same slots instead of leaking a second pair of listeners.
let unlistenUsage: UnlistenFn | null = null;
let unlistenSettings: UnlistenFn | null = null;

const initialUsage: UsageState = {
  status: "idle",
  data: null,
  error: null,
  lastSuccessAt: null,
  nextRefreshAt: null,
};

export const useStore = create<Store>((set, get) => ({
  usage: initialUsage,
  settings: DEFAULT_SETTINGS,
  initialized: false,
  settingsLoaded: false,
  demoMode: false,
  settingsError: null,
  settingsSavedAt: null,
  settingsSavedFields: [],
  seats: [],
  currentSeatId: null,
  viewSeatId: null,
  seatSnapshot: null,

  loadDemo: () => {
    set({
      usage: MOCK_STATE,
      settings: DEFAULT_SETTINGS,
      initialized: true,
      demoMode: true,
      settingsError: null,
      settingsSavedAt: null,
      settingsSavedFields: [],
      seats: MOCK_SEATS,
      currentSeatId: MOCK_SEATS[0]?.id ?? null,
      viewSeatId: null,
      seatSnapshot: null,
    });
  },

  loadSeats: async () => {
    if (get().demoMode) return;
    try {
      const r = await cmd.listSeats();
      set({ seats: r.seats, currentSeatId: r.currentSeatId });
      const view = get().viewSeatId;
      if (view != null && view === r.currentSeatId) {
        // 切回目前帳號了（例如主人真的登入了那個帳號）——即時面接手。
        set({ viewSeatId: null, seatSnapshot: null });
      } else if (view != null) {
        const snap = await cmd.getSeatSnapshot(view);
        set({ seatSnapshot: snap });
      }
    } catch (err) {
      console.warn("loadSeats failed", err);
    }
  },

  setViewSeat: async (id) => {
    const cur = get().currentSeatId;
    const next = id != null && id !== cur ? id : null;
    if (next == null) {
      set({ viewSeatId: null, seatSnapshot: null });
      return;
    }
    if (get().demoMode) {
      set({ viewSeatId: next, seatSnapshot: MOCK_SEAT_SNAPSHOT });
      return;
    }
    set({ viewSeatId: next });
    try {
      const snap = await cmd.getSeatSnapshot(next);
      if (get().viewSeatId === next) set({ seatSnapshot: snap });
    } catch (err) {
      console.warn("getSeatSnapshot failed", err);
      set({ viewSeatId: null, seatSnapshot: null });
    }
  },

  cycleSeat: async () => {
    const { seats, currentSeatId, viewSeatId } = get();
    if (seats.length < 2) return;
    const now = viewSeatId ?? currentSeatId;
    const idx = seats.findIndex((s) => s.id === now);
    const next = seats[(idx + 1) % seats.length];
    await get().setViewSeat(next.id);
  },

  clearSettingsError: () => set({ settingsError: null }),

  init: async () => {
    if (get().initialized) return;

    // v1.8.6：先掛 settings://update 再去問——後端 load 完的那次廣播不能漏接。
    unlistenSettings?.();
    unlistenSettings = await events.onSettingsUpdate((settings) =>
      set({ settings, settingsLoaded: !!settings.webhook?.token }),
    );

    try {
      const usage = await cmd.getState();
      // v1.8.5（D85 追加）：App 剛啟動時 get_settings 可能搶在後端載入前回來，拿到的是
      // 預設值（密碼牌是空的就是記號）。等一下再問，最多 3 秒；後端載完也會廣播一次
      // settings://update 當保險。
      let settings = await cmd.getSettings();
      for (let i = 0; i < 6 && !settings.webhook?.token && !get().demoMode; i++) {
        await new Promise((r) => setTimeout(r, 500));
        settings = await cmd.getSettings();
      }
      // 舊形狀的 settings（v1.1 fixture、手改過的 settings.json）可能缺整個群組——
      // 逐群組補上預設，UI 才不會在 `.accounts.aliases` 這種地方炸掉。
      const merged: AppSettings = {
        ...DEFAULT_SETTINGS,
        ...settings,
        general: { ...DEFAULT_SETTINGS.general, ...settings.general },
        widget: { ...DEFAULT_SETTINGS.widget, ...settings.widget },
        notifications: { ...DEFAULT_SETTINGS.notifications, ...settings.notifications },
        webhook: { ...DEFAULT_SETTINGS.webhook, ...settings.webhook },
        statisticsWindow: { ...DEFAULT_SETTINGS.statisticsWindow, ...settings.statisticsWindow },
        accounts: { ...DEFAULT_SETTINGS.accounts, ...settings.accounts },
      };
      set({ usage, settings: merged, initialized: true, settingsLoaded: !!merged.webhook?.token });
    } catch (err) {
      console.error("Failed to init store", err);
      // 標 initialized 讓畫面別卡在骨架，但設定沒進來——存檔仍鎖著，等 settings://update。
      set({ initialized: true, settingsLoaded: false });
    }
    void get().loadSeats();

    // Subscribe to backend events. Capture the unlisten fns so `dispose`
    // can detach later — without this the listeners outlive the store
    // and accumulate on every re-init.
    unlistenUsage?.();
    unlistenUsage = await events.onUsageUpdate((usage) => {
      set({ usage });
      // 每次刷新後座位名字／目前座位可能變（主人切帳號），順手重抓；很便宜。
      if (usage.status === "ok") void get().loadSeats();
    });
  },

  dispose: () => {
    unlistenUsage?.();
    unlistenSettings?.();
    unlistenUsage = null;
    unlistenSettings = null;
    set({ initialized: false });
  },

  refresh: async () => {
    if (get().demoMode) return;
    set((s) => ({ usage: { ...s.usage, status: "loading" } }));
    try {
      await cmd.refreshNow();
    } catch (err) {
      set((s) => ({
        usage: {
          ...s.usage,
          status: "error",
          error: String(err),
        },
      }));
    }
  },

  updateSettings: async (group, patch) => {
    // v1.8.4：init 還沒把後端設定拿回來之前，store 裡是 DEFAULT_SETTINGS——這時存檔
    // 等於把預設值（autoStart=false、token 空…）蓋回去。懸浮窗還原位置的 onMoved 每次
    // 啟動都會在 0.6 秒內觸發一次，就是這條路把開機自啟關掉的。沒 init 完一律不存。
    if (!get().initialized) return;
    // v1.8.6：initialized 不夠——init 失敗的 catch 也會標 initialized。要的是「設定真的
    // 從後端進來了」（settingsLoaded）；demo 模式沒有後端，照常放行。
    if (!get().demoMode && !get().settingsLoaded) return;
    const prev = get().settings;
    const next: AppSettings = {
      ...prev,
      [group]: { ...prev[group], ...patch },
    };
    // Optimistic update so the UI feels snappy; rolled back below on failure.
    set({ settings: next, settingsError: null });
    // v1.4 E15（D77）：存檔成功的回執。demo 模式不落盤，但「使用者按了、
    // 畫面收下了」這件事一樣成立，所以兩條路都記。
    const receipt = () =>
      set({
        settingsSavedAt: Date.now(),
        settingsSavedFields: Object.keys(patch).map((k) => `${String(group)}.${k}`),
      });
    if (get().demoMode) {
      receipt();
      return;
    }
    try {
      await cmd.saveSettings(next);
      receipt();
    } catch (err) {
      // The save didn't actually persist — rewind the UI and surface the
      // error so the user knows the toggle they just flipped didn't stick.
      console.error("Settings save failed; rolling back:", err);
      set({ settings: prev, settingsError: String(err) });
    }
  },
}));
