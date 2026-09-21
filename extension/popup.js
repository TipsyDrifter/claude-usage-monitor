const $ = (id) => document.getElementById(id);

const REASON_TEXT = {
  "no-token": ["✗ 還沒貼密碼牌——從桌面 App 設定頁複製過來", "err"],
  unauthorized: ["✗ 桌面 App 說密碼牌不對（401）——重新複製一次", "err"],
  unreachable: ["✗ 按不到門鈴：桌面 App 沒開，或 port 不對", "err"],
  disabled: ["門鈴已關閉，先勾「啟用門鈴」", "warn"],
  debounce: ["剛按過了，等 1.5 秒再按", "warn"],
  "non-2xx": ["✗ 桌面 App 回了非預期的狀態碼", "err"],
};

function setStatus(text, kind) {
  const el = $("status");
  el.textContent = text;
  el.className = `status ${kind || ""}`;
}

function relTime(ts) {
  const s = Math.max(0, Math.round((Date.now() - ts) / 1000));
  if (s < 60) return `${s} 秒前`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m} 分鐘前`;
  const h = Math.round(m / 60);
  if (h < 48) return `${h} 小時前`;
  return `${Math.round(h / 24)} 天前`;
}

function renderLast(lastRing) {
  const el = $("last");
  if (!lastRing) {
    el.textContent = "還沒響過鈴。";
    return;
  }
  const who = lastRing.source === "extension-test" ? "手動測試" : "看到 claude.ai 送出訊息";
  const outcome = lastRing.ok
    ? "桌面 App 收到了"
    : (REASON_TEXT[lastRing.reason]?.[0] ?? `失敗（${lastRing.reason}）`).replace(/^✗ /, "");
  el.replaceChildren();
  el.append("上次響鈴：");
  const b = document.createElement("b");
  b.textContent = relTime(lastRing.at);
  el.append(b, ` · ${who} · ${outcome}`);
}

function renderDot(settings, lastRing) {
  const dot = $("dot");
  dot.className = "dot";
  if (!settings.enabled) {
    dot.title = "門鈴已關閉";
  } else if (!settings.webhookToken) {
    dot.classList.add("warn");
    dot.title = "還沒貼密碼牌";
  } else if (lastRing && !lastRing.ok) {
    dot.classList.add("warn");
    dot.title = "上次響鈴失敗";
  } else {
    dot.classList.add("on");
    dot.title = "門鈴就緒";
  }
}

async function load() {
  const st = await chrome.runtime.sendMessage({ type: "get-status" });
  const s = st.settings;
  $("enabled").checked = s.enabled;
  $("port").value = s.webhookPort;
  $("delay").value = Math.round(s.delayMs / 1000);
  $("token").value = s.webhookToken || "";
  $("ver").textContent = `v${st.version}`;
  renderLast(st.lastRing);
  renderDot(s, st.lastRing);
}

async function save(patch) {
  await chrome.storage.local.set(patch);
  await load();
}

$("enabled").addEventListener("change", (e) => save({ enabled: e.target.checked }));
$("port").addEventListener("change", (e) => save({ webhookPort: Number(e.target.value) || 17819 }));
$("delay").addEventListener("change", (e) => {
  const sec = Number(e.target.value);
  save({ delayMs: Number.isFinite(sec) && sec >= 0 ? sec * 1000 : 3000 });
});
$("token").addEventListener("change", (e) => save({ webhookToken: e.target.value.trim() }));

$("paste").addEventListener("click", async () => {
  try {
    const text = (await navigator.clipboard.readText()).trim();
    if (!text) {
      setStatus("剪貼簿是空的", "warn");
      return;
    }
    $("token").value = text;
    await save({ webhookToken: text });
    setStatus("✓ 已貼上並存好", "ok");
  } catch {
    setStatus("讀不到剪貼簿，請手動貼上", "err");
  }
});

$("test").addEventListener("click", async () => {
  setStatus("按鈴中…", "");
  const res = await chrome.runtime.sendMessage({ type: "manual-ping" });
  if (res?.ok) {
    setStatus("✓ 桌面 App 收到了，正在查額度", "ok");
  } else {
    const [text, kind] = REASON_TEXT[res?.reason] ?? [`✗ 失敗（${res?.reason || "unknown"}）`, "err"];
    setStatus(text, kind);
  }
  await load();
});

load();
