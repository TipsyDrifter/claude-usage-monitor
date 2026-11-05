import { projectLabel } from "@/lib/projectLabel";

// =============================================================================
// 模型 × 專案並排兩欄（按 API 計價，D97）——歷史頁「這段期間用了什麼」與今日頁
// 「這個 5h 窗用了什麼」共用，兩邊只差資料範圍。
// =============================================================================

/** 後端 `usage_mix` 每一列都帶的量：呼叫數、API 計價、四種 tokens。
 *  v1.9.4（D99 B2）：`usd` 可能是 null（整組都沒有官方牌價）；`unpricedCalls > 0` 時 `usd`
 *  只是有牌價那部分的下限；`priceBasis`／`familyPriced` 說價格是不是按家族估的。 */
export interface MixAmounts {
  calls: number;
  usd: number | null;
  inputTokens: number;
  cacheWriteTokens: number;
  cacheReadTokens: number;
  outputTokens: number;
  unpricedCalls?: number;
  familyPriced?: boolean;
}
export interface ModelMix extends MixAmounts {
  model: string;
  label?: string;
  /** exact＝官方價目表有這個版本；family＝沒收錄，按同家族的價估；none＝沒有牌價。 */
  priceBasis?: "exact" | "family" | "none";
}
export interface ProjectMix extends MixAmounts { project: string }

export const fmtTokens = (n: number) =>
  n >= 1_000_000 ? `${(n / 1_000_000).toFixed(1)}M` : n >= 1000 ? `${Math.round(n / 1000)}k` : String(n);

/** v1.9.4（D99 B2）：後端對「整組都沒有牌價」的模型／專案回 `usd: null`（不是 0）——顯示「無牌價」，絕不能拋錯（舊版 `null.toFixed` 會讓整頁白屏）。 */
export const fmtUsd = (v: number | null | undefined) =>
  v == null ? "無牌價" : v >= 100 ? `$${Math.round(v).toLocaleString()}` : `$${v.toFixed(2)}`;

/** 一列的計價：整組沒牌價→「無牌價」；部分沒牌價→「≥$X」（只算得到有牌價的那部分）。
 *  分佈表、窗口詳情、今日頁來源表都走這裡，同一種情況同一種寫法。 */
export const fmtMixUsd = (m: Pick<MixAmounts, "usd" | "unpricedCalls">) =>
  m.usd == null ? "無牌價" : (m.unpricedCalls ?? 0) > 0 ? `≥${fmtUsd(m.usd)}` : fmtUsd(m.usd);

/** 列尾的價格附註（滑鼠移上去看）：沒牌價、部分沒牌價、按家族估價。沒有附註回空字串。 */
export function priceNote(m: MixAmounts & { priceBasis?: string }): string {
  if (m.usd == null) return "這個模型官方價目表沒有牌價，用量照算、美元不算";
  const u = m.unpricedCalls ?? 0;
  const notes: string[] = [];
  if (u > 0) notes.push(`其中 ${u.toLocaleString()} 次呼叫的模型沒有牌價，美元只算有牌價的部分`);
  if (m.priceBasis === "family" || (m.priceBasis == null && m.familyPriced)) notes.push("官方價目表沒收錄這個版本，按同家族的價格估");
  else if (m.familyPriced) notes.push("含官方價目表沒收錄的版本，那部分按同家族的價格估");
  return notes.join("；");
}

/** 滑鼠移上去看的拆解：API 計價是這四種 tokens 照牌價折算的。 */
export const mixBreakdown = (m: MixAmounts & { priceBasis?: string }) => {
  const base = `輸入 ${fmtTokens(m.inputTokens)} · 寫快取 ${fmtTokens(m.cacheWriteTokens)} · 讀快取 ${fmtTokens(m.cacheReadTokens)} · 輸出 ${fmtTokens(m.outputTokens)}`;
  const note = priceNote(m);
  return note ? `${base}\n${note}` : base;
};

/** 價格是按家族估的時，在數字後面掛一個小「估」字，滑鼠移上去看原因。
 *  （審查 23r N3：以前用「*」，趨勢頁又拿「＊」表示跨方案，同一個符號兩種意思。） */
export function PriceMark({ m }: { m: MixAmounts & { priceBasis?: string } }) {
  if (m.usd == null) return null;
  const est = m.priceBasis === "family" || !!m.familyPriced;
  if (!est) return null;
  return (
    <span title={priceNote(m)} style={{ marginLeft: 3, fontSize: 9, color: "var(--h5-ink3)" }}>
      估
    </span>
  );
}

/** 一組分佈裡有沒有「無牌價」或「按家族估價」的列——有的話表尾要說一句。 */
export function mixPriceFootnote(rows: (MixAmounts & { priceBasis?: string })[]): string {
  const none = rows.filter((r) => r.usd == null).length;
  const partial = rows.some((r) => r.usd != null && (r.unpricedCalls ?? 0) > 0);
  const est = rows.some((r) => r.usd != null && (r.priceBasis === "family" || r.familyPriced));
  const parts: string[] = [];
  if (none > 0) parts.push("「無牌價」＝官方價目表沒有這個模型的價格，用量照算、美元不算");
  if (partial) parts.push("「≥」＝裡面有沒牌價的模型，美元只算得到有牌價的那部分");
  if (est) parts.push("「估」＝官方價目表沒收錄這個版本，按同家族的價格估");
  return parts.join("；");
}

export function UsageMix({ byModel, byProject }: { byModel: ModelMix[]; byProject: ProjectMix[] }) {
  const note = mixPriceFootnote([...byModel, ...byProject]);
  return (
    <>
      <div className="grid gap-3 md:grid-cols-2" style={{ marginTop: 8 }}>
        <div>
          <div className="ml-title zh">模型 · 按 API 計價</div>
          <table className="tbl">
            <tbody>
              {byModel.map((m) => (
                <tr key={m.model} title={`${m.model}\n${mixBreakdown(m)}`}>
                  <td style={{ fontSize: 10.5 }}>{m.label ?? m.model}</td>
                  <td className="r mut">
                    {fmtMixUsd(m)}
                    <PriceMark m={m} /> · {m.calls.toLocaleString()} 次
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <div>
          <div className="ml-title zh">專案 · 按 API 計價</div>
          <table className="tbl">
            <tbody>
              {byProject.map((p) => (
                <tr key={p.project} title={`${p.project}\n${mixBreakdown(p)}`}>
                  <td style={{ fontSize: 10.5 }}>{projectLabel(p.project)}</td>
                  <td className="r mut">
                    {fmtMixUsd(p)}
                    <PriceMark m={p} /> · {p.calls.toLocaleString()} 次
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
      {note && (
        <p className="foot-note zh" style={{ marginTop: 6 }}>
          {note}。
        </p>
      )}
    </>
  );
}
