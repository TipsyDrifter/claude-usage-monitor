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

export function formatTimeAgo(iso: string | null | undefined): string {
  if (!iso) return "尚未抓取";
  const diff = Date.now() - new Date(iso).getTime();
  if (diff < 5_000) return "剛剛";
  if (diff < 60_000) return `${Math.floor(diff / 1000)} 秒前`;
  if (diff < 3600_000) return `${Math.floor(diff / 60_000)} 分前`;
  return `${Math.floor(diff / 3600_000)} 小時前`;
}
