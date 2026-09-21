import { useEffect, useState } from "react";
import { RefreshCw, BellOff } from "lucide-react";
import { useStore } from "@/store/usageStore";
import { cmd } from "@/lib/tauri";
import { formatTimeAgo } from "@/lib/format";
import { seatLabel, seatShortId } from "@/lib/seatLabel";
import { PageSkeleton } from "../Motion";
import "../h5.css";

// =============================================================================
// 資料健康度 (M1, D55 H5 換裝) — "時間從此為我們工作" 的儀表板。
// 加碼 D55：門鈴白響提醒（連續 ≥3 次 webhook 響但 CLI 數字沒動 →
// 瀏覽器可能登著第三帳號，門鈴形同虛設）。
// =============================================================================

interface HealthData {
  doorbellFutileCount: number;
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
  topModels: { model: string; calls: number; outputTokens: number }[];
  eventsSpan: { first: string | null; last: string | null };
}

export function HealthPage() {
  const usage = useStore((s) => s.usage);
  const aliases = useStore((s) => s.settings.accounts.aliases);
  const [data, setData] = useState<HealthData | null>(null);
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
                  <td className="r mut">{s.last ? formatTimeAgo(s.last) : "—"}</td>
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

      {/* 匯率佔位＋模型分佈 */}
      <div className="grid gap-3 md:grid-cols-2">
        <article className="card">
          <div className="card-head">
            <span className="card-title zh">額度匯率</span>
            <span className="card-tag">FX</span>
          </div>
          <p className="zh" style={{ fontSize: 12, color: "var(--h5-ink2)", marginTop: 8 }}>
            粗估已上線（見「趨勢」）
            <span className="lim-chip zh" style={{ marginLeft: 8 }}>
              信心區間 M5 轉正
            </span>
          </p>
          <p className="foot-note" style={{ marginTop: 4 }}>
            記帳起點:{data.eventsSpan.first?.slice(0, 10) ?? "—"} ～ {data.eventsSpan.last?.slice(0, 10) ?? "—"}
          </p>
        </article>
        <article className="card">
          <div className="card-head">
            <span className="card-title zh">模型分佈</span>
            <span className="card-tag">BY OUTPUT TOKENS</span>
          </div>
          <table className="tbl" style={{ marginTop: 8 }}>
            <tbody>
              {data.topModels.slice(0, 5).map((m) => (
                <tr key={m.model}>
                  <td style={{ fontSize: 10.5 }}>{m.model}</td>
                  <td className="r mut">{(m.outputTokens / 1_000_000).toFixed(1)}M</td>
                </tr>
              ))}
            </tbody>
          </table>
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
                <span>{g.source}</span>
                <span style={{ fontVariantNumeric: "tabular-nums" }}>
                  {g.startedAt.slice(0, 16).replace("T", " ")} → {g.endedAt.slice(11, 16)}
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
