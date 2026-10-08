import { projectLabel } from "@/lib/projectLabel";

// =============================================================================
// 模型 × 專案並排兩欄（按 API 計價，D97）——歷史頁「這段期間用了什麼」與今日頁
// 「這個 5h 窗用了什麼」共用，兩邊只差資料範圍。
// =============================================================================

/** 後端 `usage_mix` 每一列都帶的量：呼叫數、API 計價、四種 tokens。 */
export interface MixAmounts {
  calls: number;
  usd: number;
  inputTokens: number;
  cacheWriteTokens: number;
  cacheReadTokens: number;
  outputTokens: number;
}
export interface ModelMix extends MixAmounts { model: string; label?: string }
export interface ProjectMix extends MixAmounts { project: string }

export const fmtTokens = (n: number) =>
  n >= 1_000_000 ? `${(n / 1_000_000).toFixed(1)}M` : n >= 1000 ? `${Math.round(n / 1000)}k` : String(n);

export const fmtUsd = (v: number) => (v >= 100 ? `$${Math.round(v).toLocaleString()}` : `$${v.toFixed(2)}`);

/** 滑鼠移上去看的拆解：API 計價是這四種 tokens 照牌價折算的。 */
export const mixBreakdown = (m: MixAmounts) =>
  `輸入 ${fmtTokens(m.inputTokens)} · 寫快取 ${fmtTokens(m.cacheWriteTokens)} · 讀快取 ${fmtTokens(m.cacheReadTokens)} · 輸出 ${fmtTokens(m.outputTokens)}`;

export function UsageMix({ byModel, byProject }: { byModel: ModelMix[]; byProject: ProjectMix[] }) {
  return (
    <div className="grid gap-3 md:grid-cols-2" style={{ marginTop: 8 }}>
      <div>
        <div className="ml-title zh">模型 · 按 API 計價</div>
        <table className="tbl">
          <tbody>
            {byModel.map((m) => (
              <tr key={m.model} title={`${m.model}\n${mixBreakdown(m)}`}>
                <td style={{ fontSize: 10.5 }}>{m.label ?? m.model}</td>
                <td className="r mut">{fmtUsd(m.usd)} · {m.calls.toLocaleString()} 次</td>
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
                <td className="r mut">{fmtUsd(p.usd)} · {p.calls.toLocaleString()} 次</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
