// =============================================================================
// M6 (D65-3) 圖卡：1200×630 自足 SVG → PNG。
// 貼上網的圖要自己站得住：標題、修剪匯率＋CI、膠囊圖、逐項檢查、方法一行、
// 產生日期都在圖裡。字型退回系統字（canvas 拿不到 Google Fonts）。
// 幾何規則同半年頁：膠囊高＝CI、沒 CI 不畫帶（D42）。
// =============================================================================

import { planShort, planShortAll } from "./planLabel";


interface CardMonth {
  month: string;
  samples: number;
  rate: number | null;
  ci: [number, number] | null;
  qualified: boolean;
  /** v1.9.4（D99）：這一列屬於哪個方案——同一個月換過方案就有兩列；線在方案切點斷開。 */
  plan?: string;
  planLabel?: string;
}
interface CardGate {
  key?: string;
  label: string;
  pass: boolean;
  detail: string;
  /** v1.9.5：圖卡一行放不下長句時用的短句（後端給；沒有就用 detail）。 */
  short?: string;
}
export interface CardInput {
  fx5: CardMonth[];
  verdict: {
    declare: boolean;
    gates: CardGate[];
    /** v1.9.4（D99 Q2）：不同方案（或方案未知）的月份不能比——false 時沒有 momPct，改印 sentence。 */
    comparable?: boolean;
    /** 不能比的原因：plan_changed｜plan_unknown_before｜plan_unknown｜plan_unknown_history｜all_seats。 */
    reason?: string;
    sentence?: string;
    /** prev 與 cur 相隔幾個月；>1 時不能寫「較上月」。 */
    monthsApart?: number;
    /** 兩個月共同的方案鍵（可比時）。 */
    plan?: string;
    curMonth?: string;
    prevMonth?: string;
    momPct?: number;
    walletPct?: number;
    p5h?: number | null;
  };
  /** declarePremise：宣告句的前提，後端 `method.declarePremise` 唯一一份（趨勢頁、圖卡、證據報告共用）。 */
  method: { trim: [number, number]; bootstrapReps: number; permReps: number; alpha: number; declarePremise?: string };
  /** 帳號名；全量視角由呼叫端傳 ALL_SEATS_LABEL。v1.8.13（D91 T-14）：卡上前綴由「座位」改「帳號」
   *  （跟頂欄一致；全量時「座位 全部帳號」讀不通）。 */
  seatLabel: string;
  periodLabel: string;
  generatedAt: string;
  appVersion?: string;
}

const FONT = "'Segoe UI', 'Noto Sans TC', 'Microsoft JhengHei', system-ui, sans-serif";
const C = {
  accent: "#4E7ECF",
  accentDeep: "#2E5FB7",
  accentInk: "#1F4D9F",
  accentSoft: "#E3EBF8",
  red: "#C13A30",
  ink: "#1A1D23",
  ink2: "#5E6470",
  ink3: "#969EAC",
  line: "#E1E5EB",
  bg: "#FFFFFF",
  faint: "#F0F5FC",
};

// 審查 23r S1：左欄文字以前按「字數」截斷——英數字多的句子提早被切掉關鍵字（「不能跨比」），中文多的又壓進右邊圖表。
// 改成量寬度：瀏覽器裡用 canvas measureText（跟 SVG 同一個字族），量不到時用 CJK 1em／其他 0.56em 估。
// 左欄右緣留在 510（右邊圖表的 y 軸刻度從約 520 起）。
const LEFT_RIGHT = 510;
let measureCtx: CanvasRenderingContext2D | null | undefined;
function textWidth(text: string, px: number, weight = 400): number {
  if (measureCtx === undefined) {
    measureCtx = typeof document !== "undefined" ? document.createElement("canvas").getContext("2d") : null;
  }
  if (measureCtx) {
    measureCtx.font = `${weight} ${px}px ${FONT}`;
    return measureCtx.measureText(text).width;
  }
  let w = 0;
  for (const ch of text) w += /[\u2E80-\uFFEF]/.test(ch) ? px : px * 0.56;
  return w;
}
/** 依寬度斷行，最多 maxLines 行；放不下時最後一行以「…」收尾。 */
function wrapText(text: string, maxW: number, px: number, maxLines: number, weight = 400): string[] {
  const lines: string[] = [];
  let cur = "";
  for (const ch of text) {
    if (textWidth(cur + ch, px, weight) > maxW) {
      // 盡量在標點或空白後面斷（最後 10 個字內有的話），不要把「不明」切成「不／明」
      const cut = Math.max(...["，", "、", "；", "（", " "].map((p) => cur.lastIndexOf(p)));
      const at = cut >= 0 && cur.length - cut <= 10 ? (cur[cut] === "（" ? cut : cut + 1) : cur.length;
      lines.push(cur.slice(0, at));
      cur = (cur.slice(at) + ch).replace(/^\s+/, "");
      if (lines.length === maxLines) break;
    } else cur += ch;
  }
  if (lines.length < maxLines) {
    if (cur) lines.push(cur);
    return lines;
  }
  // 超過 maxLines：最後一行收成「…」
  let last = lines[maxLines - 1];
  while (last && textWidth(last + "…", px, weight) > maxW) last = last.slice(0, -1);
  lines[maxLines - 1] = last + "…";
  return lines;
}
/** 單行，超寬就截（給閘的說明用）。 */
function fitLine(text: string, maxW: number, px: number, weight = 400): string {
  return wrapText(text, maxW, px, 1, weight)[0] ?? "";
}

const esc = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

export function evidenceCardSvg(d: CardInput): string {
  const W = 1200, H = 630;
  const months = d.fx5;
  const cur = [...months].reverse().find((m) => m.rate != null);
  const v = d.verdict;
  // 審查 23r S2：全部帳號視角不合匯率（各帳號的池可能不一樣大）——圖卡不寫「還沒有配對樣本」「卡在同一個方案」。
  const allSeats = v.reason === "all_seats";

  // ---- right: capsule chart (x 560..1150, y 90..430) ----
  const cx0 = 560, cw = 590, cy0 = 96, ch = 330;
  const vals: number[] = [];
  months.forEach((m) => {
    if (m.rate != null) vals.push(m.rate);
    if (m.ci) vals.push(m.ci[0], m.ci[1]);
  });
  let chart = "";
  if (vals.length > 0) {
    const dMin = Math.min(...vals), dMax = Math.max(...vals);
    const pad = Math.max((dMax - dMin) * 0.15, dMax * 0.06);
    const lo = Math.max(0, dMin - pad), hi = dMax + pad;
    const y = (val: number) => cy0 + ch * (1 - (val - lo) / (hi - lo));
    const pitch = cw / months.length;
    const cxOf = (i: number) => cx0 + pitch * (i + 0.5);
    const capW = Math.min(90, pitch * 0.5);
    const step = hi - lo > 3 ? 1 : hi - lo > 0.9 ? 0.5 : 0.2;
    for (let g = Math.ceil(lo / step) * step; g < hi - 0.001; g += step) {
      chart += `<line x1="${cx0}" y1="${y(g)}" x2="${cx0 + cw}" y2="${y(g)}" stroke="${C.line}" stroke-dasharray="3 6"/>
        <text x="${cx0 - 10}" y="${y(g) + 5}" text-anchor="end" font-size="14" fill="${C.ink3}">${g.toFixed(1)}</text>`;
    }
    chart += `<text x="${cx0 - 10}" y="${cy0 - 12}" text-anchor="end" font-size="11" letter-spacing="1.5" fill="${C.ink3}">USD/1%</text>`;
    // v1.9.4（D99 A3）：折線在方案切點斷開（相鄰列 plan 不同＝另一個池），切點畫一條虛線。
    let pts = "";
    let lines = "";
    const flush = () => {
      if (pts.trim().includes(" ")) lines += `<polyline points="${pts.trim()}" fill="none" stroke="${C.accentDeep}" stroke-width="3" stroke-linejoin="round" stroke-linecap="round"/>`;
      pts = "";
    };
    months.forEach((m, i) => {
      if (i > 0 && (m.plan ?? "") !== (months[i - 1].plan ?? "")) {
        flush();
        const xb = cx0 + pitch * i;
        chart += `<line x1="${xb}" y1="${cy0}" x2="${xb}" y2="${cy0 + ch}" stroke="${C.ink3}" stroke-dasharray="4 4"/>`;
      }
      const x = cxOf(i) - capW / 2;
      if (m.rate == null) {
        chart += `<rect x="${x}" y="${cy0}" width="${capW}" height="${ch}" rx="16" fill="none" stroke="${C.line}" stroke-width="2" stroke-dasharray="5 6"/>
          <text x="${cxOf(i)}" y="${cy0 + ch + 30}" text-anchor="middle" font-size="16" fill="${C.ink3}">${m.month.slice(5)} 月</text>
          <text x="${cxOf(i)}" y="${cy0 + ch + 50}" text-anchor="middle" font-size="12" fill="${C.ink3}">無真值</text>`;
        return;
      }
      const isNow = m === cur;
      if (m.ci) {
        const yT = y(m.ci[1]), yB = y(m.ci[0]);
        chart += `<rect x="${x}" y="${yT}" width="${capW}" height="${yB - yT}" rx="${Math.min(16, (yB - yT) / 2)}" fill="${C.accentSoft}"/>
          ${isNow ? `<rect x="${x}" y="${yT}" width="${capW}" height="${yB - yT}" rx="${Math.min(16, (yB - yT) / 2)}" fill="none" stroke="${C.accent}" stroke-width="2.5"/>` : ""}`;
      } else {
        chart += `<line x1="${cxOf(i)}" y1="${cy0}" x2="${cxOf(i)}" y2="${cy0 + ch}" stroke="${C.line}" stroke-dasharray="3 6"/>`;
      }
      pts += `${cxOf(i)},${y(m.rate)} `;
      chart += `<text x="${cxOf(i)}" y="${(m.ci ? y(m.ci[1]) : y(m.rate)) - 14}" text-anchor="middle" font-size="${isNow ? 22 : 16}" font-weight="700" fill="${isNow ? C.accentInk : C.ink2}">${m.rate.toFixed(2)}</text>
        <text x="${cxOf(i)}" y="${cy0 + ch + 30}" text-anchor="middle" font-size="16" font-weight="${isNow ? 700 : 500}" fill="${isNow ? C.ink : C.ink2}">${m.month.slice(5)} 月</text>
        <text x="${cxOf(i)}" y="${cy0 + ch + 50}" text-anchor="middle" font-size="12" fill="${C.ink3}">n=${m.samples}${m.qualified || months.length > 8 ? "" : " · 校準中"}${m.planLabel && months.length <= 6 ? ` · ${esc(planShort(m.planLabel))}` : ""}</text>`;
    });
    flush();
    chart += lines;
    months.forEach((m, i) => {
      if (m.rate != null)
        chart += `<circle cx="${cxOf(i)}" cy="${y(m.rate)}" r="5" fill="#fff" stroke="${C.accentDeep}" stroke-width="3"/>`;
    });
  } else {
    chart += `<text x="${cx0 + cw / 2}" y="${cy0 + ch / 2}" text-anchor="middle" font-size="18" fill="${C.ink3}">${allSeats ? "全部帳號不合併匯率，請選單一帳號" : "還沒有配對樣本"}</text>`;
  }

  // ---- left: headline block ----
  const rateTxt = cur?.rate != null ? `$${cur.rate.toFixed(2)}` : "—";
  const ciTxt = cur?.ci ? `95% CI $${cur.ci[0].toFixed(2)}–$${cur.ci[1].toFixed(2)} · n=${cur.samples}` : cur ? `校準中 · n=${cur.samples}` : "";
  const failed = v.gates.find((g) => !g.pass);
  const verdictLine = v.declare
    ? `${v.curMonth?.slice(5)} 月比 ${v.prevMonth?.slice(5)} 月${(v.walletPct ?? 0) > 0 ? "貴" : "便宜"} ${Math.abs(v.walletPct ?? 0).toFixed(1)}%（每項檢查都過）`
    : allSeats
      ? "全部帳號：不合併匯率、不判斷縮水"
      : `不宣告縮水或放寬 · 卡在${v.reason?.startsWith("plan_unknown") ? "方案未知" : failed?.label ?? "檢定"}`;
  // v1.9.4（D99 Q2）：不同方案不能比——不印「較上月」，改印原因；隔著別的方案比的寫「較 YYYY-MM」。
  const comparable = v.comparable !== false;
  const vs = (v.monthsApart ?? 1) > 1 && v.prevMonth ? `較 ${v.prevMonth} ` : "較上月";
  // 比較的月份不是大數字那一列時（例：本月剛換方案、還在校準），寫清楚是哪個月比的。
  const lead = cur && v.curMonth && (v.curMonth !== cur.month || (v.plan != null && cur.plan != null && v.plan !== cur.plan)) ? `${v.curMonth.slice(5)} 月的` : "";
  const momLine =
    comparable && v.momPct != null && v.walletPct != null
      ? `${lead}匯率${vs}${v.momPct < 0 ? "跌" : "升"} ${Math.abs(v.momPct).toFixed(1)}% —— 同樣的工作要${v.walletPct > 0 ? "多" : "少"}花 ${Math.abs(v.walletPct).toFixed(1)}% 的額度${v.declare ? "" : "（未過檢定，當雜訊看）"}`
      : allSeats
        ? "各帳號的額度池可能不一樣大，合起來的匯率不代表任何一個帳號；請選單一帳號再匯出。"
        : !comparable && v.sentence
          ? planShortAll(v.sentence)
          : "";
  // 主句量寬度斷兩行（第 296、315 行），不截關鍵字、不壓進圖表
  const momLines = momLine ? wrapText(momLine, LEFT_RIGHT - 62, 14, 2) : [];
  // 不能比時只有「同一個方案」那一道，說明就是上面那句——只印標籤，不重印（跟頁面一致）
  const gateList = allSeats ? [] : v.gates;
  const onlyPlanGate = !comparable && gateList.every((g) => g.key === "plan");
  let gates = "";
  gateList.forEach((g, i) => {
    const gy = 372 + i * 30;
    // 一行只放得下 ~25 個中文字：有 short（後端給的短句）就用它，趨勢頁仍用長句
    const detail = onlyPlanGate ? "" : fitLine(g.key === "plan" ? planShortAll(g.detail) : g.short ?? g.detail, LEFT_RIGHT - 170, 13.5);
    gates += `<rect x="60" y="${gy - 11}" width="12" height="12" rx="3" fill="${g.pass ? C.accent : C.line}"/>
      <text x="82" y="${gy}" font-size="15" font-weight="700" fill="${g.pass ? C.accentInk : C.ink3}">${esc(g.label)}</text>
      ${detail ? `<text class="gate-detail" x="170" y="${gy}" font-size="13.5" fill="${C.ink2}">${esc(detail)}</text>` : ""}`;
  });

  const m = d.method;
  const methodLine = `修剪 ${Math.round(m.trim[0] * 100)}/${Math.round((1 - m.trim[1]) * 100)}% · bootstrap ${m.bootstrapReps} · 置換 ${m.permReps} · α=${m.alpha} · 區間有時序相關，p 偏樂觀${v.declare && m.declarePremise ? ` · ${m.declarePremise}` : ""}`;

  return `<svg xmlns="http://www.w3.org/2000/svg" width="${W}" height="${H}" viewBox="0 0 ${W} ${H}" font-family="${FONT}">
  <rect width="${W}" height="${H}" fill="${C.bg}"/>
  <rect x="0" y="0" width="${W}" height="6" fill="${C.accent}"/>
  <g>
    ${[0, 1, 2].flatMap((r) => [0, 1, 2].map((c) => `<rect x="${60 + c * 10}" y="${48 + r * 10}" width="7.5" height="7.5" rx="2" fill="${r === 2 && c === 2 ? C.ink : C.accent}"/>`)).join("")}
    <text x="100" y="66" font-size="20" font-weight="800" fill="${C.ink}">Claude 訂閱額度匯率</text>
  <text x="${W - 60}" y="66" text-anchor="end" font-size="13" fill="${C.ink3}">帳號 ${esc(d.seatLabel)}</text>
    <text x="100" y="86" font-size="11" letter-spacing="2.2" font-weight="600" fill="${C.ink3}">EXCHANGE RATE · USD PER 1% OF 5H QUOTA · ${esc(d.periodLabel)}</text>
  </g>
  <text x="60" y="180" font-size="88" font-weight="700" letter-spacing="-2" fill="${C.accentDeep}">${rateTxt}</text>
  <text x="${60 + rateTxt.length * 50}" y="180" font-size="22" font-weight="600" letter-spacing="1" fill="${C.ink3}">/ 1%</text>
  <text x="62" y="212" font-size="15" fill="${C.ink2}">${esc(ciTxt)}${cur ? ` · ${cur.month.slice(0, 4)} 年 ${cur.month.slice(5)} 月` : ""}</text>
  <rect x="60" y="244" width="4" height="26" rx="2" fill="${v.declare ? C.red : C.accent}"/>
  <text x="76" y="264" font-size="21" font-weight="700" fill="${v.declare ? C.red : C.ink}">${esc(verdictLine)}</text>
  ${momLines.map((l, i) => `<text class="mom-line" x="62" y="${296 + i * 19}" font-size="14" fill="${C.ink2}">${esc(l)}</text>`).join("")}
  ${gateList.length > 0 ? `<text x="62" y="338" font-size="11" letter-spacing="2" font-weight="700" fill="${C.ink3}">縮水判定 · 逐項檢查</text>` : ""}
  ${gates}
  ${chart}
  <line x1="60" y1="548" x2="${W - 60}" y2="548" stroke="${C.line}"/>
  <text x="60" y="574" font-size="12.5" fill="${C.ink3}">${esc(methodLine)}</text>
  <text x="60" y="596" font-size="12.5" fill="${C.ink3}">分子＝本機所有入口 Claude 記錄 API 計價 · 分母＝訂閱額度真值 Δ% · 網頁端用量不在分子（已估佔比）</text>
  <text x="${W - 60}" y="596" text-anchor="end" font-size="12.5" fill="${C.ink3}">Claude Usage Monitor${d.appVersion ? ` v${d.appVersion}` : ""} · ${esc(d.generatedAt)}</text>
</svg>`;
}

/** SVG 字串 → PNG base64（無 data: 前綴）。scale 2 = 2400×1260。 */
export function svgToPngBase64(svg: string, scale = 2): Promise<string> {
  return new Promise((resolve, reject) => {
    const img = new Image();
    img.onload = () => {
      try {
        const canvas = document.createElement("canvas");
        canvas.width = img.width * scale;
        canvas.height = img.height * scale;
        const ctx = canvas.getContext("2d");
        if (!ctx) throw new Error("canvas 2d context unavailable");
        ctx.scale(scale, scale);
        ctx.drawImage(img, 0, 0);
        const url = canvas.toDataURL("image/png");
        resolve(url.replace(/^data:image\/png;base64,/, ""));
      } catch (e) {
        reject(e);
      }
    };
    img.onerror = () => reject(new Error("SVG 轉圖失敗"));
    img.src = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
  });
}

export function svgDataUri(svg: string): string {
  return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
}
