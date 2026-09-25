// v1.2 兩級告警的唯一來源（D67-Q17、D74）：留意值＝象牙針位置＋轉琥珀，
// 撞牆值＝轉珊瑚。Widget、Today 裁決、設定頁預覽全部從這裡拿 tone，
// 不准再各自寫 70／90。後端對應 `NotificationsSettings::thresholds()`。
import type { AppSettings } from "./types";

export type Tone = "ok" | "warn" | "crit" | "unknown";

export interface Thresholds {
  /** 留意值（%）：針的位置、轉琥珀、第一級通知。 */
  notice: number;
  /** 撞牆值（%）：轉珊瑚、第二級通知。 */
  wall: number;
}

export const DEFAULT_THRESHOLDS: Thresholds = { notice: 70, wall: 90 };

export function thresholdsOf(settings: AppSettings): Thresholds {
  return {
    notice: settings.notifications.noticePct,
    wall: settings.notifications.wallPct,
  };
}

export function toneOf(pct: number | null | undefined, th: Thresholds): Tone {
  if (pct == null || Number.isNaN(pct)) return "unknown";
  if (pct >= th.wall) return "crit";
  if (pct >= th.notice) return "warn";
  return "ok";
}
