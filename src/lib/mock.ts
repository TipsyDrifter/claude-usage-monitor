import type { UsageSnapshot, UsageState, SeatInfo } from "./types";

const minutesFromNow = (h: number, m: number): string =>
  new Date(Date.now() + (h * 60 + m) * 60_000).toISOString();

export const MOCK_SNAPSHOT: UsageSnapshot = {
  planName: "Max (5x)",
  currentSession: {
    usedPercent: 72,
    resetAt: minutesFromNow(2, 59),
    note: null,
  },
  weeklyAllModels: {
    usedPercent: 13,
    resetAt: minutesFromNow(16, 37),
    note: null,
  },
  weeklyFable: {
    usedPercent: 94,
    resetAt: minutesFromNow(16, 37),
    note: "cli · 2 分前",
  },
  scrapedAt: new Date().toISOString(),
};

/** v1.9.5（D100）：demo 的兩種新狀態（瀏覽器驗證用）——
 *  `?na=fable,7d,5h`＝那幾條「此方案沒有這條」；`?extra=1`＝開了超額付費、5h 已用滿。 */
function demoVariant(s: UsageSnapshot): UsageSnapshot {
  if (typeof window === "undefined") return s;
  const q = new URLSearchParams(window.location.search);
  const na = new Set((q.get("na") ?? "").split(",").map((x) => x.trim().toLowerCase()));
  const off = { usedPercent: null, resetAt: null, note: null, notApplicable: true };
  const out: UsageSnapshot = { ...s };
  if (na.has("5h")) out.currentSession = off;
  if (na.has("7d")) out.weeklyAllModels = off;
  if (na.has("fable")) out.weeklyFable = off;
  if (q.get("extra") === "1") {
    out.currentSession = { ...out.currentSession, usedPercent: 100 };
    out.extraUsage = { enabled: true, active: true, spendLimitReached: false, disabledReason: null };
  }
  return out;
}

export const MOCK_STATE: UsageState = {
  status: "ok",
  data: demoVariant(MOCK_SNAPSHOT),
  error: null,
  lastSuccessAt: new Date().toISOString(),
  nextRefreshAt: null,
};

/** v1.3 demo：兩個座位（第一個＝目前登入）。 */
export const MOCK_SEATS: SeatInfo[] = [
  {
    id: "seat-demo-a",
    accountUuid: "df258ff1-0000-0000-0000-000000000000",
    orgUuid: "ce2ad5b8-0000-0000-0000-000000000000",
    email: "kosa@example.com",
    orgName: "Kosa's Organization",
    lastSeenAt: new Date().toISOString(),
    liveSamples: 5623,
    lastSampleAt: new Date().toISOString(),
  },
  {
    id: "seat-demo-b",
    accountUuid: "646c6f2e-0000-0000-0000-000000000000",
    orgUuid: "8dd80005-0000-0000-0000-000000000000",
    email: "school@example.edu",
    orgName: "School Org",
    lastSeenAt: new Date(Date.now() - 6 * 3600_000).toISOString(),
    liveSamples: 470,
    lastSampleAt: new Date(Date.now() - 6 * 3600_000).toISOString(),
  },
];

export const MOCK_SEAT_SNAPSHOT: UsageSnapshot = {
  planName: null,
  currentSession: { usedPercent: 31, resetAt: minutesFromNow(-3, 10), note: "最後已知值" },
  weeklyAllModels: { usedPercent: 58, resetAt: minutesFromNow(40, 0), note: "最後已知值" },
  weeklyFable: { usedPercent: 12, resetAt: minutesFromNow(40, 0), note: "最後已知值" },
  scrapedAt: new Date(Date.now() - 6 * 3600_000).toISOString(),
};

export function isDemoMode(): boolean {
  if (typeof window === "undefined") return false;
  const params = new URLSearchParams(window.location.search);
  return params.has("demo");
}
