import { useEffect, useState } from "react";
import type { UsageItem } from "@/lib/types";
import { fmtMomentCompact, type TimeMode } from "@/lib/timefmt";

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
  /** v1.4.1（D78 #1）：看別的座位時最後已知值的窗已過重置——數字「—」、寫「已重置」。 */
  expired: boolean;
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
  /** 新鮮度戳（來源＋幾分前）。 */
  freshNote: string;
  stale: boolean;
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

/** 最後已知值的窗已經過了重置時間？（只對非目前座位有意義；目前座位的值是即時的） */
export function isExpired(viewing: boolean, item: UsageItem | undefined): boolean {
  if (!viewing || !item?.resetAt) return false;
  const t = new Date(item.resetAt).getTime();
  return Number.isFinite(t) && t < Date.now();
}

/** 新鮮度戳粗判：來源標記出現「小時前／天前」＝資料已冷。 */
export function isStale(note: string | null | undefined): boolean {
  return !!note && (note.includes("小時前") || note.includes("天前"));
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

/** 數字：過期＝「—」，否則整數（來源實質精度就是 1%，O4）。 */
export function pctText(row: FaceRow): string {
  const p = row.expired ? null : row.item?.usedPercent;
  return p == null ? "—" : String(Math.round(p));
}
export function pctValue(row: FaceRow): number {
  const p = row.expired ? null : row.item?.usedPercent;
  return p == null ? 0 : Math.min(100, Math.max(0, p));
}
/** 重置欄文字：過期寫「已重置」。 */
export function resetText(row: FaceRow, mode: TimeMode): { when: string; verb: string } {
  return { when: fmtMomentCompact(row.item?.resetAt, mode), verb: row.expired ? "已重置" : "重置" };
}
