import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { UsageState, AppSettings, UsageSnapshot, SeatInfo } from "./types";

const inTauri =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

// Dev-only fixture mode (M5 verification): open a page in a plain browser with
// `?fixture=1` and every IPC call is answered from
// `/prototypes/fixtures/<command>.json` (dumped from the real ledger by the
// `probe` test in ledger.rs). Never active inside Tauri or in production builds.
const fixtureMode =
  import.meta.env.DEV &&
  typeof window !== "undefined" &&
  new URLSearchParams(window.location.search).has("fixture");

function safeInvoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!inTauri && fixtureMode) {
    // v1.5：帶 `kind` 的呼叫（歷史頁三窗）先找 `<command>-<kind>.json`，沒有再退回通用檔。
    const kind = typeof args?.kind === "string" ? (args.kind as string) : null;
    const base = `/prototypes/fixtures/${command}`;
    const load = (url: string) => fetch(url).then((r) => (r.ok ? (r.json() as Promise<T>) : null));
    return (kind ? load(`${base}-${kind}.json`) : Promise.resolve(null)).then(
      (hit) =>
        hit ??
        fetch(`${base}.json`).then((r) => {
          if (!r.ok) throw new Error(`fixture ${command}.json missing (${r.status})`);
          return r.json() as Promise<T>;
        }),
    );
  }
  if (!inTauri) {
    return Promise.reject(
      new Error(`Tauri not available (browser/demo mode); skipping ${command}`),
    );
  }
  return invoke<T>(command, args);
}

export const cmd = {
  getState: () => safeInvoke<UsageState>("get_usage_state"),
  refreshNow: () => safeInvoke<void>("refresh_now"),
  getSettings: () => safeInvoke<AppSettings>("get_settings"),
  saveSettings: (settings: AppSettings) =>
    safeInvoke<void>("save_settings", { settings }),
  showSettings: () => safeInvoke<void>("show_settings"),
  showStatistics: () => safeInvoke<void>("show_statistics"),
  quit: () => safeInvoke<void>("quit_app"),
  revealLogFolder: () => safeInvoke<void>("reveal_log_folder"),
  /** v1.8.2：把 log／設定（密碼牌遮掉）／健康度／環境打成一個 zip，存到使用者選的位置並在檔案總管選取。 */
  packDiagnostics: () =>
    safeInvoke<{ cancelled: boolean; path: string; fileName: string; bytes: number }>("pack_diagnostics"),
  // v0.2
  installClaudeCodeHook: () => safeInvoke<string>("install_claude_code_hook"),
  regenerateWebhookToken: () =>
    safeInvoke<string>("regenerate_webhook_token"),
  // M1: data-health snapshot for the 健康度 page (loose JSON by design).
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  getDataHealth: () => safeInvoke<any>("get_data_health"),
  // M2/W5: burn-rate + ETA for the widget's expanded panel. v1.3: seat=null → CLI 目前座位。
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  getBurnStats: (seat?: string | null) => safeInvoke<any>("get_burn_stats", { seat: seat ?? null }),
  // M3: decision-layer payload for the 今天撞牆嗎 page. v1.3: seat=null → CLI 目前座位。
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  getTodayOutlook: (seat?: string | null) => safeInvoke<any>("get_today_outlook", { seat: seat ?? null }),
  // v1.3 (D75): account switcher
  listSeats: () => safeInvoke<{ seats: SeatInfo[]; currentSeatId: string | null }>("list_seats"),
  getSeatSnapshot: (seatId: string) =>
    safeInvoke<UsageSnapshot & { seatId: string }>("get_seat_snapshot", { seatId }),
  // M4: research-layer payload for the 這半年 dashboard.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  getDashboard: (seatAccount?: string | null, days?: number) =>
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    safeInvoke<any>("get_dashboard", { seatAccount: seatAccount ?? null, days: days ?? 180 }),
  // M6 (D65): history page + evidence export
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  // v1.5 (D79): `kind` picks the limit — "5h" | "7d" | "fable".
  getHistory: (seatAccount: string | null, unit: "day" | "week" | "month", offset: number, kind: "5h" | "7d" | "fable" = "5h") =>
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    safeInvoke<any>("get_history", { seatAccount, unit, offset, kind }),
  exportEvidence: (seatAccount: string | null, days: number) =>
    safeInvoke<{ dir: string; files: string[] }>("export_evidence", { seatAccount, days }),
  saveExportFile: (name: string, base64Data: string) =>
    safeInvoke<string>("save_export_file", { name, base64Data }),
  revealExportFolder: () => safeInvoke<void>("reveal_export_folder"),
};

export const events = {
  onUsageUpdate: (cb: (state: UsageState) => void): Promise<UnlistenFn> => {
    if (!inTauri) return Promise.resolve(() => {});
    return listen<UsageState>("usage://update", (e) => cb(e.payload));
  },

  onSettingsUpdate: (cb: (s: AppSettings) => void): Promise<UnlistenFn> => {
    if (!inTauri) return Promise.resolve(() => {});
    return listen<AppSettings>("settings://update", (e) => cb(e.payload));
  },
};

export type { UsageState, AppSettings, UsageSnapshot };
