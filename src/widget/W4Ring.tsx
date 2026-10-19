import { useId } from "react";
import {
  BurnStat,
  EtaStat,
  budgetGeom,
  withBudget,
  withMicroBudget,
  NoticeMark,
  pctText,
  pctValue,
  resetText,
  rowTitle,
  stampTitle,
  withExtra,
  type FaceProps,
  type FaceRow,
  type FaceTone,
} from "./shared";
import "./w4.css";

// =============================================================================
// W4 · 環形儀表（v1.8.1，D82；原型 prototypes/widget-W4-ring.html →
// theme-T1-compare.html 三環版）。弧長讀餘光；5H 大環帶本窗時間刻度（＝W5 的 ▼）；
// 環上 70% 處畫留意值刻度（＝W5 的針）；低於留意值時環穿灰衣（象牙灰，不上彩）。
// 微型條照主人 2026-09-20 圈選：左＝5h 環＋數字，右邊兩行（上 7d／fable，下重置）。
// =============================================================================

const T = {
  dark: { safe: "#cfcbc1", warn: "#e8a33d", crit: "#f0594c", txt: "#eef0f4", faint: "rgba(238,240,244,.36)", track: "rgba(255,255,255,.10)", trackCrit: "rgba(240,89,76,.17)", tick: "rgba(238,240,244,.6)", band: "rgba(238,240,244,.3)", hatch: "rgba(21,23,29,.66)" },
  light: { safe: "#6b7280", warn: "#b97705", crit: "#dc3a44", txt: "#1b2027", faint: "rgba(27,32,39,.4)", track: "rgba(23,32,44,.13)", trackCrit: "rgba(220,58,68,.16)", tick: "rgba(27,32,39,.55)", band: "rgba(23,32,44,.26)", hatch: "rgba(255,255,255,.72)" },
};
/** v1.10（D101）今天的份的淡弧只有環身寬度的這麼多（置中）——跟填色同寬會被讀成「第二段填色」。 */
const BAND_WIDTH = 0.42;
const WORST: Record<FaceTone, number> = { safe: 0, warn: 1, crit: 2 };
const worstOf = (rows: FaceRow[]): FaceTone =>
  rows.reduce<FaceTone>((a, r) => (WORST[r.tone] > WORST[a] ? r.tone : a), "safe");

function Ring({
  dark, d, sw, row, needle, val, lab, vs,
}: {
  dark: boolean; d: number; sw: number; row: FaceRow; needle: number; val?: string; lab?: string; vs?: number;
}) {
  const t = dark ? T.dark : T.light;
  const pct = pctValue(row);
  const r = (d - sw) / 2, c = d / 2;
  const col = t[row.tone];
  const trk = row.tone === "crit" ? t.trackCrit : t.track;
  const showTick = row.pace != null && !row.expired && d >= 56;
  const vy = vs ? c + vs * 0.34 : 0;
  // v1.10（D101 原型 D）：今天的份。微型條的 12px 小環畫不出來（跟留意值刻度一樣 d<40 不畫）。
  const g = budgetGeom(pct, d >= 40 && !row.expired && !row.na ? row.budget : null);
  const hatchId = `w4h${useId().replace(/:/g, "")}`;
  return (
    <svg width={d} height={d} viewBox={`0 0 ${d} ${d}`} aria-hidden="true">
      {g.over && (
        <defs>
          <pattern id={hatchId} patternUnits="userSpaceOnUse" width={3.2} height={3.2} patternTransform="rotate(45)">
            <rect x={0} y={0} width={1.6} height={3.2} fill={t.hatch} />
          </pattern>
        </defs>
      )}
      <circle cx={c} cy={c} r={r} fill="none" stroke={trk} strokeWidth={sw} />
      {g.band && (
        <circle
          className="budget-band" cx={c} cy={c} r={r} fill="none" stroke={t.band} strokeWidth={sw * BAND_WIDTH}
          pathLength={100} strokeDasharray={`${g.band[1] - g.band[0]} 100`} strokeDashoffset={-g.band[0]}
          transform={`rotate(-90 ${c} ${c})`}
        />
      )}
      {d >= 40 && (
        <g transform={`rotate(${needle * 3.6} ${c} ${c})`}>
          <line x1={c} y1={c - r - sw / 2 - 1} x2={c} y2={c - r + sw / 2 + 1} stroke="rgba(240,89,76,.55)" strokeWidth={1} />
        </g>
      )}
      {showTick && (
        <g transform={`rotate(${(row.pace as number) * 3.6} ${c} ${c})`}>
          <line x1={c} y1={c - r + sw / 2 + 2.5} x2={c} y2={c - r + sw / 2 + 7} stroke={t.tick} strokeWidth={1.8} strokeLinecap="round" />
        </g>
      )}
      {/* pct 0 不畫弧：round linecap 在長度 0 時會留一顆點，看起來像 1% */}
      {row.tone === "crit" && pct > 0 && (
        <circle
          className="pulse" cx={c} cy={c} r={r} fill="none" stroke={col} strokeWidth={sw}
          pathLength={100} strokeDasharray={`${pct} 100`} strokeLinecap="round"
          transform={`rotate(-90 ${c} ${c})`} style={{ filter: `blur(${Math.max(2.5, sw * 0.55)}px)` }}
        />
      )}
      {pct > 0 && (
        <circle
          className="arc" cx={c} cy={c} r={r} fill="none" stroke={col} strokeWidth={sw}
          pathLength={100} strokeDasharray={`${pct} 100`} strokeLinecap="round" transform={`rotate(-90 ${c} ${c})`}
        />
      )}
      {g.over && (
        <circle
          className="budget-over" cx={c} cy={c} r={r} fill="none" stroke={`url(#${hatchId})`} strokeWidth={sw}
          pathLength={100} strokeDasharray={`${g.over[1] - g.over[0]} 100`} strokeDashoffset={-g.over[0]}
          transform={`rotate(-90 ${c} ${c})`}
        />
      )}
      {val && vs && (
        <>
          <text x={c} y={vy} textAnchor="middle" fontSize={vs} fontWeight={700} fill={row.tone === "safe" ? t.txt : col} style={{ letterSpacing: "-0.01em" }}>
            {val}
          </text>
          <text x={c} y={vy + vs * 0.78} textAnchor="middle" fontSize={Math.max(6.5, vs * 0.5)} fontWeight={700} fill={t.faint} style={{ letterSpacing: "0.14em" }}>
            {lab}
          </text>
        </>
      )}
    </svg>
  );
}

function Chip({ tone, dark }: { tone: FaceTone; dark: boolean }) {
  if (tone === "safe") return null;
  const col = (dark ? T.dark : T.light)[tone];
  return (
    <span className="chip" style={{ color: col, background: `${col}22` }}>
      <i style={{ background: col }} />
      {tone === "warn" ? "注意" : "警戒"}
    </span>
  );
}

export function W4Face(p: FaceProps) {
  const th = p.dark ? "t-dark" : "t-light";
  const t = p.dark ? T.dark : T.light;
  const [s5, s7, sf] = p.rows;
  const worst = worstOf(p.rows);
  const ghostStyle = p.ghost ? { opacity: 0.62 } : undefined;
  const col = (row: FaceRow) => (row.tone === "safe" ? t.txt : t[row.tone]);
  const r5 = resetText(s5, p.mode);

  if (p.density === "micro") {
    return (
      <div
        className={`r ${th} w-micro s-${worst}`}
        style={ghostStyle}
        title={withMicroBudget(s5.na ? rowTitle(s5, p.extraActive) : withExtra(stampTitle(s5.item), s5.item, p.extraActive), p.rows)}
      >
        <Ring dark={p.dark} d={22} sw={3.6} row={s5} needle={p.needle} />
        <NoticeMark notice={p.notice} />
        <span className="m-tag">5h</span>
        <span className="m-val" style={{ color: col(s5) }}>{pctText(s5)}%</span>
        <span className="m-flex" />
        <div className="m-right">
          <div className="m-row">
            <span className="m-tag">7d</span>
            <Ring dark={p.dark} d={12} sw={2.4} row={s7} needle={p.needle} />
            <span className="m-val2" style={{ color: col(s7) }}>{pctText(s7)}</span>
            <span className="m-tag" style={{ marginLeft: 5 }}>F</span>
            <Ring dark={p.dark} d={12} sw={2.4} row={sf} needle={p.needle} />
            <span className="m-val2" style={{ color: col(sf) }}>{pctText(sf)}</span>
          </div>
          <div className="m-row m-sub">⟳ {r5.when} {r5.verb}</div>
        </div>
      </div>
    );
  }

  const head = (
    <div className="c-head">
      <button className="brand" title={p.canCycleSeat ? "點一下切換帳號" : undefined} onClick={p.canCycleSeat ? p.onCycleSeat : undefined}>
        {p.acctLabel ?? "USAGE"}
      </button>
      {p.ago && <span className="ago">{p.ago}</span>}
      <NoticeMark notice={p.notice} />
      <Chip tone={worst} dark={p.dark} />
    </div>
  );
  // v1.9.5（D100 E2）：此方案沒有這條——環中「—」（不帶 %）、環下寫「此方案沒有這條」。
  const ringVal = (row: FaceRow) => (row.na ? "—" : `${pctText(row)}%`);
  // 週額度兩環以前沒有一般 title——沿用：只有已重置／此方案沒有／滿格＋超額付費時才給。
  // v1.10：今天的份那句也進來（有線時才有）。
  const sideTitle = (row: FaceRow) =>
    row.expired || row.na ? rowTitle(row, p.extraActive) : withBudget(withExtra(undefined, row.item, p.extraActive), row);
  const rings = (big: number) => (
    <>
      <div className="rcol" title={rowTitle(s5, p.extraActive)}>
        <Ring dark={p.dark} d={big} sw={8} row={s5} needle={p.needle} val={ringVal(s5)} lab="5H" vs={16} />
        <div className="rmeta">{r5.when} {r5.verb}</div>
      </div>
      <div className="rcol" title={sideTitle(s7)}>
        <Ring dark={p.dark} d={66} sw={6.5} row={s7} needle={p.needle} val={ringVal(s7)} lab="7D" vs={12} />
        <div className="rmeta">{resetText(s7, p.mode).when}{s7.expired ? " 已重置" : ""}</div>
      </div>
      <div className="rcol" title={sideTitle(sf)}>
        <Ring dark={p.dark} d={66} sw={6.5} row={sf} needle={p.needle} val={ringVal(sf)} lab="FABLE" vs={12} />
        <div className="rmeta">{resetText(sf, p.mode).when}{sf.expired ? " 已重置" : ""}</div>
      </div>
    </>
  );

  if (p.density === "card") {
    return (
      <div className={`r ${th} w-card s-${worst}`} style={ghostStyle}>
        {head}
        <div className="c-rings">{rings(96)}</div>
      </div>
    );
  }
  return (
    <div className={`r ${th} w-panel s-${worst}`} style={ghostStyle}>
      {head}
      <div className="p-rings">{rings(104)}</div>
      <div className="p-hr" />
      <div className="stats">
        <BurnStat burn={p.burn} />
        <div className="p-vdiv" />
        <EtaStat burn={p.burn} mode={p.mode} />
      </div>
      <div className="p-hr" />
      <div className={`fresh${p.stale ? " stale" : ""}`}>{p.freshNote}</div>
    </div>
  );
}
