# Claude Usage Monitor 門鈴（瀏覽器擴充）

> 給 Chrome／Edge 的小擴充：你在瀏覽器對 claude.ai 送出訊息時，它按一下桌面 App 的門鈴，
> App 立刻去查一次額度，數字不用等下一次定時查詢才動。
>
> **它只看「有沒有送出請求」，不讀對話內容、不碰 claude.ai。** 查額度的事全交給桌面 App
> 走官方 CLI（`claude -p "/usage"`）。
>
> 不上 Chrome Web Store／Edge Add-ons，只能用「開發人員模式」從這個資料夾載入（sideload）。
> 需要桌面 App **v1.8.2 以上**（舊版 App 也能收到門鈴，只是 log 裡看不到是誰按的）。

## 為什麼要有門鈴

桌面 App 平常靠兩個訊號決定「該不該去查額度」：Claude Code 的對話記錄有沒有在動、以及
Claude Code 回覆完一輪的 Stop hook。你在**瀏覽器**用 claude.ai 時這兩個訊號都不會動，
額度照燒、數字卻停在舊值——最久要等閒置心跳（預設 45 分鐘）才會追上。

這顆擴充補的就是這個盲區：瀏覽器一送出訊息，門鈴就響，App 幾秒內就去查。

## 它到底做了什麼（可以自己對照原始碼，只有三個檔）

| 檔案 | 做什麼 |
|---|---|
| `background.js` | 用 `chrome.webRequest.onCompleted` 看到「POST 到 claude.ai 的 `…/ConversationService/PerformAction`（2026-09 起 claude.ai 送訊息走這條 RPC；舊的 `…/completion` 也認）」這件事**發生了**，等 3 秒，然後 `POST http://127.0.0.1:17819/refresh`（帶密碼牌）。 |
| `popup.html`／`popup.js` | 工具列小視窗：貼密碼牌、開關門鈴、按一次試試、看上次響鈴。 |
| `manifest.json` | 權限只有 `webRequest`＋`storage`；host 只有 `claude.ai`、`localhost`、`127.0.0.1`。 |

**明確不做**：不讀 request／response 內容、不注入任何腳本到 claude.ai 頁面、不碰 cookie 或
token、不對 claude.ai 發任何請求。密碼牌存在這台機器的 `chrome.storage.local`，不跟著
Google 帳號同步。

## 安裝（sideload）

### Chrome

1. 網址列輸入 `chrome://extensions`，右上角打開 **開發人員模式**。
2. 按 **載入未封裝項目**，選這個 `extension/` 資料夾。
3. 工具列會多一顆 Claude Usage Monitor 的圖示（可以在拼圖圖示裡把它釘出來）。

### Edge

1. 網址列輸入 `edge://extensions`，左下角打開 **開發人員模式**。
2. 按 **載入解壓縮的擴充功能**，選這個 `extension/` 資料夾。

> 兩邊都會警告「這是開發人員模式的擴充功能」——這是 sideload 的正常現象，不是它壞了。
> 不想每次開瀏覽器都被提醒，就得上商店；我們選擇不上（見下面〈為什麼不上商店〉）。

## 第一次設定：交鑰匙

1. 打開桌面 App 的 **設定** 視窗 → **系統** 卡 → **門鈴** 區塊 → 按密碼牌旁邊的 **複製**。
2. 點工具列的擴充圖示 → 按 **貼上**（或手動貼進「門鈴密碼牌」欄）。
3. 按 **按一次門鈴試試**，看到「✓ 桌面 App 收到了」就通了。

之後在 claude.ai 送一則訊息，擴充圖示上會閃一下綠色 ✓，桌面 App 的懸浮窗幾秒後跟著更新。

## 怎麼確認門鈴真的有響

- **擴充視窗**：「上次響鈴」那行會寫幾秒前、是手動測試還是看到訊息、App 有沒有收到。
- **桌面 App 的 log**：設定 → 系統 → 「打開 log 資料夾」→ `collector.log`，每次響鈴一行
  `"phase":"refresh-webhook"`，`source` 是 `extension`（真的看到訊息）、`extension-test`
  （手動按）或 `claude-code-hook`（Claude Code 的 Stop hook）。密碼牌不對也會留一行
  `"reason":"unauthorized"`。
- **資料健康度頁**：門鈴連響 3 次以上、CLI 的數字卻都沒動，頁上會提醒「瀏覽器登入的可能不是
  CLI 帳號」——瀏覽器和 CLI 要登同一個帳號，門鈴才有意義。

## 疑難排解

| 擴充視窗顯示 | 意思 | 怎麼辦 |
|---|---|---|
| ✓ 桌面 App 收到了 | 通了 | — |
| ✗ 還沒貼密碼牌 | 密碼牌欄是空的 | 從桌面 App 複製貼上 |
| ✗ 密碼牌不對（401） | App 重新產生過密碼牌 | 重新複製貼上 |
| ✗ 按不到門鈴 | App 沒開，或 port 改了 | 開 App；port 在「進階」對齊 |
| 門鈴已關閉 | 你關了開關 | 勾回「啟用門鈴」 |
| 剛按過了 | 1.5 秒防抖 | 等一下再按 |

桌面 App 沒開時門鈴會安靜失敗，不影響你正常用 claude.ai。

## 為什麼不上商店

上商店要過審核、要交隱私聲明，而 `webRequest`＋`claude.ai` 的權限組合在觀感上像在監看流量
（實際上只看「有沒有請求」）。這顆擴充服務的是「自己裝桌面 App 的人」，sideload 夠用，
就不去揹那套流程。原始碼就這三個檔，想確認它做了什麼直接讀。

## 從 v0.2 升上來

v0.2 把密碼牌放在 `chrome.storage.sync`。v1.8.2 第一次跑會自動搬到 `local`、把 sync 的清掉，
不用重貼。
