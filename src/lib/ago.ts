// =============================================================================
// v1.9.4（情境稽核 D1）· 「多久以前」的唯一定義
//
// 以前懸浮窗與今日頁頂端顯示的「cli · 16 分前」是後端刷新當下烙進 `UsageItem.note`
// 的字串，之後不會走——主人 30 分查一次，畫面就停在「16 分前」直到下一輪。現在：
//   - 時間那段用 `sampledAt`（樣本時刻）對「現在」即時算（`fmtAgo`）；
//   - 來源那段（cli／probe／desktop）照 note 保留（`noteSource`）；
//   - 「資料已冷」看時刻（`isAgedOut`），不看字串裡有沒有「小時前」。
// 呼叫端要自己每分鐘重畫一次（懸浮窗已有 60 秒 tick）才會走。
//
// 規則與後端 `collector/model.rs age_label` 相同（托盤 tooltip 用那一份）：
//   未滿 1 分「剛剛」、未滿 60 分「N 分前」、未滿 48 小時「N 小時前」、其餘「N 天前」。
// =============================================================================

import type { UsageItem } from "./types";

const MIN = 60_000;

/** 「剛剛」／「16 分前」／「2 小時前」／「3 天前」。時刻解析不了回 null。 */
export function fmtAgo(iso: string | null | undefined, now: number = Date.now()): string | null {
  if (!iso) return null;
  const t = new Date(iso).getTime();
  if (!Number.isFinite(t)) return null;
  // 跟 Rust `num_minutes()` 一樣往零截；樣本在未來（時鐘差）算「剛剛」。
  const mins = Math.trunc((now - t) / MIN);
  if (mins < 1) return "剛剛";
  if (mins < 60) return `${mins} 分前`;
  if (mins < 48 * 60) return `${Math.floor(mins / 60)} 小時前`;
  return `${Math.floor(mins / (24 * 60))} 天前`;
}

const AGE_TAIL = /^(剛剛|\d+ (分|小時|天)前)$/;

/** note「cli · 16 分前」→「cli」。尾端不是時間的形狀就原樣回傳（「最後已知值」）。
    後端 `model.rs note_source` 同一個口徑。 */
export function noteSource(note: string | null | undefined): string | null {
  if (!note) return null;
  const i = note.lastIndexOf(" · ");
  if (i < 0) return note;
  return AGE_TAIL.test(note.slice(i + 3)) ? note.slice(0, i) : note;
}

/** 新鮮度戳「cli · 16 分前」：時間用樣本時刻即時算。
    沒有樣本時刻（座位最後已知值、舊後端）退回 note 原樣；兩個都沒有寫「來源不明」。 */
export function freshStamp(item: UsageItem | null | undefined, now: number = Date.now()): string {
  const ago = fmtAgo(item?.sampledAt, now);
  if (ago == null) return item?.note ?? "來源不明";
  // v1.9.4：後端已經直接給來源（sourceLabel）就用它；舊後端才從 note 拆。
  const src = item?.sourceLabel || noteSource(item?.note);
  return src ? `${src} · ${ago}` : ago;
}

/** 多舊算「資料已冷」：滿 1 小時（等於以前字串判斷「出現小時前／天前」的門檻）。 */
export const STALE_AFTER_MS = 60 * MIN;

/** 這條額度的數字已經冷了嗎？看樣本時刻；沒有樣本時刻才退回舊的字串判斷。 */
export function isAgedOut(item: UsageItem | null | undefined, now: number = Date.now()): boolean {
  const s = item?.sampledAt ? new Date(item.sampledAt).getTime() : NaN;
  if (Number.isFinite(s)) return now - s >= STALE_AFTER_MS;
  const note = item?.note;
  return !!note && (note.includes("小時前") || note.includes("天前"));
}
