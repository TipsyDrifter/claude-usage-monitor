import { useEffect, useState } from "react";

// =============================================================================
// v1.9.4（情境稽核 D1）· 每分鐘重畫一次——「N 分前」要會自己走。
//
// `fmtAgo`／`freshStamp`（lib/ago.ts）只會算「現在」那一刻；畫面不重畫，字就停在
// 上一次刷新時的樣子（以前今日頁頂端停在「16 分前」直到下一輪採集）。
// 懸浮窗自己有一個 60 秒 tick（widget/Widget.tsx）；統計視窗與設定視窗用這個。
// 回傳值是這一次重畫的「現在」（毫秒），直接傳給 fmtAgo(iso, now)。
// =============================================================================

export const TICK_MS = 60_000;

export function useMinuteTick(): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), TICK_MS);
    return () => window.clearInterval(id);
  }, []);
  return now;
}
