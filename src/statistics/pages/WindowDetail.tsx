import { useEffect, useMemo, useRef, useState } from "react";
import { X } from "lucide-react";
import { cmd } from "@/lib/tauri";
import { seatLabel } from "@/lib/seatLabel";
import { pageCache } from "@/lib/pageCache";
import { hideTip, showTip } from "../Motion";
import "../h5.css";

// =============================================================================
// v1.8.12（D90，主人 2026-09-28）：歷史頁點一個窗口看詳情。
// 上面那張時間軸與明細表只給每窗一行摘要（起／末／峰值／樣本數），主人想知道
// 「這個窗裡面到底發生了什麼」——每一筆觀測（含哪筆是 Desktop、哪筆是 CLI）、
// 什麼時候撞的牆、這段時間本機在燒哪些模型／專案、API 計價多少。
// 幾何全由資料推（D42）：x＝窗內時間、y＝%；點的顏色＝來源；紅刻＝429。
// =============================================================================

export type Kind = "5h" | "7d" | "fable";

export interface WindowPick {
  seatId: string;
  kind: Kind;
  start: string;
  end: string;
}

interface Detail {
  seat: { id: string; accountUuid?: string | null; orgUuid?: string | null; email?: string | null; orgName?: string | null };
  kind: Kind;
  start: string;
  end: string;
  reset: string | null;
  peak: number;
  saturated: boolean;
  durationMinutes: number;
  avgBurnPerHour: number | null;
  samples: { at: string; percent: number; resetsAt: string | null; source: string }[];
  sourceCounts: Record<string, number>;
  anchors: string[];
  usage: {
    from: string;
    to: string;
    byModel: { model: string; label?: string; calls: number; outputTokens: number; usd: number }[];
    byProject: { project: string; calls: number; outputTokens: number }[];
    byEntrypoint: { entrypoint: string; calls: number; outputTokens: number }[];
    calls: number;
    outputTokens: number;
    usd: number;
    tenureSpans: number;
  };
}

const KIND_TAG: Record<Kind, string> = { "5h": "5H", "7d": "7D", fable: "FABLE" };
const KIND_HOURS: Record<Kind, number> = { "5h": 5, "7d": 168, fable: 168 };
// 來源＝點的顏色：探針深靛、CLI 快取靛藍、Desktop 紫（h5 的 --h5-violet）。
export const SRC_COLOR: Record<string, string> = {
  probe: "#2E5FB7",
  "cli-cache": "#4E7ECF",
  "desktop-history": "#7E6AC8",
};
export const SRC_LABEL: Record<string, string> = { probe: "探針", "cli-cache": "CLI 快取", "desktop-history": "Desktop", statusline: "statusline" };
const SRC_CLASS: Record<string, string> = { probe: "probe", "cli-cache": "cli", "desktop-history": "desktop" };
const ENTRYPOINT_LABEL: Record<string, string> = {
  "claude-desktop": "Claude Desktop",
  cli: "Claude Code",
  "local-agent": "雲端 session",
  unknown: "不明",
};

const pad2 = (n: number) => String(n).padStart(2, "0");
const hhmm = (iso: string) => {
  const d = new Date(iso);
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
};
const mdhm = (iso: string) => {
  const d = new Date(iso);
  return `${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ${hhmm(iso)}`;
};
const spanTxt = (mins: number) => {
  if (mins < 60) return `${mins} 分`;
  if (mins >= 48 * 60) return `${Math.floor(mins / 1440)} 天 ${Math.floor((mins % 1440) / 60)} 時`;
  return `${Math.floor(mins / 60)} 時 ${pad2(mins % 60)} 分`;
};
const fmtTokens = (n: number) =>
  n >= 1_000_000 ? `${(n / 1_000_000).toFixed(1)}M` : n >= 1000 ? `${Math.round(n / 1000)}k` : String(n);
const fmtUsd = (v: number) => (v >= 100 ? `$${Math.round(v).toLocaleString()}` : `$${v.toFixed(2)}`);
const projectShort = (p: string) => p.replace(/^C--/, "").split("-").slice(-2).join("/");

/* ---------------- 窗內階梯：每筆樣本一個點（顏色＝來源），紅刻＝429 ---------------- */
function WindowStairs({ d }: { d: Detail }) {
  const W = 1040, H = 170, L = 34, R = 12, T = 14, B = 24;
  const ms = (s: string) => new Date(s).getTime();
  const long = d.kind !== "5h";
  const t0 = ms(d.start);
  // 右緣：有重置時刻且在末筆之後就畫到重置（看得出窗還剩多少），否則到末筆。
  const tEnd = ms(d.end);
  const t1 = d.reset && ms(d.reset) > tEnd ? ms(d.reset) : tEnd;
  const span = Math.max(1, t1 - t0);
  const x = (t: number) => L + (W - L - R) * Math.min(1, Math.max(0, (t - t0) / span));
  const y = (p: number) => T + (H - T - B) * (1 - Math.min(100, Math.max(0, p)) / 100);
  const now = Date.now();

  let path = "";
  d.samples.forEach((s, i) => {
    const X = x(ms(s.at)), Y = y(s.percent);
    path += i === 0 ? `M${X.toFixed(1)},${Y.toFixed(1)}` : ` H${X.toFixed(1)} V${Y.toFixed(1)}`;
  });

  // 時間刻度：5h 每小時、7d 每天
  const ticks: number[] = [];
  const step = long ? 86400_000 : 3600_000;
  const first = Math.ceil(t0 / step) * step;
  for (let t = first; t <= t1; t += step) ticks.push(t);

  const svgRef = useRef<SVGSVGElement | null>(null);
  const [cross, setCross] = useState<{ x: number; y: number } | null>(null);
  const onMove = (ev: React.MouseEvent<SVGSVGElement>) => {
    const anchorAt = (ev.target as SVGElement).dataset?.anchor;
    if (anchorAt) {
      setCross(null);
      showTip(ev, `${mdhm(anchorAt)} 撞牆（429）`);
      return;
    }
    const el = svgRef.current;
    if (!el || d.samples.length === 0) return;
    const r = el.getBoundingClientRect();
    const vx = ((ev.clientX - r.left) / r.width) * W;
    let best = d.samples[0];
    let bestD = Infinity;
    for (const s of d.samples) {
      const dist = Math.abs(x(ms(s.at)) - vx);
      if (dist < bestD) {
        bestD = dist;
        best = s;
      }
    }
    setCross({ x: x(ms(best.at)), y: y(best.percent) });
    showTip(ev, `${mdhm(best.at)} ＝ ${Math.round(best.percent)}%（${SRC_LABEL[best.source] ?? best.source}）`);
  };
  const onLeave = () => {
    setCross(null);
    hideTip();
  };

  return (
    <svg
      ref={svgRef}
      width="100%"
      viewBox={`0 0 ${W} ${H}`}
      style={{ display: "block" }}
      onMouseMove={onMove}
      onMouseLeave={onLeave}
    >
      {[25, 50, 75].map((v) => (
        <g key={v}>
          <line x1={L} y1={y(v)} x2={W - R} y2={y(v)} stroke="#E1E5EB" strokeDasharray="2 5" />
          <text x={L - 6} y={y(v) + 3.5} textAnchor="end" fontSize={9} fill="#969EAC">{v}</text>
        </g>
      ))}
      <line x1={L} y1={y(100)} x2={W - R} y2={y(100)} stroke="#C13A30" strokeDasharray="3 4" />
      <text x={L - 6} y={y(100) + 3.5} textAnchor="end" fontSize={9} fill="#C13A30" fontWeight={700}>100</text>
      {ticks.map((t) => (
        <g key={t}>
          <line x1={x(t)} y1={T} x2={x(t)} y2={y(0)} stroke="#E1E5EB" strokeWidth={0.8} />
          <text x={x(t)} y={H - 8} textAnchor="middle" fontSize={8.6} fill="#969EAC">
            {long ? mdhm(new Date(t).toISOString()).slice(0, 5) : hhmm(new Date(t).toISOString())}
          </text>
        </g>
      ))}
      <line x1={L} y1={y(0)} x2={W - R} y2={y(0)} stroke="#D3D9E2" />
      {path && <path d={path} fill="none" stroke="#2E5FB7" strokeWidth={1.8} strokeLinejoin="round" />}
      {d.samples.map((s, i) => (
        <circle key={i} cx={x(ms(s.at))} cy={y(s.percent)} r={2.6} fill={SRC_COLOR[s.source] ?? "#969EAC"} />
      ))}
      {d.anchors.map((a, i) => (
        <g key={`a${i}`}>
          <line x1={x(ms(a))} y1={T - 4} x2={x(ms(a))} y2={T + 8} stroke="#C13A30" strokeWidth={2} />
          <rect x={x(ms(a)) - 5} y={T - 7} width={10} height={19} fill="transparent" data-anchor={a} />
        </g>
      ))}
      {now > t0 && now < t1 && (
        <>
          <line x1={x(now)} y1={T} x2={x(now)} y2={y(0)} stroke="#969EAC" strokeDasharray="2 3" />
          <text x={x(now)} y={T - 5} textAnchor="middle" fontSize={8.5} fill="#5E6470">現在</text>
        </>
      )}
      {cross && (
        <g pointerEvents="none">
          <line x1={cross.x} y1={T} x2={cross.x} y2={y(0)} stroke="#969EAC" strokeDasharray="2 2" />
          <circle cx={cross.x} cy={cross.y} r={4} fill="none" stroke="#1A1D23" strokeWidth={1} />
        </g>
      )}
      <text x={W - R} y={T - 5} textAnchor="end" fontSize={8.5} fill={d.reset ? "#969EAC" : "#C2C8D2"}>
        {d.reset ? `${long ? mdhm(d.reset) : hhmm(d.reset)} 重置` : "重置時間未知（只有 CLI 快取記得）"}
      </text>
    </svg>
  );
}

export function WindowDetail({
  pick,
  aliases,
  onClose,
}: {
  pick: WindowPick;
  aliases: Record<string, string>;
  onClose: () => void;
}) {
  const [d, setD] = useState<Detail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const ref = useRef<HTMLElement | null>(null);

  useEffect(() => {
    const key = `histwin|${pick.seatId}|${pick.kind}|${pick.start}|${pick.end}`;
    const cached = pageCache.get<Detail>(key);
    setD(cached);
    setError(null);
    cmd
      .getHistoryWindow(pick.seatId, pick.kind, pick.start, pick.end)
      .then((v) => {
        pageCache.set(key, v);
        setD(v as Detail);
      })
      .catch((e) => setError(String(e)));
  }, [pick.seatId, pick.kind, pick.start, pick.end]);

  // 從明細表點過來時卡片在上面——捲到看得見的位置；Esc 關閉。
  useEffect(() => {
    ref.current?.scrollIntoView({ behavior: "smooth", block: "nearest" });
  }, [pick.start, pick.end]);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const rows = useMemo(() => {
    if (!d) return [];
    return d.samples.map((s, i) => ({ ...s, delta: i === 0 ? null : s.percent - d.samples[i - 1].percent }));
  }, [d]);

  const tag = KIND_TAG[pick.kind];
  const long = pick.kind !== "5h";

  return (
    <article ref={ref} className="card g-full win-detail">
      <div className="card-head">
        <span className="card-title zh">窗口詳情</span>
        <span className="card-tag">
          {tag} WINDOW · {mdhm(pick.start)} → {long ? mdhm(d?.end ?? pick.end) : hhmm(d?.end ?? pick.end)}
        </span>
        <div className="right">
          {d && (
            <span className="lim-chip">{seatLabel({ accountUuid: d.seat.accountUuid ?? null, orgUuid: d.seat.orgUuid ?? null, email: d.seat.email, orgName: d.seat.orgName }, aliases)}</span>
          )}
          <button className="win-close" onClick={onClose} aria-label="關閉窗口詳情">
            <X size={11} />
            <span className="zh">關閉</span>
          </button>
        </div>
      </div>

      {error && <div className="warn-band zh" style={{ marginTop: 8 }}>讀取失敗:{error}</div>}
      {!d && !error && (
        <div style={{ marginTop: 8 }}>
          <div className="skel" style={{ width: "40%" }} />
          <div className="skel" style={{ width: "100%", height: 120 }} />
        </div>
      )}

      {d && (
        <>
          <div className="pstats" style={{ gap: 24, marginTop: 8 }}>
            <div className="pstat">
              <div className="k">峰值</div>
              <div className={`v${d.saturated ? " red" : ""}`}>{Math.round(d.peak)}<i>%</i></div>
            </div>
            <div className="pstat">
              <div className="k">觀測時長</div>
              <div className="v" style={{ fontSize: 17 }}>{spanTxt(d.durationMinutes)}</div>
            </div>
            <div className="pstat">
              <div className="k">樣本</div>
              <div className="v">{d.samples.length}<i>筆</i></div>
            </div>
            <div className="pstat">
              <div className="k">平均燒速</div>
              <div className="v">{d.avgBurnPerHour != null ? d.avgBurnPerHour.toFixed(1) : "—"}<i>%/h</i></div>
            </div>
            <div className="pstat">
              <div className="k">429</div>
              <div className={`v${d.anchors.length > 0 ? " red" : ""}`}>{d.anchors.length}<i>次</i></div>
            </div>
            <div className="pstat">
              <div className="k">重置</div>
              <div className="v" style={{ fontSize: 17 }}>{d.reset ? (long ? mdhm(d.reset) : hhmm(d.reset)) : <span className="mut">—</span>}</div>
            </div>
            <div className="pstat" style={{ marginLeft: "auto" }}>
              <div className="k">本機 API 計價</div>
              <div className="v" style={{ fontSize: 17 }}>{d.usage.calls > 0 ? fmtUsd(d.usage.usd) : <span className="mut">—</span>}</div>
            </div>
          </div>

          {/* 外層區塊圓角（.card）、區塊內的卡方角＋細框（.win-sub，同 M6 證據匯出卡）——D69／D70 */}
          <div className="win-sub" style={{ marginTop: 10 }}>
            <WindowStairs d={d} />
            <div className="legend-row">
            {Object.entries(d.sourceCounts).map(([s, n]) => (
              <span className="lg" key={s}>
                <span className="sw" style={{ borderRadius: "50%", background: SRC_COLOR[s] ?? "#969EAC" }} />
                {SRC_LABEL[s] ?? s} × {n}
              </span>
            ))}
            <span className="lg"><span className="sw wall" />被限流記錄（429，本機）</span>
            {d.reset && (
              <span className="lg zh" style={{ color: "var(--h5-ink3)" }}>
                右緣＝重置時刻；末筆之後沒有記錄，之後用了多少不知道
              </span>
            )}
            </div>
          </div>

          <div className="grid2" style={{ marginTop: 10 }}>
            <div className="win-sub">
              <div className="sec-label zh">每一筆觀測</div>
              <div style={{ maxHeight: 300, overflowY: "auto" }}>
                <table className="tbl">
                  <thead>
                    <tr><th>時間</th><th className="r">%</th><th className="r">變化</th><th>來源</th><th>重置</th></tr>
                  </thead>
                  <tbody>
                    {rows.map((s, i) => (
                      <tr key={i}>
                        <td>{long ? mdhm(s.at) : hhmm(s.at)}</td>
                        <td className="r" style={{ fontWeight: s.percent >= 100 ? 700 : 500, color: s.percent >= 100 ? "#C13A30" : undefined }}>{Math.round(s.percent)}</td>
                        <td className="r mut">
                          {s.delta == null || Math.round(s.delta) === 0 ? "" : s.delta > 0 ? `+${Math.round(s.delta)}` : `${Math.round(s.delta)}`}
                        </td>
                        <td><span className={`src-badge ${SRC_CLASS[s.source] ?? ""}`}>{SRC_LABEL[s.source] ?? s.source}</span></td>
                        <td className="mut" style={{ fontSize: 9.8 }}>{s.resetsAt ? (long ? mdhm(s.resetsAt) : hhmm(s.resetsAt)) : ""}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              <p className="foot-note zh" style={{ marginTop: 6 }}>
                「變化」是跟前一筆的差；相鄰兩筆來源不同時差 1～3 點是正常的（Desktop 與 CLI 各自四捨五入、取樣時刻也不同），不是額度真的掉了。
              </p>
            </div>

            <div className="win-sub">
              <div className="sec-label zh">這個窗口本機用了什麼</div>
              {d.usage.calls === 0 ? (
                <p className="foot-note zh" style={{ marginTop: 6 }}>
                  {mdhm(d.usage.from)} → {mdhm(d.usage.to)} 之間沒有這個帳號登入中的本機 API 記錄（沒在用、在網頁版用、或當時登入的是別的帳號）。
                </p>
              ) : (
                <>
                  <div className="counts zh" style={{ marginTop: 2 }}>
                    <b>{d.usage.calls.toLocaleString()}</b> 次呼叫 · <b>{fmtTokens(d.usage.outputTokens)}</b> 輸出 tokens · API 計價 <b>{fmtUsd(d.usage.usd)}</b>
                    {d.usage.byEntrypoint.length > 0 && (
                      <>
                        {" · "}
                        {d.usage.byEntrypoint.map((e) => `${ENTRYPOINT_LABEL[e.entrypoint] ?? e.entrypoint} ${e.calls}`).join("、")}
                      </>
                    )}
                  </div>
                  <table className="tbl" style={{ marginTop: 6 }}>
                    <thead>
                      <tr><th>模型</th><th className="r">呼叫</th><th className="r">輸出</th><th className="r">計價</th></tr>
                    </thead>
                    <tbody>
                      {d.usage.byModel.map((m) => (
                        <tr key={m.model}>
                          <td style={{ fontSize: 10.5 }}>{m.label ?? m.model}</td>
                          <td className="r mut">{m.calls.toLocaleString()}</td>
                          <td className="r mut">{fmtTokens(m.outputTokens)}</td>
                          <td className="r">{fmtUsd(m.usd)}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                  {d.usage.byProject.length > 0 && (
                    <table className="tbl" style={{ marginTop: 10 }}>
                      <thead>
                        <tr><th>專案</th><th className="r">呼叫</th><th className="r">輸出</th></tr>
                      </thead>
                      <tbody>
                        {d.usage.byProject.map((p) => (
                          <tr key={p.project}>
                            <td style={{ fontSize: 10.5 }} title={p.project}>{projectShort(p.project)}</td>
                            <td className="r mut">{p.calls.toLocaleString()}</td>
                            <td className="r mut">{fmtTokens(p.outputTokens)}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  )}
                </>
              )}
              <p className="foot-note zh" style={{ marginTop: 6 }}>
                本機事件的範圍：{mdhm(d.usage.from)} → {mdhm(d.usage.to)}
                {d.reset ? `（有重置時刻，用真正的 ${KIND_HOURS[pick.kind] >= 168 ? "7 天" : "5 小時"}額度窗）` : "（沒有重置時刻，用首末樣本前後）"}
                ；只算這個帳號登入中的時段（規則同「這段期間用了什麼」）；API 計價是把本機事件用官方 API 牌價換算的參考值，不是訂閱實際扣款。
              </p>
            </div>
          </div>
        </>
      )}
    </article>
  );
}
