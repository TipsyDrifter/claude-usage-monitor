// =============================================================================
// v1.9.4（D99）· 方案短名的唯一定義。
// 後端的方案顯示名是「Claude Max 20x」（types.rs plan_label）；圖上段標、表格、句子裡
// 一再出現時去掉「Claude 」前綴讀起來比較短。沒有方案（從沒被記到）寫「方案未知」。
// =============================================================================

export function planShort(label: string | null | undefined): string {
  return label ? label.replace(/^Claude\s+/, "") : "方案未知";
}

/** 句子裡所有「Claude X」都縮短（後端寫好的句子，例如「這期間換過方案（Claude Max 5x→Claude Max 20x，10-09）」）。 */
export function planShortAll(text: string): string {
  return text.replace(/Claude\s+(?=(Max|Pro|Team|Enterprise|Free)\b)/g, "");
}
