import { useEffect, useState } from "react";
import { RefreshCw, BellOff } from "lucide-react";
import { useStore } from "@/store/usageStore";
import { cmd } from "@/lib/tauri";
import { fmtAgo } from "@/lib/ago";
import { useMinuteTick } from "@/lib/useTick";
import { seatLabel, seatShortId, ALL_SEATS, ALL_SEATS_LABEL, type SeatLike } from "@/lib/seatLabel";
import { PageSkeleton } from "../Motion";
import { useSeatPick } from "../useSeatPick";
import { localDayOf, localSpan, mdhm } from "../dates";
import { planShort } from "@/lib/planLabel";

const mdOf = (iso: string) => mdhm(iso).slice(0, 5);
/** 校準中＝段數不到 minN，或（有 Δ 合計時）額度累計掉不到 minDeltaSum。 */
const fxCal = (f: { intervals: number; minN: number; deltaSum: number | null; minDeltaSum: number }) =>
  f.intervals < f.minN || (f.deltaSum != null && f.deltaSum < f.minDeltaSum);
import { fmtMixUsd, fmtTokens, mixBreakdown, PriceMark, type ModelMix } from "../UsageMix";
import "../h5.css";

// =============================================================================
// 資料健康度 (M1, D55 H5 換裝) — "時間從此為我們工作" 的儀表板。
// 加碼 D55：門鈴白響提醒（連續 ≥3 次 webhook 響但 CLI 數字沒動 →
// 瀏覽器可能登著第三帳號，門鈴形同虛設）。
// =============================================================================

interface HealthData {
  doorbellFutileCount: number;
  /** v1.8.7（D86 蟲 5）／v1.8.8 按來源分開：門鈴按了但密碼牌對不上（連續次數、最近一次）。 */
  doorbellUnauthorized?: { source: string; count: number; last: string | null }[];
  truthSamples: number;
  usageEvents: number;
  anchors: number;
  gapCount: number;
  bySource: { source: string; count: number; first: string | null; last: string | null }[];
  seats: {
    id: string;
    accountUuid: string | null;
    orgUuid: string | null;
    email: string | null;
    orgName: string | null;
    firstSeenAt: string;
    sampleCount: number;
  }[];
  coverage7d: number;
  coverage30d: number;
  recentGaps: { source: string; startedAt: string; endedAt: string }[];
  /** D97：按 API 計價排。 */
  topModels: ModelMix[];
  eventsSpan: { first: string | null; last: string | null };
  /** v1.9.4（D99 B2）：全帳本裡官方價目表沒有牌價的模型（用量沒算進任何匯率）。舊後端沒有。 */
  unpricedModels?: { model: string; label?: string; calls: number; tokens: number; firstSeen: string | null; lastSeen: string | null }[];
}

/** v1.8.7（D86 蟲 4）：匯率卡只要趨勢頁 dashboard 的幾個數字（後端有快取，毫秒級）。 */
interface FxSummary {
  rateTrim: number | null;
  rateRaw: number | null;
  intervals: number;
  dataSince: string | null;
  latest: { month: string; rate: number | null; ci: [number, number] | null; samples: number; planLabel?: string | null } | null;
  /** v1.9.4（D99 A10）：全部帳號視角不合匯率（每個帳號的池可能不一樣大）。 */
  allSeats: boolean;
  /** v1.9.4（D99 A3）：期間內換過方案時，上面的匯率只算目前方案（planLabel）那一段。 */
  planScopedLabel: string | null;
  /** 審查 23r M1：真的換過方案（true）還是只是之前方案未知、從 firstObservedAt 起才記到（false）。 */
  realChange: boolean;
  firstObservedAt: string | null;
  /** 審查 23r M2：段數門檻（後端 method.minN）與 Δ 合計門檻（minDeltaSum）；任一不到就是校準中。 */
  minN: number;
  minDeltaSum: number;
  deltaSum: number | null;
  /** v1.8.13（D91 T-15 審查）：算的是哪個帳號——座位可能是頂欄選的或清單第一個，要講明。
   *  存座位本身、render 時才套別名，改別名不用重抓。null＝全部帳號。 */
  seat: SeatLike | null;
}

const SOURCE_LABEL: Record<string, string> = {
  "claude-code-hook": "Claude Code hook",
  extension: "瀏覽器擴充",
  "extension-test": "瀏覽器擴充（測試鈴）",
  unknown: "不明來源（舊版 hook？）",
};

/** v1.8.13（D91 T-13）：缺口的來源欄說人話（以前直接印 desktop-history）。 */
const GAP_SOURCE_LABEL: Record<string, string> = {
  "desktop-history": "Desktop 歷史",
  probe: "探針",
  "cli-cache": "CLI 快取",
};

export function HealthPage() {
  const usage = useStore((s) => s.usage);
  const aliases = useStore((s) => s.settings.accounts.aliases);
  const [data, setData] = useState<HealthData | null>(null);
  const [fx, setFx] = useState<FxSummary | null | undefined>(undefined);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const load = async () => {
    setLoading(true);
    try {
      setData(await cmd.getDataHealth());
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void load();
  }, [usage.lastSuccessAt]);

  // v1.8.7（D86 蟲 4）：以前這張卡是 M1 的佔位（「粗估已上線（見趨勢）」＋「信心區間 M5 轉正」），
  // M5 早就做完了沒人回頭改。現在真的去拿目前帳號 180 天的期間匯率。
  //
  // v1.8.13（D91 T-15 審查）：座位跟趨勢頁用同一個 useSeatPick（頂欄選的 ?? 目前登入的 ??
  // 清單第一個）。以前只認 CLI 的目前帳號：(a) 視窗剛開 store 還沒載到就先說「沒有記錄」；
  // (b) 只用 Desktop 的人永遠沒有「目前帳號」，這張卡永遠說沒有，趨勢頁卻算得出匯率。
  // 同一個座位、同樣 180 天，後端快取 key 也跟趨勢頁同一把，不會多冷算一份。
  const { seat: fxSeat, ready: fxReady } = useSeatPick();

  // 換座位（或座位才剛定下來）時回到「計算中…」；每輪採集重抓時保留舊的，不閃。
  useEffect(() => {
    setFx(undefined);
  }, [fxSeat, fxReady]);

  useEffect(() => {
    // 座位還沒定下來就不問、維持「計算中…」。
    if (!fxReady) return;
    let alive = true;
    cmd
      .getDashboard(fxSeat, 180)
      .then((d) => {
        if (!alive) return;
        if (!d || d.empty) {
          setFx(null);
          return;
        }
        type DashSeat = SeatLike & { id: string };
        const sel = d.selected as DashSeat | undefined;
        const seatRow = ((d.seats ?? []) as DashSeat[]).find((s) => s.id === sel?.id) ?? sel;
        const seat = sel?.id === ALL_SEATS ? null : (seatRow ?? { accountUuid: null, orgUuid: null });
        const months = (d.fx5 ?? []) as { month: string; rate: number | null; ci: [number, number] | null; samples: number; qualified: boolean; planLabel?: string | null }[];
        const latest = [...months].reverse().find((m) => m.qualified && m.rate != null) ?? null;
        // v1.9.4（D99 A3）：期間內混了不同方案時，整段的匯率是兩個池的加權——改用只算目前方案的那一份。
        const rec = d.reconcile ?? {};
        const scoped = rec.mixedPlans && rec.planScoped ? rec.planScoped : null;
        setFx({
          rateTrim: (scoped ?? rec).rateTrim ?? null,
          rateRaw: (scoped ?? rec).rateRaw ?? null,
          intervals: (scoped ?? rec).intervals ?? 0,
          dataSince: d.dataSince ?? null,
          latest: latest ? { month: latest.month, rate: latest.rate, ci: latest.ci, samples: latest.samples, planLabel: latest.planLabel ?? null } : null,
          seat,
          allSeats: sel?.id === ALL_SEATS,
          planScopedLabel: scoped ? (scoped.scope?.planLabel ?? null) : null,
          realChange: !!d.plan?.crossesKnownPlans,
          firstObservedAt: d.plan?.firstObservedAt ?? null,
          minN: d.method?.minN ?? 20,
          minDeltaSum: d.method?.minDeltaSum ?? 30,
          deltaSum: scoped ? (scoped.deltaSum ?? null) : null,
        });
      })
      .catch(() => {
        if (alive) setFx(null);
      });
    return () => {
      alive = false;
    };
  }, [fxSeat, fxReady, usage.lastSuccessAt]);
  const fxWho = (f: FxSummary) => (f.seat ? seatLabel(f.seat, aliases) : ALL_SEATS_LABEL);
  // v1.9.4（D1）：「最後取得 N 分前」每分鐘自己走。
  const now = useMinuteTick();

  if (error) {
    return <div className="h5 warn-band zh">讀取健康度失敗:{error}</div>;
  }
  if (!data) {
    return (
      // E28（D77）：骨架 shimmer 取代「載入中…」一行字
      <PageSkeleton />
    );
  }

  return (
    <div className="h5 space-y-4">
      {usage.notice && <div className="warn-band zh">⚠ {usage.notice}</div>}
      {data.doorbellFutileCount >= 3 && (
        <div className="warn-band zh">
          <BellOff size={14} style={{ flex: "none", marginTop: 2 }} />
          <span>
            門鈴響了 {data.doorbellFutileCount} 次，但 CLI 的數字都沒有變動——瀏覽器登入的帳號可能不是
            CLI 帳號，門鈴就形同虛設。要讓瀏覽器擴充功能發揮作用，瀏覽器和 CLI 需要登入同一個帳號。
          </span>
        </div>
      )}
      {(data.doorbellUnauthorized ?? []).filter((u) => u.count > 0).map((u) => (
        <div className="warn-band zh" key={u.source}>
          <BellOff size={14} style={{ flex: "none", marginTop: 2 }} />
          <span>
            {SOURCE_LABEL[u.source] ?? u.source}按了 {u.count} 次門鈴，但密碼牌對不上，App 沒有理它
            {u.last ? `（最近一次 ${fmtAgo(u.last, now) ?? "時間不明"}）` : ""}。
            {u.source === "extension" || u.source === "extension-test"
              ? "到設定頁複製現在的密碼牌，重新貼進瀏覽器擴充的視窗。"
              : "Claude Code hook 的牌 App 啟動時會自動換新；也可以到設定頁按一次「一鍵安裝 Claude Code Hook」。"}
          </span>
        </div>
      ))}

      {/* 累積量 */}
      <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
        <StatTile label="觀測記錄 TRUTH" value={data.truthSamples.toLocaleString()} />
        <StatTile label="API 呼叫（去重）EVENTS" value={data.usageEvents.toLocaleString()} />
        <StatTile label="被限流記錄 WALLS" value={data.anchors.toLocaleString()} />
        <StatTile label="觀測缺口 GAPS" value={data.gapCount.toLocaleString()} />
      </div>

      {/* 覆蓋率 */}
      <article className="card">
        <div className="card-head">
          <span className="card-title zh">觀測覆蓋率</span>
          <span className="card-tag">COVERAGE</span>
        </div>
        <div style={{ marginTop: 10 }}>
          {[
            { label: "近 7 天", v: data.coverage7d },
            { label: "近 30 天", v: data.coverage30d },
          ].map((r) => (
            <div key={r.label} className="mb-2 flex items-center gap-3">
              <span className="zh" style={{ width: 62, flex: "none", fontSize: 11, color: "var(--h5-ink2)" }}>
                {r.label}
              </span>
              {/* 量條歸靛藍（D57）；狀態由右側數字染色小面積表達 */}
              <div className="covbar">
                <div style={{ width: `${Math.round(r.v * 100)}%` }} />
              </div>
              <span
                style={{
                  width: 40,
                  flex: "none",
                  textAlign: "right",
                  fontSize: 11,
                  fontVariantNumeric: "tabular-nums",
                  color: r.v < 0.3 ? "var(--h5-red)" : "var(--h5-ink)",
                }}
              >
                {Math.round(r.v * 100)}%
              </span>
            </div>
          ))}
        </div>
        <p className="foot-note zh" style={{ marginTop: 4 }}>
          覆蓋率是「有觀測記錄的小時」佔總時間的比例。機器關機、或兩端 app 都沒開的時段不會有記錄——這是「實際觀測到了多少時間」的誠實指標。
        </p>
      </article>

      {/* 來源分佈＋座位 */}
      <div className="grid gap-3 md:grid-cols-2">
        <article className="card">
          <div className="card-head">
            <span className="card-title zh">資料來源</span>
            <span className="card-tag">SOURCES</span>
          </div>
          <table className="tbl" style={{ marginTop: 8 }}>
            <thead>
              <tr>
                <th>SOURCE</th>
                <th className="r">樣本</th>
                <th className="r">最後取得</th>
              </tr>
            </thead>
            <tbody>
              {data.bySource.map((s) => (
                <tr key={s.source}>
                  <td>{s.source}</td>
                  <td className="r">{s.count.toLocaleString()}</td>
                  <td className="r mut">{s.last ? fmtAgo(s.last, now) ?? "—" : "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </article>

        <article className="card">
          <div className="card-head">
            <span className="card-title zh">座位（帳號 × 組織）</span>
            <span className="card-tag">SEATS</span>
          </div>
          <table className="tbl" style={{ marginTop: 8 }}>
            <tbody>
              {data.seats.map((s, i) => (
                <tr key={i}>
                  <td style={{ fontSize: 10.5 }}>
                    <span className="zh" style={{ fontWeight: 600 }}>{seatLabel(s, aliases)}</span>
                    <span className="mut" style={{ marginLeft: 6, fontSize: 9.5 }}>{seatShortId(s)}</span>
                    {!s.accountUuid && (
                      <span className="lim-chip zh" style={{ marginLeft: 6 }}>
                        推定
                      </span>
                    )}
                  </td>
                  <td className="r mut">{s.sampleCount.toLocaleString()} 筆</td>
                </tr>
              ))}
            </tbody>
          </table>
        </article>
      </div>

      {/* 匯率摘要＋模型分佈 */}
      <div className="grid gap-3 md:grid-cols-2">
        <article className="card">
          <div className="card-head">
            <span className="card-title zh">額度匯率</span>
            {/* v1.8.13（D91 T-15 審查）：座位不一定是「目前帳號」（可能是頂欄選的或清單第一個），
                帳號名改寫進內文，標籤不再寫死「目前帳號」。 */}
            <span className="card-tag">FX · 180 天</span>
          </div>
          {fx === undefined ? (
            <p className="foot-note zh" style={{ marginTop: 8 }}>計算中…</p>
          ) : fx?.allSeats ? (
            // v1.9.4（D99 A10）：全部帳號不合匯率——後端已經清空，這裡講原因，不顯示「沒有記錄」。
            <p className="foot-note zh" style={{ marginTop: 8 }}>
              頂欄選的是「全部帳號」：每個帳號的額度池可能不一樣大（例如一個 Max 5x、一個 Max 20x），合在一起的匯率沒有意義，所以不算。選單一帳號就看得到；「趨勢」頁的匯率卡有各帳號分開的數字。
            </p>
          ) : fx == null || fx.rateTrim == null ? (
            <p className="foot-note zh" style={{ marginTop: 8 }}>
              {fx ? `${fxWho(fx)}：` : ""}
              還沒有可配對的觀測記錄（需要同一個帳號×組織、45 分鐘內兩筆、額度有上升、且本機有 API 呼叫）。
            </p>
          ) : (
            <>
              <p className="zh" style={{ fontSize: 12, color: "var(--h5-ink2)", marginTop: 8 }}>
                {fxWho(fx)}：每 1% 的 5h 額度約換到{" "}
                {/* 審查 23r M2：段數不到門檻＝校準中，數字變淡、寫進度（跟趨勢頁同一條規則） */}
                <b style={{ fontSize: 15, color: fxCal(fx) ? "var(--h5-ink3)" : "var(--h5-ink)", fontVariantNumeric: "tabular-nums" }}>${fx.rateTrim.toFixed(2)}</b>{" "}
                的 API 用量（剔極值；未剔 ${fx.rateRaw?.toFixed(2) ?? "—"}），共 {fx.intervals.toLocaleString()} 段配對記錄
                {fxCal(fx) ? `（校準中 · 已累積 ${fx.intervals}／${fx.minN} 段${fx.deltaSum != null && fx.intervals >= fx.minN ? `、額度掉 ${Math.round(fx.deltaSum)}／${fx.minDeltaSum}%` : ""}，先別當真）` : ""}
                {/* 審查 23r M1：真的換過方案才說「換過」；升版當天只是之前方案未知 */}
                {fx.planScopedLabel
                  ? fx.realChange
                    ? `——這段期間換過方案，只算目前方案（${planShort(fx.planScopedLabel)}）的部分`
                    : `——App 從 ${fx.firstObservedAt ? mdOf(fx.firstObservedAt) : "最近"} 才開始記錄方案，只算之後（${planShort(fx.planScopedLabel)}）的部分；之前那段是「方案未知」，先不算`
                  : ""}
                。
              </p>
              {fx.latest && fx.latest.rate != null && (
                <p className="foot-note zh" style={{ marginTop: 4 }}>
                  最近一個樣本夠的月份 {fx.latest.month}{fx.latest.planLabel ? `（${planShort(fx.latest.planLabel)}）` : ""}：${fx.latest.rate.toFixed(2)}／1%
                  {fx.latest.ci ? `（95% 區間 ${fx.latest.ci[0].toFixed(2)}–${fx.latest.ci[1].toFixed(2)}）` : ""}，{fx.latest.samples} 段。
                </p>
              )}
              <p className="foot-note zh" style={{ marginTop: 4 }}>
                觀測起點 {localDayOf(fx.dataSince) ?? "—"}；逐月走勢、縮水判定與分模型倍率在「趨勢」頁。
              </p>
            </>
          )}
          <p className="foot-note" style={{ marginTop: 4 }}>
            記帳起點:{localDayOf(data.eventsSpan.first) ?? "—"} ～ {localDayOf(data.eventsSpan.last) ?? "—"}
          </p>
        </article>
        <article className="card">
          <div className="card-head">
            <span className="card-title zh">模型分佈</span>
            <span className="card-tag">BY API PRICE</span>
          </div>
          <table className="tbl" style={{ marginTop: 8 }}>
            <tbody>
              {data.topModels.slice(0, 5).map((m) => (
                <tr key={m.model} title={mixBreakdown(m)}>
                  <td style={{ fontSize: 10.5 }}>{m.model}</td>
                  <td className="r mut">
                    {fmtMixUsd(m)}
                    <PriceMark m={m} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {/* v1.9.4（D99 B2）：沒有牌價的模型要出聲——以前它們被當成 $0，看起來像「這個模型沒花錢」。 */}
          {data.unpricedModels !== undefined && (
            <div className="sec">
              <div className="sec-label zh">沒有牌價的模型</div>
              {data.unpricedModels.length === 0 ? (
                <p className="foot-note zh">帳本裡的模型都對得到官方牌價。</p>
              ) : (
                <>
                  <table className="tbl">
                    <thead>
                      <tr>
                        <th>模型</th>
                        <th className="r">呼叫</th>
                        <th className="r">TOKENS</th>
                        <th className="r">出現期間</th>
                      </tr>
                    </thead>
                    <tbody>
                      {data.unpricedModels.map((u) => (
                        <tr key={u.model} title={u.model}>
                          <td style={{ fontSize: 10.5 }}>{u.label ?? u.model}</td>
                          <td className="r mut">{u.calls.toLocaleString()}</td>
                          <td className="r mut">{fmtTokens(u.tokens)}</td>
                          <td className="r mut" style={{ fontSize: 9.8 }}>
                            {localDayOf(u.firstSeen)?.slice(5) ?? "—"} ～ {localDayOf(u.lastSeen)?.slice(5) ?? "—"}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                  <p className="foot-note zh" style={{ marginTop: 4 }}>
                    官方價目表找不到這些模型的價格，所以它們的用量換不成美元：模型分佈寫「無牌價」；用到它們的那幾段配對記錄不算進匯率（美元算不全，留著會把匯率拉低）；期間消耗也不含它們。
                  </p>
                </>
              )}
            </div>
          )}
        </article>
      </div>

      {/* 缺口 */}
      <article className="card">
        <div className="card-head">
          <span className="card-title zh">最近的觀測缺口</span>
          <span className="card-tag">GAPS · 共 {data.gapCount} 段</span>
        </div>
        {data.recentGaps.length === 0 ? (
          <p className="foot-note zh" style={{ marginTop: 8 }}>
            沒有記錄到缺口。
          </p>
        ) : (
          <div style={{ maxHeight: 180, overflow: "auto", marginTop: 8 }}>
            {data.recentGaps.map((g, i) => (
              <div key={i} className="mini-row" style={{ justifyContent: "space-between" }}>
                <span>{GAP_SOURCE_LABEL[g.source] ?? g.source}</span>
                <span style={{ fontVariantNumeric: "tabular-nums" }}>
                  {/* v1.8.13（D91 T-13）：以前直接切 UTC 字串當本地時間（差 8 小時、跨日像倒著走） */}
                  {localSpan(g.startedAt, g.endedAt)}
                </span>
              </div>
            ))}
          </div>
        )}
      </article>

      <div className="flex justify-end">
        <button className="h5btn" onClick={() => void load()} disabled={loading}>
          <RefreshCw size={11} className={loading ? "animate-spin" : ""} />
          <span className="zh">重新整理</span>
        </button>
      </div>
    </div>
  );
}

function StatTile({ label, value }: { label: string; value: string }) {
  return (
    <div className="stat-tile">
      <p className="lbl">{label}</p>
      <p className="val">{value}</p>
    </div>
  );
}
