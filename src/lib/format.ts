export function formatPercent(value: number | null | undefined): string {
  if (value == null || Number.isNaN(value)) return "—";
  return `${Math.round(value)}%`;
}

export function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, value));
}

export function formatResetCountdown(resetIso: string | null | undefined): string {
  if (!resetIso) return "—";
  const target = new Date(resetIso).getTime();
  const now = Date.now();
  const diff = target - now;
  if (diff <= 0) return "重置中";

  const totalMinutes = Math.floor(diff / 60_000);
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  const days = Math.floor(hours / 24);

  if (days >= 1) return `${days}d ${hours % 24}h`;
  if (hours >= 1) return `${hours}h ${minutes}m`;
  return `${minutes}m`;
}

export function formatResetCountdownVerbose(
  resetIso: string | null | undefined,
): string {
  if (!resetIso) return "—";
  const target = new Date(resetIso).getTime();
  const now = Date.now();
  const diff = target - now;
  if (diff <= 0) return "重置中";

  const totalMinutes = Math.floor(diff / 60_000);
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  const days = Math.floor(hours / 24);

  if (days >= 1) return `${days} 天 ${hours % 24} 小時後重置`;
  if (hours >= 1) return `${hours} 小時 ${minutes} 分後重置`;
  return `${minutes} 分後重置`;
}

// v1.9.4（D1）：「多久以前」只有一個定義——`lib/ago.ts fmtAgo`（搭 `lib/useTick.ts` 每分鐘重畫）。
// 這裡原本的 formatTimeAgo 規則不一樣（多一段「N 秒前」、沒有「天前」），已刪除。
