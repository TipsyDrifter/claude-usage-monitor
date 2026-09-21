import {
  BurnStat,
  EtaStat,
  expiredTitle,
  pctText,
  pctValue,
  resetText,
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

function Track({ row, needle, thin }: { row: FaceRow; needle: number; thin?: boolean }) {
  return (
    <div className={`track${thin ? " m-track" : ""}`}>
      <div className={`fill f-${row.tone}`} style={{ width: `${pctValue(row)}%` }} />
      <span className="needle" style={{ left: `${needle}%` }} />
      {row.pace != null && !row.expired && <span className="pace" style={{ left: `${row.pace}%` }} />}
    </div>
  );
}

const ROW_LABEL: Record<FaceRow["id"], string> = { "5H": "5h 窗口", "7D": "週 · 全模型", FABLE: "週 · Fable" };

function BarRow({ row, needle, mode }: { row: FaceRow; needle: number; mode: FaceProps["mode"] }) {
  const r = resetText(row, mode);
  return (
    <div title={row.expired ? expiredTitle(row.item) : row.item?.note ?? undefined}>
      <div className="btop">
        <span>{ROW_LABEL[row.id]}</span>
        <span className={`t-${row.tone}`}>
          {pctText(row)}%<em> · {r.when} {r.verb}</em>
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
      <div className={`g ${v} w-micro s-${s5.tone}`} style={ghostStyle} title={s5.item?.note ?? undefined}>
        {s5.tone === "crit" && <span className="m-dot" />}
        <span className="m-label">5H</span>
        <span className={`m-num t-${s5.tone}`}>{pctText(s5)}%</span>
        <Track row={s5} needle={p.needle} thin />
        <span className="m-mini">
          7D <b className={`t-${s7.tone}`}>{pctText(s7)}</b> · F <b className={`t-${sf.tone}`}>{pctText(sf)}</b>
        </span>
        <span className="m-reset"><b>{r5.when}</b></span>
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
      <Chip tone={worst} />
    </div>
  );
  const headline = (
    <div className="w-headline" title={s5.expired ? expiredTitle(s5.item) : undefined}>
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
      <BarRow row={s5} needle={p.needle} mode={p.mode} />
      <BarRow row={s7} needle={p.needle} mode={p.mode} />
      <BarRow row={sf} needle={p.needle} mode={p.mode} />
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
