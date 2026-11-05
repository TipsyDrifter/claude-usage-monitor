import type { ReactNode } from "react";

// =============================================================================
// v1.7（D81）：「怎麼算的 ▸」摺疊——術語版說明的家。
// 主文只留一句大白話，統計術語（修剪、Δ-加權、bootstrap、置換檢定、固定籃…）
// 全收進這裡給想查的人。用原生 <details>：鍵盤可開、無 JS 狀態、預設收起。
// 證據匯出的 method 段不走這個元件，那邊維持原術語（給質疑者看的）。
// =============================================================================

export function How({ children, label = "怎麼算的" }: { children: ReactNode; label?: string }) {
  return (
    <details className="how zh">
      <summary>{label}</summary>
      <div className="how-body">{children}</div>
    </details>
  );
}
