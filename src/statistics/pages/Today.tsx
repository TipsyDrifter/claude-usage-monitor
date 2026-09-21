import { useEffect, useRef, useState } from "react";
import { RefreshCw } from "lucide-react";
import { useStore } from "@/store/usageStore";
import { cmd } from "@/lib/tauri";
import { reduceMotion, useFlashOnChange, useTweenNumber } from "@/lib/motion";
import { PageSkeleton, hideTip, showTip } from "../Motion";
import { fmtAbsolute, fmtMoment, normalizeTimeMode, type TimeMode } from "@/lib/timefmt";
import { toneOf, thresholdsOf, type Thresholds, type Tone } from "@/lib/tone";
import { seatLabel } from "@/lib/seatLabel";
import { formatTimeAgo } from "@/lib/format";
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
}

interface Outlook {
  session: LimitOutlook;
  weekly: LimitOutlook;
  fable: LimitOutlook;
  windowStart: string | null;
  byEntrypoint: {
    entrypoint: string;
    calls: number;
    outputTokens: number;
    cacheCreation: number;
    cacheRead: number;
  }[];
  topProjects: { project: string; calls: number; outputTokens: number }[];
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
    suggestion: "calibrating" | "upgrade" | "downgrade" | "keep";
    reason: string;
  };
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
function verdictOf(o: LimitOutlook, mode: TimeMode, windowHours: number, th: Thresholds): Verdict {
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
  const shortMin = o.shortWindowMinutes ?? Math.round((windowHours / 12) * 60);
  const shortSpan = shortMin >= 60 ? `${Math.round(shortMin / 60)} 小時` : `${shortMin} 分鐘`;

  if (o.percent >= 100) {
    return { tone: "crit", headline: "已撞牆", outcome: reset, outcomeHot: false, sub: "", ghost: null };
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
  // 副句預警（forecast 線）：只有近期超速——撐得到，但近 N 的速度若持續會見底。
  if (shortHot) {
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

function fmtTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(0)}k`;
  return String(n);
}

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

  if (percent == null || pct == null || tone === "unknown") return null;
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
}: {
  label: string;
  en: string;
  o: LimitOutlook;
  windowHours: number;
  mode: TimeMode;
  th: Thresholds;
}) {
  const v = verdictOf(o, mode, windowHours, th);

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
}: {
  samples: { at: string; percent: number }[];
  anchors: string[];
  windowStart: string;
  resetsAt: string | null;
  burnPerHour: number | null;
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
      `窗內 +${Math.floor(mins / 60)}h${String(mins % 60).padStart(2, "0")} ＝ ${Math.round(best.percent)}%`,
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
        <circle key={i} cx={x(ms(s.at))} cy={y(s.percent)} r={2} fill="#2E5FB7" />
      ))}
      {anchors.map((a, i) => (
        <g key={`a${i}`}>
          <line x1={x(ms(a))} y1={T - 4} x2={x(ms(a))} y2={T + 6} stroke="#C13A30" strokeWidth={2} />
          {/* 2px 的刻太細抓不到——補一塊透明的命中區，hover 才叫得出時刻 */}
          <rect x={x(ms(a)) - 5} y={T - 7} width={10} height={17} fill="transparent" data-anchor={a} />
        </g>
      ))}
      <line x1={x(now)} y1={T} x2={x(now)} y2={y(0)} stroke="#969EAC" strokeDasharray="2 3" />
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
const PLAN_META: Record<Outlook["plan"]["suggestion"], { label: string; tone: Tone }> = {
  calibrating: { label: "樣本累積中", tone: "unknown" },
  upgrade: { label: "考慮升級", tone: "warn" },
  downgrade: { label: "可考慮降級", tone: "ok" },
  keep: { label: "現在的方案剛好", tone: "ok" },
};

function PeakBars({ peaks }: { peaks: { day: string; peak: number }[] }) {
  // 近 14 天每日 5h 峰值：長條高＝峰值 %（1:1 反推），≥100 紅
  const days: { day: string; peak: number | null }[] = [];
  const map = new Map(peaks.map((p) => [p.day, p.peak]));
  for (let i = 13; i >= 0; i--) {
    const day = new Date(Date.now() - i * 86400_000).toISOString().slice(0, 10);
    days.push({ day, peak: map.get(day) ?? null });
  }
  const W = 224, H = 46, pitch = W / 14, bw = pitch * 0.62;
  return (
    <svg width={W} height={H} viewBox={`0 0 ${W} ${H}`} aria-hidden style={{ display: "block" }}>
      <line x1={0} y1={H - 6} x2={W} y2={H - 6} stroke="#D3D9E2" />
      {days.map((d, i) => {
        const x = i * pitch + (pitch - bw) / 2;
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

  const totalOut = data.byEntrypoint.reduce((s, e) => s + e.outputTokens, 0);

  return (
    <div className="h5 space-y-4">
      {usage.notice && <div className="warn-band zh">⚠ {usage.notice}</div>}
      {viewSeat && (
        <div className="warn-band zh">
          <span>
            看的是「{seatLabel(viewSeat, settings.accounts.aliases)}」的最後已知值
            {seatSnapshot?.scrapedAt ? `（${formatTimeAgo(seatSnapshot.scrapedAt)}）` : ""}——
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
            <span className="chip">
              <span className="pulse" />
              <span>{usage.data?.currentSession?.note ?? "—"}</span>
            </span>
          </span>
        </div>
        <div style={{ marginTop: 4 }}>
          <LimitRow label="5h 窗口" en="SESSION" o={data.session} windowHours={5} mode={mode} th={th} />
          <LimitRow label="週 · 全模型" en="WEEKLY ALL" o={data.weekly} windowHours={168} mode={mode} th={th} />
          <LimitRow label="週 · Fable" en="WEEKLY FABLE" o={data.fable} windowHours={168} mode={mode} th={th} />
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
              burnPerHour={data.session.burnPerHour}
            />
            <div className="legend-row">
              <span className="lg"><span className="sw bar" />觀測記錄（探針／CLI 快取）</span>
              <span className="lg"><span className="sw stair-ghost" />照目前速度外推到重置（推測）</span>
              <span className="lg"><span className="sw wall" />被限流記錄（429，本機）</span>
            </div>
          </div>
        ) : (
          <p className="foot-note zh" style={{ marginTop: 8 }}>
            {data.windowStart ? "這個窗口還沒有確切來源的觀測記錄。" : "窗口起點未知——等下一筆 5h 記錄後才能畫圖。"}
          </p>
        )}
      </article>

      {/* 方案建議（D56/M5） */}
      <article className="card">
        <div className="card-head">
          <span className="card-title zh">方案建議</span>
          <span className="card-tag">PLAN FIT · 近 14 天 · 純看 %</span>
          {usage.data?.planName && (
            <div className="right">
              <span className="chip"><b>{usage.data.planName}</b></span>
            </div>
          )}
        </div>
        <div className="plan-body">
          <div style={{ minWidth: 0, flex: 1 }}>
            <div className="pace-verdict" style={{ marginTop: 8 }}>
              <span className={`dot ${PLAN_META[data.plan.suggestion].tone}`} />
              <span className={`plan-headline ${PLAN_META[data.plan.suggestion].tone}`}>
                {PLAN_META[data.plan.suggestion].label}
              </span>
            </div>
            <div className="pace-minis zh">{data.plan.reason}</div>
            <div className="pstats" style={{ gap: 22 }}>
              <div className="pstat">
                <div className="k">已有資料</div>
                <div className="v">{data.plan.coverageDays}<i>天{data.plan.coverageMinDays != null && data.plan.coverageDays < data.plan.coverageMinDays ? `／需 ${data.plan.coverageMinDays}` : ""}</i></div>
              </div>
              <div className="pstat">
                <div className="k">5H 見頂</div>
                <div className={`v${data.plan.sessionSatDays >= 4 ? " red" : ""}`}>{data.plan.sessionSatDays}<i>天</i></div>
              </div>
              <div className="pstat">
                <div className="k">每日最高 P90</div>
                <div className="v">{data.plan.sessionPeakP90 != null ? Math.round(data.plan.sessionPeakP90) : "—"}<i>%</i></div>
              </div>
              <div className="pstat">
                <div className="k">週峰值</div>
                <div className="v">{Math.round(data.plan.weeklyPeak)}<i>%</i></div>
              </div>
            </div>
          </div>
          <div className="plan-side">
            <div className="ml-title">每日 5H 峰值</div>
            <PeakBars peaks={data.plan.dailyPeaks} />
          </div>
        </div>
        <p className="foot-note zh" style={{ marginTop: 8 }}>
          規則：5h 見頂 ≥4 天或週峰值 ≥90% → 考慮升級；每日最高 p90 &lt;40% 且週峰值 &lt;40% → 可考慮降級；觀測不足 7 天不給建議。只看額度百分比，不讀對話內容。
        </p>
      </article>

      {/* 本機活動 */}
      <article className="card">
        <div className="card-head">
          <span className="card-title zh">這個 5h 窗，本機在燒什麼</span>
          <span className="card-tag">WINDOW ACTIVITY</span>
        </div>
        <p className="foot-note zh" style={{ margin: "3px 0 10px" }}>
          {data.windowStart ? `窗口自 ${fmtAbsolute(data.windowStart)} 起` : "窗口起點未知"}
          ｜本機記錄不區分帳號（O10），這張表是本機的全量；「Code vs 網頁」的對帳在「趨勢」頁的來源歸因卡（M5）
        </p>
        {data.byEntrypoint.length === 0 ? (
          <p className="foot-note zh">這個窗口內本機沒有觀測到任何 API 呼叫。</p>
        ) : (
          <table className="tbl">
            <thead>
              <tr>
                <th>來源</th>
                <th className="r">呼叫</th>
                <th className="r">輸出 TOKENS</th>
                <th className="r">佔比</th>
              </tr>
            </thead>
            <tbody>
              {data.byEntrypoint.map((e) => (
                <tr key={e.entrypoint}>
                  <td className="zh">{ENTRYPOINT_LABEL[e.entrypoint] ?? e.entrypoint}</td>
                  <td className="r">{e.calls.toLocaleString()}</td>
                  <td className="r">{fmtTokens(e.outputTokens)}</td>
                  <td className="r mut">
                    {totalOut > 0 ? `${Math.round((e.outputTokens / totalOut) * 100)}%` : "—"}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </article>

      {/* 專案分佈 */}
      {data.topProjects.length > 0 && (
        <article className="card">
          <div className="card-head">
            <span className="card-title zh">窗內最燒的專案</span>
            <span className="card-tag">TOP PROJECTS</span>
          </div>
          <table className="tbl" style={{ marginTop: 8 }}>
            <tbody>
              {data.topProjects.map((p) => (
                <tr key={p.project}>
                  <td style={{ fontSize: 10.5 }}>
                    {p.project.replace(/^C--/, "").split("-").slice(-2).join("/")}
                  </td>
                  <td className="r mut">
                    {fmtTokens(p.outputTokens)} · {p.calls} 次
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
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
