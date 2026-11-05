// =============================================================================
// D61 · 時間顯示的唯一真理源
//
// 全 app 的「未來時刻」顯示（重置、見底）一律經過這裡，依使用者設定走
// 相對（"2 小時 14 分後"）或絕對（"週日 09:00"）兩種模式之一——
// 不再有「5h 用相對、7d 用絕對」的混用（主人 D61 指正）。
//
// compact 版給 W5 儀器面板（等寬短格式），一般版給 H5 頁面（中文全寫）。
// =============================================================================

export type TimeMode = "relative" | "absolute";

const pad = (n: number) => String(n).padStart(2, "0");
const WEEKDAY = "日一二三四五六";

function clock(d: Date): string {
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** 絕對時刻：今天內 "16:32"；明天 "明日 01:12"；一週內 "週日 09:00"；更遠 "MM-DD（週X）HH:MM"。 */
export function fmtAbsolute(iso: string): string {
  const d = new Date(iso);
  if (!Number.isFinite(d.getTime())) return "—";
  const now = new Date();
  const dayDiff =
    (new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime() -
      new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime()) /
    86400_000;
  if (dayDiff === 0) return clock(d);
  if (dayDiff === 1) return `明日 ${clock(d)}`;
  // v1.8.13（D91 T-04）：已過的時刻（「HH:MM 已重置」）也會走到這裡。
  if (dayDiff === -1) return `昨日 ${clock(d)}`;
  if (dayDiff > 1 && dayDiff <= 7) return `週${WEEKDAY[d.getDay()]} ${clock(d)}`;
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())}（週${WEEKDAY[d.getDay()]}）${clock(d)}`;
}

/** 相對時距（中文全寫）："38 分鐘" / "2 小時 14 分" / "2 天 19 小時"。 */
export function fmtRelative(iso: string): string {
  const ms = new Date(iso).getTime() - Date.now();
  if (!Number.isFinite(ms)) return "—";
  if (ms <= 0) return "已到";
  const mins = Math.floor(ms / 60_000);
  if (mins < 60) return `${mins} 分鐘`;
  const h = Math.floor(mins / 60);
  if (h < 48) return `${h} 小時 ${mins % 60} 分`;
  return `${Math.floor(h / 24)} 天 ${h % 24} 小時`;
}

/** 相對時距（W5 儀器短格式）："38m" / "2h14m" / "2d19h"。 */
export function fmtRelativeCompact(iso: string): string {
  const ms = new Date(iso).getTime() - Date.now();
  if (!Number.isFinite(ms)) return "—";
  if (ms <= 0) return "已到";
  const mins = Math.floor(ms / 60_000);
  if (mins < 60) return `${mins}m`;
  const h = Math.floor(mins / 60);
  if (h < 48) return `${h}h${pad(mins % 60)}m`;
  return `${Math.floor(h / 24)}d${pad(h % 24)}h`;
}

/** v1.8.13（D91 T-04）：時刻已經過了嗎？ */
function isPast(iso: string): boolean {
  const t = new Date(iso).getTime();
  return Number.isFinite(t) && t <= Date.now();
}

/** 未來時刻，依模式輸出（頁面用）。
    v1.8.13（D91 T-04）：時刻已過時相對模式以前拼出「已到後」（呼叫端再接「重置」＝「已到後重置」），
    現在已過一律寫絕對時刻（「20:20」「昨日 20:20」），讓呼叫端接「已重置」讀得通。 */
export function fmtMoment(iso: string | null | undefined, mode: TimeMode): string {
  if (!iso) return "—";
  if (mode === "absolute" || isPast(iso)) return fmtAbsolute(iso);
  return `${fmtRelative(iso)}後`;
}

/** 未來時刻，依模式輸出（W5 儀器用；絕對模式同一般版、相對模式用短格式）。已過同 fmtMoment。 */
export function fmtMomentCompact(iso: string | null | undefined, mode: TimeMode): string {
  if (!iso) return "—";
  if (mode === "absolute" || isPast(iso)) return fmtAbsolute(iso);
  return `${fmtRelativeCompact(iso)} 後`;
}

export function normalizeTimeMode(v: string | undefined | null): TimeMode {
  return v === "relative" ? "relative" : "absolute";
}
