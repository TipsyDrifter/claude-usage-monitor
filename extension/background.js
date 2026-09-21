// Claude Usage Monitor 門鈴 — 背景腳本（service worker）
//
// 這顆擴充只做一件事（D54「合規門鈴」）：
//   1. 用 chrome.webRequest.onCompleted 看到「這個瀏覽器剛對 claude.ai 送出一則訊息」
//      這件事發生了（只看網址樣式與 HTTP 方法，不讀 request／response 內容、不碰 DOM）。
//   2. 等幾秒讓官方那邊把額度記上，然後對本機的桌面 App 按一下門鈴：
//      POST http://127.0.0.1:<port>/refresh（Authorization: Bearer <密碼牌>）。
//   3. 桌面 App 收到門鈴，自己去問官方 CLI（`claude -p "/usage"`）拿真值。
//
// 對 claude.ai 零請求、零讀取；密碼牌只存在這台機器的 chrome.storage.local。

const DEFAULTS = {
  webhookPort: 17819,
  webhookToken: "",
  delayMs: 3000,
  enabled: true,
};

const EXT_VERSION = chrome.runtime.getManifest().version;

// 只認「送出訊息」這類請求；其他 claude.ai 請求一律不理。
//
// 2026-09-21 實測（主人的 Chrome、performance resource entries）：claude.ai 前端已改走
// Connect RPC——送訊息＝POST /claudeai-rpc/…ConversationService/PerformAction，
// 回覆串流＝StreamTimeline。舊的 /api/organizations/…/chat_conversations/…/completion
// 已不再出現，樣式留著只是向下相容。StreamTimeline 刻意不聽：它可能是常駐的
// timeline 訂閱，聽了會讓門鈴一直響、把 App 的閒置暫停整個架空。
const SEND_PATTERNS = [
  /^https:\/\/claude\.ai\/claudeai-rpc\/[^/]*ConversationService\/PerformAction(\?|$)/,
  /^https:\/\/claude\.ai\/api\/organizations\/[^/]+\/chat_conversations\/[^/]+\/completion(\?|$)/,
  /^https:\/\/claude\.ai\/api\/organizations\/[^/]+\/chat_conversations\/[^/]+\/retry_completion/,
];

let pendingTimer = null;
let lastPingAt = 0;
const MIN_INTERVAL_MS = 1500; // 防抖：1.5 秒內不按第二次

async function getSettings() {
  // v0.2 把密碼牌放在 chrome.storage.sync（會跟著 Google 帳號同步到別台機器）。
  // v1.8.2 起改放 local；第一次跑到這裡順手搬家，搬完把 sync 的清掉。
  const local = await chrome.storage.local.get(DEFAULTS);
  if (!local.webhookToken) {
    const legacy = await chrome.storage.sync.get(["webhookToken", "webhookPort", "delayMs", "enabled"]);
    if (legacy.webhookToken) {
      await chrome.storage.local.set(legacy);
      await chrome.storage.sync.remove(["webhookToken", "webhookPort", "delayMs", "enabled"]);
      return { ...DEFAULTS, ...legacy };
    }
  }
  return { ...DEFAULTS, ...local };
}

async function recordRing(result) {
  await chrome.storage.local.set({
    lastRing: { at: Date.now(), ...result },
  });
}

function flashBadge(ok) {
  try {
    chrome.action.setBadgeBackgroundColor({ color: ok ? "#2f7d5b" : "#c13a30" });
    chrome.action.setBadgeText({ text: ok ? "✓" : "!" });
    setTimeout(() => chrome.action.setBadgeText({ text: "" }), 3000);
  } catch {
    /* badge 是點綴，失敗不影響門鈴 */
  }
}

async function pingDesktop(source) {
  const settings = await getSettings();
  if (!settings.enabled) return { ok: false, reason: "disabled" };
  if (!settings.webhookToken) {
    console.warn("[cum-doorbell] 還沒貼密碼牌——打開擴充視窗，從桌面 App 設定頁複製過來。");
    return { ok: false, reason: "no-token" };
  }

  const now = Date.now();
  if (now - lastPingAt < MIN_INTERVAL_MS) {
    return { ok: false, reason: "debounce" };
  }
  lastPingAt = now;

  // 用 127.0.0.1 不用 localhost：桌面 App 只綁 IPv4，localhost 在有些機器會先解成 ::1。
  const url = `http://127.0.0.1:${settings.webhookPort}/refresh`;
  let result;
  try {
    const res = await fetch(url, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${settings.webhookToken}`,
      },
      body: JSON.stringify({ source, at: now, extensionVersion: EXT_VERSION }),
    });
    if (res.status === 401) {
      console.warn("[cum-doorbell] 桌面 App 說密碼牌不對（401）——重新複製一次。");
      result = { ok: false, reason: "unauthorized", status: 401 };
    } else if (!res.ok) {
      console.warn("[cum-doorbell] 桌面 App 回了", res.status);
      result = { ok: false, reason: "non-2xx", status: res.status };
    } else {
      result = { ok: true };
    }
  } catch (e) {
    // 桌面 App 沒開或 port 不對——安靜失敗，不影響 claude.ai 的使用。
    console.debug("[cum-doorbell] 按不到門鈴", e?.message);
    result = { ok: false, reason: "unreachable", error: e?.message };
  }
  await recordRing({ ...result, source });
  flashBadge(result.ok);
  return result;
}

function scheduleRing() {
  if (pendingTimer) clearTimeout(pendingTimer);
  // 等幾秒再按門鈴：官方那邊要一點時間把這則訊息的用量記上，太早問會問到舊值。
  getSettings().then((s) => {
    pendingTimer = setTimeout(() => {
      pendingTimer = null;
      pingDesktop("extension");
    }, s.delayMs);
  });
}

chrome.webRequest.onCompleted.addListener(
  (details) => {
    if (details.method !== "POST") return;
    if (SEND_PATTERNS.some((re) => re.test(details.url))) {
      scheduleRing();
    }
  },
  // 整個 claude.ai 都要看得到（/claudeai-rpc/ 不在 /api/ 底下）；真正的篩選在上面的樣式。
  { urls: ["https://claude.ai/*"] },
);

// popup 用：手動按一次門鈴（回傳細節讓 popup 說人話）、讀狀態。
chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg?.type === "manual-ping") {
    pingDesktop("extension-test").then(sendResponse);
    return true;
  }
  if (msg?.type === "get-status") {
    Promise.all([getSettings(), chrome.storage.local.get("lastRing")]).then(([settings, { lastRing }]) =>
      sendResponse({ settings, lastRing: lastRing ?? null, version: EXT_VERSION }),
    );
    return true;
  }
});
