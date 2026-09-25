import { Fragment, useEffect, useMemo, useState } from "react";
import { FileDown, FolderOpen, ImageDown } from "lucide-react";
import { useStore } from "@/store/usageStore";
import { cmd } from "@/lib/tauri";
import { evidenceCardSvg, svgDataUri, svgToPngBase64 } from "@/lib/exportCard";
import { seatLabel, ALL_SEATS } from "@/lib/seatLabel";
import { pageCache } from "@/lib/pageCache";
import { PageSkeleton, hideTip, showTip, toast } from "../Motion";
import { How } from "../How";
import "../h5.css";

// =============================================================================
// 這半年發生了什麼 (M4) — 研究層，H5 靛藍帳房上身。
// 視覺基準 prototypes/dash-H5-dot-cool.html；所有幾何由資料即時運算（D42：
// 畫出來的東西必須能反推回宣稱的數值——沒有的數據就不畫，不裝）。
// M5（D64）匯率轉正：月匯率＝修剪 Δ-加權比值（Σusd÷Σδ，砍每段匯率下尾 25%／
// 上尾 5%），膠囊高＝bootstrap 95% CI、網點密度＝樣本數 n；固定籃指數（凍結
// 基期月模型組合）以紫色虛線並列；縮水宣告走四道閘（樣本／5h 檢定／雙視窗／
// 固定籃），任一沒過只呈現數據並標出卡在哪。CI 帶畫完後由 SVG 反推自驗。
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

interface FxMonth {
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
}
interface Verdict {
  declare: boolean;
  gates: Gate[];
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
interface ShrinkMonth {
  month: string;
  index: number | null;
  coverage: number | null;
  ci: [number, number] | null;
}
interface Dashboard {
  empty?: boolean;
  seats: { id: string; accountUuid: string | null; orgUuid: string | null; samples: number; isDesktop: boolean; email?: string | null; orgName?: string | null }[];
  selected: { id: string; accountUuid: string | null; orgUuid: string | null; entrypoint: string };
  fx5: FxMonth[];
  fx7: FxMonth[];
  verdict: Verdict;
  shrink: {
    baseMonth: string | null;
    basket: { family: string; share: number }[] | null;
    monthly: ShrinkMonth[];
  };
  /** v1.6（D80）：回歸法分模型倍率——模型 id 各一列，家族只當分組；families 是純區間法的交叉驗證。 */
  byModel: {
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
    rateRaw: number | null;
    rateTrim: number | null;
    unexplainedShare: number | null;
    zeroEventDeltaShare: number | null;
    intervals: number;
  };
  method: {
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
  walls: { at: string; kind: string | null }[];
  saturatedDays: string[];
  gaps: { from: string; to: string }[];
  attribution: { entrypoint: string; outputTokens: number }[];
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
function fxChartSvg(months: FxMonth[], shrink: ShrinkMonth[]): { svg: string; lo: number; hi: number; T: number; ph: number } {
  const W = 640, H = 200, L = 46, R = 14, T = 16, B = 32;
  const pw = W - L - R, ph = H - T - B;
  const withRate = months.filter((m) => m.rate != null) as (FxMonth & { rate: number })[];
  if (withRate.length === 0) return { svg: "", lo: 0, hi: 1, T, ph };

  const shrinkMap = new Map(shrink.map((s) => [s.month, s]));
  const vals: number[] = [];
  for (const m of withRate) {
    vals.push(m.rate);
    if (m.ci) vals.push(m.ci[0], m.ci[1]);
    const s = shrinkMap.get(m.month);
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

  let defs = "", caps = "", labels = "";
  months.forEach((m, i) => {
    const isNow = i === months.length - 1;
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
        <rect x="0" y="0" width="2.6" height="2.6" rx="0.9" fill="${C.accent}" opacity="0.5"/></pattern>`;
      // E24（D77）：CI 與 n 是圖上讀不出、又直接關係「這個數可不可信」的東西
      // ——改由跟游標的小標講（原生 <title> 拿掉，免得兩個 tooltip 同時出現）。
      // 注意 data-* 一律加在 height 之後：verifyFxGeometry() 的正則吃前面那段。
      caps += `<g>
      <rect class="ci-cap" data-lo="${m.ci[0]}" data-hi="${m.ci[1]}" x="${x}" y="${yTop}" width="${capW}" height="${yBot - yTop}" data-month="${m.month}" data-rate="${m.rate.toFixed(2)}" data-ci="${((m.ci[1] - m.ci[0]) / 2).toFixed(2)}" data-n="${m.samples}" data-delta="${m.deltaSum.toFixed(0)}" rx="${Math.min(10, (yBot - yTop) / 2)}" fill="${C.accentSoft}" opacity="0.7"/>
      <rect x="${x}" y="${yTop}" width="${capW}" height="${yBot - yTop}" rx="${Math.min(10, (yBot - yTop) / 2)}" fill="url(#${pid})"/>
      ${isNow ? `<rect x="${x}" y="${yTop}" width="${capW}" height="${yBot - yTop}" rx="${Math.min(10, (yBot - yTop) / 2)}" fill="none" stroke="${C.accent}" stroke-width="1.6"/>` : ""}
    </g>`;
    } else {
      // 校準中：只有點，沒有帶——沒算出 CI 就不畫帶
      caps += `<g><title>${m.month} · $${m.rate.toFixed(2)}/1%（校準中）· n=${m.samples} · Δ%合計 ${m.deltaSum.toFixed(0)}</title>
      <line x1="${cx(i)}" y1="${T}" x2="${cx(i)}" y2="${T + ph}" stroke="${C.line}" stroke-width="1" stroke-dasharray="2 4"/></g>`;
    }
    labels += `<text x="${cx(i)}" y="${(m.ci ? y(m.ci[1]) : y(m.rate)) - 9}" text-anchor="middle" font-size="${isNow ? 13 : 10}" font-weight="${isNow ? 700 : 500}" fill="${isNow ? C.accentInk : C.ink2}">${m.rate.toFixed(2)}</text>
      <text x="${cx(i)}" y="${H - 20}" text-anchor="middle" font-size="10.5" font-weight="${isNow ? 700 : 500}" fill="${isNow ? C.ink : C.ink2}">${m.month.slice(5)}${isNow ? " 月 · 今" : " 月"}</text>
      <text x="${cx(i)}" y="${H - 7}" text-anchor="middle" font-size="8.5" fill="${C.ink3}">n=${m.samples}${m.qualified ? "" : " · 校準中"}</text>`;
  });

  const linePts = months
    .map((m, i) => (m.rate != null ? `${cx(i)},${y(m.rate)}` : null))
    .filter(Boolean)
    .join(" ");
  const line = `<polyline points="${linePts}" fill="none" stroke="${C.accentDeep}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>`;
  let pts = "";
  months.forEach((m, i) => {
    if (m.rate != null)
      pts += `<circle cx="${cx(i)}" cy="${y(m.rate)}" r="3.1" fill="#fff" stroke="${C.accentDeep}" stroke-width="1.8"/>`;
  });

  // 固定籃指數：紫色虛線＋空心方點（推估，不搶主角）
  const bPts = months
    .map((m, i) => {
      const s = shrinkMap.get(m.month);
      return s?.index != null ? `${cx(i)},${y(s.index)}` : null;
    })
    .filter(Boolean)
    .join(" ");
  let basket = "";
  if (bPts) {
    basket += `<polyline points="${bPts}" fill="none" stroke="${C.violet}" stroke-width="1.5" stroke-dasharray="4 3"/>`;
    months.forEach((m, i) => {
      const s = shrinkMap.get(m.month);
      if (s?.index != null)
        basket += `<rect x="${cx(i) - 3}" y="${y(s.index) - 3}" width="6" height="6" fill="#fff" stroke="${C.violet}" stroke-width="1.4"><title>固定籃 ${m.month} · $${s.index.toFixed(2)}/1%${s.ci ? ` · CI $${s.ci[0].toFixed(2)}–$${s.ci[1].toFixed(2)}` : ""} · 籃覆蓋 ${Math.round((s.coverage ?? 0) * 100)}%</title></rect>`;
    });
  }

  return {
    svg: `<svg width="${W}" height="${H}" viewBox="0 0 ${W} ${H}"><defs>${defs}</defs>${grid}${caps}${basket}${line}${pts}${labels}</svg>`,
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
  "local-agent": { label: "雲端 session", color: C.green },
  unknown: { label: "不明", color: C.ink3 },
};

const WEEKDAYS = ["一", "二", "三", "四", "五", "六", "日"];

export function HalfYearPage() {
  const usage = useStore((s) => s.usage);
  const [dash, setDash] = useState<Dashboard | null>(null);
  const [seatPick, setSeatPick] = useState<string | null>(null);
  // v1.3：頂欄切換器選了座位就跟著走。v1.8.2 主人驗收：反過來也要——頁內 chip 點了
  // 座位，頂欄「帳號」要跟著換，所以 chip 直接改 store 的 viewSeat，seatPick 只從 store 推。
  const viewSeatId = useStore((s) => s.viewSeatId);
  const currentSeatId = useStore((s) => s.currentSeatId);
  const setViewSeat = useStore((s) => s.setViewSeat);
  const aliases = useStore((s) => s.settings.accounts.aliases);
  useEffect(() => {
    const want = viewSeatId ?? currentSeatId;
    if (want) setSeatPick(want);
  }, [viewSeatId, currentSeatId]);
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
  }, [seatPick, days, usage.lastSuccessAt]);


  // 期間序列：>60 天自動按週聚合，長條不擠。
  const series = useMemo(() => {
    if (!dash) return [];
    const usdMap = new Map(dash.daily.map((d) => [d.day, d.usd]));
    const wallDays = new Set(dash.walls.map((w) => w.at.slice(0, 10)));
    const satDays = new Set(dash.saturatedDays ?? []);
    const gapDays = new Set(dash.gaps.map((g) => g.from.slice(0, 10)));
    const span =
      days > 0
        ? days
        : dash.daily.length > 0
          ? Math.max(
              30,
              Math.ceil(
                (Date.now() - new Date(dash.daily[0].day).getTime()) / 86400_000,
              ) + 1,
            )
          : 30;
    const list: { day: string; usd: number; wall: boolean; sat: boolean; gap: boolean }[] = [];
    for (let i = span - 1; i >= 0; i--) {
      const day = new Date(Date.now() - i * 86400_000).toISOString().slice(0, 10);
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
  }, [dash, days]);
  const weeklyAgg = days === 0 || days > 60;

  // 匯率膠囊圖＋幾何自驗（D64-10）
  const fxOut = useMemo(() => {
    if (!dash) return null;
    const out = fxChartSvg(dash.fx5, dash.shrink.monthly);
    verifyFxGeometry(out);
    return out;
  }, [dash]);

  // M6 證據圖卡（D65-3）：自足 SVG，預覽＝匯出物本人
  const [busy, setBusy] = useState<string | null>(null);
  const [exportMsg, setExportMsg] = useState<{ err: boolean; text: string } | null>(null);
  const cardSvg = useMemo(() => {
    if (!dash || dash.empty) return "";
    return evidenceCardSvg({
      fx5: dash.fx5,
      verdict: dash.verdict,
      method: dash.method,
      seatLabel: `${seatLabel(dash.selected, aliases)}（${dash.selected.entrypoint === "claude-desktop" ? "Desktop" : "CLI"}）`,
      periodLabel: days === 0 ? "全部資料" : `近 ${days} 天`,
      generatedAt: new Date().toISOString().slice(0, 16).replace("T", " ") + " UTC",
    });
  }, [dash, days]);
  const cardUri = useMemo(() => (cardSvg ? svgDataUri(cardSvg) : ""), [cardSvg]);
  const exportPng = async () => {
    if (!cardSvg) return;
    setBusy("轉圖中…");
    setExportMsg(null);
    try {
      const b64 = await svgToPngBase64(cardSvg, 2);
      const stamp = new Date().toISOString().slice(0, 16).replace(/[-:T]/g, "").replace(/(\d{8})(\d{4})/, "$1-$2");
      const name = `evidence-card-${dash?.selected.accountUuid?.slice(0, 8) ?? "seat"}-${stamp}.png`;
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

  const fx = dash.fx5;
  const cur = [...fx].reverse().find((m) => m.rate != null);
  const v = dash.verdict;
  const hasMom = v.momPct != null && v.walletPct != null;
  const failedGate = v.gates.find((g) => !g.pass);
  const fx7cur = [...dash.fx7].reverse().find((m) => m.rate != null);

  const dailySum = series.reduce((s, d) => s + d.usd, 0);
  const satCount = (dash.saturatedDays ?? []).length;

  const attrTotal = dash.attribution.reduce((s, a) => s + a.outputTokens, 0);
  const attrSegs = dash.attribution
    .filter((a) => a.outputTokens > 0)
    .map((a) => ({
      label: (ATTR_META[a.entrypoint] ?? ATTR_META.unknown).label,
      color: (ATTR_META[a.entrypoint] ?? ATTR_META.unknown).color,
      percent: attrTotal > 0 ? (a.outputTokens / attrTotal) * 100 : 0,
    }));

  // 期間總帳（D60 開場錨點）：期間匯率＝期間所有區間的修剪比值（骨層 reconcile）
  const periodDelta = fx.reduce((s, m) => s + m.deltaSum, 0);
  const periodRate = periodDelta >= 5 ? dash.reconcile.rateTrim : null;
  const PERIODS: { d: number; label: string }[] = [
    { d: 30, label: "30D" },
    { d: 90, label: "90D" },
    { d: 180, label: "半年" },
    { d: 0, label: "全部" },
  ];

  const heatMax = Math.max(...dash.heat.cells, 1);
  const basketText = dash.shrink.basket
    ?.filter((b) => b.share >= 0.005)
    .map((b) => `${FAMILY_LABEL[b.family] ?? b.family} ${Math.round(b.share * 100)}%`)
    .join("／");

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
          <div className="pstat">
            <div className="k">額度共掉了多少</div>
            <div className="v">
              {periodDelta.toFixed(0)}
              <i>%</i>
            </div>
          </div>
          <div className="pstat" title={dash.reconcile.rateRaw != null ? `原始匯率（未剔極值）$${dash.reconcile.rateRaw.toFixed(2)}／1%` : undefined}>
            <div className="k">期間匯率（剔極值）</div>
            <div className="v">
              {periodRate != null ? `$${periodRate.toFixed(2)}` : "—"}
              <i>/1%</i>
            </div>
          </div>
          <div className="pstat">
            <div className="k">撞牆 WALLS</div>
            <div className={`v${dash.walls.length + satCount > 0 ? " red" : ""}`}>
              {dash.walls.length}
              <i>429</i>
              {" "}
              {satCount}
              <i>見頂天</i>
            </div>
          </div>
        </div>
      </article>

      <div className="grid2">
        {/* 匯率卡（主角） */}
        <article className="card">
          <div className="card-head">
            <span className="card-title zh">額度匯率</span>
            <span className="card-tag">EXCHANGE RATE · USD PER 1% · 修剪估計 ± 95% CI</span>
          </div>
          {cur?.rate != null ? (
            <>
              <div className="bignum-row">
                <span className="bignum">${cur.rate.toFixed(2)}</span>
                <span className="bigunit">/ 1%</span>
                {hasMom && (
                  <span className={`mom-chip${v.declare ? (v.momPct! > 0 ? " down" : "") : " ns"}`}>
                    {v.momPct! > 0 ? "▴" : "▾"} 較上月 {Math.abs(v.momPct!).toFixed(1)}%
                  </span>
                )}
                <span className="ref-note">
                  {cur.ci ? (
                    <>估算區間 ${cur.ci[0].toFixed(2)}–${cur.ci[1].toFixed(2)}（95%）<br /></>
                  ) : (
                    <>校準中 · 配對記錄還不夠（需 {dash.method.minN} 筆、額度累計掉 {dash.method.minDeltaSum}%）<br /></>
                  )}
                  本月 {cur.samples} 筆配對 · 額度共掉 {cur.deltaSum.toFixed(0)}%
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
                    `${d.month} · $${d.rate}／1%，CI ±${d.ci}，n=${d.n}（Δ% 合計 ${d.delta}）`,
                  );
                }}
                onMouseLeave={hideTip}
                dangerouslySetInnerHTML={{ __html: fxOut?.svg ?? "" }}
              />
              {hasMom && (
                <div className="fx-sentence zh">
                  匯率 ${v.rateCur!.toFixed(2)}／1%，較上月{v.momPct! < 0 ? "跌" : "升"}{" "}
                  {Math.abs(v.momPct!).toFixed(1)}% —— 同樣的工作要
                  {v.walletPct! > 0 ? "多" : "少"}花 {Math.abs(v.walletPct!).toFixed(1)}% 的額度
                  {!v.declare && <span style={{ color: "var(--h5-ink3)", fontWeight: 400 }}>（差異不夠顯著，先當雜訊看）</span>}
                </div>
              )}
              <div className="fx-foot">
                <span className={`verdict-box${v.declare ? " declared" : ""}`}>
                  <span className="lbl">縮水判定</span>
                  <span className="zh">
                    {v.declare
                      ? `宣告：${v.curMonth!.slice(5)} 月比 ${v.prevMonth!.slice(5)} 月${v.walletPct! > 0 ? "貴" : "便宜"} ${Math.abs(v.walletPct!).toFixed(1)}%（四項檢查全過）`
                      : `不宣告 · 沒過「${failedGate?.label ?? "檢查"}」`}
                  </span>
                </span>
                <span className="legend">
                  膠囊高＝估算區間（95%）· 點密度＝樣本多寡 · 折線＝本月匯率（剔極值）
                  {basketText && <> · 紫色虛線＝固定模型組合的匯率</>}
                  <br />
                  分子＝本機 API 花費（{dash.selected.entrypoint === "claude-desktop" ? "Desktop" : "CLI"}）· 分母＝這個帳號×組織的 5h 額度下降百分點
                </span>
              </div>
              <div className="gates">
                {v.gates.map((g) => (
                  <Fragment key={g.key}>
                    <span className={`g${g.pass ? " pass" : ""}`}>{g.label}</span>
                    <span className="d zh">{g.detail}</span>
                  </Fragment>
                ))}
              </div>
              {basketText && (
                <div className="basket-line zh">
                  固定模型組合：以 {dash.shrink.baseMonth?.slice(5)} 月的模型比例（{basketText}）凍結不動，算出{" "}
                  {dash.shrink.monthly
                    .filter((m) => m.index != null)
                    .map((m) => `${m.month.slice(5)} 月 $${m.index!.toFixed(2)}`)
                    .join(" → ")}
                  。這樣排除了「自己換模型」的影響，只看廠商給的價值有沒有變。
                </div>
              )}
              {fx7cur?.rate != null && (
                <div className="basket-line zh">
                  7d 對照（驗證方向是否一致）：{fx7cur.month.slice(5)} 月 ${fx7cur.rate.toFixed(2)}／1% 週額度
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
                四項檢查的術語名：樣本閘、5h 置換檢定、7d 雙視窗同向、固定籃指數同向。
                圖上膠囊高＝bootstrap 95% CI；分子＝該座位（帳號×組織）對應入口的事件 API 計價（美元）、分母＝同座位 5h Δ%。
                {basketText && <>固定籃指數＝以基期月各模型家族 API$ 佔比為權重的 Laspeyres 式匯率，1÷Σ(w÷r)。</>}
                7d 對照＝同一套算法換成週額度 Δ% 當分母，只驗方向（O8：窗長不同，不宣稱絕對值可比）。
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
                <span className="lg"><span className="sw gap" />記錄缺口 —— 沒資料不等於沒用量</span>
              </div>
              {/* v1.8.7（D86 蟲 3）：說清楚這張圖只算這個帳號登入中的時段 */}
              <p className="foot-note zh" style={{ marginTop: 6 }}>
                {dash.tenure?.all ? (
                  <>全量視角：本機所有帳號的觀測與 API 記錄都算進來（含最早觀測之前的用量與雲端 session），不套登入時段。撞牆與來源歸因同一規則。</>
                ) : dash.tenure && dash.tenure.spans > 0 ? (
                  <>
                    只計入這個帳號在 Claude Code／Desktop 登入中的時段（依觀測記錄判斷，自 {dash.tenure.from?.slice(0, 10) ?? "—"} 起，共 {dash.tenure.spans} 段）；
                    更早的用量分不出是哪個帳號，不計入任何帳號。撞牆與來源歸因同一規則。
                  </>
                ) : (
                  <>這個帳號沒有登入中的觀測記錄，所以期間消耗、撞牆、來源歸因都是空的。</>
                )}
              </p>
            </div>
            <aside className="health-side">
              <div className="counts">
                被限流（429）<b>{dash.walls.length}</b> 次（期間）
                <br />
                見頂 <b>{satCount}</b> 天
                <br />
                缺口 <b>{dash.gaps.length}</b> 段
              </div>
              <div className="mini-list">
                <div className="ml-title">WALLS</div>
                {dash.walls.slice(-4).reverse().map((w, i) => (
                  <div className="mini-row" key={i}>
                    <span className="md wall" />
                    <span>
                      <b>{w.at.slice(5, 16).replace("T", " ")}</b> · {w.kind ?? "?"}
                    </span>
                  </div>
                ))}
                {dash.walls.length === 0 && (
                  <div className="mini-row"><span>期間內零撞牆</span></div>
                )}
              </div>
              <div className="mini-list">
                <div className="ml-title">GAPS</div>
                {dash.gaps.slice(0, 3).map((g, i) => (
                  <div className="mini-row" key={i}>
                    <span className="md" />
                    <span>
                      {g.from.slice(5, 16).replace("T", " ")}–{g.to.slice(11, 16)}
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
            按輸出 tokens 分佈{dash.tenure?.all ? "，本機全量、不分帳號" : "，只算這個帳號登入中的時段"}；本機記錄不含網頁版活動。
            {dash.reconcile.unexplainedShare != null ? (
              <>
                {" "}對帳（推估）：有 <b>{Math.round(dash.reconcile.unexplainedShare * 100)}%</b> 的 5h 額度下降找不到對應的本機 API 呼叫
                （網頁版或其他裝置），其中 {Math.round((dash.reconcile.zeroEventDeltaShare ?? 0) * 100)}% 整個區間都沒有本機記錄。
              </>
            ) : (
              " 對帳需要更多配對記錄。"
            )}
          </p>
          <How>
            本機 JSONL 事件按輸出 tokens 分佈到入口（Claude Code／Desktop）；真值對帳＝1−未修剪匯率÷修剪匯率，
            推估 5h Δ% 中本機事件缺席的佔比；「完全零事件」＝區間內一筆本機事件都沒有的 Δ% 佔比。
          </How>
        </article>

        {/* 模型倍率（M5，D55 挪入；v1.6 D80 改回歸法、模型 id 各一列） */}
        <article className="card">
          <div className="card-head">
            <span className="card-title zh">模型別額度倍率</span>
            <span className="card-tag">QUOTA MULTIPLIER · 回歸 · 期間 · n={dash.byModel.n}{dash.byModel.r2 != null ? ` · R² ${dash.byModel.r2.toFixed(2)}` : ""}</span>
          </div>
          {dash.byModel.rows.length === 0 ? (
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
                倍率＝該模型匯率 ÷ 期間修剪匯率（v1.8.7 起；之前是倒數，&gt;1 代表傷額度）。回歸：每段區間 Δ% ＝ Σ 各模型 API$ × 係數（加權 NNLS：係數 ≥0、無截距、權重 1/usd、與匯率同一套修剪），
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
                        showTip(ev, `週${w} ${String(h).padStart(2, "0")}:00 · Σ Δ% ${val.toFixed(0)}`)
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
              <div className="band" key={b.label}>
                <div className="k">{b.label}</div>
                <div className="v">
                  {b.rate != null ? `$${b.rate.toFixed(2)}` : "—"}
                  <i>/1%</i>
                </div>
                <div>n={b.n}</div>
              </div>
            ))}
          </div>
          <p className="foot-note zh" style={{ marginTop: 8 }}>
            格子顏色越深，代表那個時段的額度消耗總量越多；下方是各 6 小時時段的匯率（至少 {dash.method.minN} 筆才顯示）。
            純描述——「某個時段比較傷額度」需要更多月的資料才能驗證，這裡不下結論。
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
                {dash.dataSince ? `（資料自 ${dash.dataSince.slice(0, 10)} 起，共 ${Math.max(1, Math.round((Date.now() - new Date(dash.dataSince).getTime()) / 86_400_000))} 天）` : ""}
                {" · 座位 "}{dash.selected.accountUuid?.slice(0, 8) ?? "?"}
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
