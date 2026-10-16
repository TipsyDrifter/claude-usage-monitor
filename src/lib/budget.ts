import type { BudgetLine } from "./types";

/** 本地 HH:MM（跟全 app 的時刻寫法一致）。 */
const hhmm = (iso: string) => {
  const d = new Date(iso);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
};

// =============================================================================
// v1.10（D98／D101）自己的預算——「今天的份」的說法唯一來源。懸浮窗三張臉的 hover 提示與
// 今日頁裁決卡「你自己的線」都從這裡拿字，兩邊的數字才對得起來。
//
// 數字一律用畫面上顯示的整數相減（線 65.7 顯示 66、現在 52 → 還有 14），不用原始小數——
// 不然會出現「到 66%、現在 52%、還有 13%」這種畫面上自己算不攏的句子。
// =============================================================================

/** 「動詞＋差多少」：差＝畫面上那兩個整數相減；0 寫「不到 1%」（「還有不到 1%」「已超過不到 1%」）。 */
export function budgetGapText(verb: string, a: number, b: number): string {
  const d = Math.abs(Math.round(a) - Math.round(b));
  return d < 1 ? `${verb}不到 1%` : `${verb} ${d}%`;
}

/** 越線判準的容差：後端 `percent > line + 1e-3`——帳本的 % 是 f32，轉 f64 會多出尾數
    （12.3f32 → 12.300000190734863），不加容差的話剛好在線上的值會被判成越線。前後端同一個數。 */
export const OVER_EPS = 1e-3;

/** 越線了嗎（跟後端 over 同一個判準）。懸浮窗的淡段／斜紋、今日頁、提示文字都用這一支。 */
export function isOverBudget(percent: number, line: number): boolean {
  return percent > line + OVER_EPS;
}

/** 重置當天（line＝100）的說法：已經用滿就不能說「剩下的都能用」。 */
function lastDayText(percent: number | null | undefined): string {
  return percent != null && percent >= 100 ? "已用完，今天重置" : "今天重置，剩下的都能用";
}

/** 懸浮窗 hover 補的那句。line＝null 表示不知道重置時間（不畫，但要說）。 */
export function budgetShortNote(percent: number, line: number | null, lastDay: boolean): string {
  if (line == null) return "不知道重置時間，算不出今天的份";
  if (lastDay) return lastDayText(percent);
  if (isOverBudget(percent, line)) return budgetGapText("已超過今天的份", percent, line);
  return `今天的份到 ${Math.round(line)}%（${budgetGapText("還有", line, percent)}）`;
}

/** 起點時刻：一律照日曆日寫——日曆上的今天「HH:MM」、昨天「昨天 HH:MM」、更早「M/D HH:MM」。
    （一天從 6 點算，現在 02:00 時起點是日曆上的昨天 06:00，不能寫成今天。） */
function whenText(iso: string, now = new Date()): string {
  const d = new Date(iso);
  const dayDiff = Math.round(
    (new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime() -
      new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime()) /
      86400_000,
  );
  if (dayDiff === 0) return hhmm(iso);
  if (dayDiff === 1) return `昨天 ${hhmm(iso)}`;
  return `${d.getMonth() + 1}/${d.getDate()} ${hhmm(iso)}`;
}

/** 「剛重置」只用在重置後這麼久以內；更久就只寫「HH:MM 重置」。 */
const JUST_RESET_MS = 2 * 3600_000;

/** 今日頁「你自己的線」的起點說明（括號裡那段）。 */
function anchorText(l: BudgetLine): string | null {
  const at = l.anchorAt ? whenText(l.anchorAt) : null;
  const p = l.anchorPercent != null ? `${Math.round(l.anchorPercent)}%` : null;
  switch (l.anchorKind) {
    case "dayStart":
      return at && p ? `${at} 時 ${p}` : null;
    case "firstAfter":
      // 起算時刻以前 App 沒開——起點用起算之後的第一筆
      return at && p ? `起點用 ${at} 的 ${p}（起算後第一筆）` : null;
    case "windowStart": {
      if (!at || !l.anchorAt) return "這一天中途重置，從 0 起算";
      const just = Date.now() - new Date(l.anchorAt).getTime() <= JUST_RESET_MS;
      return `${at} ${just ? "剛重置" : "重置"}，從 0 起算`;
    }
    case "planChange":
      return p ? `換方案後從 ${p} 起算` : "換方案後重新起算";
    default:
      return null;
  }
}

/** 今日頁「你自己的線」整句；null＝整行不出現（none、預算關掉、沒有目前 %）。
    percent 用頁面那一列的數字（跟大百分比同一個值）。 */
export function budgetSentence(l: BudgetLine | null | undefined, percent: number | null): { text: string; over: boolean } | null {
  if (!l || l.status === "none") return null;
  if (l.status === "noReset") return { text: "不知道重置時間，算不出今天的份", over: false };
  if (l.status === "lastDay") return { text: lastDayText(percent), over: false };
  if (l.line == null || percent == null) return null;
  const line = Math.round(l.line);
  if (isOverBudget(percent, l.line)) {
    return { text: `今天的份到 ${line}%，${budgetGapText("已超過", percent, l.line)}｜之後每天的份會自動變小`, over: true };
  }
  const a = anchorText(l);
  return {
    text: `今天的份到 ${line}%${a ? `（${a}）` : ""}｜現在 ${Math.round(percent)}%，${budgetGapText("今天還有", l.line, percent)}`,
    over: false,
  };
}
