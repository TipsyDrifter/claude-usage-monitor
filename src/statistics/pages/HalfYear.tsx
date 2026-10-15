import { Fragment, useEffect, useMemo, useState } from "react";
import { FileDown, FolderOpen, ImageDown } from "lucide-react";
import { useStore } from "@/store/usageStore";
import { cmd } from "@/lib/tauri";
import { evidenceCardSvg, svgDataUri, svgToPngBase64 } from "@/lib/exportCard";
import { seatLabel, ALL_SEATS, ALL_SEATS_LABEL } from "@/lib/seatLabel";
import { planShort, planShortAll } from "@/lib/planLabel";
import { pageCache } from "@/lib/pageCache";
import { PageSkeleton, hideTip, showTip, toast } from "../Motion";
import { How } from "../How";
import { EmptyLedger } from "../PageGuards";
import { useSeatPick } from "../useSeatPick";
import { dateOfLocalDay, localDayBack, localDayOf, localDaysSpanning, localSpan, mdhm } from "../dates";
import "../h5.css";

// =============================================================================
// 這半年發生了什麼 (M4) — 研究層，H5 靛藍帳房上身。
// 視覺基準 prototypes/dash-H5-dot-cool.html；所有幾何由資料即時運算（D42：
// 畫出來的東西必須能反推回宣稱的數值——沒有的數據就不畫，不裝）。
// M5（D64）匯率轉正：月匯率＝修剪 Δ-加權比值（Σusd÷Σδ，砍每段匯率下尾 25%／
// 上尾 5%），膠囊高＝bootstrap 95% CI、網點密度＝樣本數 n；固定籃指數（凍結
// 基期月模型組合）以紫色虛線並列；縮水宣告走逐項檢查（同方案／樣本／5h 檢定／雙視窗／
// 固定籃／看得到的比例），任一沒過只呈現數據並標出卡在哪。CI 帶畫完後由 SVG 反推自驗。
// 撞牆雙訊號（D62）：紅點＝本機觀測到的 429 事件（anchors）；整條紅＝該日
// 任一額度真值見頂 100%（truth_samples，含未重置）——網頁端 429 不落本機
// JSONL，見頂紅條補這個盲區。
// =============================================================================

const C = {
  accent: "#4E7ECF",
  accentDeep: "#2E5FB7",
  accentInk: "#1F4D9F",
  accentSoft: "#E3EBF8",
  violet: "#7E6AC8",
  green: "#4C8C74",
  red: "#C13A30",
  ink: "#1A1D23",
  ink2: "#5E6470",
  ink3: "#969EAC",
  cellEmpty: "#E8ECF2",
  line: "#E1E5EB",
};

/** v1.9.4（D99，線 B §6）：每一列／每一段都帶它屬於哪個方案。plan＝可比性的鍵（max_20x、max_5x、pro、
 *  team…；方案未知＝"unknown"），只拿來比較，顯示一律用 planLabel。 */
interface PlanTagged {
  plan?: string;
  planKey?: string;
  planLabel?: string;
  planKnown?: boolean;
}
/** 月匯率一列＝（月, 方案）一組：同一個月換過方案就有兩列，列序是時間順序——不能拿 month 當唯一鍵。 */
interface FxMonth extends PlanTagged {
  month: string;
  samples: number;
  deltaSum: number;
  usdSum: number;
  rateRaw: number | null;
  rate: number | null;
  ci: [number, number] | null;
  qualified: boolean;
}
interface Gate {
  key: string;
  label: string;
  pass: boolean;
  detail: string;
  /** 圖卡用的短句（舊後端沒有）。 */
  short?: string;
}
interface Verdict {
  declare: boolean;
  gates: Gate[];
  /** v1.9.4（D99 Q2）：兩個月是不是同一個方案、能不能比。舊後端沒有＝當作能比。 */
  comparable?: boolean;
  /** 不能比的原因：plan_changed｜plan_unknown_before｜plan_unknown｜plan_unknown_history｜all_seats。 */
  reason?: string;
  /** 後端寫好的中文句（不能比時），直接顯示。 */
  sentence?: string;
  /** prev 與 cur 相隔幾個月（隔著別的方案的月份也能比）——1 才是「較上月」。 */
  monthsApart?: number;
  plan?: string;
  planLabel?: string;
  curMonth?: string;
  prevMonth?: string;
  rateCur?: number;
  ratePrev?: number;
  momPct?: number;
  walletPct?: number;
  p5h?: number | null;
  p7d?: number | null;
  pBasket?: number | null;
}
interface ShrinkMonth extends PlanTagged {
  month: string;
  index: number | null;
  coverage: number | null;
  ci: [number, number] | null;
  isBase?: boolean;
  /** 前一列是別的方案——固定籃的線要在這裡斷開。 */
  planBreak?: boolean;
}
/** 線 B §6.1：座位的方案（共用形狀）。 */
interface SeatPlan {
  known: boolean;
  label: string | null;
  key: string | null;
  since: string | null;
  observedAt: string | null;
}
interface PlanSegment {
  plan: string;
  planKey: string;
  planLabel: string;
  planKnown: boolean;
  since: string;
  until: string | null;
  intervals: number;
}
interface Dashboard {
  empty?: boolean;
  seats: { id: string; accountUuid: string | null; orgUuid: string | null; samples: number; isDesktop: boolean; email?: string | null; orgName?: string | null; plan?: SeatPlan | null }[];
  selected: { id: string; accountUuid: string | null; orgUuid: string | null; entrypoint: string; plan?: SeatPlan | null };
  fx5: FxMonth[];
  fx7: FxMonth[];
  /** v1.9.4（D99 A10）："per_seat"＝全部帳號視角：fx5／fx7 是空的、匯率類欄位都清成 null。 */
  fxMode?: "plan_month" | "per_seat";
  /** 全部帳號視角才有：每個帳號在自己目前方案下的期間匯率。 */
  perSeat?: { seat: string; email: string | null; orgName: string | null; plan: SeatPlan | null; scopePlan: string; scopePlanLabel: string; intervals: number; deltaSum: number; rateTrim: number | null }[];
  /** v1.9.4（D99 A13）：方案時間軸。crossesPlan＝期間內不只一種方案（含方案未知）。 */
  plan?: {
    current: SeatPlan | null;
    segments: PlanSegment[];
    changes: { at: string; fromLabel: string; toLabel: string; fromKey: string; toKey: string }[];
    crossesPlan: boolean;
    crossesKnownPlans: boolean;
    mixedPlans: boolean;
    firstObservedAt: string | null;
  };
  /** v1.9.4（D99 B2）：官方價目表沒有牌價的模型——用量沒算進匯率。 */
  unpriced?: { models: { model: string; label?: string; calls: number; tokens: number; firstSeen: string | null; lastSeen: string | null }[]; intervals: number; deltaShare: number };
  verdict: Verdict;
  shrink: {
    baseMonth: string | null;
    basket: { family: string; share: number }[] | null;
    monthly: ShrinkMonth[];
    /** v1.9.4（D99 A4）：每個方案一個基期（按基期月份由舊到新）。 */
    bases?: (PlanTagged & { baseMonth: string; basket: { family: string; share: number }[] })[];
  };
  /** v1.6（D80）：回歸法分模型倍率——模型 id 各一列，家族只當分組；families 是純區間法的交叉驗證。 */
  byModel: {
    /** v1.9.4（D99 A5）：回歸只用目前方案的區間；allSeats＝全部帳號視角（不算）。 */
    scope?: { plan: string | null; planLabel: string | null; planKnown: boolean; intervals: number; excludedIntervals: number; excludedOtherPlan?: number; excludedUnknownPlan?: number; excludedNoFamily?: number; allSeats: boolean };
    method: string;
    n: number;
    r2: number | null;
    rows: {
      model: string;
      label: string;
      family: string | null;
      rate: number | null;
      ci: [number, number | null] | null;
      nSeg: number;
      zeroShare: number;
      status: "ok" | "calibrating" | "collinear";
      collinearWith: string | null;
      multiplier: number | null;
    }[];
    families: {
      family: string;
      pureRate: number | null;
      pureN: number | null;
      regRate: number | null;
      regCi: [number, number | null] | null;
      regStatus: string;
      zeroShare?: number;
      nSeg: number;
      multiplier: number | null;
      agree: boolean | null;
    }[];
  };
  heat: { cells: number[]; bands: { label: string; n: number; rate: number | null }[] };
  /** v1.4.1：這個座位第一筆真值（probe／cli-cache）的時刻；期間再長也只能從這裡起算。 */
  dataSince?: string | null;
  reconcile: {
    /** 整個期間（可能混了不同方案）——mixedPlans 時改看 planScoped。全部帳號視角為 null。 */
    rateRaw: number | null;
    rateTrim: number | null;
    unexplainedShare: number | null;
    zeroEventDeltaShare: number | null;
    intervals: number;
    mixedPlans?: boolean;
    planScoped?: { rateRaw: number | null; rateTrim: number | null; intervals: number; deltaSum?: number; scope?: { planLabel: string | null } } | null;
  };
  method: {
    /** 縮水宣告句的前提（後端唯一一份）。舊後端沒有。 */
    declarePremise?: string;
    trim: [number, number];
    bootstrapReps: number;
    permReps: number;
    alpha: number;
    minN: number;
    minDeltaSum: number;
    pureShare: number;
    familyMinN: number;
    basketMinCoverage: number;
    regression?: { weight: string; intercept: boolean; minSegments: number; zeroShareMax: number; maxCiRatio: number; collinearR: number };
  };
  workRate: { turnsPerPercent: number | null; outputTokensPerPercent: number | null };
  daily: { day: string; usd: number }[];
  /** v1.9.4（D99 E4）：class＝這次 429 撞的是哪條額度（five_hour｜seven_day｜fable｜other）。 */
  walls: { at: string; kind: string | null; class?: string }[];
  saturatedDays: string[];
  /** v1.9.5（D100 D4）：觀測缺口＝App 沒在跑、而且 Desktop 檔也沒樣本（後端讀取時算）；
   *  legacy＝v1.9.5 以前留下的舊記錄（當時記的是 Desktop 檔缺口）。舊後端沒有 source／legacy。 */
  gaps: { from: string; to: string; source?: string; legacy?: boolean }[];
  /** D97：按 API 計價（usd）分；也帶四種 tokens。v1.9.4（B2）：整組沒牌價時 usd 是 null。 */
  attribution: { entrypoint: string; usd: number | null; outputTokens: number }[];
  /** v1.8.7（D86 蟲 3）：期間消耗／撞牆／來源歸因只算這個帳號登入中的時段；from＝最早一段的起點。
   *  v1.8.11（D89）：all＝全量視角（不分帳號、不套登入時段）。 */
  tenure?: { all?: boolean; spans: number; cli: number; desktop: number; from: string | null };
}

const lerp = (a: number, b: number, t: number) => a + (b - a) * t;

const FAMILY_LABEL: Record<string, string> = {
  fable: "Fable",
  opus: "Opus",
  "sonnet-5": "Sonnet 5",
  sonnet: "Sonnet 4.x",
  haiku: "Haiku",
};
/** v1.6：倍率表的家族分組（後端 stats::GROUPS 同序；主人拍板四家族，Sonnet 5 與 4.x 同一家）。 */
const FAMILY_ORDER = ["fable", "opus", "sonnet", "haiku"];
const GROUP_LABEL: Record<string, string> = { fable: "Fable", opus: "Opus", sonnet: "Sonnet", haiku: "Haiku" };

/* ---------------- 匯率膠囊圖：膠囊高＝95% CI、網點密度＝n；固定籃紫色虛線 --------
   規格工程紅線：畫出來的長度必須可反推——每個 CI 膠囊帶 data-lo/data-hi，
   verifyFxGeometry() 從產出的 SVG 字串把 y/height 反推回數值自驗。 */
/** 一列的方案鍵；舊後端沒有方案欄位時整串當同一段。 */
const planOf = (m: PlanTagged) => m.plan ?? "";
const isUnknownPlan = (m: PlanTagged) => m.planKnown === false;

/** v1.9.4（D99 A3）：相鄰列方案相同＝同一段。回傳每段的 [起, 迄] 索引（含兩端）。 */
function planRuns<T extends PlanTagged>(rows: T[]): { from: number; to: number; row: T }[] {
  const runs: { from: number; to: number; row: T }[] = [];
  rows.forEach((r, i) => {
    const last = runs[runs.length - 1];
    if (last && planOf(last.row) === planOf(r)) last.to = i;
    else runs.push({ from: i, to: i, row: r });
  });
  return runs;
}

/** 固定籃那一列跟月匯率那一列對得上：同月＋同方案（同一個月可以有兩個方案，不能只看月）。 */
const fxKey = (m: { month: string } & PlanTagged) => `${m.month}|${planOf(m)}`;

function fxChartSvg(months: FxMonth[], shrink: ShrinkMonth[]): { svg: string; lo: number; hi: number; T: number; ph: number } {
  // v1.9.4：頂上多一排方案段標（T 從 16 → 30），其餘幾何不變。
  const W = 640, H = 214, L = 46, R = 14, T = 30, B = 32;
  const pw = W - L - R, ph = H - T - B;
  const withRate = months.filter((m) => m.rate != null) as (FxMonth & { rate: number })[];
  if (withRate.length === 0) return { svg: "", lo: 0, hi: 1, T, ph };

  const shrinkMap = new Map(shrink.map((s) => [fxKey(s), s]));
  const vals: number[] = [];
  for (const m of withRate) {
    vals.push(m.rate);
    if (m.ci) vals.push(m.ci[0], m.ci[1]);
    const s = shrinkMap.get(fxKey(m));
    if (s?.index != null) vals.push(s.index);
  }
  const dMin = Math.min(...vals), dMax = Math.max(...vals);
  const pad = Math.max((dMax - dMin) * 0.15, dMax * 0.06);
  const lo = Math.max(0, dMin - pad), hi = dMax + pad;
  const y = (v: number) => T + ph * (1 - (v - lo) / (hi - lo));
  const pitch = pw / months.length;
  const cx = (i: number) => L + pitch * (i + 0.5);
  const capW = Math.min(56, pitch * 0.52);

  const sLo = Math.min(...months.map((m) => m.samples));
  const sHi = Math.max(...months.map((m) => m.samples), sLo + 1);
  const dotSpacing = (s: number) => lerp(9.2, 4.6, (s - sLo) / (sHi - sLo));

  const step = hi - lo > 3 ? 1 : hi - lo > 0.9 ? 0.5 : 0.2;
  let grid = "";
  for (let v = Math.ceil(lo / step) * step; v < hi - 0.001; v += step) {
    grid += `<line x1="${L}" y1="${y(v)}" x2="${W - R}" y2="${y(v)}" stroke="${C.line}" stroke-width="1" stroke-dasharray="2 5"/>
      <text x="${L - 8}" y="${y(v) + 3.5}" text-anchor="end" font-size="9.5" fill="${C.ink3}">${v.toFixed(1)}</text>`;
  }
  grid += `<text x="${L - 8}" y="${T - 6}" text-anchor="end" font-size="8" fill="${C.ink3}" letter-spacing=".08em">USD/1%</text>`;

  // v1.9.4（D99 A3）：方案段——頂上一排段標＋段與段之間的虛線分隔；方案未知的段整段放淡。
  const runs = planRuns(months);
  const hasPlans = months.some((m) => m.plan != null);
  let bands = "";
  if (hasPlans) {
    runs.forEach((r, k) => {
      const x0 = L + pitch * r.from + 3, x1 = L + pitch * (r.to + 1) - 3;
      const unk = isUnknownPlan(r.row);
      bands += `<g class="plan-band" data-plan="${planOf(r.row)}">
        <path d="M${x0},${T - 7} V${T - 12} H${x1} V${T - 7}" fill="none" stroke="${unk ? C.ink3 : C.accent}" stroke-width="1.2" ${unk ? `stroke-dasharray="2 3"` : ""} stroke-linejoin="round"/>
        <text x="${(x0 + x1) / 2}" y="${T - 17}" text-anchor="middle" font-size="9" font-weight="${unk ? 500 : 700}" fill="${unk ? C.ink3 : C.accentInk}">${planShort(r.row.planLabel)}</text>
      </g>`;
      if (k > 0) {
        const xb = L + pitch * r.from;
        bands += `<line class="plan-cut" x1="${xb}" y1="${T - 8}" x2="${xb}" y2="${T + ph}" stroke="${C.ink3}" stroke-width="1" stroke-dasharray="3 3" opacity="0.7"/>`;
      }
    });
  }

  let defs = "", caps = "", labels = "";
  months.forEach((m, i) => {
    const isNow = i === months.length - 1;
    const unk = isUnknownPlan(m);
    const x = cx(i) - capW / 2;
    if (m.rate == null) {
      // 沒有真值配對的月份：虛線空膠囊，不裝數字
      caps += `<rect x="${x}" y="${T}" width="${capW}" height="${ph}" rx="10" fill="none" stroke="${C.line}" stroke-width="1.2" stroke-dasharray="3 4"/>`;
      labels += `<text x="${cx(i)}" y="${H - 20}" text-anchor="middle" font-size="10.5" fill="${C.ink3}">${m.month.slice(5)} 月</text>
        <text x="${cx(i)}" y="${H - 7}" text-anchor="middle" font-size="8.5" fill="${C.ink3}">無真值</text>`;
      return;
    }
    if (m.ci) {
      // CI 膠囊：頂＝上界、底＝下界（幾何＝資料）
      const yTop = y(m.ci[1]), yBot = y(m.ci[0]);
      const sp = dotSpacing(m.samples);
      const pid = `hcap${i}`;
      defs += `<pattern id="${pid}" patternUnits="userSpaceOnUse" width="${sp}" height="${sp}">
        <rect x="0" y="0" width="2.6" height="2.6" rx="0.9" fill="${unk ? C.ink3 : C.accent}" opacity="${unk ? 0.35 : 0.5}"/></pattern>`;
      // E24（D77）：CI 與 n 是圖上讀不出、又直接關係「這個數可不可信」的東西
      // ——改由跟游標的小標講（原生 <title> 拿掉，免得兩個 tooltip 同時出現）。
      // 注意 data-* 一律加在 height 之後：verifyFxGeometry() 的正則吃前面那段。
      caps += `<g>
      <rect class="ci-cap" data-lo="${m.ci[0]}" data-hi="${m.ci[1]}" x="${x}" y="${yTop}" width="${capW}" height="${yBot - yTop}" data-month="${m.month}" data-plan="${planShort(m.planLabel)}" data-rate="${m.rate.toFixed(2)}" data-ci="${((m.ci[1] - m.ci[0]) / 2).toFixed(2)}" data-n="${m.samples}" data-delta="${m.deltaSum.toFixed(0)}" rx="${Math.min(10, (yBot - yTop) / 2)}" fill="${unk ? C.cellEmpty : C.accentSoft}" opacity="0.7"/>
      <rect x="${x}" y="${yTop}" width="${capW}" height="${yBot - yTop}" rx="${Math.min(10, (yBot - yTop) / 2)}" fill="url(#${pid})" pointer-events="none"/>
      ${isNow ? `<rect x="${x}" y="${yTop}" width="${capW}" height="${yBot - yTop}" rx="${Math.min(10, (yBot - yTop) / 2)}" fill="none" stroke="${unk ? C.ink3 : C.accent}" stroke-width="1.6" pointer-events="none"/>` : ""}
    </g>`;
    } else {
      // 校準中：只有點，沒有帶——沒算出 CI 就不畫帶
      caps += `<g><title>${m.month}${m.planLabel ? `（${planShort(m.planLabel)}）` : ""} · $${m.rate.toFixed(2)}/1%（校準中）· n=${m.samples} · Δ%合計 ${m.deltaSum.toFixed(0)}</title>
      <line x1="${cx(i)}" y1="${T}" x2="${cx(i)}" y2="${T + ph}" stroke="${C.line}" stroke-width="1" stroke-dasharray="2 4"/></g>`;
    }
    labels += `<text x="${cx(i)}" y="${(m.ci ? y(m.ci[1]) : y(m.rate)) - 9}" text-anchor="middle" font-size="${isNow ? 13 : 10}" font-weight="${isNow ? 700 : 500}" fill="${unk ? C.ink3 : isNow ? C.accentInk : C.ink2}">${m.rate.toFixed(2)}</text>
      <text x="${cx(i)}" y="${H - 20}" text-anchor="middle" font-size="10.5" font-weight="${isNow ? 700 : 500}" fill="${isNow ? C.ink : C.ink2}">${m.month.slice(5)}${isNow ? " 月 · 今" : " 月"}</text>
      <text x="${cx(i)}" y="${H - 7}" text-anchor="middle" font-size="8.5" fill="${C.ink3}">n=${m.samples}${m.qualified || months.length > 8 ? "" : " · 校準中"}</text>`; /* 審查 23r N6：欄數多時鄰欄標籤會疊，只留 n（校準中的那欄本來就沒有膠囊、只有虛線） */
  });

  // 折線在方案切點斷開：每段各畫一條（跨方案連起來＝把兩個池的匯率當成同一條趨勢）。
  let line = "", pts = "";
  for (const r of runs) {
    const seg = months.slice(r.from, r.to + 1);
    const unk = isUnknownPlan(r.row);
    const p = seg
      .map((m, j) => (m.rate != null ? `${cx(r.from + j)},${y(m.rate)}` : null))
      .filter(Boolean)
      .join(" ");
    if (p.includes(" "))
      line += `<polyline class="fx-line" data-plan="${planOf(r.row)}" points="${p}" fill="none" stroke="${unk ? C.ink3 : C.accentDeep}" stroke-width="${unk ? 1.5 : 2}" ${unk ? `stroke-dasharray="4 3"` : ""} stroke-linejoin="round" stroke-linecap="round"/>`;
    seg.forEach((m, j) => {
      if (m.rate != null)
        pts += `<circle cx="${cx(r.from + j)}" cy="${y(m.rate)}" r="3.1" fill="#fff" stroke="${unk ? C.ink3 : C.accentDeep}" stroke-width="1.8"/>`;
    });
  }

  // 固定籃指數：紫色虛線＋空心方點（推估，不搶主角）；基期按方案各一個，線也在切點斷開。
  let basket = "";
  for (const r of runs) {
    const bPts = months
      .slice(r.from, r.to + 1)
      .map((m, j) => {
        const s = shrinkMap.get(fxKey(m));
        return s?.index != null ? `${cx(r.from + j)},${y(s.index)}` : null;
      })
      .filter(Boolean)
      .join(" ");
    if (bPts.includes(" ")) basket += `<polyline class="basket-line" points="${bPts}" fill="none" stroke="${C.violet}" stroke-width="1.5" stroke-dasharray="4 3"/>`;
  }
  months.forEach((m, i) => {
    const s = shrinkMap.get(fxKey(m));
    if (s?.index != null)
      basket += `<rect x="${cx(i) - 3}" y="${y(s.index) - 3}" width="6" height="6" fill="#fff" stroke="${C.violet}" stroke-width="1.4"><title>固定籃 ${m.month}${m.planLabel ? `（${planShort(m.planLabel)}）` : ""} · $${s.index.toFixed(2)}/1%${s.ci ? ` · CI $${s.ci[0].toFixed(2)}–$${s.ci[1].toFixed(2)}` : ""} · 籃覆蓋 ${Math.round((s.coverage ?? 0) * 100)}%</title></rect>`;
  });

  return {
    // v1.9.4：寬度跟著卡片縮（最寬 640）——視窗最窄 800 時卡片只有約 560，以前固定 640 會把最右邊的「本月」那一欄切掉。
    svg: `<svg width="100%" viewBox="0 0 ${W} ${H}" style="display:block;max-width:${W}px"><defs>${defs}</defs>${grid}${bands}${caps}${basket}${line}${pts}${labels}</svg>`,
    lo,
    hi,
    T,
    ph,
  };
}

/** 幾何自驗（規格書 §5 工程紅線／D64-10）：從產出的 SVG 字串反推每個 CI 膠囊的
    上下界，與宣稱值差 >0.5% 就 console.error——尺量得出來的錯，開發時就先量。 */
function verifyFxGeometry(out: { svg: string; lo: number; hi: number; T: number; ph: number }) {
  const inv = (Y: number) => out.lo + (out.hi - out.lo) * (1 - (Y - out.T) / out.ph);
  const re = /<rect class="ci-cap" data-lo="([\d.]+)" data-hi="([\d.]+)" x="[\d.]+" y="([\d.]+)" width="[\d.]+" height="([\d.]+)"/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(out.svg))) {
    const [lo, hi, y, h] = m.slice(1).map(Number);
    const hi2 = inv(y), lo2 = inv(y + h);
    if (Math.abs(hi2 - hi) > hi * 0.005 || Math.abs(lo2 - lo) > lo * 0.005) {
      console.error("[fx-geometry] CI 膠囊反推不符", { claimed: [lo, hi], drawn: [lo2, hi2] });
    }
  }
}

/* ---------------- 期間消耗長條（D60：>60 天自動按週聚合）
   D62 撞牆雙訊號：sat（見頂 100%）＝整條紅；wall（429 事件）＝條頂紅點 ---- */
function dailyChartSvg(
  days: { day: string; usd: number; wall: boolean; sat: boolean; gap: boolean }[],
): string {
  const W = 470, H = 172, L = 36, R = 10, T = 10, B = 24;
  const pw = W - L - R, ph = H - T - B;
  const vMax = Math.max(...days.map((d) => d.usd), 1);
  const yMax = Math.ceil(vMax / 100) * 100 + 50;
  const y = (v: number) => T + ph * (1 - v / yMax);
  const pitch = pw / days.length, barW = pitch * 0.66;
  const x = (i: number) => L + pitch * i + (pitch - barW) / 2;

  let grid = "";
  for (const v of [200, 400, 600, 800].filter((v) => v < yMax)) {
    grid += `<line x1="${L}" y1="${y(v)}" x2="${W - R}" y2="${y(v)}" stroke="${C.line}" stroke-width="1" stroke-dasharray="2 5"/>
      <text x="${L - 6}" y="${y(v) + 3.5}" text-anchor="end" font-size="9" fill="${C.ink3}">${v}</text>`;
  }

  let bars = "", marks = "", axis = "";
  days.forEach((d, i) => {
    const fill = d.sat ? C.red : C.accent;
    const tip = `${d.day.slice(5)} · $${d.usd.toFixed(0)}${d.sat ? " · 額度見頂 100%" : ""}${d.wall ? " · 429 事件" : ""}${d.gap ? " · 有採集缺口" : ""}`;
    if (d.usd < 0.5) {
      bars += `<rect x="${x(i)}" y="${y(0) - 2.5}" width="${barW}" height="2.5" rx="1.2" fill="${d.sat ? C.red : "#C2C8D2"}"><title>${tip}</title></rect>`;
    } else {
      const h = y(0) - y(d.usd);
      bars += `<rect x="${x(i)}" y="${y(d.usd)}" width="${barW}" height="${h}" rx="${Math.min(3, h / 2)}" fill="${fill}"><title>${tip}</title></rect>`;
    }
    if (d.wall) marks += `<circle cx="${x(i) + barW / 2}" cy="${y(d.usd) - 6.5}" r="2.6" fill="${C.red}"/>`;
    if (d.gap)
      marks += `<rect x="${x(i) + barW / 2 - 2.6}" y="${y(0) + 5.5}" width="5.2" height="5.2" rx="1.2" fill="none" stroke="${C.ink3}" stroke-width="1.2"/>`;
  });

  const lblSet = new Set([0, 7, 14, 21, days.length - 1]);
  days.forEach((d, i) => {
    const hot = d.wall || d.sat;
    if (!lblSet.has(i) && !hot) return;
    axis += `<text x="${x(i) + barW / 2}" y="${H - 6}" text-anchor="middle" font-size="8.6" font-weight="${hot ? 700 : 400}" fill="${hot ? C.red : C.ink3}">${d.day.slice(5)}</text>`;
  });
  axis += `<line x1="${L}" y1="${y(0)}" x2="${W - R}" y2="${y(0)}" stroke="#D3D9E2" stroke-width="1"/>`;

  return `<svg width="${W}" height="${H}" viewBox="0 0 ${W} ${H}">${grid}${bars}${axis}${marks}</svg>`;
}

/* ---------------- 像素網點歸因環 ---------------- */
function attrDonutSvg(segs: { label: string; percent: number; color: string }[]): string {
  const SZ = 176, cx = SZ / 2, cy = SZ / 2, rOut = 74, rIn = 47;
  const GAP = 7, FADE = 5, step = 6, cell = 4.3;
  let acc = 0;
  const angles = segs.map((s) => {
    const a0 = acc * 3.6, a1 = (acc + s.percent) * 3.6;
    acc += s.percent;
    return { a0, a1, color: s.color };
  });
  let cells = "";
  for (let gx = 0; gx <= SZ; gx += step) {
    for (let gy = 0; gy <= SZ; gy += step) {
      const dx = gx - cx, dy = gy - cy;
      const d = Math.hypot(dx, dy);
      if (d < rIn + 2 || d > rOut - 1) continue;
      let ang = (Math.atan2(dy, dx) * 180) / Math.PI + 90;
      if (ang < 0) ang += 360;
      const seg = angles.find((s) => ang >= s.a0 + GAP / 2 && ang <= s.a1 - GAP / 2);
      if (!seg) continue;
      const edge = Math.min(ang - (seg.a0 + GAP / 2), seg.a1 - GAP / 2 - ang);
      const k = edge < FADE ? 0.45 + 0.55 * (edge / FADE) : 1;
      const s = cell * k;
      cells += `<rect x="${gx - s / 2}" y="${gy - s / 2}" width="${s}" height="${s}" rx="${s * 0.28}" fill="${seg.color}" opacity="0.92"/>`;
    }
  }
  const lead = segs[0];
  return `<svg width="${SZ}" height="${SZ}" viewBox="0 0 ${SZ} ${SZ}">${cells}
    <text x="${cx}" y="${cy - 2}" text-anchor="middle" font-size="27" font-weight="700" fill="${C.ink}">${lead ? Math.round(lead.percent) : 0}%</text>
    <text x="${cx}" y="${cy + 15}" text-anchor="middle" font-size="8" letter-spacing="1.2" font-weight="600" fill="${C.ink3}">${lead ? lead.label.toUpperCase() : ""}</text></svg>`;
}

/* ================================================================ */

const ATTR_META: Record<string, { label: string; color: string }> = {
  cli: { label: "Claude Code", color: C.accent },
  "claude-desktop": { label: "Desktop", color: C.violet },
  "local-agent": { label: "Desktop Cowork", color: C.green },
  sdk: { label: "SDK", color: C.ink2 },
  unknown: { label: "不明", color: C.ink3 },
};
// sdk-py／sdk-ts／sdk-cli 都歸「SDK」；其餘沒列的歸「不明」。
const attrMeta = (ep: string) => ATTR_META[ep] ?? (ep.startsWith("sdk-") ? ATTR_META.sdk : ATTR_META.unknown);

const WEEKDAYS = ["一", "二", "三", "四", "五", "六", "日"];

/** v1.9.4（D99 E4）：429 撞的是哪條額度（後端 walls[].class）。 */
const WALL_CLASS: Record<string, string> = { five_hour: "5h", seven_day: "7d", fable: "Fable 週", other: "其他" };

export function HalfYearPage() {
  const usage = useStore((s) => s.usage);
  const [dash, setDash] = useState<Dashboard | null>(null);
  // v1.3：頂欄切換器選了座位就跟著走。v1.8.2 主人驗收：反過來也要——頁內 chip 點了
  // 座位，頂欄「帳號」要跟著換，所以 chip 直接改 store 的 viewSeat，seatPick 只從 store 推。
  // v1.8.13（D91 T-15）：不再有「先用 null 打一次」——座位沒定下來（ready=false）就不發請求。
  const { seat: seatPick, ready: seatReady } = useSeatPick();
  const setViewSeat = useStore((s) => s.setViewSeat);
  const aliases = useStore((s) => s.settings.accounts.aliases);
  // D60 自定義期間：30/90/180 天或 0=全部，全頁連動。
  const [days, setDays] = useState(180);
  const [error, setError] = useState<string | null>(null);
  // E15（D77）：期間 pills 切換後跳一次「✓ 已套用」小字，1.5s 自己收掉。
  const [applied, setApplied] = useState<number | null>(null);
  useEffect(() => {
    if (applied == null) return;
    const t = window.setTimeout(() => setApplied(null), 1500);
    return () => window.clearTimeout(t);
  }, [applied]);

  useEffect(() => {
    if (!seatReady) return;
    // v1.8.2 效能：先拿上一次的畫（切頁回來不閃骨架），IPC 回來再換新的。
    const key = `dash|${seatPick ?? ""}|${days}`;
    const cached = pageCache.get<Dashboard>(key);
    if (cached) setDash(cached);
    cmd
      .getDashboard(seatPick, days)
      .then((d) => {
        pageCache.set(key, d);
        setDash(d as Dashboard);
        setError(null);
      })
      .catch((e) => setError(String(e)));
  }, [seatPick, seatReady, days, usage.lastSuccessAt]);


  // v1.8.13（D91 T-03）：帳本沒有任何帳號時後端回 {seats:[],empty:true}，下面每個 useMemo
  // 都會在 render 時讀 undefined 丟 TypeError（整窗白屏）。hooks 不能提早 return，
  // 所以 memo 一律吃 `live`——空的 payload 在這裡就被擋成 null。
  const live = dash && !dash.empty ? dash : null;

  // 期間序列：>60 天自動按週聚合，長條不擠。
  const series = useMemo(() => {
    const dash = live;
    if (!dash) return [];
    const usdMap = new Map(dash.daily.map((d) => [d.day, d.usd]));
    // v1.8.13（D91 T-13）：撞牆、缺口是時間戳（UTC），要換成本地日再對日條；
    // 日條本身也按本地日排（以前 toISOString 切 UTC 日，台北 00:00～08:00 歸到前一天）。
    const wallDays = new Set(dash.walls.map((w) => localDayOf(w.at)));
    const satDays = new Set(dash.saturatedDays ?? []);
    // v1.9.5（D100 D4）：缺口可以跨好幾天，跨過的每一天都標（以前只標 from 那一天，其餘幾天像「沒用」）。
    // 舊記錄（v1.9.5 以前的 Desktop 檔空檔）多半是 App 開著但閒置，不一定是真缺口——不畫日條，只在健康度列出。
    const gapDays = new Set(dash.gaps.filter((g) => !g.legacy).flatMap((g) => localDaysSpanning(g.from, g.to)));
    const span =
      days > 0
        ? days
        : dash.daily.length > 0
          ? Math.max(
              30,
              Math.ceil(
                (Date.now() - dateOfLocalDay(dash.daily[0].day).getTime()) / 86400_000,
              ) + 1,
            )
          : 30;
    const list: { day: string; usd: number; wall: boolean; sat: boolean; gap: boolean }[] = [];
    for (let i = span - 1; i >= 0; i--) {
      const day = localDayBack(i);
      list.push({
        day,
        usd: usdMap.get(day) ?? 0,
        wall: wallDays.has(day),
        sat: satDays.has(day),
        gap: gapDays.has(day),
      });
    }
    if (span <= 60) return list;
    const weeks: typeof list = [];
    for (let i = 0; i < list.length; i += 7) {
      const chunk = list.slice(i, i + 7);
      weeks.push({
        day: chunk[0].day,
        usd: chunk.reduce((s, d) => s + d.usd, 0),
        wall: chunk.some((d) => d.wall),
        sat: chunk.some((d) => d.sat),
        gap: chunk.some((d) => d.gap),
      });
    }
    return weeks;
  }, [live, days]);
  const weeklyAgg = days === 0 || days > 60;

  // 匯率膠囊圖＋幾何自驗（D64-10）
  const fxOut = useMemo(() => {
    if (!live) return null;
    const out = fxChartSvg(live.fx5, live.shrink.monthly);
    verifyFxGeometry(out);
    return out;
  }, [live]);

  // M6 證據圖卡（D65-3）：自足 SVG，預覽＝匯出物本人
  const [busy, setBusy] = useState<string | null>(null);
  const [exportMsg, setExportMsg] = useState<{ err: boolean; text: string } | null>(null);
  const cardSvg = useMemo(() => {
    const dash = live;
    if (!dash) return "";
    return evidenceCardSvg({
      fx5: dash.fx5,
      verdict: dash.verdict,
      method: dash.method,
      // v1.8.13（D91 T-14）：全量視角的 selected 沒有帳號 uuid（entrypoint "all"），以前落到
      // seatDefaultLabel 寫成「帳號未知（CLI）」——拿出去的證據帳號欄寫錯最傷。
      seatLabel:
        dash.selected.id === ALL_SEATS
          ? ALL_SEATS_LABEL
          : seatLabel(dash.selected, aliases),
      periodLabel: days === 0 ? "全部資料" : `近 ${days} 天`,
      generatedAt: new Date().toISOString().slice(0, 16).replace("T", " ") + " UTC",
    });
  }, [live, days]);
  const cardUri = useMemo(() => (cardSvg ? svgDataUri(cardSvg) : ""), [cardSvg]);
  const exportPng = async () => {
    if (!cardSvg) return;
    setBusy("轉圖中…");
    setExportMsg(null);
    try {
      const b64 = await svgToPngBase64(cardSvg, 2);
      const stamp = new Date().toISOString().slice(0, 16).replace(/[-:T]/g, "").replace(/(\d{8})(\d{4})/, "$1-$2");
      // v1.8.13（D91 T-14）：全量視角檔名用 all，不再是 evidence-card-seat-*
      const who = dash?.selected.id === ALL_SEATS ? "all" : dash?.selected.accountUuid?.slice(0, 8) ?? "seat";
      const name = `evidence-card-${who}-${stamp}.png`;
      const path = await cmd.saveExportFile(name, b64);
      setExportMsg({ err: false, text: `已存：${path}` });
      // E13（D77 規則 3：匯出＝toast，一次事件一種回答）。按鈕旁那行狀態小字
      // 留著——它講的是「存到哪」，toast 講的是「做完了什麼」。
      toast(`已匯出圖卡 ${name}`);
    } catch (e) {
      setExportMsg({ err: true, text: `匯出失敗：${String(e)}` });
    } finally {
      setBusy(null);
    }
  };
  const exportBundle = async () => {
    setBusy("寫檔中…");
    setExportMsg(null);
    try {
      const r = await cmd.exportEvidence(seatPick, days);
      setExportMsg({ err: false, text: `已存 ${r.files.length} 個檔到 ${r.dir}：${r.files.map((f) => f.split(/[\\/]/).pop()).join("、")}` });
      toast(`已匯出證據包 ${r.files.length} 個檔`); // E13
    } catch (e) {
      setExportMsg({ err: true, text: `匯出失敗：${String(e)}` });
    } finally {
      setBusy(null);
    }
  };

  if (error) {
    return <div className="h5 warn-band zh">讀取失敗:{error}</div>;
  }
  if (!dash) {
    // E28（D77）：骨架 shimmer 取代「載入中…」一行字——半年頁的 dashboard
    // 查詢是四頁裡最會等的一個，這裡最需要「會有東西，正在拿」。
    return <PageSkeleton />;
  }
  // v1.8.13（D91 T-03）：帳本是不是空的以後端的 empty 為準——前端的座位清單只列有帳號 uuid
  // 的座位，只用 Desktop 的帳本可能清單空、資料卻在（見 useSeatPick）。
  if (dash.empty) return <EmptyLedger />;

  const fx = dash.fx5;
  const cur = [...fx].reverse().find((m) => m.rate != null);
  const v = dash.verdict;
  // v1.9.4（D99 A10）：全部帳號視角——後端不合匯率（fx5／fx7 空、匯率類欄位 null），整張匯率卡改講原因。
  const perSeatMode = dash.fxMode === "per_seat" || dash.selected.id === ALL_SEATS;
  // v1.9.4（D99 Q2）：「較上月」只在兩個月是同一個方案、真的能比的時候出（舊後端沒有 comparable＝當能比）。
  const comparable = v.comparable !== false;
  const hasMom = comparable && v.momPct != null && v.walletPct != null;
  // 隔著別的方案的月份也能比（例：這次的 20x 期 vs 上次的 20x 期）——那就不是「上月」。
  const apart = v.monthsApart ?? 1;
  // 比較的那一列不一定是畫面大數字那一列（例：本月剛換方案、還在校準中，比的是上兩個月）——不同時寫清楚是哪個月。
  const momLead = cur && v.curMonth && (v.curMonth !== cur.month || (v.plan != null && cur.plan != null && v.plan !== cur.plan)) ? `${v.curMonth.slice(5)} 月` : "";
  const momVs = `${momLead}較${apart > 1 && v.prevMonth ? ` ${v.prevMonth}` : "上月"}`;
  const failedGate = v.gates.find((g) => !g.pass);
  const fx7cur = [...dash.fx7].reverse().find((m) => m.rate != null);
  const planInfo = dash.plan;
  // A13：期間內不只一種方案。審查 23r M1：要分清楚兩種——
  //   realChange＝真的換過方案（兩段已知方案不同）→ 不同大小的額度池；
  //   只有「方案未知＋一種已知」（例：升版當天，歷史全是未知）→ 只是「不知道」，不能說換過、不能說另一個池。
  const realChange = !!planInfo?.crossesKnownPlans;
  const crossNote = !planInfo?.crossesPlan ? "" : realChange ? "（含換方案前後）" : "（含方案未知的時段）";
  const unpricedModels = dash.unpriced?.models ?? [];

  const dailySum = series.reduce((s, d) => s + d.usd, 0);
  const satCount = (dash.saturatedDays ?? []).length;

  // v1.9.4（D99 B2）：沒牌價的入口 usd 是 null——歸因只分有牌價的那部分。
  const attrTotal = dash.attribution.reduce((s, a) => s + (a.usd ?? 0), 0);
  // 同一個標籤（例如兩種 SDK）合成一段——不然甜甜圈與清單會出現兩列同名。
  const attrSegs: { label: string; color: string; percent: number }[] = [];
  for (const a of dash.attribution) {
    if (a.usd == null || a.usd <= 0) continue;
    const meta = attrMeta(a.entrypoint);
    const pct = attrTotal > 0 ? (a.usd / attrTotal) * 100 : 0;
    const seg = attrSegs.find((s) => s.label === meta.label);
    if (seg) seg.percent += pct;
    else attrSegs.push({ ...meta, percent: pct });
  }

  // 期間總帳（D60 開場錨點）：期間匯率＝期間所有區間的修剪比值（骨層 reconcile）
  // v1.9.4（D99 A3）：期間內換過方案，整段的匯率是兩個池混在一起——改用只算目前方案的那一份並標出方案。
  const periodDelta = fx.reduce((s, m) => s + m.deltaSum, 0);
  const scoped = dash.reconcile.mixedPlans && dash.reconcile.planScoped ? dash.reconcile.planScoped : null;
  // 審查 23r M2：只用目前方案那批算的數字，段數不到 method.minN 就是「校準中」——升版當天那批可能只有十幾段。
  // 保護門檻看 scoped 那批（以前看整個期間的 Δ，scoped 只有幾段時照樣當定論顯示）。
  // 不退回用全部歷史：「方案未知」那段裡可能混了 5x 和 20x，拿來算是錯的。
  // 段數（minN）與 Δ 合計（minDeltaSum）兩道都要過——planScoped.deltaSum 是後端補的（舊後端沒有就只看段數）。
  const minN = dash.method.minN;
  const periodN = scoped ? scoped.intervals : dash.reconcile.intervals;
  const periodDeltaN = scoped ? scoped.deltaSum : periodDelta;
  const periodCal = periodN < minN || (periodDeltaN != null && periodDeltaN < dash.method.minDeltaSum);
  const periodRate = (scoped ? periodN > 0 : periodDelta >= 5) ? (scoped ? scoped.rateTrim : dash.reconcile.rateTrim) : null;
  const periodRateRaw = scoped ? scoped.rateRaw : dash.reconcile.rateRaw;
  const scopedLabel = scoped ? planShort(scoped.scope?.planLabel ?? dash.byModel.scope?.planLabel) : null;
  /** 「校準中 · 已累積 N／20 段」——門檻讀後端的 method.minN，不寫死。 */
  const calText = (n: number, delta?: number | null) =>
    `校準中 · 已累積 ${n.toLocaleString()}／${minN} 段${delta != null && n >= minN && delta < dash.method.minDeltaSum ? `、額度掉 ${Math.round(delta)}／${dash.method.minDeltaSum}%` : ""}`;
  const PERIODS: { d: number; label: string }[] = [
    { d: 30, label: "30D" },
    { d: 90, label: "90D" },
    { d: 180, label: "半年" },
    { d: 0, label: "全部" },
  ];

  const heatMax = Math.max(...dash.heat.cells, 1);
  const basketOf = (basket: { family: string; share: number }[] | null | undefined) =>
    basket
      ?.filter((b) => b.share >= 0.005)
      .map((b) => `${FAMILY_LABEL[b.family] ?? b.family} ${Math.round(b.share * 100)}%`)
      .join("／");
  const basketText = basketOf(dash.shrink.basket);
  // v1.9.4（D99 A4）：每個方案各一個基期；舊後端只有單一 baseMonth／basket。
  const bases = dash.shrink.bases && dash.shrink.bases.length > 0
    ? dash.shrink.bases
    : dash.shrink.baseMonth && dash.shrink.basket
      ? [{ baseMonth: dash.shrink.baseMonth, basket: dash.shrink.basket } as { baseMonth: string; basket: { family: string; share: number }[] } & PlanTagged]
      : [];
  // 固定籃逐月數字，方案換了就用「｜」隔開（換方案後是另一條線、另一個基期，不能接著讀）。
  const basketRuns = planRuns(dash.shrink.monthly)
    .map((r) =>
      dash.shrink.monthly
        .slice(r.from, r.to + 1)
        .filter((m) => m.index != null)
        .map((m) => `${m.month.slice(5)} 月 $${m.index!.toFixed(2)}`)
        .join(" → "),
    )
    .filter(Boolean);
  const multiPlan = bases.length > 1;
  const scope = dash.byModel.scope;
  // 方案時間軸的說明（期間總帳腳注、熱圖腳注共用）。審查 23r M1／N2：
  //   - 真的換了等級的切點寫「X→Y，MM-DD」；
  //   - 方案未知的時段一律叫「方案未知」：開頭那段＝「App 從 MM-DD 才開始記錄方案」，中間的洞＝「MM-DD～MM-DD 方案未知」。
  //   同等級的切點（洞的兩端都是 20x）不寫成「20x→20x」，改寫成那個洞。
  const md5 = (iso: string) => mdhm(iso).slice(0, 5);
  const planChangeText = (planInfo?.changes ?? [])
    .filter((c) => c.fromKey !== c.toKey)
    .map((c) => `${planShort(c.fromLabel)}→${planShort(c.toLabel)}，${md5(c.at)}`)
    .join("；");
  const unknownText = (() => {
    if (!planInfo) return "";
    const parts: string[] = [];
    planInfo.segments.forEach((s, i) => {
      if (s.planKnown || s.intervals === 0) return;
      if (i === 0) parts.push(s.until ? `App 從 ${md5(s.until)} 才開始記錄方案，之前那段是「方案未知」` : "這段期間的方案都還沒記到（「方案未知」）");
      else parts.push(`${md5(s.since)}～${s.until ? md5(s.until) : "現在"} 是「方案未知」`);
    });
    return parts.join("；");
  })();
  // 方案未知第一次出現時附的解釋（N2：同一個狀態只用一種說法）。
  const UNKNOWN_HINT = "App 還沒記到是哪個方案，不確定跟現在是不是同一個";

  return (
    <div className="h5 space-y-4">
      {/* 頂欄 chips */}
      <div className="head">
        {dash.seats.map((s) => {
          const on = s.id === dash.selected.id;
          return (
            <button
              key={s.id}
              className={`chip pick${on ? " on" : ""}`}
              onClick={() => void setViewSeat(s.id)}
            >
              <b>{seatLabel(s, aliases)}</b>
              <span style={{ color: "var(--h5-ink3)" }}>
                {s.isDesktop ? "Desktop" : "CLI"} · {s.samples.toLocaleString()} 筆
              </span>
            </button>
          );
        })}
        {/* v1.8.11（D89，主人）：全量視角——所有帳號合起來算，不套登入時段 */}
        {(() => {
          const on = dash.selected.id === ALL_SEATS;
          return (
            <button className={`chip pick${on ? " on" : ""}`} onClick={() => void setViewSeat(ALL_SEATS)}>
              <b>全部帳號</b>
              <span style={{ color: "var(--h5-ink3)" }}>不分帳號 · {dash.seats.reduce((n, s) => n + s.samples, 0).toLocaleString()} 筆</span>
            </button>
          );
        })()}
        {/* v1.4.1（D78 #5）：刷新時間 chip 搬到今日頁——即時事件不住趨勢頁。 */}
        {usage.notice && (
          <span className="chip ghost zh" style={{ color: "var(--h5-amber-ink)" }}>
            ⚠ {usage.notice}
          </span>
        )}
      </div>

      {/* v1.9.4（D99 B2）：有官方價目表沒收錄價格的模型——它們的用量換不成美元，以前被當成 $0 悄悄拉低匯率。 */}
      {unpricedModels.length > 0 && (
        <div className="warn-band zh">
          <span>
            有 {unpricedModels.length} 個模型在官方價目表上找不到價格：
            {unpricedModels.slice(0, 3).map((m) => m.label ?? m.model).join("、")}
            {unpricedModels.length > 3 ? ` 等` : ""}
            （共 {unpricedModels.reduce((n, m) => n + m.calls, 0).toLocaleString()} 次呼叫）。
            它們的用量換不成美元，用到它們的 {dash.unpriced?.intervals ?? 0} 段配對記錄沒算進匯率，期間消耗也不含它們。
          </span>
        </div>
      )}

      {/* 期間總帳（D60 開場錨點）——研究層以「你選的這段時間」開場 */}
      <article className="card">
        <div className="card-head">
          <span className="card-title zh">期間總帳</span>
          <span className="card-tag">PERIOD LEDGER</span>
          <div className="right">
            {/* E15（D77）：切完期間跳一次小字。措辭用「已套用」而不是「已儲存」
                ——期間是頁面內的選擇，不寫進設定檔，不能說存了。 */}
            {applied != null && (
              <span className="saved zh" key={applied}>
                ✓ 已套用
              </span>
            )}
            <div className="pills">
              {PERIODS.map((p) => (
                <button
                  key={p.d}
                  className={days === p.d ? "on" : ""}
                  onClick={() => {
                    if (p.d === days) return; // 沒變就不回答
                    setDays(p.d);
                    setApplied(Date.now());
                  }}
                >
                  {p.label}
                </button>
              ))}
            </div>
          </div>
        </div>
        <div className="pstats">
          <div className="pstat">
            <div className="k">期間消耗（估）</div>
            <div className="v">${dailySum.toFixed(0)}</div>
          </div>
          <div
            className="pstat"
            title={
              perSeatMode
                ? "全部帳號視角不合計：各帳號的額度池可能不一樣大，百分比加在一起沒有意義"
                : realChange
                  ? "這段期間換過方案：不同方案的 1% 不一樣大，這是不同額度池的百分比加總"
                  : crossNote
                    ? "含方案未知的時段（App 還沒記到是哪個方案），那段也算在這個合計裡"
                    : undefined
            }
          >
            <div className="k">額度共掉了多少</div>
            <div className="v">
              {perSeatMode ? "—" : periodDelta.toFixed(0)}
              <i>{perSeatMode ? "不合計" : `%${crossNote ? "＊" : ""}`}</i>
            </div>
          </div>
          <div
            className="pstat"
            title={perSeatMode ? "全部帳號視角不合併匯率（各帳號的池可能不一樣大）" : periodRateRaw != null ? `原始匯率（未剔極值）$${periodRateRaw.toFixed(2)}／1%${scopedLabel ? `；只算目前方案（${scopedLabel}）的 ${periodN} 段` : ""}` : undefined}
          >
            <div className="k">{scopedLabel ? `期間匯率 · ${scopedLabel}` : "期間匯率（剔極值）"}</div>
            {/* 審查 23r M2：段數不到門檻＝還在校準，數字變淡、旁邊寫進度（跟匯率卡的大數字同一條規則） */}
            <div className="v" style={periodRate != null && periodCal ? { color: "var(--h5-ink3)" } : undefined}>
              {periodRate != null ? `$${periodRate.toFixed(2)}` : "—"}
              <i>{perSeatMode ? "不合併" : "/1%"}</i>
            </div>
            {!perSeatMode && periodRate != null && periodCal && <div className="foot-note zh">{calText(periodN, periodDeltaN)}</div>}
          </div>
          {/* 審查 23r S5（D99 Q7／F1 中性措辭）：「被限流」只算真的 429；用到 100% 另外一格，不叫「撞牆」 */}
          <div className="pstat" title="本機對話記錄裡被拒絕的請求（429）次數">
            <div className="k">被限流 429</div>
            <div className={`v${dash.walls.length > 0 ? " red" : ""}`}>
              {dash.walls.length}
              <i>次</i>
            </div>
          </div>
          <div className="pstat" title="額度有一刻用到 100% 的天數（含窗口還沒重置的）。開了 Extra usage 的帳號，100% 不一定會被擋。">
            <div className="k">用滿方案額度</div>
            <div className={`v${satCount > 0 ? " red" : ""}`}>
              {satCount}
              <i>天</i>
            </div>
          </div>
        </div>
        {/* v1.9.4（D99 A13；審查 23r M1）：期間跨了方案——真的換過方案 vs 只是有一段方案未知，兩種說法分開 */}
        {!perSeatMode && planInfo?.crossesPlan && (
          <p className="foot-note zh" style={{ marginTop: 8 }}>
            {realChange ? (
              <>
                ＊這段期間換過方案（{[planChangeText, unknownText].filter(Boolean).join("；")}）：不同方案的額度池大小不同（1% 代表的用量不一樣），所以「額度共掉了多少」是不同池的百分比加總；匯率、縮水判定、模型倍率都分方案算，不跨方案比。
              </>
            ) : (
              <>
                ＊{unknownText}（{UNKNOWN_HINT}）。那段先不算進匯率、縮水判定與模型倍率；「額度共掉了多少」仍包含那段。
              </>
            )}
          </p>
        )}
      </article>

      <div className="grid2">
        {/* 匯率卡（主角） */}
        <article className="card">
          <div className="card-head">
            <span className="card-title zh">額度匯率</span>
            <span className="card-tag">EXCHANGE RATE · USD PER 1% · 修剪估計 ± 95% CI</span>
          </div>
          {perSeatMode ? (
            // v1.9.4（D99 A10）：全部帳號視角不合匯率——兩個帳號的池可能不一樣大（一個 Max 5x、一個 Max 20x），
            // 混在一起的「$/1%」是兩種尺的平均，不代表任何一個帳號。改列各帳號自己的數字。
            <>
              <p className="pace-minis zh" style={{ marginTop: 10 }}>
                全部帳號一起看時不合併匯率：每個帳號的額度池可能不一樣大，1% 代表的用量不同，合起來的平均不代表任何一個帳號。下面是各帳號分開算的期間匯率；逐月走勢與縮水判定請在頂欄選單一帳號。
              </p>
              {(dash.perSeat ?? []).length > 0 ? (
                <table className="tbl" style={{ marginTop: 10 }}>
                  <thead>
                    <tr>
                      <th>帳號</th>
                      <th>方案</th>
                      <th className="r">配對段數</th>
                      <th className="r">$ / 1%（剔極值）</th>
                    </tr>
                  </thead>
                  <tbody>
                    {(dash.perSeat ?? []).map((p) => {
                      const s = dash.seats.find((x) => x.id === p.seat);
                      // 審查 23r M2：段數或 Δ 合計不到門檻＝校準中（升版當天目前方案那批可能只有十幾段）
                      const cal = p.intervals < minN || p.deltaSum < dash.method.minDeltaSum;
                      return (
                        <tr key={p.seat}>
                          <td className="zh">
                            {s ? seatLabel(s, aliases) : p.email ?? p.seat.slice(0, 8)}
                          </td>
                          <td className={`zh${p.scopePlan === "unknown" ? " mut" : ""}`}>{planShort(p.scopePlanLabel)}</td>
                          <td className="r mut">{p.intervals.toLocaleString()}</td>
                          <td className="r">
                            {p.rateTrim == null ? (
                              <span className="mut">—</span>
                            ) : cal ? (
                              <span className="mut" title={`段數 ${p.intervals}／${minN}、額度累計掉 ${Math.round(p.deltaSum)}／${dash.method.minDeltaSum}%`}>
                                ${p.rateTrim.toFixed(2)}
                                <span style={{ fontSize: 9.5, marginLeft: 4 }}>{calText(p.intervals, p.deltaSum)}</span>
                              </span>
                            ) : (
                              <b>${p.rateTrim.toFixed(2)}</b>
                            )}
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              ) : (
                <p className="foot-note zh" style={{ marginTop: 8 }}>還沒有任何帳號有足夠的配對記錄。</p>
              )}
              <p className="foot-note zh" style={{ marginTop: 8 }}>
                每個帳號只用它「目前方案」那一段的記錄算（換方案前的是另一個額度池、方案未知的那段不確定是不是同一個，都不算；從沒記到方案的帳號只能先用全部記錄，方案欄寫「方案未知」）。段數不到 {minN} 段的先標「校準中」。期間消耗、被限流次數、用滿天數、尖峰熱圖仍然是所有帳號合計。
              </p>
            </>
          ) : cur?.rate != null ? (
            <>
              <div className="bignum-row">
                <span className="bignum">${cur.rate.toFixed(2)}</span>
                <span className="bigunit">/ 1%</span>
                {/* v1.9.4（D99 A3）：這個數字屬於哪個方案（方案不同，1% 的大小就不同） */}
                {cur.planLabel && (
                  <span className={`chip${cur.planKnown === false ? " ghost" : ""}`} title={cur.planKnown === false ? "方案未知：App 還沒記到是哪個方案（升版前的記錄、或換方案前後沒讀到的空檔）" : undefined}>
                    <b>{planShort(cur.planLabel)}</b>
                  </span>
                )}
                {hasMom && (
                  <span className={`mom-chip${v.declare ? (v.momPct! > 0 ? " down" : "") : " ns"}`}>
                    {v.momPct! > 0 ? "▴" : "▾"} {momVs} {Math.abs(v.momPct!).toFixed(1)}%
                  </span>
                )}
                <span className="ref-note">
                  {cur.ci ? (
                    <>估算區間 ${cur.ci[0].toFixed(2)}–${cur.ci[1].toFixed(2)}（95%）<br /></>
                  ) : (
                    <>校準中 · 配對記錄還不夠（需 {dash.method.minN} 筆、額度累計掉 {dash.method.minDeltaSum}%）<br /></>
                  )}
                  本月{multiPlan || planInfo?.crossesPlan ? `（${planShort(cur.planLabel)}）` : ""} {cur.samples} 筆配對 · 額度共掉 {cur.deltaSum.toFixed(0)}%
                  {cur.rateRaw != null && <> · 原始匯率 ${cur.rateRaw.toFixed(2)}</>}
                </span>
              </div>
              {/* E24（D77）：膠囊 hover 出「月份 · 匯率，CI ±Y，n=Z」。SVG 是字串
                  注入的，所以事件掛在外層 div 上、靠 data-* 認是哪一顆膠囊。 */}
              <div
                onMouseMove={(ev) => {
                  const d = (ev.target as HTMLElement).dataset;
                  if (!d?.month) {
                    hideTip();
                    return;
                  }
                  showTip(
                    ev,
                    `${d.month}${d.plan ? `（${d.plan}）` : ""} · $${d.rate}／1%，CI ±${d.ci}，n=${d.n}（Δ% 合計 ${d.delta}）`,
                  );
                }}
                onMouseLeave={hideTip}
                dangerouslySetInnerHTML={{ __html: fxOut?.svg ?? "" }}
              />
              {hasMom ? (
                <div className="fx-sentence zh">
                  {momLead ? `${momLead}的` : ""}匯率 ${v.rateCur!.toFixed(2)}／1%，{momVs.replace(momLead, "")}{apart > 1 ? " " : ""}{v.momPct! < 0 ? "跌" : "升"}{" "}
                  {Math.abs(v.momPct!).toFixed(1)}% —— 同樣的工作要
                  {v.walletPct! > 0 ? "多" : "少"}花 {Math.abs(v.walletPct!).toFixed(1)}% 的額度
                  {!v.declare && <span style={{ color: "var(--h5-ink3)", fontWeight: 400 }}>（差異不夠顯著，先當雜訊看）</span>}
                </div>
              ) : (
                !comparable &&
                v.sentence && (
                  // v1.9.4（D99 Q2）：不能比的時候講原因（後端寫好的句子），不出「較上月」。
                  <div className="fx-sentence zh" style={{ fontSize: 12.5, color: "var(--h5-ink2)" }}>
                    {planShortAll(v.sentence)}
                  </div>
                )
              )}
              <div className="fx-foot">
                <span className={`verdict-box${v.declare ? " declared" : ""}`}>
                  <span className="lbl">縮水判定</span>
                  <span className="zh">
                    {v.declare
                      ? `宣告：${v.curMonth!.slice(5)} 月比 ${v.prevMonth!.slice(5)} 月${v.walletPct! > 0 ? "貴" : "便宜"} ${Math.abs(v.walletPct!).toFixed(1)}%（每項檢查都過${dash.method.declarePremise ? `；${dash.method.declarePremise}` : ""}）`
                      : `不宣告 · 沒過「${failedGate?.label ?? "檢查"}」`}
                  </span>
                </span>
                <span className="legend">
                  膠囊高＝估算區間（95%）· 點密度＝樣本多寡 · 折線＝本月匯率（剔極值）
                  {basketText && <> · 紫色虛線＝固定模型組合的匯率</>}
                  {planInfo?.crossesPlan && (realChange ? <> · 虛線分隔＝換方案（線不跨方案相連）</> : <> · 虛線分隔＝開始記錄方案（方案未知那段灰色，線不相連）</>)}
                  <br />
                  分子＝本機 API 花費（所有入口，只算這個帳號登入中的時段）· 分母＝這個帳號×組織的 5h 額度下降百分點
                </span>
              </div>
              {/* 不能比的時候只有「同一個方案」那一道（它的說明就是上面那句），不再重印一次 */}
              {!(!comparable && v.sentence && v.gates.every((g) => g.key === "plan")) && (
              <div className="gates">
                {v.gates.map((g) => (
                  <Fragment key={g.key}>
                    <span className={`g${g.pass ? " pass" : ""}`}>{g.label}</span>
                    <span className="d zh">{g.key === "plan" ? planShortAll(g.detail) : g.detail}</span>
                  </Fragment>
                ))}
              </div>
              )}
              {bases.length > 0 && basketRuns.length > 0 && (
                <div className="basket-line zh">
                  {multiPlan ? (
                    <>
                      固定模型組合：每個方案各自凍結一份模型比例——
                      {bases.map((b, i) => (
                        <Fragment key={`${b.plan ?? ""}|${b.baseMonth}`}>
                          {i > 0 ? "；" : ""}
                          {planShort(b.planLabel)} 以 {b.baseMonth.slice(5)} 月（{basketOf(b.basket)}）
                        </Fragment>
                      ))}
                      ，算出 {basketRuns.join("｜換方案｜")}。
                    </>
                  ) : (
                    <>
                      固定模型組合：{planInfo?.crossesPlan && bases[0].planLabel && bases[0].planKnown !== false ? `${planShort(bases[0].planLabel)} ` : ""}以 {bases[0].baseMonth.slice(5)} 月的模型比例（{basketOf(bases[0].basket)}）凍結不動，算出 {basketRuns[0]}。
                    </>
                  )}
                  這樣排除了「自己換模型」的影響，只看廠商給的價值有沒有變。
                </div>
              )}
              {fx7cur?.rate != null && (
                <div className="basket-line zh">
                  7d 對照（驗證方向是否一致）：{fx7cur.month.slice(5)} 月{multiPlan || planInfo?.crossesPlan ? `（${planShort(fx7cur.planLabel)}）` : ""} ${fx7cur.rate.toFixed(2)}／1% 週額度
                  {fx7cur.ci && <>（估算區間 ${fx7cur.ci[0].toFixed(2)}–${fx7cur.ci[1].toFixed(2)}）</>}
                  ，{fx7cur.samples} 筆——只比漲跌方向，7d 的絕對值不跟 5h 直接比。
                </div>
              )}
              {/* D60：工作量換算是匯率層的事實，住進主角卡腳注 */}
              <div className="wr-line zh" style={{ marginTop: 6 }}>
                {dash.workRate.turnsPerPercent != null ? (
                  <>
                    1% 額度大約能做 <b>{dash.workRate.turnsPerPercent.toFixed(1)}</b> 次 API 呼叫、產出{" "}
                    <b>{((dash.workRate.outputTokensPerPercent ?? 0) / 1000).toFixed(1)}k</b>{" "}
                    tokens
                    {dash.reconcile.mixedPlans && scopedLabel && <span className="mut">（只算目前方案 {scopedLabel}{periodCal ? `，${calText(periodN, periodDeltaN)}` : ""}）</span>}
                  </>
                ) : (
                  "工作量換算：資料累積中"
                )}
              </div>
              {/* v1.7（D81）：主文只留讀者最需要的那一句；術語版收進「怎麼算的 ▸」 */}
              <div className="method-note zh">
                已知弱點：樣本彼此有時序關聯，檢查比實際更容易通過——這是雜訊過濾，不是數學證明。
              </div>
              <How>
                每段區間匯率去掉下尾 {Math.round(dash.method.trim[0] * 100)}%／上尾 {Math.round((1 - dash.method.trim[1]) * 100)}%
                （非對稱修剪）後取 Δ-加權比值 Σusd÷Σδ；CI＝區間層級 bootstrap {dash.method.bootstrapReps} 次（固定種子）；
                月差＝置換檢定 {dash.method.permReps} 次、α={dash.method.alpha}
                {v.p5h != null && <>，本月 p={v.p5h.toFixed(3)}</>}。
                檢查的術語名：同方案閘、樣本閘、5h 置換檢定、7d 雙視窗同向、固定籃指數同向、看得到的比例閘
                （每段區間的本機美元除以它所在 7d 窗的「本機看得到的比例」s——Anthropic 的用量來源分佈裡 Claude Code＋Cowork 的百分比——
                兩個月校正後再跑同一套置換檢定，仍與 5h 同向且顯著才放行；v1.9.5 起才開始記來源分佈，沒有記錄的月份不判斷。
                別台電腦、網頁版 Claude Code 在分佈裡同樣算 Claude Code、量不到{dash.method.declarePremise ? `，所以宣告句帶「${dash.method.declarePremise}」` : ""}）。
                月匯率按（月, 方案）分組：同一個月換過方案就是兩組；只有同一個方案等級的兩組才互比（可以隔著別的方案，例如這次的 20x 期對上次的 20x 期），方案未知的組不跟任何組比。
                圖上膠囊高＝bootstrap 95% CI；分子＝該座位（帳號×組織）登入中時段內所有入口（Claude Code、Desktop、Cowork…）的事件 API 計價（美元）、分母＝同座位 5h Δ%。
                {basketText && <>固定籃指數＝以基期月各模型家族 API$ 佔比為權重的 Laspeyres 式匯率，1÷Σ(w÷r)；每個方案各取帳本裡第一個合格月當基期，不隨期間選擇改變。</>}
                7d 對照＝同一套算法換成週額度 Δ% 當分母，只驗方向（窗長不同，不宣稱絕對值可比）。
                區間彼此有時序相關，p 值偏樂觀。
              </How>
            </>
          ) : (
            <p className="foot-note zh" style={{ marginTop: 12 }}>
              這個帳號×組織在此期間的配對記錄還不夠（需要：額度有下降、且本機同時有 API 呼叫的記錄）——試試拉長時間範圍，或等幾天後再看。
            </p>
          )}
        </article>

        {/* 期間消耗（橫跨全寬） */}
        <article className="card g-full">
          <div className="card-head">
            <span className="card-title zh">期間消耗</span>
            <span className="card-tag">{weeklyAgg ? "WEEKLY" : "DAILY"} SPEND · EST. USD</span>
            <div className="right">
              <span className="card-tag" style={{ color: "var(--h5-ink2)" }}>
                Σ ${dailySum.toFixed(0)} · 估值
              </span>
            </div>
          </div>
          <div className="daily-body">
            <div style={{ minWidth: 0 }}>
              <div dangerouslySetInnerHTML={{ __html: dailyChartSvg(series) }} />
              <div className="legend-row">
                <span className="lg"><span className="sw bar" />{weeklyAgg ? "週消耗（估值）" : "日消耗（估值）"}</span>
                <span className="lg"><span className="sw wall" />額度用滿的{weeklyAgg ? "週" : "天"}（100%，含窗口未重置）</span>
                <span className="lg"><span className="sw walldot" />被限流記錄（429，本機）</span>
                <span className="lg"><span className="sw zero" />無觀測活動</span>
                <span className="lg"><span className="sw gap" />記錄缺口（App 沒在跑、Desktop 也沒記錄）—— 沒資料不等於沒用量</span>
              </div>
              {/* v1.8.7（D86 蟲 3）：說清楚這張圖只算這個帳號登入中的時段 */}
              <p className="foot-note zh" style={{ marginTop: 6 }}>
                {dash.tenure?.all ? (
                  <>全量視角：本機所有帳號的觀測與 API 記錄都算進來（含最早觀測之前的用量與 Desktop Cowork），不套登入時段。被限流次數、用滿天數與來源歸因同一規則。</>
                ) : dash.tenure && dash.tenure.spans > 0 ? (
                  <>
                    只計入這個帳號在 Claude Code／Desktop 登入中的時段（依觀測記錄判斷，自 {localDayOf(dash.tenure.from) ?? "—"} 起，共 {dash.tenure.spans} 段）；
                    更早的用量分不出是哪個帳號，不計入任何帳號。被限流次數、用滿天數與來源歸因同一規則。
                  </>
                ) : (
                  <>這個帳號沒有登入中的觀測記錄，所以期間消耗、被限流次數、來源歸因都是空的。</>
                )}
              </p>
            </div>
            <aside className="health-side">
              <div className="counts">
                被限流（429）<b>{dash.walls.length}</b> 次（期間）
                <br />
                見頂 <b>{satCount}</b> 天
                <br />
                缺口 <b>{dash.gaps.filter((g) => !g.legacy).length}</b> 段
                {dash.gaps.some((g) => g.legacy) && (
                  <span style={{ color: "var(--h5-ink3)" }}>（另 {dash.gaps.filter((g) => g.legacy).length} 段舊記錄）</span>
                )}
              </div>
              <div className="mini-list">
                <div className="ml-title">WALLS</div>
                {dash.walls.slice(-4).reverse().map((w, i) => (
                  <div className="mini-row" key={i}>
                    <span className="md wall" />
                    <span>
                      {/* v1.9.4（D99 E4）：撞的是哪條額度——後端照 429 內文歸類（以前型別空白一律算 5h） */}
                      <b>{mdhm(w.at)}</b> · {(w.class && WALL_CLASS[w.class]) ?? w.kind ?? "?"}
                    </span>
                  </div>
                ))}
                {dash.walls.length === 0 && (
                  <div className="mini-row"><span>期間內沒有被限流</span></div>
                )}
              </div>
              <div className="mini-list">
                <div className="ml-title">GAPS</div>
                {/* 新定義的缺口排前面、舊記錄接後面（後端按新→舊排，這裡把 legacy 沉到後面） */}
                {[...dash.gaps].sort((a, b) => Number(!!a.legacy) - Number(!!b.legacy)).slice(0, 3).map((g, i) => (
                  <div className="mini-row" key={i}>
                    <span className="md" />
                    <span>
                      {localSpan(g.from, g.to)}
                      {g.legacy && <span style={{ color: "var(--h5-ink3)" }}>（舊記錄）</span>}
                    </span>
                  </div>
                ))}
              </div>
            </aside>
          </div>
        </article>

        {/* 來源歸因（第一排右欄，與匯率卡並肩） */}
        <article className="card g-r1c2">
          <div className="card-head">
            <span className="card-title zh">來源歸因</span>
            <span className="card-tag">ATTRIBUTION · 期間 · 本機觀測</span>
          </div>
          <div className="attr-body">
            <div dangerouslySetInnerHTML={{ __html: attrDonutSvg(attrSegs) }} />
            <div className="attr-list">
              {attrSegs.map((a, i) => (
                <div key={a.label} className={`attr-row${i === 0 ? " hl" : ""}`}>
                  <span className="sw" style={{ background: a.color }} />
                  <span className="nm zh">{a.label}</span>
                  <span className="pc">
                    {/* v1.8.10（主人）：0.2% 被進位成 0% 看起來像消失了——不到 1% 就寫 <1 */}
                    {a.percent > 0 && a.percent < 1 ? "<1" : Math.round(a.percent)}
                    <i>%</i>
                  </span>
                </div>
              ))}
            </div>
          </div>
          <p className="foot-note zh" style={{ marginTop: 8 }}>
            按 API 計價分佈{dash.tenure?.all ? "，本機全量、不分帳號" : "，只算這個帳號登入中的時段"}；本機記錄不含網頁版活動。
            {perSeatMode ? (
              // v1.9.4（D99 A10）：對帳比例是同一個池裡「額度掉了、本機卻沒記錄」的佔比，跨帳號合不起來。
              " 對帳（本機看不到的用量）要選單一帳號才算——各帳號的池可能不一樣大。"
            ) : dash.reconcile.unexplainedShare != null ? (
              // v1.9.4（D99 C2）：看不到的不只網頁版——手機、其他電腦、本機沒納入的入口都算；而且這是下限。
              <span title={crossNote ? `這段期間${crossNote.slice(1, -1)}；比例只算目前方案（${scopedLabel ?? "目前方案"}）的配對記錄` : undefined}>
                {" "}對帳（推估）：至少有 <b>{Math.round(dash.reconcile.unexplainedShare * 100)}%</b> 的 5h 額度下降，本機看不到對應的 API 呼叫
                （網頁、手機、其他電腦、或本機沒有納入的入口），其中 {Math.round((dash.reconcile.zeroEventDeltaShare ?? 0) * 100)}% 整個區間都沒有本機記錄
                {dash.reconcile.mixedPlans && scopedLabel ? `；只算目前方案（${scopedLabel}）${periodCal ? `，${calText(periodN, periodDeltaN)}，比例先別當真` : ""}` : ""}。
              </span>
            ) : (
              " 對帳需要更多配對記錄。"
            )}
          </p>
          <How>
            本機 JSONL 事件按 API 計價（輸入、寫快取、讀快取、輸出各照官方牌價折算）分佈到入口（Claude Code／Desktop）；真值對帳＝1−未修剪匯率÷修剪匯率，
            推估 5h Δ% 中本機事件缺席的佔比；「完全零事件」＝區間內一筆本機事件都沒有的 Δ% 佔比。
            這個比例是<b>下限</b>：它只抓得到「有些區間額度掉很多、本機卻幾乎沒記錄」這種忽高忽低的落差；如果每段都固定有一部分用量在本機看不到（例如一直同時開著網頁版），每段的匯率會一起變低、修剪抓不出來，那部分不會算進這個百分比。
            期間內換過方案時只用目前方案的配對記錄算（不同方案的 1% 大小不同，混在一起會失真）。
          </How>
        </article>

        {/* 模型倍率（M5，D55 挪入；v1.6 D80 改回歸法、模型 id 各一列） */}
        <article className="card">
          <div className="card-head">
            <span className="card-title zh">模型別額度倍率</span>
            <span className="card-tag">
              QUOTA MULTIPLIER · 回歸 · {perSeatMode ? "全部帳號不算" : <>{scope?.planKnown && scope.excludedIntervals > 0 ? `只用 ${planShort(scope.planLabel)}` : "期間"} · n={dash.byModel.n}</>}
              {!perSeatMode && dash.byModel.r2 != null ? ` · R² ${dash.byModel.r2.toFixed(2)}` : ""}
            </span>
          </div>
          {/* v1.9.4（D99 A5）：回歸只用目前方案的區間——換方案前的是另一個池，混進來會把方案的差別算到模型頭上。 */}
          {scope && !scope.allSeats && scope.planKnown && scope.excludedIntervals > 0 && (
            <p className="pace-minis zh" style={{ marginTop: 8 }}>
              {/* 審查 23r M1：「剛換方案」只在真的換過時說；升版當天（只是之前方案未知）說「剛開始記錄方案」 */}
              {dash.byModel.rows.length === 0 || dash.byModel.rows.every((r) => r.status !== "ok") ? <b>校準中（{realChange ? "剛換方案" : "剛開始記錄方案"}）：</b> : null}
              只用目前方案（{planShort(scope.planLabel)}）的 {scope.intervals.toLocaleString()} 段配對記錄
              {dash.method.regression?.minSegments != null ? `（每個模型要 ${dash.method.regression.minSegments} 段以上才出數字）` : ""}；
              {/* 後端把排除的原因拆開了：換方案前（另一個額度池）vs 方案未知（不確定是不是同一個）。舊後端沒有就用合計。 */}
              {scope.excludedOtherPlan != null && scope.excludedUnknownPlan != null
                ? [
                    scope.excludedOtherPlan > 0 ? `換方案前的 ${scope.excludedOtherPlan.toLocaleString()} 段是另一個額度池，不算` : "",
                    scope.excludedUnknownPlan > 0 ? `方案未知的 ${scope.excludedUnknownPlan.toLocaleString()} 段先不算（${UNKNOWN_HINT}）` : "",
                    (scope.excludedNoFamily ?? 0) > 0 ? `另有 ${scope.excludedNoFamily} 段用到沒有家族欄的模型，不進回歸` : "",
                  ]
                    .filter(Boolean)
                    .join("；") + "。"
                : realChange
                  ? `換方案前（另一個額度池）或方案未知的 ${scope.excludedIntervals.toLocaleString()} 段不算。`
                  : `方案未知的 ${scope.excludedIntervals.toLocaleString()} 段先不算（${UNKNOWN_HINT}）。`}
            </p>
          )}
          {scope && !scope.allSeats && !scope.planKnown && dash.byModel.rows.length > 0 && (
            <p className="foot-note zh" style={{ marginTop: 8 }}>
              這個帳號的方案還沒記到（記錄都是「方案未知」：{UNKNOWN_HINT}），先用期間內全部的配對記錄算；之後如果換過方案，會改成只用目前方案的那一段。
            </p>
          )}
          {perSeatMode ? (
            <p className="foot-note zh" style={{ marginTop: 10 }}>
              全部帳號一起看時不算模型倍率：倍率是「這個模型的 1% 換到多少用量」，各帳號的池可能不一樣大，混在一起會失真。請在頂欄選單一帳號。
            </p>
          ) : dash.byModel.rows.length === 0 ? (
            <p className="foot-note zh" style={{ marginTop: 10 }}>
              還沒有可歸因的配對記錄（需要：同一個帳號×組織、45 分鐘內、額度有上升、且本機有 API 呼叫）。
            </p>
          ) : (
            <>
              <table className="tbl" style={{ marginTop: 10 }}>
                <thead>
                  <tr>
                    <th>模型</th>
                    <th className="r">$ / 1%</th>
                    <th className="r">估算區間（95%）</th>
                    <th className="r">段數</th>
                    <th className="r">每 1% 額度換到的用量</th>
                  </tr>
                </thead>
                <tbody>
                  {FAMILY_ORDER.filter((f) => dash.byModel.rows.some((r) => r.family === f)).map((fam) => {
                    const xc = dash.byModel.families.find((x) => x.family === fam);
                    const rows = dash.byModel.rows.filter((r) => r.family === fam).sort((a, b) => (a.model < b.model ? 1 : -1));
                    // 家族列＝各模型合併再回歸的綜合費率（上一版家族表的表示、新算法）；旁邊掛純區間法交叉驗證
                    const badge =
                      xc == null ? null : xc.agree === true ? "✓ 兩種算法吻合" : xc.agree === false ? "△ 兩種算法結果不同" : "舊算法樣本不足";
                    return [
                      <tr key={`h-${fam}`} className="grp">
                        <td className="zh">
                          <b>{GROUP_LABEL[fam] ?? fam}</b>
                          {rows.length > 1 && <span className="mut" style={{ marginLeft: 6, fontSize: 9.5 }}>合併 {rows.length} 版</span>}
                          {badge && (
                            <span className="mut" style={{ marginLeft: 8, fontSize: 9.5 }} title={xc?.pureRate != null ? `純區間法 $${xc.pureRate.toFixed(2)}／1%，n=${xc.pureN}` : undefined}>
                              {badge}
                            </span>
                          )}
                        </td>
                        {xc && xc.regStatus === "ok" && xc.regRate != null ? (
                          <>
                            <td className="r"><b>${xc.regRate.toFixed(2)}</b></td>
                            <td className="r mut">{xc.regCi ? `${xc.regCi[0].toFixed(2)}–${xc.regCi[1] != null ? xc.regCi[1].toFixed(2) : "∞"}` : "—"}</td>
                          </>
                        ) : (
                          <td className="r mut zh" colSpan={2} style={{ fontSize: 9.8 }}>
                            校準中{xc?.regRate != null ? `（暫估 $${xc.regRate.toFixed(2)}）` : ""}
                          </td>
                        )}
                        <td className="r mut">{xc?.nSeg ?? ""}</td>
                        <td className="r" style={{ width: 140 }}>
                          {xc?.multiplier != null ? <b>{xc.multiplier.toFixed(2)}× 平均</b> : "—"}
                          {xc?.multiplier != null && (
                            <div className="mult-bar">
                              <div className={xc.multiplier < 0.85 ? "hot" : ""} style={{ width: `${Math.min(100, xc.multiplier * 50)}%` }} />
                            </div>
                          )}
                        </td>
                      </tr>,
                      ...rows.map((m) => (
                        <tr key={m.model}>
                          <td className="zh" style={{ paddingLeft: 18 }}>{m.label}</td>
                          {m.status === "ok" && m.rate != null ? (
                            <>
                              <td className="r">${m.rate.toFixed(2)}</td>
                              <td className="r mut">{m.ci ? `${m.ci[0].toFixed(2)}–${m.ci[1] != null ? m.ci[1].toFixed(2) : "∞"}` : "—"}</td>
                            </>
                          ) : m.status === "collinear" ? (
                            <td className="r mut zh" colSpan={2} style={{ fontSize: 9.8 }}>難以分開 · 總是跟 {m.collinearWith} 一起出現，只能合著看</td>
                          ) : (
                            <td className="r mut zh" colSpan={2} style={{ fontSize: 9.8 }}>
                              校準中{m.rate != null ? `（暫估 $${m.rate.toFixed(2)}）` : ""}
                            </td>
                          )}
                          <td className="r mut">{m.nSeg}</td>
                          <td className="r" style={{ width: 140 }}>
                            {m.multiplier != null ? `${m.multiplier.toFixed(2)}× 平均` : "—"}
                            {m.multiplier != null && (
                              <div className="mult-bar">
                                <div className={m.multiplier < 0.85 ? "hot" : ""} style={{ width: `${Math.min(100, m.multiplier * 50)}%` }} />
                              </div>
                            )}
                          </td>
                        </tr>
                      )),
                    ];
                  })}
                </tbody>
              </table>
              <p className="foot-note zh" style={{ marginTop: 8 }}>
                $/1% 越高，代表同樣 1% 的額度能換到的 API 用量越多——這個模型比較「划算」；量條越長越划算，跟整體平均比（1.0×＝平均，最高顯示到 2.0×），低於 0.85× 的標橘色。
                「校準中」表示樣本還不夠，數字先不要當真。家族列是同家族各版本合併計算的綜合費率；旁邊的符號顯示新舊兩種算法的結果是否吻合。
              </p>
              <How>
                倍率＝該模型匯率 ÷ 期間修剪匯率（&gt;1 代表比平均划算）。回歸：每段區間 Δ% ＝ Σ 各模型 API$ × 係數（加權 NNLS：係數 ≥0、無截距、權重 1/usd、與匯率同一套修剪），
                混用的區間也算數，所以 Sonnet 這種總是跟別人一起用的模型也有數字；$/1% 是係數的倒數，CI 是 case-resampling bootstrap。
                「校準中」條件：有花錢的段數 &lt;{dash.method.regression?.minSegments ?? 10}、或 bootstrap 有超過 {Math.round((dash.method.regression?.zeroShareMax ?? 0.05) * 100)}% 把係數壓到 0（邊界，區間不可信）、或 CI 寬過 {dash.method.regression?.maxCiRatio ?? 5} 倍。
                家族列＝該家族各版模型合併成一欄再回歸（Sonnet 5 與 4.x 同一家）；交叉驗證＝舊的純區間法（單一家族佔區間 API$ ≥{Math.round(dash.method.pureShare * 100)}% 且 Δ≥2，n≥{dash.method.familyMinN}），純區間匯率落在回歸 CI 內或差 &lt;25% 算吻合。
              </How>
            </>
          )}
        </article>

        {/* 尖峰熱圖（M5，D55 挪入） */}
        <article className="card">
          <div className="card-head">
            <span className="card-title zh">尖峰時段</span>
            <span className="card-tag">PEAK HEAT · 本地時間 · Σ Δ%</span>
          </div>
          <div className="heat">
            <span />
            {Array.from({ length: 24 }, (_, h) => (
              <span key={`x${h}`} className="hx">{h % 6 === 0 ? h : ""}</span>
            ))}
            {WEEKDAYS.map((w, d) => (
              <Fragment key={`row${d}`}>
                <span className="hl">{w}</span>
                {Array.from({ length: 24 }, (_, h) => {
                  const val = dash.heat.cells[d * 24 + h] ?? 0;
                  return (
                    <span
                      key={`c${d}-${h}`}
                      className="hc"
                      style={{ opacity: val > 0 ? 0.15 + 0.85 * (val / heatMax) : 0.06 }}
                      /* E23（D77 改版）：不放大——放大會蓋住鄰格，而熱圖的價值
                         正是鄰格的相對深淺；只留 1px 黑框（CSS）＋值標。
                         標的是格子真正承載的 Σ Δ%，不是「峰值」。 */
                      onMouseMove={(ev) =>
                        showTip(ev, `週${w} ${String(h).padStart(2, "0")}:00 · Σ Δ% ${val.toFixed(0)}${perSeatMode ? "（各帳號合計）" : crossNote}`)
                      }
                      onMouseLeave={hideTip}
                    />
                  );
                })}
              </Fragment>
            ))}
          </div>
          <div className="bands">
            {dash.heat.bands.map((b) => (
              <div
                className="band"
                key={b.label}
                title={perSeatMode ? "全部帳號視角不合併匯率（各帳號的池可能不一樣大）" : dash.reconcile.mixedPlans && scopedLabel ? `${crossNote.slice(1, -1)}；時段匯率只算目前方案（${scopedLabel}）` : undefined}
              >
                <div className="k">{b.label}</div>
                <div className="v">
                  {b.rate != null ? `$${b.rate.toFixed(2)}` : "—"}
                  <i>/1%</i>
                </div>
                {/* 審查 23r M2：還不到門檻的時段寫進度（門檻讀後端 method.minN） */}
                <div>{b.rate == null && b.n > 0 && !perSeatMode ? `校準中 ${b.n}／${minN}` : `n=${b.n}`}</div>
              </div>
            ))}
          </div>
          <p className="foot-note zh" style={{ marginTop: 8 }}>
            格子顏色越深，代表那個時段的額度消耗總量越多；下方是各 6 小時時段的匯率（至少 {dash.method.minN} 筆才顯示）。
            純描述——「某個時段比較傷額度」需要更多月的資料才能驗證，這裡不下結論。
            {perSeatMode
              ? "全部帳號：格子是各帳號的百分比加總；時段匯率不合併（各帳號的池可能不一樣大）。"
              : planInfo?.crossesPlan
                ? `${realChange ? "這段期間換過方案：格子是不同額度池的百分比加總，深淺只能粗看" : "這段期間含方案未知的時段：格子照樣加總，深淺只能粗看"}${dash.reconcile.mixedPlans && scopedLabel ? `；時段匯率只算目前方案（${scopedLabel}）` : ""}。`
                : ""}
          </p>
          <How>
            格色＝該星期×小時格內 5h 額度的 Δ% 合計；各 6 小時時段＝該時段區間的修剪匯率（n≥{dash.method.minN}）。純描述性統計，不做推斷、不進縮水判定。
          </How>
        </article>
        {/* 拿得出去的證據（M6，D65） */}
        <article className="card g-full">
          <div className="card-head">
            <span className="card-title zh">拿得出去的證據</span>
            <span className="card-tag">EVIDENCE · 圖卡 PNG ＋ 證據包（JSON · CSV · MARKDOWN）</span>
            <div className="right">
              <span className="card-tag" style={{ color: "var(--h5-ink2)" }}>
                期間 {days === 0 ? "全部" : `${days} 天`}
                {/* v1.4.1（D78 #4）：真值資料可能比期間短，標出實際起點，免得四個範圍長一樣像壞掉 */}
                {dash.dataSince ? `（資料自 ${localDayOf(dash.dataSince)} 起，共 ${Math.max(1, Math.round((Date.now() - new Date(dash.dataSince).getTime()) / 86_400_000))} 天）` : ""}
                {/* v1.8.13（D91 T-14）：全量視角寫「全部帳號」，不是「座位 ?」 */}
                {dash.selected.id === ALL_SEATS ? " · 全部帳號" : ` · 帳號 ${dash.selected.accountUuid?.slice(0, 8) ?? "?"}`}
              </span>
            </div>
          </div>
          <div className="evi-body">
            <div className="evi-preview">
              <img src={cardUri} alt="證據圖卡預覽" />
            </div>
            <div className="evi-side">
              <button className="evi-btn primary" disabled={busy !== null} onClick={() => void exportPng()}>
                <span className="ico"><ImageDown size={15} style={{ color: "var(--h5-accent)" }} /></span>
                <span>
                  <b>匯出圖卡 PNG</b>
                  <small>2400×1260 · 左邊這張，可直接貼上網</small>
                </span>
              </button>
              <button className="evi-btn" disabled={busy !== null} onClick={() => void exportBundle()}>
                <span className="ico"><FileDown size={15} style={{ color: "var(--h5-accent)" }} /></span>
                <span>
                  <b>匯出證據包</b>
                  <small>JSON（含欄位定義＋每段區間）· CSV×2 · Markdown 報告</small>
                </span>
              </button>
              <button className="evi-btn" onClick={() => void cmd.revealExportFolder().catch((e) => setExportMsg({ err: true, text: String(e) }))}>
                <span className="ico"><FolderOpen size={15} style={{ color: "var(--h5-ink3)" }} /></span>
                <span>
                  <b>開啟匯出資料夾</b>
                  <small>app 資料夾下的 exports／</small>
                </span>
              </button>
              {exportMsg && <div className={`evi-status zh${exportMsg.err ? " err" : ""}`}>{exportMsg.text}</div>}
              {!exportMsg && (
                <div className="evi-status zh">
                  {busy ? busy : "匯出的每個數字都和這頁同源，且可以用區間 CSV 重算驗證。「校準中」「不宣告」的標注在匯出裡也會照寫——證據的意義在經得起別人質疑。"}
                </div>
              )}
            </div>
          </div>
        </article>

      </div>
    </div>
  );
}
