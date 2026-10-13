import {
  BurnStat,
  EtaStat,
  budgetGeom,
  withMicroBudget,
  NA_TEXT,
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
import "./w1.css";

// =============================================================================
// W1 · 毛玻璃光暈（v1.8.1，D82；原型 prototypes/widget-W1-glass-revival.html →
// theme-T1-compare.html 三條額度版）。D35 主人稱讚的資產：留意態靜態光暈、
// 警戒態呼吸光暈。深色桌布用深玻璃、淺色用白玻璃（跟系統）。
// 資訊架構與 W5 一致：5H 大數字＋三條針軌、帳號行、面板兩格＋新鮮度戳。
// 「懸浮窗變色」關掉時 tone 已被 Widget.tsx 壓成 safe，光暈自然不出現。
// =============================================================================

const WORST: Record<FaceTone, number> = { safe: 0, warn: 1, crit: 2 };
const worstOf = (rows: FaceRow[]): FaceTone =>
  rows.reduce<FaceTone>((a, r) => (WORST[r.tone] > WORST[a] ? r.tone : a), "safe");

/** v1.10（D101 原型 D）：今天的份——目前到線那段淡色（.budget-band）、越線後超出那段斜紋（.budget-over）。 */
function Track({ row, needle, thin }: { row: FaceRow; needle: number; thin?: boolean }) {
  const g = budgetGeom(pctValue(row), row.expired || row.na ? null : row.budget);
  return (
    <div className={`track${thin ? " m-track" : ""}`}>
      {g.band && <i className="budget-band" style={{ left: `${g.band[0]}%`, width: `${g.band[1] - g.band[0]}%` }} />}
      <div className={`fill f-${row.tone}`} style={{ width: `${pctValue(row)}%` }} />
      {g.over && <i className="budget-over" style={{ left: `${g.over[0]}%`, width: `${g.over[1] - g.over[0]}%` }} />}
      <span className="needle" style={{ left: `${needle}%` }} />
      {row.pace != null && !row.expired && <span className="pace" style={{ left: `${row.pace}%` }} />}
    </div>
  );
}

const ROW_LABEL: Record<FaceRow["id"], string> = { "5H": "5h 窗口", "7D": "週 · 全模型", FABLE: "週 · Fable" };

function BarRow({ row, needle, mode, extraActive }: { row: FaceRow; needle: number; mode: FaceProps["mode"]; extraActive: boolean }) {
  const r = resetText(row, mode);
  return (
    <div title={rowTitle(row, extraActive)}>
      <div className="btop">
        <span>{ROW_LABEL[row.id]}</span>
        <span className={`t-${row.tone}`}>
          {/* v1.9.5（D100 E2）：此方案沒有這條——「—」不帶 %、沒有重置時刻 */}
          {row.na ? (
            <>
              —<em> · {NA_TEXT}</em>
            </>
          ) : (
            <>
              {pctText(row)}%<em> · {r.when} {r.verb}</em>
            </>
          )}
        </span>
      </div>
      <Track row={row} needle={needle} />
    </div>
  );
}

function Chip({ tone }: { tone: FaceTone }) {
  if (tone === "safe") return null;
  return (
    <span className={`chip ${tone}`}>
      <span className="dot" />
      {tone === "warn" ? "接近上限" : "即將見底"}
    </span>
  );
}

export function W1Face(p: FaceProps) {
  const v = p.dark ? "dark" : "light";
  const [s5, s7, sf] = p.rows;
  const worst = worstOf(p.rows);
  const ghostStyle = p.ghost ? { opacity: 0.62 } : undefined;
  const r5 = resetText(s5, p.mode);

  if (p.density === "micro") {
    return (
      <div
        className={`g ${v} w-micro s-${s5.tone}`}
        style={ghostStyle}
        title={withMicroBudget(s5.na ? rowTitle(s5, p.extraActive) : withExtra(stampTitle(s5.item), s5.item, p.extraActive), p.rows)}
      >
        {s5.tone === "crit" && <span className="m-dot" />}
        <NoticeMark notice={p.notice} />
        <span className="m-label">5H</span>
        <span className={`m-num t-${s5.tone}`}>{pctText(s5)}%</span>
        <Track row={s5} needle={p.needle} thin />
        <span className="m-mini">
          7D <b className={`t-${s7.tone}`}>{pctText(s7)}</b> · F <b className={`t-${sf.tone}`}>{pctText(sf)}</b>
        </span>
        <span className="m-reset"><b>{s5.expired ? r5.verb : r5.when}</b></span>
      </div>
    );
  }

  const head = (
    <div className="w-head">
      <button
        className="w-plan"
        title={p.canCycleSeat ? "點一下切換帳號" : undefined}
        onClick={p.canCycleSeat ? p.onCycleSeat : undefined}
      >
        {p.acctLabel ?? "Claude"}
      </button>
      {p.ago && <span className="ago">{p.ago}</span>}
      <NoticeMark notice={p.notice} />
      <Chip tone={worst} />
    </div>
  );
  const headline = (
    <div className="w-headline" title={s5.expired || s5.na ? rowTitle(s5, p.extraActive) : withExtra(undefined, s5.item, p.extraActive)}>
      <div>
        <div className={`bignum t-${s5.tone}`}>
          <b>{pctText(s5)}</b>
          <i>%</i>
        </div>
        <span className="w-sub">used · 5h window</span>
      </div>
      <div className="w-reset">
        <b>{r5.when}</b>
        <span>{r5.verb === "已重置" ? "已重置" : "RESET"}</span>
      </div>
    </div>
  );
  const bars = (
    <div className="bars">
      <BarRow row={s5} needle={p.needle} mode={p.mode} extraActive={p.extraActive} />
      <BarRow row={s7} needle={p.needle} mode={p.mode} extraActive={p.extraActive} />
      <BarRow row={sf} needle={p.needle} mode={p.mode} extraActive={p.extraActive} />
    </div>
  );

  if (p.density === "card") {
    return (
      <div className={`g ${v} w-card s-${worst}`} style={ghostStyle}>
        {head}
        {headline}
        {bars}
      </div>
    );
  }
  return (
    <div className={`g ${v} w-panel s-${worst}`} style={ghostStyle}>
      {head}
      {headline}
      {bars}
      <div className="divider" />
      <div className="stats">
        <BurnStat burn={p.burn} />
        <EtaStat burn={p.burn} mode={p.mode} />
      </div>
      <div className={`fresh${p.stale ? " stale" : ""}`}>{p.freshNote}</div>
    </div>
  );
}
