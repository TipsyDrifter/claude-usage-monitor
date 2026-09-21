# Claude Usage Monitor

> 一個住在 Windows 系統匣裡的小工具：不用開分頁就看得到 Claude 訂閱額度還剩多少，
> 而且把每一次讀到的真值記進帳本——半年後，「額度有沒有縮水」這個問題會有答案。

![version](https://img.shields.io/badge/version-1.8.6-4E7ECF)
![platform](https://img.shields.io/badge/platform-Windows%2011-blue)
![framework](https://img.shields.io/badge/framework-Tauri%202-yellow)
![license](https://img.shields.io/badge/license-MIT-lightgrey)

**Not affiliated with Anthropic.** This is an independent, open-source (MIT) Windows tray + floating-widget app that shows your Claude subscription usage (5h / 7d / Fable windows) and keeps a local ledger of every reading so you can tell, months later, whether your quota quietly shrank. It reads only local files written by the official Claude Code CLI and Claude Desktop — it never scrapes claude.ai and never touches your credentials. Chinese (Traditional) UI.

非官方工具，與 Anthropic 無關。不爬網頁、不碰憑證、所有資料留在本機。

## 它做什麼

- **餘光**：系統匣圖示＋桌面懸浮窗（三段密度：微型條／標準卡／展開面板；三種外觀：深色儀器／毛玻璃光暈／環形），5h／7d／Fable 三條額度常駐螢幕角落；全螢幕自動閃避、位置記憶。
- **今天撞牆嗎**（決策層）：即時額度單卡三列——燃燒速度、撐不撐得到重置、觸頂時刻或重置時百分比；本窗爬升階梯圖；近 14 天方案建議。
- **趨勢**（研究層，產品的靈魂）：額度匯率（1% 額度約當多少美金 API 用量）月走勢＋95% 信心區間；縮水判定走四項檢查（樣本數／5h 匯率變化／7d 方向一致／固定模型組合），任一沒過只呈現數據、不宣告；分模型倍率（回歸法）；尖峰時段熱圖；期間消耗與撞牆雙訊號。
- **歷史檢視**：日／週／月翻頁瀏覽過去每個 5h／7d／Fable 窗口的峰值、樣本數、被限流記錄。
- **拿得出去的證據**：一鍵匯出 1200×630 圖卡 PNG、JSON（含欄位定義與每段配對區間）、CSV、Markdown 報告——每個數字與畫面同源、可由區間 CSV 重算。
- **兩級告警**：留意值／警戒值自己定；懸浮窗變色、系統匣圖示變色、系統通知。
- **兩個帳號**：機器上登過幾個 Claude 帳號就有幾個座位，切著看，各自的帳本不混。
- **資料健康度**：樣本數、覆蓋率、缺口、跨帳號提醒、門鈴白響提醒。

## 安裝

到 [Releases](https://github.com/TipsyDrifter/claude-usage-monitor/releases) 下載最新的 `Claude Usage Monitor_x.y.z_x64-setup.exe`（或 `.msi`），雙擊安裝。

**Windows 會攔一下（SmartScreen）**：安裝包目前沒有付費簽章，第一次執行會出現「Windows 已保護您的電腦」。按 **其他資訊 → 仍要執行** 就好。Release 頁附 SHA256，想確認下載的東西沒被動過可以對一下，或自己從原始碼 `npm run tauri build` 一份對照。

**需要什麼**

| 想要的 | 需要 |
|---|---|
| 隨叫隨到的即時額度（三條百分比、重置時刻） | 裝 [Claude Code](https://docs.anthropic.com/claude-code) CLI 並 `claude login` 過一次 |
| 30 天歷史（15 分鐘一筆） | 裝 Claude Desktop 並登入 |
| 額度匯率、來源歸因、分模型倍率 | 用 Claude Code 寫程式的本機對話記錄（自動讀，只讀 token 用量與限流事件） |

只裝 Desktop、沒登入 CLI 也能跑：App 會安靜地退回 Desktop 的 15 分鐘歷史，不會報錯，只是沒有即時值。要即時就 `claude login` 一次。

**第一次啟動**：設定視窗可開「開機自動啟動」——**App 要常駐，帳本才會一直累積**。系統匣圖示右鍵開統計視窗；懸浮窗 hover 可切密度、開統計、開設定。匯出的證據在 `%APPDATA%\com.kosa.claude-usage-monitor\exports\`；帳本 `ledger.sqlite` 每日自動備份到 `backups\`。

**升級與退回**：直接跑新版安裝包就會蓋掉舊版，資料夾不動。新版有問題想退回，到 Releases 抓上一版的安裝包再跑一次即可；帳本與設定都留在 `%APPDATA%\com.kosa.claude-usage-monitor\`，不會因為裝舊版而消失。

**遇到問題**：設定 → 系統 → 「打包診斷資料」會得到一個 zip（採集紀錄、遮掉密碼牌的設定、資料健康度；不含對話與帳本），到 [Issues](https://github.com/TipsyDrifter/claude-usage-monitor/issues) 開一則附上就好。這個 App 沒有任何遙測，不會自己回報任何東西——你不說，開發者不知道。

## 已知限制

- 只在 Windows 11 x64 上開發與測試過；Windows 10 理論上可跑（需要 WebView2），但沒驗過。
- 安裝包未簽章。開著 **Smart App Control** 的 Windows 11 機器可能直接封鎖未簽章的安裝檔，那要先關掉它或自己從原始碼 build。
- 懸浮窗的「毛玻璃」外觀在 Tauri 透明視窗下只模糊自己的內容，不會透出桌布。
- 額度匯率、縮水判定等統計要有足夠樣本才會出結論；剛裝好的頭幾週趨勢頁大多是「資料還不夠」。
- 只認 Claude 一家；GitHub Copilot 等其他訂閱沒有做。

## 資料從哪來（全部合規、全部本機）

| 管道 | 來源 | 用途 |
|---|---|---|
| A 探針 | `claude -p "/usage"`（直接起 `claude.exe`，0 額度、不經 shell） | 隨叫真值，三條百分比 |
| B Desktop 歷史 | `%APPDATA%\Claude\plan-usage-history.json` | 30 天歷史，15 分鐘一筆 |
| C CLI 快取 | `~/.claude.json` 的 `cachedUsageUtilization` | 結構化重置時刻、帳號錨 |
| 本機對話記錄 | `~/.claude/projects/**/*.jsonl`（只讀 token 用量與限流事件） | 匯率的分子、來源歸因 |

**明確不做**：爬 claude.ai 網頁（消費者條款明文禁止、有封鎖前例）、借用 `~/.claude/.credentials.json` 的 token 打 API（官方法遵文件明文禁止第三方收集或轉用憑證）、任何雲端同步。

## 門鈴：讓數字更即時（選配）

App 什麼時候去問 CLI，靠的是「有人剛用過 Claude」的訊號——我們叫它**門鈴**。兩個門鈴共用同一個本機位址（`http://localhost:17819/refresh`）與密碼牌，都在設定 → 系統 → 「門鈴」區塊：

| 門鈴 | 什麼時候響 | 怎麼裝 |
|---|---|---|
| Claude Code Stop hook | Claude Code 每回覆完一輪 | 設定頁按「一鍵安裝 Claude Code Hook」 |
| 瀏覽器擴充 | 你在 Chrome／Edge 對 claude.ai 送出訊息 | 開發人員模式載入 `extension/`，步驟見 [`extension/README.md`](extension/README.md) |

門鈴只知道「有人按了」，不夾帶內容。瀏覽器擴充**只看「有沒有對 claude.ai 送出請求」**，不讀對話、不碰 claude.ai，也**不上商店**（原因寫在它的 README）。

## 開發

- Windows 11、Node.js 22+、Rust stable（MSVC）、VS Build Tools 2022、WebView2。

```bash
npm install
npm run tauri dev          # 開發實例（會跟著終端機一起關）
npm run tauri build        # 安裝包：src-tauri/target/release/bundle/{nsis,msi}
```

```bash
cd src-tauri && cargo test --lib                     # 統計核心單元測試（stats.rs）
CUM_LEDGER_DIR="$APPDATA/com.kosa.claude-usage-monitor" CUM_PROBE_OUT=probe-out cargo test --lib probe
CUM_LEDGER_DIR="$APPDATA/com.kosa.claude-usage-monitor" CUM_TIMING=1 cargo test --lib probe::timing -- --nocapture   # 各頁後端耗時
```

純瀏覽器對照（不用 Tauri）：把探針倒出的 JSON 放到 `prototypes/fixtures/<command>.json`（資料夾自己建，不進版控），開 `http://localhost:1420/statistics.html?fixture=1`。

版本記錄見 [CHANGELOG.md](CHANGELOG.md)。

## 技術棧

| 層 | 技術 |
|---|---|
| 前端 | React 19 · TypeScript · Tailwind CSS v4 · Framer Motion · Zustand |
| 後端 | Rust · Tauri 2 · rusqlite（`ledger.sqlite`） · Axum（本機 webhook） |
| 統計 | `src-tauri/src/stats.rs`：修剪 Δ-加權比值、bootstrap CI、置換檢定、固定籃指數、加權 NNLS 回歸 |
| 視覺 | H5 設計系統（`src/statistics/h5.css`）＋懸浮窗三張臉（W5／W1／W4） |

## 隱私與授權

非官方工具，與 Anthropic 無關；「Claude」是 Anthropic 的商標，這裡只用來描述這個工具監看的是什麼。只讀你自己機器上、你自己帳號的資料；所有資料留在本機，不上傳、沒有遙測。

MIT License，見 [LICENSE](LICENSE)。
