import { projectLabel } from "@/lib/projectLabel";

// =============================================================================
// 模型 × 專案並排兩欄（按輸出 tokens）——歷史頁「這段期間用了什麼」與今日頁
// 「這個 5h 窗用了什麼」共用，兩邊只差資料範圍。
// =============================================================================

export interface ModelMix { model: string; label?: string; calls: number; outputTokens: number }
export interface ProjectMix { project: string; calls: number; outputTokens: number }

export const fmtTokens = (n: number) =>
  n >= 1_000_000 ? `${(n / 1_000_000).toFixed(1)}M` : n >= 1000 ? `${Math.round(n / 1000)}k` : String(n);

export function UsageMix({ byModel, byProject }: { byModel: ModelMix[]; byProject: ProjectMix[] }) {
  return (
    <div className="grid gap-3 md:grid-cols-2" style={{ marginTop: 8 }}>
      <div>
        <div className="ml-title zh">模型 · 按輸出 tokens</div>
        <table className="tbl">
          <tbody>
            {byModel.map((m) => (
              <tr key={m.model}>
                <td style={{ fontSize: 10.5 }} title={m.model}>{m.label ?? m.model}</td>
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
            {byProject.map((p) => (
              <tr key={p.project}>
                <td style={{ fontSize: 10.5 }} title={p.project}>{projectLabel(p.project)}</td>
                <td className="r mut">{fmtTokens(p.outputTokens)} · {p.calls.toLocaleString()} 次</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
