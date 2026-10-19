import type { UsageSnapshot, UsageState, SeatInfo, BudgetLine, BudgetLines } from "./types";

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

/** v1.10（D101）：瀏覽器 demo／fixture 的 `get_budget_lines`。預設＝7d 線下（ok）、Fable 越線（over）；
 *  `?budget=lastday`＝7d 今天重置；`?budget=noreset`＝7d 不知道重置時間；`?budget=windowstart`＝7d 今天中途剛重置；
 *  `?budget=firstafter`＝7d 起點用今天第一筆；`?budget=off`＝預算關掉。數字跟 MOCK_SNAPSHOT 對得上。 */
export function mockBudgetLines(): BudgetLines {
  const q = typeof window === "undefined" ? new URLSearchParams() : new URLSearchParams(window.location.search);
  const v = (q.get("budget") ?? "").toLowerCase();
  if (v === "off") return { enabled: false };
  const now = new Date();
  const start = new Date(now);
  start.setHours(6, 0, 0, 0);
  if (now.getHours() < 6) start.setDate(start.getDate() - 1);
  const end = new Date(start.getTime() + 86400_000);
  const base = (over: Partial<BudgetLine>): BudgetLine => ({
    status: "ok",
    line: null,
    percent: null,
    over: false,
    dayStart: start.toISOString(),
    dayEnd: end.toISOString(),
    resetsAt: minutesFromNow(16, 37),
    anchorAt: start.toISOString(),
    anchorPercent: null,
    anchorKind: "dayStart",
    ...over,
  });
  const wp = MOCK_SNAPSHOT.weeklyAllModels.usedPercent ?? 0;
  const fp = MOCK_SNAPSHOT.weeklyFable?.usedPercent ?? 0;
  let weekly = base({ line: 25.4, percent: wp, anchorPercent: 6 });
  if (v === "lastday") weekly = base({ status: "lastDay", line: 100, percent: wp, anchorPercent: 6 });
  if (v === "noreset") weekly = base({ status: "noReset", percent: wp, resetsAt: null, anchorAt: null, anchorKind: null });
  if (v === "windowstart")
    weekly = base({ line: 14.3, percent: wp, anchorPercent: 0, anchorKind: "windowStart", anchorAt: new Date(now.getTime() - 2 * 3600_000).toISOString() });
  if (v === "firstafter")
    weekly = base({ line: 25.4, percent: wp, anchorPercent: 9, anchorKind: "firstAfter", anchorAt: new Date(start.getTime() + 75 * 60_000).toISOString() });
  const fable = base({ line: 85.2, percent: fp, over: fp > 85.2, anchorPercent: 70 });
  return { enabled: true, seatId: MOCK_SEATS[0].id, dayStartHour: 6, weekly, fable };
}
