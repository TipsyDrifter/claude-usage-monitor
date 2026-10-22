// 專案名字的唯一來源（今日頁、歷史頁、窗口詳情共用）。
// 後端給的是 ~/.claude/projects/ 底下的資料夾名（如 C--dev-NextStop），
// worktree 已在後端 PROJECT_KEY_SQL 歸回主專案；這裡只負責縮成「上一層/專案」。
export const projectLabel = (dir: string) => dir.replace(/^C--/, "").split("-").slice(-2).join("/");
