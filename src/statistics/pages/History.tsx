import { useEffect, useMemo, useState } from "react";
import { ChevronLeft, ChevronRight } from "lucide-react";
import { useStore } from "@/store/usageStore";
import { cmd } from "@/lib/tauri";
import { seatLabel, ALL_SEATS } from "@/lib/seatLabel";
import { pageCache } from "@/lib/pageCache";
import { PageSkeleton } from "../Motion";
import { How } from "../How";
import "../h5.css";

// =============================================================================
// 歷史檢視 (M6, D56 重生, D65-4/5；v1.5 D79 三窗) — 日／週／月翻頁瀏覽過去的
// 額度窗口，頂欄切 5h／7d／Fable。全靠 truth_samples＋anchors，不碰對話記錄。
// 窗口由真值 % 的下降（>2）、靜默超過窗長、窗長到期、或 CLI 快取的重置時刻
// 跳動切分——探針與 Desktop 樣本都不帶 resets_at，只有 CLI 快取樣本有。
// 幾何全由資料推：時間軸上每窗一條，x＝時間、寬＝時長、高＝峰值（D42）；
// 7d／Fable 一週才一窗，三個尺度都畫窗內爬升階梯。
// =============================================================================

type Unit = "day" | "week" | "month";
type Kind = "5h" | "7d" | "fable";

interface HistWindow {
  start: string;
  end: string;
  reset: string | null;
  peak: number;
  samples: number;
  sources: string[];
  anchors: number;
  saturated: boolean;
  points?: [string, number][];
  /** v1.8.11（D89）：全量視角時每個窗口帶它屬於哪個座位。 */
  seat?: string;
}
interface HistDay {
  day: string;
  peak: number | null;
  samples: number;
  windows: number;
  anchors: number;
  saturated: boolean;
}
interface History {
  empty?: boolean;
  unit: Unit;
  kind: Kind;
  offset: number;
  label: string;
  from: string;
  to: string;
  fromLocal: string;
  toLocal: string;
  seats: { id: string; accountUuid: string | null; orgUuid: string | null; samples: number; isDesktop: boolean; email?: string | null; orgName?: string | null }[];
  selected: { id: string; accountUuid: string | null; orgUuid: string | null };
  windows: HistWindow[];
  days: HistDay[];
  peaks: Record<Kind, number | null>;
  totals: { samples: number; windows: number; saturated: number; anchors: number };
  /** v1.8.8（D87）：期間內本機事件的模型／專案分佈（只計這個帳號登入中的時段）。 */
  usage?: {
    byModel: { model: string; label?: string; calls: number; outputTokens: number }[];
    byProject: { project: string; calls: number; outputTokens: number }[];
    tenureSpans: number;
    all?: boolean;
  };
}

const fmtTokens = (n: number) =>
  n >= 1_000_000 ? `${(n / 1_000_000).toFixed(1)}M` : n >= 1000 ? `${Math.round(n / 1000)}k` : String(n);
const projectShort = (p: string) => p.replace(/^C--/, "").split("-").slice(-2).join("/");

const C = { accent: "#4E7ECF", deep: "#2E5FB7", red: "#C13A30", ink3: "#969EAC", ink2: "#5E6470", line: "#E1E5EB", cell: "#E8ECF2" };
const SRC_LABEL: Record<string, string> = { probe: "探針", "cli-cache": "CLI 快取", "desktop-history": "Desktop", statusline: "statusline" };
const WD = ["一", "二", "三", "四", "五", "六", "日"];

// v1.5（D79）：三條額度的名字、窗長文案、切窗規則說明——只在這裡定義。
const KINDS: Kind[] = ["5h", "7d", "fable"];
// v1.7（D81）：cut＝主文白話版；how＝術語版收進「怎麼算的 ▸」。
const KIND_META: Record<Kind, { tag: string; zh: string; spanZh: string; cut: string; how: string }> = {
  "5h": {
    tag: "5H",
    zh: "5 小時",
    spanZh: "5h",
    cut: "窗口的分割點：額度百分比明顯下降（超過 2 且後面半小時內沒有彈回）、兩筆記錄間隔超過 5 小時、從第一筆起已過 5 小時 15 分、或百分比已接近 0 時重置時刻跳到了下一窗。重置的確切時刻只有 CLI 快取記錄得到，沒有就顯示「—」。",
    how: "切窗條件（segment_windows）：真值 % 下降 >2 且往後 30 分鐘內全都低於門檻、取樣間隔 >5h、自窗首筆起 >5h15m、resets_at 跳動 >30 分且 % ≤5；孤立低值算樣本不進階梯；resets_at 只有 cli-cache 帶。",
  },
  "7d": {
    tag: "7D",
    zh: "7 天",
    spanZh: "7d",
    cut: "窗口的分割點：額度百分比明顯下降（超過 2 且後面半小時內沒有彈回）、兩筆記錄間隔超過 7 天、從第一筆起已過 7 天、或百分比已接近 0 時重置時刻跳到了下一窗。同一週被切成幾段，通常是 Desktop 與 CLI 在同一時刻回報了不同數字（例如切換帳號的那幾天），不是額度真的歸零。",
    how: "切窗條件同 5h，窗長換成 168h（寬容 1h）；Desktop 歷史樣本不帶 resets_at，只能靠下降規則；來源打架（Desktop 與 CLI 同刻報不同 %）用 30 分鐘 look-ahead 吸收。",
  },
  fable: {
    tag: "FABLE",
    zh: "Fable 週",
    spanZh: "Fable 7d",
    cut: "窗口的分割點跟 7 天那條一樣。Fable 這條只有探針與 CLI 快取有記錄（Desktop 歷史裡沒有這條額度），app 沒跑的日子就是空的。",
    how: "切窗條件同 7d；樣本來源只有 probe 與 cli-cache（limit_kind=weekly_scoped、scope=Fable），Desktop 歷史檔無此額度。",
  },
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
const spanTxt = (a: string, b: string) => {
  const mins = Math.max(0, Math.round((new Date(b).getTime() - new Date(a).getTime()) / 60_000));
  if (mins < 60) return `${mins} 分`;
  // v1.5：7d 的窗用「天 時」講，165 時 12 分沒人看得懂
  if (mins >= 48 * 60) return `${Math.floor(mins / 1440)} 天 ${Math.floor((mins % 1440) / 60)} 時`;
  return `${Math.floor(mins / 60)} 時 ${pad2(mins % 60)} 分`;
};

/* ---------------- 窗口峰值時間軸 ---------------- */
function TimelineSvg({ h }: { h: History }) {
  const W = 1040, H = 190, L = 34, R = 12, T = 14, B = 26;
  const pw = W - L - R, ph = H - T - B;
  const t0 = new Date(h.from).getTime(), t1 = new Date(h.to).getTime();
  const x = (ms: number) => L + (pw * Math.min(1, Math.max(0, (ms - t0) / (t1 - t0))));
  const y = (p: number) => T + ph * (1 - Math.min(100, Math.max(0, p)) / 100);
  const ms = (s: string) => new Date(s).getTime();
  const long = h.kind !== "5h";

  // grid: day (hours) / week (days) / month (dates)
  const ticks: { at: number; label: string }[] = [];
  if (h.unit === "day") {
    for (let hr = 0; hr <= 24; hr += 3) ticks.push({ at: t0 + hr * 3600_000, label: `${pad2(hr % 24)}:00` });
  } else {
    const nDays = Math.round((t1 - t0) / 86400_000);
    for (let i = 0; i <= nDays; i++) {
      const at = t0 + i * 86400_000;
      const d = new Date(at);
      const label =
        h.unit === "week"
          ? i < nDays ? `週${WD[(d.getDay() + 6) % 7]} ${pad2(d.getDate())}` : ""
          : i < nDays && (d.getDate() === 1 || d.getDate() % 7 === 1) ? `${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}` : "";
      ticks.push({ at, label });
    }
  }

  return (
    <svg width="100%" viewBox={`0 0 ${W} ${H}`} style={{ display: "block" }}>
      {[25, 50, 75].map((v) => (
        <g key={v}>
          <line x1={L} y1={y(v)} x2={W - R} y2={y(v)} stroke={C.line} strokeDasharray="2 5" />
          <text x={L - 6} y={y(v) + 3.5} textAnchor="end" fontSize={9} fill={C.ink3}>{v}</text>
        </g>
      ))}
      <line x1={L} y1={y(100)} x2={W - R} y2={y(100)} stroke={C.red} strokeDasharray="3 4" />
      <text x={L - 6} y={y(100) + 3.5} textAnchor="end" fontSize={9} fill={C.red} fontWeight={700}>100</text>
      {ticks.map((tk, i) => (
        <g key={i}>
          <line x1={x(tk.at)} y1={T} x2={x(tk.at)} y2={y(0)} stroke={C.line} strokeWidth={h.unit === "day" ? 0.8 : 1} />
          {tk.label && (
            <text x={x(tk.at) + (h.unit === "day" ? 0 : 4)} y={H - 8} textAnchor={h.unit === "day" ? "middle" : "start"} fontSize={8.6} fill={C.ink3}>
              {tk.label}
            </text>
          )}
        </g>
      ))}
      <line x1={L} y1={y(0)} x2={W - R} y2={y(0)} stroke="#D3D9E2" />
      {h.windows.map((w, i) => {
        const xs = x(ms(w.start)), xe = Math.max(x(ms(w.end)), xs + 2);
        const fill = w.saturated ? C.red : C.accent;
        const tip = `${mdhm(w.start)} → ${long ? mdhm(w.end) : hhmm(w.end)} · 峰值 ${Math.round(w.peak)}% · ${w.samples} 筆${w.anchors ? ` · 429 ×${w.anchors}` : ""}`;
        return (
          <g key={i}>
            <rect x={xs} y={y(w.peak)} width={xe - xs} height={y(0) - y(w.peak)} fill={fill} opacity={0.82} rx={1.5}>
              <title>{tip}</title>
            </rect>
            {w.points && w.points.length > 1 && (
              <polyline
                points={w.points.map(([at, p], j) => `${x(ms(at))},${y(p)}${j === w.points!.length - 1 ? "" : ` ${x(ms(w.points![j + 1][0]))},${y(p)}`}`).join(" ")}
                fill="none"
                stroke="#fff"
                strokeWidth={1.2}
                opacity={0.9}
              />
            )}
            {w.anchors > 0 && <circle cx={(xs + xe) / 2} cy={y(w.peak) - 6} r={2.8} fill={C.red} />}
          </g>
        );
      })}
    </svg>
  );
}

export function HistoryPage() {
  const usage = useStore((s) => s.usage);
  const [unit, setUnit] = useState<Unit>("week");
  const [kind, setKind] = useState<Kind>("5h");
  const [offset, setOffset] = useState(0);
  const [seatPick, setSeatPick] = useState<string | null>(null);
  // v1.3：頂欄切換器選了座位就跟著走。v1.8.2 主人驗收：頁內 chip 也要反向帶動頂欄，
  // 所以 chip 改 store 的 viewSeat，seatPick 只從 store 推（同 HalfYear）。
  const viewSeatId = useStore((s) => s.viewSeatId);
  const currentSeatId = useStore((s) => s.currentSeatId);
  const setViewSeat = useStore((s) => s.setViewSeat);
  const aliases = useStore((s) => s.settings.accounts.aliases);
  useEffect(() => {
    const want = viewSeatId ?? currentSeatId;
    if (want) setSeatPick(want);
  }, [viewSeatId, currentSeatId]);
  const [h, setH] = useState<History | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    // v1.8.2 效能：先拿上一次的畫（翻頁／切頁回來不閃骨架），IPC 回來再換新的。
    const key = `hist|${seatPick ?? ""}|${unit}|${offset}|${kind}`;
    const cached = pageCache.get<History>(key);
    if (cached) setH(cached);
    cmd
      .getHistory(seatPick, unit, offset, kind)
      .then((d) => {
        pageCache.set(key, d);
        setH(d as History);
        setError(null);
      })
      .catch((e) => setError(String(e)));
  }, [seatPick, unit, offset, kind, usage.lastSuccessAt]);

  const nowLabel = unit === "day" ? "今天" : unit === "week" ? "本週" : "本月";
  const sorted = useMemo(() => (h ? [...h.windows].sort((a, b) => (a.start < b.start ? 1 : -1)) : []), [h]);

  if (error) return <div className="h5 warn-band zh">讀取失敗:{error}</div>;
  if (!h) {
    return (
      // E28（D77）：骨架 shimmer 取代「載入中…」一行字
      <PageSkeleton />
    );
  }
  if (h.empty) return <p className="h5 foot-note zh">帳本裡還沒有座位。</p>;

  // 舊 payload（沒有 kind／peaks）也能畫：當 5h 看。
  const k: Kind = (h.kind ?? "5h") as Kind;
  const meta = KIND_META[k];
  // v1.8.11（D89）：全量視角——窗口明細多一欄「帳號」
  const isAll = h.selected.id === ALL_SEATS;
  const seatName = (id?: string) => {
    const s = h.seats.find((x) => x.id === id);
    return s ? seatLabel(s, aliases) : id?.slice(0, 8) ?? "?";
  };
  const long = k !== "5h";
  const otherPeaks = KINDS.filter((x) => x !== k);
  const peakOf = (x: Kind) => h.peaks?.[x] ?? null;
  const showStairs = h.windows.some((w) => w.points && w.points.length > 1);

  return (
    <div className="h5 space-y-4">
      <div className="head">
        {h.seats.map((s) => {
          const on = s.id === h.selected.id;
          return (
            <button key={s.id} className={`chip pick${on ? " on" : ""}`} onClick={() => void setViewSeat(s.id)}>
              <b>{seatLabel(s, aliases)}</b>
              <span style={{ color: "var(--h5-ink3)" }}>{s.isDesktop ? "Desktop" : "CLI"} · {s.samples.toLocaleString()} 筆</span>
            </button>
          );
        })}
        {/* v1.8.11（D89，主人）：全量視角——每個帳號的窗口合成一條時間軸 */}
        <button className={`chip pick${isAll ? " on" : ""}`} onClick={() => void setViewSeat(ALL_SEATS)}>
          <b>全部帳號</b>
          <span style={{ color: "var(--h5-ink3)" }}>不分帳號</span>
        </button>
        {/* v1.5（D79）：三條額度切換——歷史頁從此不只 5h。切換不動日／週／月與翻頁位置。 */}
        <div className="pills" style={{ marginLeft: "auto" }} role="tablist" aria-label="額度">
          {KINDS.map((x) => (
            <button key={x} className={kind === x ? "on" : ""} onClick={() => setKind(x)} role="tab" aria-selected={kind === x}>
              {KIND_META[x].tag}
            </button>
          ))}
        </div>
      </div>

      {/* v1.4.1（D78 #3）：整段期間這個座位沒有真值樣本、卻有 429——429 是本機對話記錄回填的（不分帳號，O10），
          樣本才分座位；對不上就是那段時間 app 沒跑、或登入的是別的帳號。 */}
      {h.totals.samples === 0 && h.totals.anchors > 0 && (
        <div className="warn-band zh">
          這段期間這個帳號×組織沒有 {meta.zh}額度的記錄（app 沒在跑，或當時登入的是別的帳號）；下面的 {h.totals.anchors} 次被限流記錄（429）來自本機對話，不區分帳號。
        </div>
      )}

      <article className="card">
        <div className="card-head">
          <span className="card-title zh">窗口峰值時間軸</span>
          <span className="card-tag">{meta.tag} WINDOWS · PEAK · {h.unit.toUpperCase()}</span>
          <div className="right">
            <div className="pills">
              {(["day", "week", "month"] as Unit[]).map((u) => (
                <button key={u} className={unit === u ? "on" : ""} onClick={() => { setUnit(u); setOffset(0); }}>
                  {u === "day" ? "日" : u === "week" ? "週" : "月"}
                </button>
              ))}
            </div>
            <div className="pager">
              <button onClick={() => setOffset((o) => o - 1)} aria-label="上一頁"><ChevronLeft size={13} /></button>
              <span className="zh">{offset === 0 ? nowLabel : ""} {h.label}</span>
              <button onClick={() => setOffset((o) => Math.min(0, o + 1))} disabled={offset >= 0} aria-label="下一頁"><ChevronRight size={13} /></button>
            </div>
          </div>
        </div>
        <div className="pstats" style={{ gap: 24, marginTop: 8 }}>
          <div className="pstat"><div className="k">{meta.spanZh} 窗口</div><div className="v">{h.totals.windows}<i>個</i></div></div>
          <div className="pstat"><div className="k">樣本數</div><div className="v">{h.totals.samples.toLocaleString()}<i>筆</i></div></div>
          <div className="pstat"><div className="k">見頂窗</div><div className={`v${h.totals.saturated > 0 ? " red" : ""}`}>{h.totals.saturated}<i>個</i></div></div>
          <div className="pstat"><div className="k">429</div><div className={`v${h.totals.anchors > 0 ? " red" : ""}`}>{h.totals.anchors}<i>次</i></div></div>
          <div className="pstat"><div className="k">{meta.spanZh} 峰值</div><div className={`v${(peakOf(k) ?? 0) >= 100 ? " red" : ""}`}>{peakOf(k) != null ? Math.round(peakOf(k)!) : "—"}<i>%</i></div></div>
          {/* 另外兩條額度同期間的峰值——脈絡列，看 7d 時也知道 5h／Fable 到哪 */}
          {otherPeaks.map((x) => (
            <div className="pstat" key={x} style={{ opacity: 0.72 }}>
              <div className="k">{KIND_META[x].spanZh} 峰值</div>
              <div className="v">{peakOf(x) != null ? Math.round(peakOf(x)!) : "—"}<i>%</i></div>
            </div>
          ))}
        </div>
        <div style={{ marginTop: 10 }}>
          {h.windows.length === 0 ? (
            <p className="foot-note zh">這段期間這個帳號×組織沒有任何 {meta.zh}額度的使用記錄（可能真的沒在用，也可能 app 當時沒在記錄——可到「資料健康度」頁確認缺口）。</p>
          ) : (
            <TimelineSvg h={h} />
          )}
        </div>
        <div className="legend-row">
          <span className="lg"><span className="sw bar" />每窗一條：寬＝時長、高＝峰值</span>
          <span className="lg"><span className="sw wall" />見頂窗（峰值 100%）</span>
          <span className="lg"><span className="sw walldot" />窗內被限流記錄（429，本機）</span>
          {/* v1.4.1（D78 #3）：429 來自本機對話記錄，不分帳號（O10）；樣本是這個座位的。兩者對不上的日子＝那天這個座位沒在跑 app。 */}
          <span className="lg zh" style={{ color: "var(--h5-ink3)" }}>429 被限流記錄來自本機所有帳號的對話，不區分帳號×組織；有 429 卻沒有額度記錄的日子，代表那天這組帳號的 app 沒有在跑</span>
          {showStairs && <span className="lg"><span className="sw stair-white" />窗內爬升階梯</span>}
        </div>
      </article>

      <div className="grid2">
        {h.unit !== "day" && (
          <article className="card g-r1c2">
            <div className="card-head">
              <span className="card-title zh">每日峰值</span>
              <span className="card-tag">DAILY PEAK · {meta.tag}</span>
            </div>
            <table className="tbl" style={{ marginTop: 8 }}>
              <thead>
                <tr><th>日</th><th className="r">{meta.tag} 峰值</th><th className="r">窗</th><th className="r">樣本</th><th className="r">429</th></tr>
              </thead>
              <tbody>
                {h.days.map((d) => {
                  const dt = new Date(`${d.day}T00:00:00`);
                  return (
                    <tr key={d.day}>
                      <td>{d.day.slice(5)} <span className="mut" style={{ fontSize: 9.5 }}>週{WD[(dt.getDay() + 6) % 7]}</span></td>
                      <td className="r" style={{ minWidth: 120 }}>
                        {d.peak != null ? (
                          <span style={{ display: "inline-flex", alignItems: "center", gap: 6, justifyContent: "flex-end" }}>
                            <span className="covbar" style={{ width: 70, flex: "none" }}><div style={{ width: `${Math.min(100, d.peak)}%`, background: d.saturated ? C.red : undefined }} /></span>
                            <span style={{ color: d.saturated ? C.red : undefined, fontWeight: d.saturated ? 700 : 400 }}>{Math.round(d.peak)}%</span>
                          </span>
                        ) : d.anchors > 0 ? (
                          // v1.4.1（D78 #3）：429 是掃本機對話記錄回填的，app 沒在跑的日子也有；
                          // 沒有真值樣本卻有 429，就是那天 app 沒跑，不是資料壞掉。
                          <span className="mut zh" style={{ fontSize: 9.5 }}>app 未運行 · 僅有 429 紀錄</span>
                        ) : <span className="mut">—</span>}
                      </td>
                      <td className="r mut">{d.windows}</td>
                      <td className="r mut">{d.samples}</td>
                      <td className="r" style={{ color: d.anchors ? C.red : undefined }}>{d.anchors || ""}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
            {long && (
              <p className="foot-note zh" style={{ marginTop: 8 }}>
                「窗」欄是當天起算的 {meta.zh}窗口（通常一週一個）；每日峰值是當天記錄到的最高百分比。
              </p>
            )}
          </article>
        )}

        <article className={`card${h.unit === "day" ? " g-full" : ""}`} style={h.unit === "day" ? undefined : { gridRow: 1, gridColumn: 1 }}>
          <div className="card-head">
            <span className="card-title zh">窗口明細</span>
            <span className="card-tag">{meta.tag} WINDOWS · 新到舊</span>
          </div>
          {sorted.length === 0 ? (
            <p className="foot-note zh" style={{ marginTop: 8 }}>沒有窗口。</p>
          ) : (
            <div style={{ maxHeight: 420, overflowY: "auto", marginTop: 8 }}>
              <table className="tbl">
                <thead>
                  <tr>{isAll && <th>帳號</th>}<th>起</th><th>末筆</th><th className="r">時長</th><th className="r">峰值</th><th className="r">樣本</th><th>來源</th><th className="r">429</th><th>重置</th></tr>
                </thead>
                <tbody>
                  {sorted.map((w, i) => (
                    <tr key={i}>
                      {isAll && <td className="zh" style={{ fontSize: 9.8, maxWidth: 140, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }} title={w.seat}>{seatName(w.seat)}</td>}
                      <td>{mdhm(w.start)}</td>
                      <td className="mut">{long ? mdhm(w.end) : hhmm(w.end)}</td>
                      <td className="r mut">{spanTxt(w.start, w.end)}</td>
                      <td className="r" style={{ color: w.saturated ? C.red : undefined, fontWeight: w.saturated ? 700 : 500 }}>{Math.round(w.peak)}%</td>
                      <td className="r mut">{w.samples}</td>
                      <td className="mut" style={{ fontSize: 9.8 }}>{w.sources.map((s) => SRC_LABEL[s] ?? s).join("＋")}</td>
                      <td className="r" style={{ color: w.anchors ? C.red : undefined }}>{w.anchors || ""}</td>
                      <td className="mut">{w.reset ? (long ? mdhm(w.reset) : hhmm(w.reset)) : <span style={{ color: "var(--h5-ink3)" }}>—</span>}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
          <p className="foot-note zh" style={{ marginTop: 8 }}>
            {meta.cut}
            這頁只顯示訂閱額度的觀測記錄與被限流次數（429），不讀取對話內容——計數欄顯示的是「樣本數」，不是「訊息數」（D56）。
          </p>
          <How>{meta.how}</How>
        </article>
      </div>

      {/* v1.8.8（D87，主人 2026-09-25）：模型與專案分佈以前散在健康頁（全期間）與今日頁（本窗），
          歷史頁翻到哪一天／週／月就看那段期間的。 */}
      <article className="card g-full">
        <div className="card-head">
          <span className="card-title zh">這段期間用了什麼</span>
          <span className="card-tag">USAGE MIX · {h.label} · 本機記錄</span>
        </div>
        {!h.usage || (h.usage.byModel.length === 0 && h.usage.byProject.length === 0) ? (
          <p className="foot-note zh" style={{ marginTop: 8 }}>
            這段期間沒有這個帳號登入中的本機 API 記錄（沒在用、或當時登入的是別的帳號）。
          </p>
        ) : (
          <div className="grid gap-3 md:grid-cols-2" style={{ marginTop: 8 }}>
            <div>
              <div className="ml-title zh">模型 · 按輸出 tokens</div>
              <table className="tbl">
                <tbody>
                  {h.usage.byModel.map((m) => (
                    <tr key={m.model}>
                      <td style={{ fontSize: 10.5 }}>{m.label ?? m.model}</td>
                      <td className="r mut">{fmtTokens(m.outputTokens)} · {m.calls.toLocaleString()} 次</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <div>
              <div className="ml-title zh">專案 · 按輸出 tokens</div>
              <table className="tbl">
                <tbody>
                  {h.usage.byProject.map((p) => (
                    <tr key={p.project}>
                      <td style={{ fontSize: 10.5 }} title={p.project}>{projectShort(p.project)}</td>
                      <td className="r mut">{fmtTokens(p.outputTokens)} · {p.calls.toLocaleString()} 次</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>
        )}
        <p className="foot-note zh" style={{ marginTop: 8 }}>
          只計本機對話記錄裡的 API 呼叫{h.usage?.all ? "，本機全量、不分帳號" : "、且只算這個帳號登入中的時段（規則同趨勢頁）"}；不含網頁版。
        </p>
      </article>
    </div>
  );
}
