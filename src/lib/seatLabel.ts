// v1.3 兩個帳號（D67-Q9、D75）：座位名字的唯一來源。
// 別名 → 「組織名 · email」→ 只有其中一個就顯示那個 → 都沒有退回 uuid 前 8 碼。
// 懸浮窗、三個統計頁、設定頁、證據卡都從這裡拿名字，不准各自拼字串。

export interface SeatLike {
  accountUuid: string | null;
  orgUuid: string | null;
  email?: string | null;
  orgName?: string | null;
}

/** 別名的 key：帳號 × 組織這一對（跨 ledger 重建穩定；seat id 不穩）。 */
export function seatKey(s: SeatLike): string {
  return `${s.accountUuid ?? ""}|${s.orgUuid ?? ""}`;
}

export function seatDefaultLabel(s: SeatLike): string {
  const org = s.orgName?.trim();
  const mail = s.email?.trim();
  if (org && mail) return `${org} · ${mail}`;
  if (mail) return mail;
  if (org) return org;
  return s.accountUuid ? s.accountUuid.slice(0, 8) : "帳號未知";
}

export function seatLabel(s: SeatLike, aliases: Record<string, string>): string {
  const alias = aliases[seatKey(s)]?.trim();
  return alias || seatDefaultLabel(s);
}

/** v1.8.11（D89，主人）：全量視角＝不分帳號。後端認這個 id，前端也用同一個。 */
export const ALL_SEATS = "*";
export const ALL_SEATS_LABEL = "全部帳號（不分帳號）";

/** 短 id 給次要文字用（SEATS 卡、證據卡檔名）。 */
export function seatShortId(s: SeatLike): string {
  return `${s.accountUuid ? s.accountUuid.slice(0, 8) : "?"} × ${s.orgUuid ? s.orgUuid.slice(0, 8) : "?"}`;
}
