// =============================================================================
// v1.8.13（D91 T-13）：統計視窗的「本地日」與本地時刻字串——只在這裡定義。
//
// 以前趨勢頁日條、今日頁峰值長條用 `toISOString().slice(0,10)` 切日（那是 UTC 日，
// 台北 00:00～08:00 的用量會歸到前一天），健康度的缺口直接切 UTC 字串當本地時間顯示
// （差 8 小時、跨日看起來像倒著走）。歷史頁與熱圖一直是本地日；統一照它。
// =============================================================================

const pad2 = (n: number) => String(n).padStart(2, "0");

/** Date → 本地日 "YYYY-MM-DD"。 */
export function localDay(d: Date): string {
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
}

/**
 * 時間戳（ISO，通常帶 Z／+00:00）→ 本地日 "YYYY-MM-DD"。
 * 只有日期的字串（"2026-09-20"）已經是某一天了，原樣回傳——丟給 `new Date` 會被當成
 * UTC 午夜，在 UTC 以西的時區會退一天。解析不了也原樣回傳前 10 碼，不讓畫面炸掉。
 */
export function localDayOf(iso: string | null | undefined): string | null {
  if (!iso) return null;
  if (/^\d{4}-\d{2}-\d{2}$/.test(iso)) return iso;
  const d = new Date(iso);
  return Number.isFinite(d.getTime()) ? localDay(d) : iso.slice(0, 10);
}

/** 今天往回推 `back` 天的本地日（用日曆加減，不是減 86400 秒——夏令時區也對）。 */
export function localDayBack(back: number, now = new Date()): string {
  return localDay(new Date(now.getFullYear(), now.getMonth(), now.getDate() - back));
}

/** 本地日 "YYYY-MM-DD" → 那天本地午夜的 Date。 */
export function dateOfLocalDay(day: string): Date {
  return new Date(`${day}T00:00:00`);
}

/**
 * 一段時間跨過的每一個本地日（含頭尾）。v1.9.5（D100 D4）：缺口可以跨好幾天
 * （08-29 → 09-03），以前只標 `from` 那一天，其餘幾天看起來像「沒用」。
 * 結束時刻剛好是本地午夜 → 那天不算（區間右開）。解析不了回空陣列。
 */
export function localDaysSpanning(fromIso: string, toIso: string): string[] {
  const a = new Date(fromIso);
  const b = new Date(toIso);
  if (!Number.isFinite(a.getTime()) || !Number.isFinite(b.getTime()) || b < a) return [];
  const last = localDay(new Date(Math.max(a.getTime(), b.getTime() - 1)));
  const out: string[] = [];
  const cur = new Date(a.getFullYear(), a.getMonth(), a.getDate());
  // 上限 800 天：壞資料不能把畫面卡死
  for (let i = 0; i < 800; i++) {
    const day = localDay(cur);
    out.push(day);
    if (day >= last) break;
    cur.setDate(cur.getDate() + 1);
  }
  return out;
}

/** 本地 "HH:MM"。 */
export function hhmm(iso: string): string {
  const d = new Date(iso);
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}

/** 本地 "MM-DD HH:MM"。 */
export function mdhm(iso: string): string {
  const d = new Date(iso);
  return `${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ${hhmm(iso)}`;
}

/** 一段時間 "MM-DD HH:MM → HH:MM"；跨到別天時結尾也帶日期（"→ MM-DD HH:MM"）。 */
export function localSpan(fromIso: string, toIso: string): string {
  const sameDay = localDayOf(fromIso) === localDayOf(toIso);
  return `${mdhm(fromIso)} → ${sameDay ? hhmm(toIso) : mdhm(toIso)}`;
}
