import { useEffect, useRef, useState } from "react";
import { RefreshCw } from "lucide-react";
import { useStore } from "@/store/usageStore";
import { cmd } from "@/lib/tauri";
import { reduceMotion, useFlashOnChange, useTweenNumber } from "@/lib/motion";
import { PageSkeleton, hideTip, showTip } from "../Motion";
import { fmtAbsolute, fmtMoment, normalizeTimeMode, type TimeMode } from "@/lib/timefmt";
import { toneOf, thresholdsOf, type Thresholds, type Tone } from "@/lib/tone";
import { seatLabel } from "@/lib/seatLabel";
import { fmtAgo, freshStamp } from "@/lib/ago";
import { useMinuteTick } from "@/lib/useTick";
import { planShort } from "@/lib/planLabel";
import { localDayBack, localDayOf, hhmm, mdhm } from "../dates";
import { UsageMix, fmtMixUsd, mixBreakdown, mixPriceFootnote, PriceMark, type MixAmounts, type ModelMix, type ProjectMix } from "../UsageMix";
import "../h5.css";

// =============================================================================
// 今天會不會撞牆 (M3, D55 H5 換裝, D62 列式改版, D63 右欄結局值) — 決策層。
// 即時額度＝單卡三列：文字歸左（純判斷句＋錨點事實副行）、數字歸右
// （大百分比＋「這條額度的結局」——會撞牆顯觸頂時間、撐得到顯重置時
// 百分比、已撞牆才顯重置時間）。點陣條 1 格 = 1%（7×7 正方格、固定
// 尺寸不隨視窗伸縮），ghost 空格＝配速預測。
// 數字全部 CLI 帳號真值（D51）；本機活動表不分帳號（O10）並如實標示。
// =============================================================================

interface LimitOutlook {
  samples: number;
  percent: number | null;
  fetchedAt: string | null;
  resetsAt: string | null;
  /** v1.4.1（D78 #1）：最後樣本的窗已過重置時間——之後用了多少未知；percent 仍是上次的值。 */
  expired?: boolean;
  lastKnownPercent?: number | null;
  /** D71：短窗（窗長÷12）平滑斜率，%/h——燃燒速度顯示、流速、副句預警都用它。 */
  burnPerHour: number | null;
  /** D71：長窗＝整窗至今平均（%/h），與 pace ▼ 同一把尺；projectedAtReset 是它外推到重置時的 %。 */
  paceBurnPerHour?: number | null;
  projectedAtReset?: number | null;
  rShort?: number | null;
  rLong?: number | null;
  shortWindowMinutes?: number;
  /** 短窗速度若持續的見底時刻（rShort ≥ 1 才有）。 */
  etaShort?: string | null;
  /** 長窗平均外推的見底時刻（rLong ≥ 1 才有）；D72：5h 的見底時刻用它，不用短窗。 */
  etaLong?: string | null;
  /** 裁決見底時刻：兩窗都 ≥ 1 才有。 */
  etaAt: string | null;
  /** 重置後未滿一個短窗，先不裁決。 */
  insufficient?: boolean;
  /** v1.9.4（D99 D2）：最後一筆太舊（5h 條 1 小時、週條 24 小時）或外推的見底時刻已在過去——不給預測。 */
  stale?: boolean;
  /** v1.9.4（D99 A11）：這個窗裡換過方案——切點之前的樣本不算；到下一次重置前不下見底裁決。 */
  planChange?: { at: string; fromLabel: string; toLabel: string } | null;
  /** 換方案之後還沒有新樣本：percent 是 null，lastKnownPercent 是換方案前最後一筆（不是「已重置」）。 */
  awaitingAfterPlanChange?: boolean;
}

/** v1.9.4（D99 A8，線 B §6.1）：座位的方案（共用形狀）。known=false＝這個座位的方案從沒被記到。 */
interface SeatPlan {
  known: boolean;
  label: string | null;
  key: string | null;
  since: string | null;
  observedAt: string | null;
  previous: { key: string | null; label: string | null; until: string | null } | null;
}

type PlanSuggestion = "calibrating" | "upgrade" | "add_account" | "downgrade" | "keep" | "none";

interface Outlook {
  session: LimitOutlook;
  weekly: LimitOutlook;
  fable: LimitOutlook;
  windowStart: string | null;
  /** D97：按 API 計價排，帶四種 tokens。 */
  byEntrypoint: (MixAmounts & { entrypoint: string })[];
  topProjects: ProjectMix[];
  /** 2026-10-03：本窗模型分佈（本機全量）；舊 payload 沒有。 */
  topModels?: ModelMix[];
  /** D56/M5：本窗 exact 來源樣本（階梯圖）與窗內 429 事件時刻 */
  windowSamples: { at: string; percent: number; source: string }[];
  windowAnchors: string[];
  /** D56/M5：方案建議（近 14 天純看 %） */
  plan: {
    coverageDays: number;
    /** v1.4.1：門檻改累計（近 N 天內 ≥ M 天有樣本）。 */
    coverageLookbackDays?: number;
    coverageMinDays?: number;
    sessionSatDays: number;
    sessionPeakP90: number | null;
    weeklyPeak: number;
    fablePeak: number;
    dailyPeaks: { day: string; peak: number }[];
    /** v1.9.4（D99 Q3）：add_account＝Max 20x 用滿（上面沒有一級，多訂一個帳號）；none＝Team／Enterprise 等不給建議（卡片整張不顯示）。 */
    suggestion: PlanSuggestion;
    reason: string;
    /** v1.9.4（D99 A6）：統計窗起點（本地日）；limitedByPlan＝被方案起點截短，不是完整 14 天。 */
    statsFrom?: string;
    limitedByPlan?: boolean;
    minStatsDays?: number;
    statsDays?: number;
    statsWindowDays?: number;
    planLabel?: string | null;
    targetLabel?: string | null;
    stepFactor?: number | null;
    projectedPeakOnTarget?: number | null;
    downgradeSafetyPercent?: number;
  };
  /** v1.9.4（D99 A8）：被看的座位自己的方案（不是方案「建議」——那個是 plan）。舊後端沒有。 */
  seatPlan?: SeatPlan | null;
}

type Verdict = {
  tone: Tone;
  headline: string; // 純判斷句（D63：時間數字不再住這裡——文字歸左、數字歸右）
  outcome: string; // 右欄結局值：觸頂時間／重置時 %／重置時刻（隨裁決切換）
  outcomeHot: boolean; // 觸頂時間用 tone 色標警
  sub: string; // 左下副行：錨點事實（重置時刻）＋推論，D61 語序先事實後推論
  ghost: number | null; // 配速預測到重置時的 %，畫成點陣 ghost 空格（D42：畫得出＝反推得回）
};

function fmtSpan(ms: number): string {
  const mins = Math.max(0, Math.floor(ms / 60_000));
  if (mins < 60) return `${mins} 分鐘`;
  const h = Math.floor(mins / 60);
  if (h < 48) return `${h} 小時 ${mins % 60} 分`;
  return `${Math.floor(h / 24)} 天 ${h % 24} 小時`;
}

const TONE_RANK: Record<Tone, number> = { unknown: 0, ok: 1, warn: 2, crit: 3 };

/** 撞牆裁決：已滿 → 快見底 → 撐得到 → 樣本不足。
    D61 三修保留：時刻走使用者模式；tone 百分比地板（percent≥90 永不低於
    crit）＋閒置門檻相對化（0.05×par）。
    D63（側聊拍板）：右欄第二數值＝「這條額度的結局」——會撞牆顯觸頂時間、
    撐得到顯重置時 %、已撞牆／樣本不足退回重置時刻；重置時刻降級到左下
    副行常駐（錨點事實永遠可見，只是不佔主位）。 */
function verdictOf(o: LimitOutlook, mode: TimeMode, windowHours: number, th: Thresholds, now: number): Verdict {
  // v1.9.4（D99 A11）：換方案的那一刻（時刻用本地 HH:MM，不是今天就帶日期）。
  const switchAt = (iso: string) => (localDayOf(iso) === localDayOf(new Date(now).toISOString()) ? hhmm(iso) : mdhm(iso));
  const planSwitch = o.planChange ? `${planShort(o.planChange.fromLabel)}→${planShort(o.planChange.toLabel)}，${switchAt(o.planChange.at)}` : null;
  // v1.9.4（D99 A11）：換方案之後還沒有新樣本——換方案前的數字是另一個池的，不拿來裁決，也不是「已重置」。
  if (o.awaitingAfterPlanChange) {
    const last = o.lastKnownPercent;
    return {
      tone: "unknown",
      headline: "剛換方案，等新方案的第一筆",
      outcome: o.resetsAt ? `${fmtMoment(o.resetsAt, mode)} 重置` : "",
      outcomeHot: false,
      sub: `${planSwitch ? `剛換方案（${planSwitch}）` : "剛換方案"}｜換之前最後一筆 ${last != null ? `${Math.round(last)}%` : "不明"}，那是舊方案的額度池（1% 大小不同），不拿來預測`,
      ghost: null,
    };
  }
  // v1.4.1（D78 #1）：窗已過重置時間、之後沒有新樣本——不拿舊值裁決，老實說「不知道」。
  if (o.expired) {
    const last = o.lastKnownPercent ?? o.percent;
    const when = o.fetchedAt ? fmtMoment(o.fetchedAt, mode) : "先前";
    return {
      tone: "unknown",
      headline: "已重置 · 尚無新樣本",
      outcome: o.resetsAt ? `${fmtMoment(o.resetsAt, mode)} 已重置` : "",
      outcomeHot: false,
      sub: `重置後用了多少不知道｜上次 ${when} 為 ${last != null ? Math.round(last) : "?"}%`,
      ghost: null,
    };
  }
  if (o.percent == null) {
    return { tone: "unknown", headline: "無資料", outcome: "", outcomeHot: false, sub: "還沒有這條額度的觀測記錄", ghost: null };
  }
  // v1.2：地板門檻讀設定的留意值／撞牆值（lib/tone.ts 唯一來源）。
  const pctFloor: Tone = toneOf(o.percent, th);
  const floor = (v: Verdict): Verdict =>
    TONE_RANK[pctFloor] > TONE_RANK[v.tone] ? { ...v, tone: pctFloor } : v;
  // v1.9.4：「不預測」的狀態（資料太舊、剛換方案）維持灰色——只有百分比本身已到留意值以上才上色，
  // 不然 51% 會被染成「沒事」的藍，跟「先不預測」的意思打架。
  const floorHot = (v: Verdict): Verdict => (pctFloor === "warn" || pctFloor === "crit" ? floor(v) : v);

  const reset = o.resetsAt ? `${fmtMoment(o.resetsAt, mode)} 重置` : "重置時間未知";
  // D71：「重置時約 X%」＝長窗（整窗平均）外推，跟 ▼ 同尺，是 forecast 線。
  const proj = o.projectedAtReset != null ? Math.min(100, Math.round(o.projectedAtReset)) : null;
  const projOutcome = proj != null ? `□ 重置時約 ${proj}%` : reset;
  const safeOutcome = (v: Omit<Verdict, "outcome" | "outcomeHot" | "sub" | "ghost">): Verdict =>
    floor(
      proj != null
        ? { ...v, outcome: projOutcome, outcomeHot: false, sub: reset, ghost: proj }
        : { ...v, outcome: reset, outcomeHot: false, sub: "", ghost: null },
    );
  // 後端一定會帶；fallback 照 D73／D93：5h 看 60 分、7d／Fable 看 24 小時。
  const shortMin = o.shortWindowMinutes ?? (windowHours <= 5 ? 60 : 24 * 60);
  const shortSpan = shortMin >= 60 ? `${Math.round(shortMin / 60)} 小時` : `${shortMin} 分鐘`;

  // v1.9.4（D99 Q7／F1）：滿格措辭中性化——開了 Extra usage 的人 100% 不一定是牆，
  // 這裡只陳述事實（方案額度用完了），不預設「到 100% 就停」。
  if (o.percent >= 100) {
    return { tone: "crit", headline: "已用完方案額度", outcome: reset, outcomeHot: false, sub: "", ghost: null };
  }
  // v1.9.4（D99 A11）：這個窗裡剛換過方案——切點前的樣本已經切掉，到下一次重置前不下見底裁決。
  if (o.planChange) {
    return floorHot({
      tone: "unknown",
      headline: "剛換方案，重新觀察",
      outcome: reset,
      outcomeHot: false,
      sub: `剛換方案（${planSwitch}）｜換之前的記錄是另一個額度池（1% 大小不同），不混在一起；到下一次重置前先不預測`,
      ghost: null,
    });
  }
  // v1.9.4（D99 D2）：最後一筆太舊（或外推出的見底時刻已經過了）——不拿舊斜率往後推。
  if (o.stale) {
    const ago = fmtAgo(o.fetchedAt, now);
    return floorHot({
      tone: "unknown",
      headline: "資料太舊，先不預測",
      outcome: reset,
      outcomeHot: false,
      sub: `最後一筆是${ago ? ` ${ago}` : "很久以前"}的，等新的記錄進來再預測`,
      ghost: null,
    });
  }
  // D71：重置後未滿一個短窗——樣本太少，先不裁決（AWS／GCP 資料不足不預測）。
  if (o.insufficient) {
    return floor({ tone: "unknown", headline: "觀察中", outcome: reset, outcomeHot: false, sub: `重置後未滿 ${shortSpan}，先不裁決`, ghost: null });
  }
  const shortHot = o.rShort != null && o.rShort >= 1;
  const longHot = o.rLong != null && o.rLong >= 1;

  // D73：三條額度同一套——觸頂時刻＝短窗（照現在的速度），重置時 %＝長窗（照整窗平均）。
  // 5h 的短窗已從 25 分拉長到 60 分（回放：跳動 3%、誤差中位 24 分），不再需要 D72 的借長窗特例。
  const verdictEta = o.etaAt;

  // D71 主句：SRE 式雙窗 AND——整窗平均與近期速度都超過配速才判「會提早見底」。
  if (shortHot && longHot && verdictEta && o.resetsAt) {
    const eta = new Date(verdictEta).getTime();
    const resetMs = new Date(o.resetsAt).getTime();
    if (eta < resetMs) {
      return floor({
        tone: o.percent >= th.notice ? "crit" : "warn",
        headline: "會提早見底",
        outcome: `約 ${fmtMoment(verdictEta, mode)} 觸頂`,
        outcomeHot: true,
        sub: `${reset}｜□ 比重置早 ${fmtSpan(resetMs - eta)}`,
        ghost: null,
      });
    }
  }
  // v1.9.4（D99 D5）：只有 Desktop 樣本的座位借不到重置時刻——不知道什麼時候重置，
  // 就不能說「撐得到」（以前同一行印「重置時間未知｜…仍撐得到重置」，自相矛盾）。
  const resetUnknown = !o.resetsAt;
  const cantTell = "重置時間未知，無法判斷撐不撐得到";
  // 副句預警（forecast 線）：只有近期超速——撐得到，但近 N 的速度若持續會見底。
  if (shortHot) {
    if (resetUnknown) {
      return floor({ tone: "warn", headline: "近期偏快", outcome: projOutcome, outcomeHot: false, sub: `近 ${shortSpan}燒得比配速快｜${cantTell}`, ghost: proj });
    }
    const etaBeforeReset =
      o.etaShort && o.resetsAt && new Date(o.etaShort).getTime() < new Date(o.resetsAt).getTime();
    const warn = etaBeforeReset
      ? `近 ${shortSpan}的速度若持續，約 ${fmtMoment(o.etaShort as string, mode)} 見底`
      : `近 ${shortSpan}燒得比配速快，照這速度仍撐得到重置`;
    return floor({ tone: "warn", headline: "撐得到，但近期偏快", outcome: projOutcome, outcomeHot: false, sub: `${reset}｜${warn}`, ghost: proj });
  }
  // 只有整窗平均超前——近期已慢下來，不算「還在燒」（SRE 短窗負責快速熄燈）。
  if (longHot) {
    return floor({ tone: "warn", headline: "超前配速，近期已放慢", outcome: projOutcome, outcomeHot: false, sub: `${reset}｜整窗平均已超過配速，近 ${shortSpan}已慢下來`, ghost: proj });
  }
  if (resetUnknown && o.samples >= 2) {
    // 燒得不快，但不知道重置在哪——只講速度，不講撐不撐得到。
    const slow = o.rShort != null && o.rShort <= 0.05;
    return floor({ tone: "ok", headline: slow ? "幾乎沒在燒" : "速度在配速內", outcome: projOutcome, outcomeHot: false, sub: cantTell, ghost: proj });
  }
  if (o.rShort != null && o.rShort <= 0.05) return safeOutcome({ tone: "ok", headline: "幾乎沒在燒" });
  if (o.samples < 2) {
    return floor({
      tone: "unknown",
      headline: "樣本累積中",
      outcome: reset,
      outcomeHot: false,
      sub: "有足夠的觀測記錄後開始預測",
      ghost: null,
    });
  }
  return safeOutcome({ tone: "ok", headline: "撐得到重置" });
}

const ENTRYPOINT_LABEL: Record<string, string> = {
  "claude-desktop": "Claude Desktop",
  cli: "Claude Code CLI",
  "local-agent": "雲端 session",
  unknown: "不明",
};

// D59 速度感：流速依「相對重置窗」比值 R = burn ÷ (100%/窗長)——
// 寬裕慢流、跟重置同速較快（R=1→12s）、明顯超限變急（R≥5→2.4s）。
const flowDur = (ratio: number) => Math.min(36, Math.max(2.4, 12 / Math.max(ratio, 0.01)));

// D62 點陣條：1 格 = 1%（兩排 × 50 格）。
// D63：7×7 正方格、格距 3、固定尺寸（寬 500）——不隨視窗伸縮。
const DOT_COLS = 50;
const DOT_ROWS = 2;
const DOT_PITCH = 10; // 格距（水平與垂直同值 → 正方格陣）
const DOT_CELL = 7; // 格邊長（正方形）
const TONE_COLOR: Record<Tone, string> = {
  ok: "#4E7ECF",
  warn: "#D99A2B",
  crit: "#C13A30",
  unknown: "#969EAC",
};

function DotStrip({
  percent,
  ghostTo,
  burnPerHour,
  tone,
  windowHours,
}: {
  percent: number | null;
  ghostTo: number | null;
  burnPerHour: number | null;
  tone: Tone;
  windowHours: number;
}) {
  const pct = percent == null ? null : Math.round(Math.min(percent, 100));

  // E10（D77）：記上一幀 percent，只有「新亮的格子」彈出（原本亮的不動）。
  // 第一次載入 prevRef 還是 null → 不彈，符合規則 1（入場不算變化）。
  const prevRef = useRef<number | null>(null);
  const [popFrom, setPopFrom] = useState<number | null>(null);
  useEffect(() => {
    const prev = prevRef.current;
    prevRef.current = pct;
    if (pct == null || prev == null || pct <= prev || reduceMotion()) return;
    setPopFrom(prev);
    const t = window.setTimeout(() => setPopFrom(null), (pct - prev) * 35 + 400);
    return () => window.clearTimeout(t);
  }, [pct]);

  // v1.9.4：灰色（unknown）的「先不預測」狀態仍有百分比——照畫格子（灰），只是不帶預測。
  if (percent == null || pct == null) return null;
  const p = pct;
  const g = ghostTo != null ? Math.round(Math.min(ghostTo, 100)) : null;
  const wall = percent >= 100;
  const ratio = burnPerHour != null ? burnPerHour / (100 / windowHours) : null;
  const idle = !wall && (ratio == null || ratio <= 0.05);
  const flowing = !wall && !idle && p > 0;
  const inset = (DOT_PITCH - DOT_CELL) / 2;
  const color = TONE_COLOR[tone];
  const cells = [];
  for (let i = 0; i < DOT_COLS * DOT_ROWS; i++) {
    const col = Math.floor(i / DOT_ROWS);
    const row = i % DOT_ROWS;
    const x = col * DOT_PITCH + inset;
    const y = row * DOT_PITCH + 1;
    if (i < p) {
      // 這一格是這次刷新新亮的 → 彈出（stagger 35ms），彈完才交還給流動動畫。
      const isNew = popFrom != null && i >= popFrom;
      cells.push(
        <rect
          key={i}
          x={x}
          y={y}
          width={DOT_CELL}
          height={DOT_CELL}
          rx={1.9}
          fill={color}
          data-i={i}
          className={isNew ? "pop" : flowing || wall || idle ? "pf-cell" : undefined}
          style={
            {
              "--pf-i": col,
              ...(isNew ? { animationDelay: `${(i - (popFrom as number)) * 35}ms` } : {}),
            } as React.CSSProperties
          }
        />,
      );
    } else if (g != null && i < g) {
      // ghost 配速預測格：只描邊、不參與流動（推測不是流量）
      cells.push(
        <rect
          key={i}
          x={x + 0.5}
          y={y + 0.5}
          width={DOT_CELL - 1}
          height={DOT_CELL - 1}
          rx={1.6}
          fill="none"
          stroke={color}
          strokeWidth={1}
          opacity={0.75}
          data-i={i}
          pointerEvents="all"
        />,
      );
    } else {
      cells.push(
        <rect key={i} x={x} y={y} width={DOT_CELL} height={DOT_CELL} rx={1.9} fill="#E8ECF2" data-i={i} />,
      );
    }
  }
  const w = DOT_COLS * DOT_PITCH;
  const h = DOT_ROWS * DOT_PITCH + 2;
  const style: Record<string, string | number> = { "--pf-n": DOT_COLS };
  if (flowing && ratio != null) style["--pf-dur"] = `${flowDur(ratio).toFixed(1)}s`;
  return (
    <svg
      className={`dots pace-flow-svg${wall ? " wall" : idle ? " idle" : ""}`}
      style={style as React.CSSProperties}
      width={w}
      height={h}
      viewBox={`0 0 ${w} ${h}`}
      aria-hidden
      /* E21（D77）：hover 一格說「第 N 格 ＝ N%」，並標明它是已用／預測／未用
         ——ghost 格的「這是配速預測、不是已用」是圖上最讀不出來的一件事。 */
      onMouseMove={(ev) => {
        const t = ev.target as SVGElement;
        const raw = t.dataset?.i;
        if (raw == null) {
          hideTip();
          return;
        }
        const i = Number(raw);
        const what = i < p ? "已用" : g != null && i < g ? "配速預測" : "未用";
        showTip(ev, `第 ${i + 1} 格 ＝ ${i + 1}%（${what}）`);
      }}
      onMouseLeave={hideTip}
    >
      {cells}
    </svg>
  );
}

function LimitRow({
  label,
  en,
  o,
  windowHours,
  mode,
  th,
  now,
}: {
  label: string;
  en: string;
  o: LimitOutlook;
  windowHours: number;
  mode: TimeMode;
  th: Thresholds;
  /** v1.9.4（D1）：頁面的 60 秒 tick——「最後一筆是 N 分前的」要會走。 */
  now: number;
}) {
  const v = verdictOf(o, mode, windowHours, th, now);

  // ---- v1.4 動效（D77 規則 1：全部記上一幀比較，第一次載入不算「變」）----
  // E07 大數字補間 0.6s；E09 變了的數字底色閃一下琥珀（E07 說變成多少、E09 說是哪一個）
  const pctTarget = o.expired || o.percent == null ? null : Math.round(o.percent);
  const shownPct = useTweenNumber(pctTarget);
  const pctRef = useFlashOnChange<HTMLSpanElement>(pctTarget);

  // E11 燃燒速度差值飄出：先四捨五入到 0.1（畫面上的精度）再比，差值為 0 就不飄。
  // 它只是「這一次刷新比上次多／少多少」的事實陳述——D72 已證明加速／減速
  // 三態訊號是噪音（翻轉率 25%），這個標籤不能被讀成趨勢判斷。
  const burn = o.burnPerHour == null ? null : Math.round(o.burnPerHour * 10) / 10;
  const burnRef = useFlashOnChange<HTMLSpanElement>(burn);
  const prevBurn = useRef<number | null>(null);
  const deltaSeq = useRef(0);
  const [delta, setDelta] = useState<{ id: number; v: number } | null>(null);
  useEffect(() => {
    const prev = prevBurn.current;
    prevBurn.current = burn;
    if (prev == null || burn == null || reduceMotion()) return;
    const d = Math.round((burn - prev) * 10) / 10;
    if (d === 0) return; // 四捨五入到 0.1 後為 0 → 小抖動，不飄
    const id = ++deltaSeq.current;
    setDelta({ id, v: d });
    const t = window.setTimeout(() => setDelta((c) => (c?.id === id ? null : c)), 1700);
    return () => window.clearTimeout(t);
  }, [burn]);

  // E12 裁決句 crossfade：結論真的變了才 0.18s 淡出→換字→淡入（同一句不動）。
  const [headline, setHeadline] = useState(v.headline);
  const [swap, setSwap] = useState(false);
  useEffect(() => {
    if (v.headline === headline) return;
    if (reduceMotion()) {
      setHeadline(v.headline);
      return;
    }
    setSwap(true);
    const t = window.setTimeout(() => {
      setHeadline(v.headline);
      setSwap(false);
    }, 180);
    return () => window.clearTimeout(t);
  }, [v.headline, headline]);

  return (
    <div className={`lrow ${v.tone}`}>
      <div className="lrow-main">
        <div className="lrow-top">
          <span className="lrow-label zh">{label}</span>
          <span className="lrow-en">{en}</span>
          <span className={`lrow-verdict zh${swap ? " swap" : ""}`}>{headline}</span>
          <span className="lrow-burn">
            <span ref={burnRef}>{burn != null ? `${burn.toFixed(1)} %/h` : ""}</span>
            {delta && (
              <span key={delta.id} className={`delta ${delta.v > 0 ? "up" : "down"}`}>
                {delta.v > 0 ? `▲ +${delta.v.toFixed(1)}` : `▼ ${delta.v.toFixed(1)}`}
              </span>
            )}
          </span>
        </div>
        <DotStrip
          percent={o.expired ? null : o.percent}
          ghostTo={v.ghost}
          burnPerHour={o.burnPerHour}
          tone={v.tone}
          windowHours={windowHours}
        />
        {v.sub && <div className="lrow-extra zh">{v.sub}</div>}
      </div>
      <div className="lrow-side">
        <span className="lrow-pct" ref={pctRef}>
          {shownPct == null ? "—" : (
            <>
              {shownPct}
              <i>%</i>
            </>
          )}
        </span>
        {v.outcome && (
          <span className={`lrow-outcome zh${v.outcomeHot ? " hot" : ""}`}>{v.outcome}</span>
        )}
      </div>
    </div>
  );
}

/* ---------------- 本窗階梯圖（D56/M5）：當前 5h 窗的 % 爬升曲線 ----------------
   全部幾何由資料推：x＝時間在窗內的位置（窗起點→重置），y＝%。階梯＝樣本間
   保持前值；虛線＝以目前燃燒速度外推到重置（推測不是流量，只描線）；紅刻＝窗
   內 429 事件；100% 線常駐。exact 來源（probe／cli-cache）才進圖（D51）。 */
function StairChart({
  samples,
  anchors,
  windowStart,
  resetsAt,
  burnPerHour,
  planSwitchAt,
}: {
  samples: { at: string; percent: number; source?: string }[];
  anchors: string[];
  windowStart: string;
  resetsAt: string | null;
  burnPerHour: number | null;
  /** v1.9.4（D99 A11）：這個窗裡換方案的時刻——畫一條線標「換方案」，之前的樣本是舊方案的池。 */
  planSwitchAt?: string | null;
}) {
  const W = 600, H = 150, L = 34, R = 12, T = 12, B = 22;
  const t0 = new Date(windowStart).getTime();
  const t1 = resetsAt ? new Date(resetsAt).getTime() : t0 + 5 * 3600_000;
  const now = Math.min(Date.now(), t1);
  const x = (ms: number) => L + (W - L - R) * Math.min(1, Math.max(0, (ms - t0) / (t1 - t0)));
  const y = (p: number) => T + (H - T - B) * (1 - Math.min(100, Math.max(0, p)) / 100);
  const ms = (s: string) => new Date(s).getTime();

  let path = "";
  samples.forEach((s, i) => {
    const X = x(ms(s.at)), Y = y(s.percent);
    path += i === 0 ? `M${X.toFixed(1)},${Y.toFixed(1)}` : ` H${X.toFixed(1)} V${Y.toFixed(1)}`;
  });
  const last = samples[samples.length - 1];
  if (last) path += ` H${x(now).toFixed(1)}`;
  let ghost = "";
  if (last && burnPerHour != null && burnPerHour > 0 && now < t1) {
    const hrs = (t1 - now) / 3600_000;
    const proj = Math.min(100, last.percent + burnPerHour * hrs);
    ghost = `M${x(now).toFixed(1)},${y(last.percent).toFixed(1)} L${x(t1).toFixed(1)},${y(proj).toFixed(1)}`;
  }
  // E22（D77）：十字線＋最近樣本的點——圖上只有形狀沒有刻度，「某一個時刻
  // 的精確 %」確實讀不出來。429 紅刻也納入 hover（顯示撞牆時刻）。
  const svgRef = useRef<SVGSVGElement | null>(null);
  const [cross, setCross] = useState<{ x: number; y: number } | null>(null);
  const onMove = (ev: React.MouseEvent<SVGSVGElement>) => {
    const anchorAt = (ev.target as SVGElement).dataset?.anchor;
    if (anchorAt) {
      setCross(null);
      showTip(ev, `${fmtAbsolute(anchorAt)} 撞牆（429）`);
      return;
    }
    const el = svgRef.current;
    if (!el || samples.length === 0) return;
    const r = el.getBoundingClientRect();
    const vx = ((ev.clientX - r.left) / r.width) * W;
    let best = samples[0];
    let bestD = Infinity;
    for (const s of samples) {
      const d = Math.abs(x(ms(s.at)) - vx);
      if (d < bestD) {
        bestD = d;
        best = s;
      }
    }
    setCross({ x: x(ms(best.at)), y: y(best.percent) });
    const mins = Math.max(0, Math.round((ms(best.at) - t0) / 60_000));
    showTip(
      ev,
      `窗內 +${Math.floor(mins / 60)}h${String(mins % 60).padStart(2, "0")} ＝ ${Math.round(best.percent)}%${best.source === "desktop-history" ? "（Desktop）" : ""}`,
    );
  };
  const onLeave = () => {
    setCross(null);
    hideTip();
  };

  const grid = [25, 50, 75].map((v) => (
    <g key={v}>
      <line x1={L} y1={y(v)} x2={W - R} y2={y(v)} stroke="#E1E5EB" strokeDasharray="2 5" />
      <text x={L - 6} y={y(v) + 3.5} textAnchor="end" fontSize={9} fill="#969EAC">{v}</text>
    </g>
  ));
  return (
    <svg
      ref={svgRef}
      className="stair"
      width={W}
      height={H}
      viewBox={`0 0 ${W} ${H}`}
      style={{ display: "block", maxWidth: "100%" }}
      onMouseMove={onMove}
      onMouseLeave={onLeave}
    >
      {grid}
      <line x1={L} y1={y(100)} x2={W - R} y2={y(100)} stroke="#C13A30" strokeDasharray="3 4" strokeWidth={1} />
      <text x={L - 6} y={y(100) + 3.5} textAnchor="end" fontSize={9} fill="#C13A30" fontWeight={700}>100</text>
      <line x1={L} y1={y(0)} x2={W - R} y2={y(0)} stroke="#D3D9E2" />
      {ghost && <path d={ghost} fill="none" stroke="#4E7ECF" strokeWidth={1.4} strokeDasharray="3 4" opacity={0.7} />}
      {path && <path d={path} fill="none" stroke="#2E5FB7" strokeWidth={2} strokeLinejoin="round" />}
      {samples.map((s, i) => (
        // v1.8.12（D90）：Desktop 補位的樣本也進圖了——點的顏色說它是誰的（Desktop 紫）
        <circle key={i} cx={x(ms(s.at))} cy={y(s.percent)} r={2} fill={s.source === "desktop-history" ? "#7E6AC8" : "#2E5FB7"} />
      ))}
      {anchors.map((a, i) => (
        <g key={`a${i}`}>
          <line x1={x(ms(a))} y1={T - 4} x2={x(ms(a))} y2={T + 6} stroke="#C13A30" strokeWidth={2} />
          {/* 2px 的刻太細抓不到——補一塊透明的命中區，hover 才叫得出時刻 */}
          <rect x={x(ms(a)) - 5} y={T - 7} width={10} height={17} fill="transparent" data-anchor={a} />
        </g>
      ))}
      <line x1={x(now)} y1={T} x2={x(now)} y2={y(0)} stroke="#969EAC" strokeDasharray="2 3" />
      {planSwitchAt && ms(planSwitchAt) > t0 && ms(planSwitchAt) < t1 && (
        <g pointerEvents="none">
          <line x1={x(ms(planSwitchAt))} y1={T} x2={x(ms(planSwitchAt))} y2={y(0)} stroke="#5E6470" strokeDasharray="4 3" strokeWidth={1.2} />
          <text x={x(ms(planSwitchAt)) + 4} y={T + 9} textAnchor="start" fontSize={8.5} fill="#5E6470" fontWeight={700}>換方案</text>
        </g>
      )}
      {cross && (
        <g pointerEvents="none">
          <line x1={cross.x} y1={T} x2={cross.x} y2={y(0)} stroke="#969EAC" strokeDasharray="2 2" />
          <circle cx={cross.x} cy={cross.y} r={3} fill="#2E5FB7" />
        </g>
      )}
      <text x={x(now)} y={H - 8} textAnchor="middle" fontSize={8.5} fill="#5E6470">現在</text>
      <text x={L} y={H - 8} textAnchor="start" fontSize={8.5} fill="#969EAC">{fmtAbsolute(windowStart)}</text>
      <text x={W - R} y={H - 8} textAnchor="end" fontSize={8.5} fill="#969EAC">
        {resetsAt ? `${fmtAbsolute(resetsAt)} 重置` : "重置時間未知"}
      </text>
    </svg>
  );
}

/* ---------------- 方案建議（D56/M5）：近 14 天純看 % ---------------- */
// v1.9.4（D99 Q3）：none 不在表裡——Team／Enterprise／認不得的方案整張卡不顯示。
const PLAN_META: Record<Exclude<PlanSuggestion, "none">, { label: string; tone: Tone }> = {
  calibrating: { label: "樣本累積中", tone: "unknown" },
  upgrade: { label: "考慮升級", tone: "warn" },
  // Max 20x 已是最高一級：用不夠的解法是多一個帳號（主人的實際做法），不是「升級」。
  add_account: { label: "可以多訂一個帳號", tone: "warn" },
  downgrade: { label: "可考慮降級", tone: "ok" },
  keep: { label: "現在的方案剛好", tone: "ok" },
};

function PeakBars({ peaks, statsFrom }: { peaks: { day: string; peak: number }[]; statsFrom?: string | null }) {
  // 近 14 天每日 5h 峰值：長條高＝峰值 %（1:1 反推），≥100 紅
  const days: { day: string; peak: number | null; before: boolean }[] = [];
  const map = new Map(peaks.map((p) => [p.day, p.peak]));
  for (let i = 13; i >= 0; i--) {
    // v1.8.13（D91 T-13）：本地日（以前 toISOString 是 UTC 日，跟後端改成的本地日對不上）
    const day = localDayBack(i);
    // v1.9.4（D99 A6）：統計窗從方案起點算——起點之前的日子不是「無樣本」，是「另一個池、不算」。
    days.push({ day, peak: map.get(day) ?? null, before: !!statsFrom && day < statsFrom });
  }
  const W = 224, H = 46, pitch = W / 14, bw = pitch * 0.62;
  return (
    <svg width={W} height={H} viewBox={`0 0 ${W} ${H}`} aria-hidden style={{ display: "block" }}>
      <line x1={0} y1={H - 6} x2={W} y2={H - 6} stroke="#D3D9E2" />
      {days.map((d, i) => {
        const x = i * pitch + (pitch - bw) / 2;
        if (d.before)
          return <rect key={d.day} x={x} y={H - 8} width={bw} height={2} fill="#E1E5EB"><title>{d.day.slice(5)} · 換方案（或開始記錄方案）之前，不算進建議</title></rect>;
        if (d.peak == null)
          return <rect key={d.day} x={x} y={H - 8} width={bw} height={2} fill="#C2C8D2"><title>{d.day.slice(5)} · 無樣本</title></rect>;
        const h = ((H - 8) * d.peak) / 100;
        return (
          <rect key={d.day} x={x} y={H - 6 - h} width={bw} height={h} rx={1} fill={d.peak >= 100 ? "#C13A30" : "#4E7ECF"}>
            <title>{d.day.slice(5)} · 峰值 {Math.round(d.peak)}%</title>
          </rect>
        );
      })}
    </svg>
  );
}

export function TodayPage() {
  const usage = useStore((s) => s.usage);
  const settings = useStore((s) => s.settings);
  const mode = normalizeTimeMode(settings.general.timeFormat);
  const th = thresholdsOf(settings);
  // v1.3：切換器選的座位（null＝CLI 目前座位）。裁決走同一條路徑，樣本舊就顯示舊結論。
  const viewSeatId = useStore((s) => s.viewSeatId);
  const seats = useStore((s) => s.seats);
  const seatSnapshot = useStore((s) => s.seatSnapshot);
  const viewSeat = seats.find((s) => s.id === viewSeatId);
  const [data, setData] = useState<Outlook | null>(null);
  const [error, setError] = useState<string | null>(null);
  // v1.9.4（D1）：「N 分前」每分鐘自己走（以前頂端照抄後端烙死的 note，停在刷新那一刻）。
  const now = useMinuteTick();

  const load = () =>
    cmd
      .getTodayOutlook(viewSeatId)
      .then((d) => {
        setData(d as Outlook);
        setError(null);
      })
      .catch((e) => setError(String(e)));

  useEffect(() => {
    void load();
  }, [usage.lastSuccessAt, viewSeatId]);

  if (error) {
    return <div className="h5 warn-band zh">讀取失敗:{error}</div>;
  }
  if (!data) {
    // E28（D77）：「這裡會有東西，正在拿」——跟「這裡沒東西」區分開。
    return <PageSkeleton />;
  }

  // v1.9.4（D99 B2）：沒牌價的入口 usd 是 null——佔比只在有牌價的部分裡算。
  const totalUsd = data.byEntrypoint.reduce((s, e) => s + (e.usd ?? 0), 0);
  const entryNote = mixPriceFootnote(data.byEntrypoint);

  // v1.9.4（D99 A8）：方案 chip 讀「被看的座位」自己的方案（以前讀目前登入帳號的 planName，
  // 切到別的帳號看時方案名是錯的）。舊後端沒有 seatPlan 才退回 planName。
  const sp = data.seatPlan;
  const planChip =
    sp === undefined
      ? usage.data?.planName
        ? { label: usage.data.planName, readAt: null as string | null, known: true }
        : null
      : sp && sp.known
        ? { label: sp.label ?? "方案不明", readAt: sp.observedAt, known: true }
        : { label: "方案未知", readAt: null, known: false };
  const readAtText = (iso: string | null) => {
    if (!iso) return null;
    return localDayOf(iso) === localDayOf(new Date(now).toISOString()) ? hhmm(iso) : mdhm(iso);
  };
  const plan = data.plan;
  const showPlanCard = plan.suggestion !== "none";
  const planMeta = plan.suggestion === "none" ? null : PLAN_META[plan.suggestion] ?? PLAN_META.calibrating;
  const useStatsDays = plan.statsDays != null && (!!plan.limitedByPlan || (plan.minStatsDays != null && plan.statsDays < plan.minStatsDays));
  // 剛換方案／資料太舊時裁決不預測——階梯圖的外推線與圖例跟著拿掉（同一個條件）。
  const noForecast = !!(data.session.planChange || data.session.stale || data.session.awaitingAfterPlanChange);
  const planWindowTag = plan.limitedByPlan && plan.statsFrom ? `自 ${plan.statsFrom.slice(5)} 起` : "近 14 天";

  return (
    <div className="h5 space-y-4">
      {usage.notice && <div className="warn-band zh">⚠ {usage.notice}</div>}
      {viewSeatId === "*" && (
        // v1.8.11（D89）：全量視角只用在趨勢與歷史——百分比是每個帳號各自的，這頁看目前登入的帳號。
        <p className="foot-note zh">頂欄選的是「全部帳號」：這頁的額度是每個帳號各自的，顯示目前登入的帳號；全量合計在「趨勢」與「歷史檢視」。</p>
      )}
      {viewSeat && (
        <div className="warn-band zh">
          <span>
            看的是「{seatLabel(viewSeat, settings.accounts.aliases)}」的最後已知值
            {seatSnapshot?.scrapedAt ? `（${fmtAgo(seatSnapshot.scrapedAt, now) ?? "時間不明"}）` : ""}——
            這個帳號沒登入 CLI 就不會更新，裁決只是照舊樣本推。
          </span>
        </div>
      )}

      {/* 即時額度：單卡三列（D62 主人圈 C，D63 右欄結局值） */}
      <article className="card">
        <div className="card-head">
          <span className="card-title zh">即時額度</span>
          <span className="card-tag">LIVE · 1 格 = 1%</span>
          {/* v1.4.1（D78 #5）：刷新時間是即時事件，從趨勢頁搬到這裡。 */}
          <span className="right">
            {!showPlanCard && planChip && (
              // 協調者交辦（審查 23r 偏差 8）：Team／Enterprise 沒有方案建議卡，方案名改放這裡——這些人也該看得到自己的方案
              <span className={`chip${planChip.known ? "" : " ghost"}`}>
                <b>{planChip.label}</b>
                {planChip.readAt && <span style={{ color: "var(--h5-ink3)" }}>方案讀自 {readAtText(planChip.readAt)}</span>}
              </span>
            )}
            <span className="chip">
              <span className="pulse" />
              {/* v1.9.4（D1）：時間那段用樣本時刻即時算；來源（cli／probe／desktop）照 note 保留 */}
              <span>{usage.data?.currentSession ? freshStamp(usage.data.currentSession, now) : "—"}</span>
            </span>
          </span>
        </div>
        <div style={{ marginTop: 4 }}>
          <LimitRow label="5h 窗口" en="SESSION" o={data.session} windowHours={5} mode={mode} th={th} now={now} />
          <LimitRow label="週 · 全模型" en="WEEKLY ALL" o={data.weekly} windowHours={168} mode={mode} th={th} now={now} />
          <LimitRow label="週 · Fable" en="WEEKLY FABLE" o={data.fable} windowHours={168} mode={mode} th={th} now={now} />
        </div>
      </article>

      {/* 本窗階梯圖（D56/M5） */}
      <article className="card">
        <div className="card-head">
          <span className="card-title zh">這個 5h 窗，消耗是怎麼累積的</span>
          <span className="card-tag">WINDOW STAIRCASE · EXACT SAMPLES</span>
          <div className="right">
            <span className="card-tag" style={{ color: "var(--h5-ink2)" }}>
              {data.windowSamples.length} 筆 · {data.windowAnchors.length} 次 429
            </span>
          </div>
        </div>
        {data.windowStart && data.windowSamples.length > 0 ? (
          <div style={{ marginTop: 8 }}>
            <StairChart
              samples={data.windowSamples}
              anchors={data.windowAnchors}
              windowStart={data.windowStart}
              resetsAt={data.session.resetsAt}
              // v1.9.4：剛換方案或資料太舊時裁決不預測——圖上也不畫「照目前速度外推」的虛線，免得兩邊說法不一。
              burnPerHour={noForecast ? null : data.session.burnPerHour}
              planSwitchAt={data.session.planChange?.at ?? null}
            />
            <div className="legend-row">
              <span className="lg"><span className="sw bar" />觀測記錄（探針／CLI 快取）</span>
              <span className="lg"><span className="sw" style={{ borderRadius: "50%", background: "#7E6AC8" }} />Desktop 補位（同帳號）</span>
              {/* 審查 23r S4：跟 burnPerHour 同一個條件——不畫外推虛線時圖例也不出 */}
              {noForecast ? (
                <span className="lg zh">先不預測（{data.session.stale ? "資料太舊" : "剛換方案"}），不畫外推線</span>
              ) : (
                <span className="lg"><span className="sw stair-ghost" />照目前速度外推到重置（推測）</span>
              )}
              <span className="lg"><span className="sw wall" />被限流記錄（429，本機）</span>
              {data.session.planChange && (
                <span className="lg zh">
                  <span className="sw" style={{ height: 0, borderRadius: 0, borderTop: "1.5px dashed #5E6470" }} />
                  換方案（線左邊是舊方案的額度池，不拿來預測）
                </span>
              )}
            </div>
          </div>
        ) : (
          <p className="foot-note zh" style={{ marginTop: 8 }}>
            {data.windowStart ? "這個窗口還沒有觀測記錄。" : "窗口起點未知——等下一筆 5h 記錄後才能畫圖。"}
          </p>
        )}
      </article>

      {/* 方案建議（D56/M5；v1.9.4 D99 Q3：認方案——Team／Enterprise 等不給建議時整張卡不顯示） */}
      {showPlanCard && planMeta && (
        <article className="card">
          <div className="card-head">
            <span className="card-title zh">方案建議</span>
            <span className="card-tag">PLAN FIT · {planWindowTag} · 純看 %</span>
            {planChip && (
              <div className="right">
                {/* v1.9.4（D99 A8）：方案名跟著被看的座位走；旁邊寫是什麼時候讀到的（方案是 App 從登入資料讀的，不是即時查詢） */}
                <span className={`chip${planChip.known ? "" : " ghost"}`} title={planChip.known ? undefined : "這個帳號的方案還沒被記錄過（登入它一次才會開始記）"}>
                  <b>{planChip.label}</b>
                  {planChip.readAt && <span style={{ color: "var(--h5-ink3)" }}>方案讀自 {readAtText(planChip.readAt)}</span>}
                </span>
              </div>
            )}
          </div>
          <div className="plan-body">
            <div style={{ minWidth: 0, flex: 1 }}>
              <div className="pace-verdict" style={{ marginTop: 8 }}>
                <span className={`dot ${planMeta.tone}`} />
                <span className={`plan-headline ${planMeta.tone}`}>{planMeta.label}</span>
              </div>
              <div className="pace-minis zh">{plan.reason}</div>
              <div className="pstats" style={{ gap: 22 }}>
                <div className="pstat">
                  {/* 審查 23r S3：統計窗被方案起點截短、或天數不夠時，看的是 statsDays／minStatsDays（不是 60 天回看的 coverageDays） */}
                  {useStatsDays ? (
                    <>
                      <div className="k">統計天數</div>
                      <div className="v">{plan.statsDays}<i>天{plan.minStatsDays != null && (plan.statsDays ?? 0) < plan.minStatsDays ? `／需 ${plan.minStatsDays}` : ""}</i></div>
                    </>
                  ) : (
                    <>
                      <div className="k">已有資料</div>
                      <div className="v">{plan.coverageDays}<i>天{plan.coverageMinDays != null && plan.coverageDays < plan.coverageMinDays ? `／需 ${plan.coverageMinDays}` : ""}</i></div>
                    </>
                  )}
                </div>
                <div className="pstat">
                  <div className="k">5H 見頂</div>
                  <div className={`v${plan.sessionSatDays >= 4 ? " red" : ""}`}>{plan.sessionSatDays}<i>天</i></div>
                </div>
                <div className="pstat">
                  <div className="k">每日最高 P90</div>
                  <div className="v">{plan.sessionPeakP90 != null ? Math.round(plan.sessionPeakP90) : "—"}<i>%</i></div>
                </div>
                <div className="pstat">
                  <div className="k">週峰值</div>
                  <div className={`v${plan.weeklyPeak >= 90 ? " red" : ""}`}>{Math.round(plan.weeklyPeak)}<i>%</i></div>
                </div>
                {/* v1.9.4（D99 A7）：Fable 週峰值也進了升級條件（Fable 週週見頂的人以前會被判「剛好」） */}
                <div className="pstat">
                  <div className="k">FABLE 峰值</div>
                  <div className={`v${plan.fablePeak >= 90 ? " red" : ""}`}>{Math.round(plan.fablePeak)}<i>%</i></div>
                </div>
              </div>
            </div>
            <div className="plan-side">
              <div className="ml-title">每日 5H 峰值</div>
              <PeakBars peaks={plan.dailyPeaks} statsFrom={plan.limitedByPlan ? plan.statsFrom : null} />
            </div>
          </div>
          <p className="foot-note zh" style={{ marginTop: 8 }}>
            規則：5h 見頂 ≥4 天、或週峰值／Fable 峰值 ≥90% → 建議升一級（Max 20x 已是最高一級，改建議多訂一個帳號）；
            降一級後池會變小，把同樣的用量換算過去，最高仍低於 {plan.downgradeSafetyPercent ?? 80}% → 可考慮降級。
            近 14 天少於 {plan.minStatsDays ?? 5} 天有記錄、或換方案（或 App 開始記錄方案）之後還不滿這麼多天，先不給建議（之前的記錄是另一個額度池、或方案未知，不混算）。
            只看額度百分比，不讀對話內容。
          </p>
        </article>
      )}

      {/* 本機活動 */}
      <article className="card">
        <div className="card-head">
          <span className="card-title zh">這個 5h 窗，本機在燒什麼</span>
          <span className="card-tag">WINDOW ACTIVITY</span>
        </div>
        <p className="foot-note zh" style={{ margin: "3px 0 10px" }}>
          {data.windowStart ? `窗口自 ${fmtAbsolute(data.windowStart)} 起` : "窗口起點未知"}
          ｜本機記錄不區分帳號，這張表是本機的全量；「Code vs 網頁」的對帳在「趨勢」頁的來源歸因卡
          ｜API 計價＝輸入、快取、輸出照官方牌價換算的參考值（不是訂閱扣款），滑鼠移到列上看拆解
        </p>
        {data.byEntrypoint.length === 0 ? (
          <p className="foot-note zh">這個窗口內本機沒有觀測到任何 API 呼叫。</p>
        ) : (
          <table className="tbl">
            <thead>
              <tr>
                <th>來源</th>
                <th className="r">呼叫</th>
                <th className="r">API 計價</th>
                <th className="r">佔比</th>
              </tr>
            </thead>
            <tbody>
              {data.byEntrypoint.map((e) => (
                <tr key={e.entrypoint} title={mixBreakdown(e)}>
                  <td className="zh">{ENTRYPOINT_LABEL[e.entrypoint] ?? e.entrypoint}</td>
                  <td className="r">{e.calls.toLocaleString()}</td>
                  <td className="r">
                    {fmtMixUsd(e)}
                    <PriceMark m={e} />
                  </td>
                  <td className="r mut">
                    {totalUsd > 0 && e.usd != null ? `${Math.round((e.usd / totalUsd) * 100)}%` : "—"}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        {entryNote && <p className="foot-note zh" style={{ marginTop: 6 }}>{entryNote}。</p>}
      </article>

      {/* 模型與專案分佈（2026-10-03 主人：跟歷史頁一樣並排；舊 payload 沒有 topModels 就只剩專案欄） */}
      {(data.topProjects.length > 0 || (data.topModels?.length ?? 0) > 0) && (
        <article className="card">
          <div className="card-head">
            <span className="card-title zh">這個 5h 窗，用了哪些模型、哪些專案</span>
            <span className="card-tag">USAGE MIX · 本窗 · 本機記錄</span>
          </div>
          <UsageMix byModel={data.topModels ?? []} byProject={data.topProjects} />
          <p className="foot-note zh" style={{ marginTop: 8 }}>
            同上表，本機全量、不分帳號，不含網頁版；各取前 6 名。
          </p>
        </article>
      )}

      <div className="flex justify-end">
        <button className="h5btn" onClick={() => void load()}>
          <RefreshCw size={11} />
          <span className="zh">重新整理</span>
        </button>
      </div>
    </div>
  );
}
