import { useEffect, useState } from "react";
import type { BudgetLine, UsageItem } from "@/lib/types";
import { budgetShortNote, isOverBudget } from "@/lib/budget";
import { fmtMomentCompact, type TimeMode } from "@/lib/timefmt";
import { freshStamp, isAgedOut } from "@/lib/ago";

// =============================================================================
// v1.8.1（D82）主題商店：三張臉共用的東西住這裡——密度尺寸、配速▼、過期判定、
// 燃燒統計型別與「見底預測」格、系統深淺色偵測。主題只換皮，不換這些。
// =============================================================================

export type Density = "micro" | "card" | "panel";
export type FaceTheme = "w5" | "w1" | "w4";
export const FACE_THEMES: FaceTheme[] = ["w5", "w1", "w4"];
export const FACE_LABEL: Record<FaceTheme, { name: string; desc: string }> = {
  w5: { name: "深色儀器", desc: "現行預設：暖象牙數字、針軌與 ▼ 配速刻度" },
  w1: { name: "毛玻璃", desc: "半透明玻璃，留意時有光暈、警戒時光暈呼吸" },
  w4: { name: "環形儀表", desc: "弧長讀餘光，低於留意值時環穿灰衣" },
};
export function normalizeTheme(v: string | null | undefined): FaceTheme {
  return v === "w1" || v === "w4" ? v : "w5";
}

// D53: 三軌（5H/7D/FABLE）後 card/panel 高度隨列數長。高度含 22px 簷廊（hover 控制列的家，不壓數值）。
export const GUTTER = 22;
export const PANEL_SIZE: Record<Density, [number, number]> = {
  micro: [220, 32],
  card: [280, 228],
  panel: [320, 336],
};
export const DENSITY_SIZE: Record<Density, [number, number]> = {
  micro: [220, 32 + GUTTER],
  card: [280, 228 + GUTTER],
  panel: [320, 336 + GUTTER],
};
export const DENSITY_CYCLE: Record<Density, Density> = {
  micro: "card",
  card: "panel",
  panel: "micro",
};

/** 主題無關的三態：留意值／警戒值由 lib/tone.ts 判、「懸浮窗變色」關掉時一律 safe。 */
export type FaceTone = "safe" | "warn" | "crit";

/** 一條額度在臉上的一列。 */
export interface FaceRow {
  id: "5H" | "7D" | "FABLE";
  item: UsageItem | undefined;
  /** 本窗已流逝時間 %（▼／環上刻度）；null 不畫。 */
  pace: number | null;
  tone: FaceTone;
  /** v1.4.1（D78 #1）：最後已知值的窗已過重置——數字「—」、寫「已重置」。
      v1.8.13（D91 T-04）：目前帳號也算（閒置一整晚時它一樣是舊值）。 */
  expired: boolean;
  /** v1.9.5（D100 E2）：此方案沒有這條（伺服器的額度清單連續 3 次沒有它）——數字「—」、
      重置欄寫「此方案沒有這條」。不是「已重置」。 */
  na: boolean;
  /** v1.10（D101）：今天的份（只有 7D／FABLE、目前登入的帳號、算得出線的列才有；其餘 null＝不畫）。 */
  budget?: RowBudget | null;
}

/** v1.10（D101 原型 D）：條上不加刻度——目前用量到 line 那段鋪淡象牙（今天還能用的量），
    越線後淡段消失、超出 line 的那段疊斜紋。lastDay 的 line＝100（照鋪到底）。 */
export interface RowBudget {
  /** null＝不知道重置時間（noReset）：不畫，只在 hover 提示說「算不出今天的份」。 */
  line: number | null;
  lastDay: boolean;
}

/** 這一列的今天的份要怎麼畫：band＝[from, to) 淡段、over＝[from, to) 斜紋。p 用畫面上的數字（跟填色同一個值）。 */
export function budgetGeom(p: number, b: RowBudget | null | undefined): { band: [number, number] | null; over: [number, number] | null } {
  if (!b || b.line == null) return { band: null, over: null };
  const pc = Math.min(100, Math.max(0, p));
  const line = Math.min(100, Math.max(0, b.line));
  if (isOverBudget(pc, line)) return { band: null, over: [line, pc] };
  if (line > pc) return { band: [pc, line], over: null };
  return { band: null, over: null };
}

/** 一列 hover 提示補的那句（v1.10）：「今天的份到 N%（還有 M%）」／「已超過今天的份 M%」／lastDay「今天重置，剩下的都能用」。 */
export function budgetNote(p: number | null | undefined, b: RowBudget | null | undefined): string | null {
  if (!b || p == null) return null;
  return budgetShortNote(p, b.line, b.lastDay);
}

/** 後端的一條 → 臉上的 RowBudget；none／沒有 line 是 null（不畫也不說），noReset 只說不畫。 */
export function rowBudgetOf(l: BudgetLine | null | undefined): RowBudget | null {
  if (!l) return null;
  if (l.status === "noReset") return { line: null, lastDay: false };
  if (l.line == null) return null;
  if (l.status === "ok") return { line: l.line, lastDay: false };
  if (l.status === "lastDay") return { line: 100, lastDay: true };
  return null;
}

/** 把今天的份那句接在既有 title 後面（換行）。 */
export function withBudget(title: string | undefined, row: Pick<FaceRow, "item" | "budget" | "expired" | "na">): string | undefined {
  if (row.expired || row.na) return title;
  const note = budgetNote(row.item?.usedPercent, row.budget);
  if (!note) return title;
  return title ? `${title}\n${note}` : note;
}

const BUDGET_TAG: Record<FaceRow["id"], string> = { "5H": "5H", "7D": "7D", FABLE: "Fable" };

/** 微型條只有一個 title：把 7D／Fable 今天的份各補一行（前面標是哪一條）。 */
export function withMicroBudget(
  title: string | undefined,
  rows: Pick<FaceRow, "id" | "item" | "budget" | "expired" | "na">[],
): string | undefined {
  const notes = rows
    .filter((r) => !r.expired && !r.na)
    .map((r) => {
      const n = budgetNote(r.item?.usedPercent, r.budget);
      return n ? `${BUDGET_TAG[r.id]}：${n}` : null;
    })
    .filter(Boolean);
  const all = [title, ...notes].filter(Boolean);
  return all.length ? all.join("\n") : undefined;
}

/** 三張臉共用的輸入。Widget.tsx 算好、臉只負責畫。 */
export interface FaceProps {
  density: Density;
  rows: [FaceRow, FaceRow, FaceRow];
  /** 留意值 %（針的位置）。 */
  needle: number;
  mode: TimeMode;
  /** 系統深色模式（W1／W4 有深淺兩副玻璃）。 */
  dark: boolean;
  /** 看別的座位：整支變灰。 */
  ghost: boolean;
  acctLabel: string | null;
  /** 看別的座位時「N 小時前的值」；即時面為 null。 */
  ago: string | null;
  canCycleSeat: boolean;
  onCycleSeat: () => void;
  burn: BurnStats | null;
  /** 新鮮度戳（來源＋幾分前）。v1.9.4（D1）：Widget.tsx 每次 tick 用 `freshNoteOf` 重算。 */
  freshNote: string;
  stale: boolean;
  /** v1.8.13（D91 T-10）：後端的提示（探針連續失敗、帳號不一致、切帳號拖尾…）；有就畫一個極小的 ⚠，hover 看全文。 */
  notice: string | null;
  /** v1.9.5（D100 F1）：目前帳號開了超額付費、而且還沒花到上限。只進 hover 提示，不動版面。 */
  extraActive: boolean;
}

/** v1.9.5（D100 E2）：此方案沒有的那條，重置欄寫這句。 */
export const NA_TEXT = "此方案沒有這條";
export const NA_TITLE = "伺服器的額度清單連續 3 次都沒有這條——此方案沒有它（或已停用），不是重置";

/** v1.9.5：一列的 hover 提示——此方案沒有 → 說明；已重置 → 上次的值；其餘 → 新鮮度戳，
    滿格且開了超額付費再補「已用完方案額度 · 超額付費中」。 */
export function rowTitle(row: FaceRow, extraActive: boolean): string | undefined {
  if (row.na) return NA_TITLE;
  if (row.expired) return expiredTitle(row.item);
  return withBudget(withExtra(stampTitle(row.item), row.item, extraActive), row);
}

/** 滿格＋超額付費成立 → 提示尾端補一句；其餘原樣。 */
export function withExtra(title: string | undefined, item: UsageItem | undefined, extraActive: boolean): string | undefined {
  const full = extraActive && (item?.usedPercent ?? 0) >= 100;
  if (!full) return title;
  return title ? `${title} · 已用完方案額度 · 超額付費中` : "已用完方案額度 · 超額付費中";
}

/** ▼ 的位置：本窗已流逝時間%。線性假設（W5 自評弱點 4）。
    D58：週限（168h 窗）也給 ▼——填色 vs ▼ 的相對位置對慢變量一樣有意義。 */
export function paceOf(resetAt: string | null | undefined, windowHours = 5): number | null {
  if (!resetAt) return null;
  const reset = new Date(resetAt).getTime();
  if (!Number.isFinite(reset)) return null;
  const win = windowHours * 3600_000;
  const p = ((Date.now() - (reset - win)) / win) * 100;
  if (p < 0 || p > 100) return null;
  return Math.round(p);
}

/** 重置後這麼短時間內量的樣本仍算上一個窗（Desktop 會在重置後 0.6 秒寫一筆舊窗的值）。
    後端 `UsageItem::is_expired_at` 同一個數。 */
const RESET_GRACE_MS = 60_000;

/** 最後已知值的窗已經過了重置時間？
    v1.8.13（D91 T-04）：以前只對「看別的帳號」生效、註解寫「目前座位的值是即時的」——但主人把閒置
    心跳關成 0，停用一整晚時目前帳號的值一樣是舊的，隔天早上懸浮窗還是昨晚的 32%／「重置 20:20」。
    現在目前帳號也套：重置時刻已過，且（沒有樣本時刻，或樣本早於重置時刻＋容差）。 */
export function isExpired(item: UsageItem | undefined): boolean {
  if (!item?.resetAt) return false;
  const t = new Date(item.resetAt).getTime();
  if (!Number.isFinite(t) || t >= Date.now()) return false;
  const s = item.sampledAt ? new Date(item.sampledAt).getTime() : NaN;
  return !Number.isFinite(s) || s < t + RESET_GRACE_MS;
}

/** v1.8.13（D91 T-10）：提示記號——極小的 ⚠，hover 看全文。不是按鈕、不是 chip。
    v1.10.5（D106）：游標是「?」，按在上面不拖曳視窗（drag.ts 認 data-no-drag）。 */
export function NoticeMark({ notice, className }: { notice: string | null; className?: string }) {
  if (!notice) return null;
  return (
    <span className={`notice-mark${className ? " " + className : ""}`} title={notice} aria-label={notice} data-no-drag="">
      ⚠
    </span>
  );
}

/** 新鮮度戳粗判：樣本滿 1 小時＝資料已冷。
    v1.9.4（情境稽核 D1）：以前看 note 字串有沒有「小時前／天前」——那個字串是刷新當下烙的，
    不刷新就永遠不會變冷。現在看樣本時刻（`lib/ago.ts isAgedOut`），每次 tick 重算。 */
export function isStale(item: UsageItem | null | undefined): boolean {
  return isAgedOut(item);
}

/** 新鮮度戳「cli · 16 分前」：來源照 note、時間用樣本時刻即時算（`lib/ago.ts freshStamp`）。 */
export function freshNoteOf(item: UsageItem | null | undefined): string {
  return freshStamp(item);
}

/** 一列／微型條的 hover 提示：即時的新鮮度戳；什麼都沒有就不給 title。 */
export function stampTitle(item: UsageItem | null | undefined): string | undefined {
  return item && (item.note || item.sampledAt) ? freshStamp(item) : undefined;
}

/** 過期列的 title：上次的值放提示（D78 #1）。 */
export function expiredTitle(item: UsageItem | undefined): string | undefined {
  const last = item?.usedPercent;
  return last != null ? `上次已知 ${Math.round(last)}%，重置後用了多少未知` : undefined;
}

export interface BurnStats {
  samples: number;
  burnPerHour: number | null;
  /** v1.4.1：最後樣本的窗已過重置時間，預測沒有意義。 */
  expired?: boolean;
  etaAt: string | null;
  etaShort?: string | null;
  etaLong?: string | null;
  rShort?: number | null;
  shortWindowMinutes?: number;
  insufficient?: boolean;
  resetsAt: string | null;
}

/** 「見底預測」格——三張臉共用同一段文案與判斷，只有外殼 class 不同（各主題 CSS 自己接 .stat/.slab/.sval/.ssub）。 */
export function EtaStat({ burn, mode }: { burn: BurnStats | null; mode: TimeMode }) {
  if (burn?.expired) {
    return (
      <div className="stat">
        <span className="slab">見底預測</span>
        <span className="sval">—</span>
        <span className="ssub">窗已重置 · 尚無新樣本</span>
      </div>
    );
  }
  if (!burn || burn.samples < 2 || burn.burnPerHour == null) {
    return (
      <div className="stat">
        <span className="slab">見底預測</span>
        <span className="sval txt">記錄累積中</span>
        <span className="ssub">有足夠記錄後推算</span>
      </div>
    );
  }
  if (burn.insufficient) {
    return (
      <div className="stat">
        <span className="slab">見底預測</span>
        <span className="sval txt">觀察中</span>
        <span className="ssub">重置後未滿 {burn.shortWindowMinutes ?? 60} 分鐘</span>
      </div>
    );
  }
  if (!burn.etaAt) {
    // D71／D73：只有近期超速（短窗 R ≥ 1）→ 撐得到但預警；短窗 ETA 早於重置才報時刻。
    const hot = burn.rShort != null && burn.rShort >= 1;
    const etaBefore =
      hot && burn.etaShort && burn.resetsAt &&
      new Date(burn.etaShort).getTime() < new Date(burn.resetsAt).getTime();
    const win = burn.shortWindowMinutes ?? 60;
    return (
      <div className="stat">
        <span className="slab">見底預測</span>
        <span className="sval txt">{hot ? "撐得到 · 近期偏快" : "重置前不見底"}</span>
        <span className={`ssub${hot ? " amber" : ""}`}>
          {etaBefore
            ? `近 ${win} 分速度持續 → 約 ${fmtMomentCompact(burn.etaShort as string, mode)}`
            : hot
              ? `近 ${win} 分燒得比配速快`
              : "配速與近期速度皆未超標"}
        </span>
      </div>
    );
  }
  const etaTxt = `約 ${fmtMomentCompact(burn.etaAt, mode)}`;
  let note = "配速與近期速度皆超標";
  let over = false;
  if (burn.resetsAt) {
    const diff = new Date(burn.resetsAt).getTime() - new Date(burn.etaAt).getTime();
    if (diff > 60_000) {
      const mins = Math.floor(diff / 60_000);
      note = `比重置早 ${Math.floor(mins / 60)}h${String(mins % 60).padStart(2, "0")}m`;
      over = true;
    } else {
      note = "撐得到重置";
    }
  }
  return (
    <div className="stat">
      <span className="slab">見底預測</span>
      <span className="sval txt">{etaTxt}</span>
      <span className={`ssub${over ? " amber" : ""}`}>{note}</span>
    </div>
  );
}

/** 燃燒速度格（面板左格），三張臉同文案。 */
export function BurnStat({ burn }: { burn: BurnStats | null }) {
  return (
    <div className="stat">
      <span className="slab">燃燒速度</span>
      <span className="sval">
        {burn?.burnPerHour != null ? burn.burnPerHour.toFixed(1) : "—"}
        <span className="sunit"> %/h</span>
      </span>
      <span className="ssub">近期真值樣本推算</span>
    </div>
  );
}

/** 系統深色模式（W1／W4 跟著桌布深淺換玻璃；舊版懸浮窗本來就這樣，D36）。 */
export function usePrefersDark(): boolean {
  const get = () => typeof window !== "undefined" && window.matchMedia?.("(prefers-color-scheme: dark)").matches;
  const [dark, setDark] = useState<boolean>(get);
  useEffect(() => {
    const mq = window.matchMedia?.("(prefers-color-scheme: dark)");
    if (!mq) return;
    const on = () => setDark(mq.matches);
    mq.addEventListener("change", on);
    return () => mq.removeEventListener("change", on);
  }, []);
  return dark;
}

/** 數字：過期／此方案沒有＝「—」，否則整數（來源實質精度就是 1%，O4）。 */
export function pctText(row: FaceRow): string {
  const p = row.expired || row.na ? null : row.item?.usedPercent;
  return p == null ? "—" : String(Math.round(p));
}
export function pctValue(row: FaceRow): number {
  const p = row.expired || row.na ? null : row.item?.usedPercent;
  return p == null ? 0 : Math.min(100, Math.max(0, p));
}
/** 重置欄文字：過期寫「已重置」；v1.9.5 此方案沒有寫「此方案沒有這條」（沒有重置可言）。 */
export function resetText(row: FaceRow, mode: TimeMode): { when: string; verb: string } {
  if (row.na) return { when: NA_TEXT, verb: "" };
  return { when: fmtMomentCompact(row.item?.resetAt, mode), verb: row.expired ? "已重置" : "重置" };
}
