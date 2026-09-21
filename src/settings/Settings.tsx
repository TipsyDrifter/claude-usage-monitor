import { useEffect, useRef, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import {
  RefreshCw,
  CheckCircle2,
  AlertCircle,
  Power,
  Eye,
  EyeOff,
  Copy,
  RotateCw,
  Check,
  Plug,
  Package,
} from "lucide-react";
import { useStore } from "@/store/usageStore";
import { cmd } from "@/lib/tauri";
import { cn } from "@/lib/cn";
import {
  formatResetCountdownVerbose,
  formatTimeAgo,
} from "@/lib/format";
import { toneOf, thresholdsOf } from "@/lib/tone";
import { seatKey, seatDefaultLabel, seatShortId } from "@/lib/seatLabel";
import { EMPTY_ITEM } from "@/lib/types";
import type { UsageItem } from "@/lib/types";
import { ToastHost, toast } from "../statistics/Motion";
import { FACE_LABEL, FACE_THEMES, normalizeTheme } from "../widget/shared";
import "../statistics/h5.css";

export function Settings() {
  const init = useStore((s) => s.init);
  const usage = useStore((s) => s.usage);
  const settings = useStore((s) => s.settings);
  const updateSettings = useStore((s) => s.updateSettings);
  const refresh = useStore((s) => s.refresh);
  const demoMode = useStore((s) => s.demoMode);
  const settingsError = useStore((s) => s.settingsError);
  const clearSettingsError = useStore((s) => s.clearSettingsError);
  const seats = useStore((s) => s.seats);
  const currentSeatId = useStore((s) => s.currentSeatId);

  useEffect(() => {
    init();
  }, [init]);

  const isOk = usage.status === "ok";
  const data = usage.data;

  return (
    <div className="h5 min-h-screen p-6" style={{ background: "var(--h5-bg)" }}>
      <motion.div
        initial={{ opacity: 0, y: 8 }}
        animate={{ opacity: 1, y: 0 }}
        transition={{ duration: 0.25 }}
        className="mx-auto max-w-md space-y-4"
      >
        <header className="space-y-1">
          <div className="flex items-center gap-2">
            <h1
              className="text-xl"
              style={{ fontFamily: "'Noto Sans TC',sans-serif", fontWeight: 900, color: "var(--h5-ink)" }}
            >
              Claude Usage Monitor
            </h1>
            {demoMode && (
              <span
                className="text-[10px] font-bold px-2 py-0.5"
                style={{ borderRadius: 2, background: "var(--h5-accent-soft)", color: "var(--h5-accent-ink)", letterSpacing: "0.08em" }}
              >
                DEMO
              </span>
            )}
          </div>
          <p className="text-xs text-slate-500">
            {data?.planName
              ? `方案：${data.planName}`
              : "追蹤 Claude.ai 方案的使用量"}
          </p>
        </header>

        <AnimatePresence>
          {settingsError && (
            <motion.div
              key="settings-error"
              initial={{ opacity: 0, y: -4 }}
              animate={{ opacity: 1, y: 0 }}
              exit={{ opacity: 0, y: -4 }}
              className="rounded-xl border border-rose-200 bg-rose-50 p-3 flex items-start gap-2"
            >
              <AlertCircle
                size={16}
                className="text-rose-500 shrink-0 mt-0.5"
              />
              <div className="flex-1 min-w-0">
                <p className="text-xs font-medium text-rose-800">
                  最近一次設定儲存失敗，畫面已還原為上一個值
                </p>
                <p className="text-[10px] text-rose-600 mt-0.5 break-all">
                  {settingsError}
                </p>
              </div>
              <button
                onClick={clearSettingsError}
                className="text-rose-400 hover:text-rose-600 shrink-0 text-[10px]"
                title="關閉"
              >
                ✕
              </button>
            </motion.div>
          )}
        </AnimatePresence>

        <Card>
          <div className="flex items-start justify-between gap-3">
            <div className="flex items-center gap-2">
              {isOk ? (
                <CheckCircle2 size={18} className="text-emerald-500" />
              ) : (
                <AlertCircle size={18} className="text-slate-400" />
              )}
              <div>
                <p className="text-sm font-medium text-slate-800">
                  {demoMode
                    ? "Demo 模式"
                    : isOk
                    ? "已連線"
                    : usage.status === "loading"
                    ? "更新中…"
                    : "尚未取得資料"}
                </p>
                <p className="text-xs text-slate-500">
                  最後更新：{formatTimeAgo(usage.lastSuccessAt)}
                </p>
                {usage.notice && (
                  <p className="mt-1 max-w-md text-xs leading-relaxed text-amber-600">
                    ⚠ {usage.notice}
                  </p>
                )}
              </div>
            </div>

            {!demoMode && (
              <div className="flex flex-col gap-2">
                <button
                  onClick={() => refresh()}
                  disabled={usage.status === "loading"}
                  className="flex items-center gap-1 rounded-full border border-slate-200 bg-white px-3 py-1.5 text-xs text-slate-700 hover:bg-slate-50 disabled:opacity-50 transition-colors"
                >
                  <RefreshCw
                    size={11}
                    className={cn(usage.status === "loading" && "animate-spin")}
                  />
                  立即刷新
                </button>
              </div>
            )}
          </div>
        </Card>

        {isOk && data && (
          <Card title="目前使用量">
            <UsageRow label="Current session (5h)" item={data.currentSession} />
            <Divider />
            <UsageRow label="Weekly · All models" item={data.weeklyAllModels} />
            <UsageRow
              label="Weekly · Fable"
              item={data.weeklyFable ?? EMPTY_ITEM}
            />
          </Card>
        )}

        <Card title="顯示" fields={["widget.show", "widget.theme", "general.timeFormat"]}>
          <ToggleRow
            label="顯示桌面浮動視窗"
            description="關閉後只在系統匣顯示"
            checked={settings.widget.show}
            onChange={(v) => updateSettings("widget", { show: v })}
          />
          {/* v1.8.1（D82）主題商店：三張臉，主人圈選全留、預設 W5。名字與說明唯一來源 widget/shared.ts。 */}
          <div>
            <p className="text-sm text-slate-700">懸浮窗外觀</p>
            <p className="text-xs text-slate-400 mt-0.5 mb-2">
              三種都用同一套資訊：三條額度、留意／警戒門檻、帳號行；只換長相
            </p>
            <div className="grid grid-cols-3 gap-2">
              {FACE_THEMES.map((k) => {
                const on = normalizeTheme(settings.widget.theme) === k;
                return (
                  <button
                    key={k}
                    onClick={() => updateSettings("widget", { theme: k })}
                    style={{
                      all: "unset",
                      cursor: "pointer",
                      display: "block",
                      padding: "8px 10px",
                      border: `1px solid ${on ? "var(--h5-accent)" : "var(--h5-line)"}`,
                      background: on ? "var(--h5-accent-faint)" : "var(--h5-bg)",
                      boxSizing: "border-box",
                    }}
                  >
                    <div style={{ fontSize: 12, fontWeight: 700, color: on ? "var(--h5-accent-ink)" : "var(--h5-ink)" }}>
                      {FACE_LABEL[k].name}
                      <span style={{ marginLeft: 6, fontSize: 9, letterSpacing: "0.12em", color: "var(--h5-ink3)", fontWeight: 600 }}>{k.toUpperCase()}</span>
                    </div>
                    <div className="text-xs text-slate-400 mt-0.5" style={{ lineHeight: 1.4 }}>{FACE_LABEL[k].desc}</div>
                  </button>
                );
              })}
            </div>
          </div>
          {/* D61：全 app 統一的未來時刻顯示模式 */}
          <div className="flex items-start justify-between gap-3">
            <div className="flex-1 min-w-0">
              <p className="text-sm text-slate-700">重置時間顯示</p>
              <p className="text-xs text-slate-400 mt-0.5">
                全部畫面統一用同一種：「2 小時 14 分後」或「週日 09:00」
              </p>
            </div>
            <div
              className="flex shrink-0"
              style={{
                background: "#ebeef3",
                border: "1px solid var(--h5-line)",
                borderRadius: 99,
                padding: 2,
                gap: 1,
              }}
            >
              {(
                [
                  { v: "absolute", label: "絕對" },
                  { v: "relative", label: "相對" },
                ] as const
              ).map((opt) => {
                const on = (settings.general.timeFormat ?? "absolute") === opt.v;
                return (
                  <button
                    key={opt.v}
                    onClick={() => updateSettings("general", { timeFormat: opt.v })}
                    style={{
                      all: "unset",
                      cursor: "pointer",
                      fontSize: 11,
                      fontWeight: 600,
                      padding: "4px 12px",
                      borderRadius: 99,
                      color: on ? "var(--h5-ink)" : "var(--h5-ink3)",
                      background: on ? "#fff" : "transparent",
                      boxShadow: on ? "0 1px 3px rgba(38,48,70,.14)" : "none",
                    }}
                  >
                    {opt.label}
                  </button>
                );
              })}
            </div>
          </div>
        </Card>

        <Card title="採集節奏" fields={["general.pollIntervalMinutes", "general.idleHeartbeatMinutes"]}>
          <SliderRow
            label="背景重讀間隔"
            description="就算沒有任何門鈴響，最久每隔這麼久也會重讀一次本機的紀錄、把畫面上的時間更新。這只是保底；真正去查額度另有自己的節奏（最少隔 3 分鐘、沒在用就不查、出錯先等一下）。"
            value={settings.general.pollIntervalMinutes}
            min={1}
            max={30}
            step={1}
            unit="分鐘"
            onChange={(v) =>
              updateSettings("general", { pollIntervalMinutes: v })
            }
          />
          <SliderRow
            label="閒置心跳"
            description="一段時間沒偵測到你在用 Claude 時，最久隔這麼久還是會去查一次額度，免得數字停在舊值（例如沒裝瀏覽器擴充、只在網頁上用的時候）。設 0 就完全不查。"
            value={settings.general.idleHeartbeatMinutes}
            min={0}
            max={120}
            step={15}
            unit="分鐘"
            zeroLabel="關閉"
            onChange={(v) =>
              updateSettings("general", { idleHeartbeatMinutes: v })
            }
          />
        </Card>

        {/* v1.2 兩級告警（D67-Q16／Q17、D74）：兩個數是全 app 的唯一來源——
            懸浮窗象牙針、琥珀／珊瑚轉色、今日頁裁決地板、托盤圖示、系統通知全部跟著走。
            留意值恆 ≤ 警戒值（v1.7 前叫撞牆值，程式內仍是 wall）− 5：拉其中一個撞到另一個就把對方推開。 */}
        <Card title="告警" fields={["notifications.noticePct", "notifications.wallPct", "notifications.widgetTint", "notifications.systemToast"]}>
          <SliderRow
            label="留意值"
            description="懸浮窗指針的位置；額度條到這裡轉為琥珀色、系統匣圖示轉黃；觸發第一級通知"
            value={settings.notifications.noticePct}
            min={30}
            max={90}
            step={5}
            unit="%"
            onChange={(v) => {
              const wall = Math.min(95, Math.max(settings.notifications.wallPct, v + 5));
              updateSettings("notifications", { noticePct: v, wallPct: wall });
            }}
          />
          <SliderRow
            label="警戒值"
            description="額度條到這裡轉為珊瑚色、系統匣圖示轉紅；觸發第二級通知"
            value={settings.notifications.wallPct}
            min={35}
            max={95}
            step={5}
            unit="%"
            onChange={(v) => {
              const notice = Math.max(30, Math.min(settings.notifications.noticePct, v - 5));
              updateSettings("notifications", { wallPct: v, noticePct: notice });
            }}
          />
          <Divider />
          <ToggleRow
            label="懸浮窗變色"
            description="越過留意值轉為琥珀色、越過警戒值轉為珊瑚色。關掉後三列的顏色全程保持象牙色，指針仍標在留意值位置"
            checked={settings.notifications.widgetTint}
            onChange={(v) => updateSettings("notifications", { widgetTint: v })}
          />
          <ToggleRow
            label="系統通知"
            description="越線時跳一次 Windows 通知；同一個窗每級只響一次、重置後歸零、啟動時已越線也只提醒一次。無聲音"
            checked={settings.notifications.systemToast}
            onChange={(v) => updateSettings("notifications", { systemToast: v })}
          />
        </Card>

        {/* v1.3 兩個帳號（D67-Q9、D75 決策點 10）：一列一個座位，別名空著就用預設名字。 */}
        <Card title="帳號" fields={["accounts.aliases"]}>
          {seats.length === 0 ? (
            <p className="text-xs text-slate-400">還沒有帳號×組織的記錄——app 取得第一筆 CLI 資料後會自動出現。</p>
          ) : (
            seats.map((s) => {
              const key = seatKey(s);
              const alias = settings.accounts.aliases[key] ?? "";
              return (
                <div key={s.id} className="space-y-1">
                  <div className="flex items-center justify-between gap-2">
                    <span className="text-sm text-slate-700 truncate">{seatDefaultLabel(s)}</span>
                    {s.id === currentSeatId && (
                      <span className="lim-chip zh shrink-0">目前登入</span>
                    )}
                  </div>
                  <input
                    type="text"
                    value={alias}
                    placeholder="別名（例如：私人帳、學校帳）"
                    onChange={(e) => {
                      const next = { ...settings.accounts.aliases };
                      const v = e.target.value;
                      if (v.trim()) next[key] = v;
                      else delete next[key];
                      void updateSettings("accounts", { aliases: next });
                    }}
                    className="w-full text-sm"
                    style={{
                      padding: "5px 8px",
                      border: "1px solid var(--h5-line)",
                      borderRadius: 6,
                      background: "var(--h5-bg)",
                      color: "var(--h5-ink)",
                    }}
                  />
                  <p className="text-[10px] text-slate-400">{seatShortId(s)}</p>
                </div>
              );
            })
          )}
        </Card>

        <Card title="系統" fields={["general.autoStart"]}>
          <ToggleRow
            label="開機自動啟動"
            description="登入 Windows 後自動執行"
            checked={settings.general.autoStart}
            onChange={(v) => updateSettings("general", { autoStart: v })}
          />
          {!demoMode && <WebhookBlock />}
          {!demoMode && (
            <DiagnosticsBlock />
          )}
        </Card>

        {!demoMode && (
          <button
            onClick={() => cmd.quit()}
            className="flex items-center gap-1.5 mx-auto text-xs text-slate-500 hover:text-rose-500 transition-colors"
          >
            <Power size={12} /> 結束程式
          </button>
        )}

        <p className="text-center text-[10px] text-slate-400 pt-2">
          非官方工具，無 Anthropic 隸屬。資料來自官方 CLI /usage 與本機檔案——不碰憑證、不爬網頁
        </p>
      </motion.div>
      {/* E13：設定頁唯一會發 toast 的事件是複製密碼牌（規則 3） */}
      <ToastHost />
    </div>
  );
}

function UsageRow({ label, item }: { label: string; item: UsageItem }) {
  const used = item.usedPercent;
  // v1.2：預覽條的顏色與數字染紅都讀留意值／撞牆值（lib/tone.ts 唯一來源）。
  const th = thresholdsOf(useStore((s) => s.settings));
  const tone = toneOf(used, th);
  const colorMap = {
    unknown: "from-emerald-400 to-emerald-500",
    ok: "from-emerald-400 to-emerald-500",
    warn: "from-amber-400 to-amber-500",
    crit: "from-rose-400 to-rose-500",
  } as const;

  return (
    <div className="space-y-1 py-1">
      <div className="flex items-center justify-between">
        <span className="text-sm text-slate-700">{label}</span>
        <span
          className="text-xs text-slate-500 tabular-nums"
          // 條體歸靛藍後，高用量狀態改由數字小面積染色表達（D57）
          style={tone === "crit" ? { color: "var(--h5-red)", fontWeight: 700 } : undefined}
        >
          {used == null ? "—" : `${Math.round(used)}% used`}
        </span>
      </div>
      <div className="relative h-1.5 w-full overflow-hidden rounded-full bg-slate-200/60">
        <motion.div
          className={cn(
            "absolute inset-y-0 left-0 rounded-full bg-gradient-to-r",
            colorMap[tone],
          )}
          initial={{ width: 0 }}
          animate={{ width: `${used ?? 0}%` }}
          transition={{ type: "spring", stiffness: 80, damping: 18 }}
        />
      </div>
      <div className="flex items-center justify-between text-[11px] text-slate-400">
        <span>{item.note ?? ""}</span>
        <span>{formatResetCountdownVerbose(item.resetAt)}</span>
      </div>
    </div>
  );
}

function Divider() {
  return <div className="h-px bg-slate-100 my-1" />;
}

function Card({
  title,
  fields,
  children,
}: {
  title?: string;
  /** E15（D77）：這張卡管哪些設定欄位（`"<group>.<key>"`）。存檔成功且動到
   *  其中之一時，標題旁跳一次「✓ 設定已儲存」——只有被改到的那張卡會跳。 */
  fields?: string[];
  children: React.ReactNode;
}) {
  const savedAt = useStore((s) => s.settingsSavedAt);
  const savedFields = useStore((s) => s.settingsSavedFields);
  const [shown, setShown] = useState<number | null>(null);
  // 開頁時 store 裡可能已經有上一次的存檔戳——記下來當基準，避免「一進設定頁
  // 六張卡一起打勾」這種入場動畫（D77 規則 1）。
  const seen = useRef<number | null>(savedAt);
  useEffect(() => {
    if (savedAt == null || savedAt === seen.current) return;
    seen.current = savedAt;
    if (!fields || !savedFields.some((k) => fields.includes(k))) return;
    setShown(savedAt);
    const t = window.setTimeout(() => setShown(null), 1500);
    return () => window.clearTimeout(t);
    // savedFields 跟著 savedAt 一起換，只掛 savedAt 當觸發即可
  }, [savedAt]);

  return (
    <section
      className="p-4 space-y-3"
      style={{
        background: "var(--h5-card)",
        border: "1px solid var(--h5-line)",
        borderRadius: 16,
        boxShadow: "0 1px 2px rgba(38,48,70,.03)",
      }}
    >
      {title && (
        <h3
          className="zh"
          style={{ fontFamily: "'Noto Sans TC',sans-serif", fontWeight: 900, fontSize: 13 }}
        >
          {title}
          {shown != null && (
            <span className="saved zh" key={shown}>
              ✓ 設定已儲存
            </span>
          )}
        </h3>
      )}
      {children}
    </section>
  );
}

function ToggleRow({
  label,
  description,
  checked,
  onChange,
}: {
  label: string;
  description?: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <div className="flex items-start justify-between gap-3">
      <div className="flex-1 min-w-0">
        <p className="text-sm text-slate-700">{label}</p>
        {description && (
          <p className="text-xs text-slate-400 mt-0.5">{description}</p>
        )}
      </div>
      <button
        role="switch"
        aria-checked={checked}
        onClick={() => onChange(!checked)}
        className="relative shrink-0 w-9 h-5 rounded-full transition-colors"
        style={{ background: checked ? "var(--h5-accent)" : "var(--h5-cell-empty)" }}
      >
        <motion.span
          className="absolute top-0.5 left-0.5 w-4 h-4 rounded-full bg-white shadow"
          animate={{ x: checked ? 16 : 0 }}
          transition={{ type: "spring", stiffness: 500, damping: 30 }}
        />
      </button>
    </div>
  );
}

function SliderRow({
  label,
  description,
  value,
  min,
  max,
  step,
  unit,
  zeroLabel,
  onChange,
}: {
  label: string;
  description?: string;
  value: number;
  min: number;
  max: number;
  step: number;
  unit: string;
  /** Shown instead of "0 <unit>" when the value is 0 (e.g. "關閉"). */
  zeroLabel?: string;
  onChange: (v: number) => void;
}) {
  return (
    <div className="space-y-1.5">
      <div className="flex items-center justify-between">
        <span className="text-sm text-slate-700">{label}</span>
        <span className="text-xs text-slate-500 tabular-nums">
          {value === 0 && zeroLabel ? zeroLabel : `${value} ${unit}`}
        </span>
      </div>
      {description && (
        <p className="text-[10px] leading-relaxed text-slate-400">{description}</p>
      )}
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        onChange={(e) => onChange(Number(e.target.value))}
        className="w-full"
        style={{ accentColor: "var(--h5-accent)" }}
      />
    </div>
  );
}

/**
 * 「門鈴」區塊（v1.8.2 改名，D54／D82）：本機 webhook 的位址與密碼牌、
 * Claude Code Stop hook 一鍵安裝、瀏覽器擴充的載入指引。兩個門鈴共用同一個
 * 位址與密碼牌。Rendered inside the Settings "系統" card, skipped in demo
 * mode (Tauri IPC unavailable).
 */
function WebhookBlock() {
  const settings = useStore((s) => s.settings);
  const [tokenVisible, setTokenVisible] = useState(false);
  const [copied, setCopied] = useState(false);
  const [installState, setInstallState] = useState<
    "idle" | "installing" | "success" | "error"
  >("idle");
  const [installMessage, setInstallMessage] = useState<string>("");

  const copyToken = async () => {
    try {
      await navigator.clipboard.writeText(settings.webhook.token);
      setCopied(true);
      toast("門鈴密碼牌已複製到剪貼簿"); // E13＋E14 是一組：按鈕說哪一顆、toast 說做了什麼
      window.setTimeout(() => setCopied(false), 1600);
    } catch (e) {
      console.error("Copy failed", e);
    }
  };

  const regenerate = async () => {
    const ok = window.confirm(
      "重新產生門鈴密碼牌？\n\n" +
        "舊密碼牌會立刻失效，兩個門鈴都要重新交鑰匙：\n" +
        "1. 重新點「一鍵安裝 Claude Code Hook」\n" +
        "2. 重新複製、貼進瀏覽器擴充的視窗（如果有裝）",
    );
    if (!ok) return;
    try {
      await cmd.regenerateWebhookToken();
      // settings://update event will propagate the new token through the store
    } catch (e) {
      console.error("Regenerate failed", e);
    }
  };

  const installHook = async () => {
    setInstallState("installing");
    setInstallMessage("");
    try {
      const path = await cmd.installClaudeCodeHook();
      setInstallState("success");
      setInstallMessage(`已更新 ${path}`);
      window.setTimeout(() => setInstallState("idle"), 3500);
    } catch (e) {
      setInstallState("error");
      setInstallMessage(String(e));
    }
  };

  const maskedToken = settings.webhook.token
    ? "•".repeat(Math.min(settings.webhook.token.length, 32))
    : "(尚未產生)";

  return (
    <div className="border-t border-slate-100 pt-3 mt-1 space-y-3">
      <div>
        <p className="text-xs text-slate-500 mb-1">門鈴（Claude Code hook／瀏覽器擴充）</p>
        <p className="text-[11px] text-slate-500 leading-relaxed">
          門鈴＝「你剛剛用了 Claude」的通知。收到門鈴，App 就馬上去查一次額度，不用等。門鈴不帶任何內容。
        </p>
        <p className="text-[11px] text-slate-400 font-mono tabular-nums mt-1">
          http://localhost:{settings.webhook.port}/refresh
        </p>
      </div>

      <div className="space-y-1">
        <div className="flex items-center justify-between gap-2">
          <p className="text-[11px] text-slate-500">門鈴密碼牌</p>
          <p className="text-[10px] text-slate-400">按門鈴要出示它</p>
        </div>
        <div className="flex items-center gap-1">
          <code className="flex-1 text-[10px] font-mono text-slate-600 bg-slate-50 border border-slate-200 px-2 py-1.5 rounded truncate">
            {tokenVisible
              ? settings.webhook.token || "(尚未產生)"
              : maskedToken}
          </code>
          <SmallIconButton
            title={tokenVisible ? "隱藏" : "顯示"}
            onClick={() => setTokenVisible((v) => !v)}
          >
            {tokenVisible ? <EyeOff size={12} /> : <Eye size={12} />}
          </SmallIconButton>
          <SmallIconButton title={copied ? "已複製" : "複製到剪貼簿"} onClick={copyToken} done={copied}>
            {copied ? <Check size={12} /> : <Copy size={12} />}
          </SmallIconButton>
          <SmallIconButton
            title="重新產生（會讓舊密碼牌失效）"
            onClick={regenerate}
          >
            <RotateCw size={12} />
          </SmallIconButton>
        </div>
      </div>

      <div className="space-y-1">
        <button
          onClick={installHook}
          disabled={installState === "installing"}
          className={cn(
            "w-full flex items-center justify-center gap-1.5 rounded-full px-3 py-1.5 text-xs font-medium transition-colors",
            installState === "success"
              ? "bg-emerald-500 text-white"
              : installState === "error"
              ? "bg-rose-500 text-white"
              : "bg-claude-500 text-white hover:bg-claude-600 disabled:opacity-60",
          )}
        >
          {installState === "success" ? (
            <>
              <CheckCircle2 size={12} /> 已安裝 Hook
            </>
          ) : installState === "error" ? (
            <>
              <AlertCircle size={12} /> 安裝失敗，重試
            </>
          ) : installState === "installing" ? (
            <>
              <RefreshCw size={12} className="animate-spin" /> 安裝中…
            </>
          ) : (
            <>
              <Plug size={12} /> 一鍵安裝 Claude Code Hook
            </>
          )}
        </button>
        <p className="text-[10px] text-slate-400">
          讓 Claude Code 每回覆完一輪就按一次門鈴。會寫進 <code className="font-mono">~/.claude/settings.json</code>，不動你其他的 hook。
        </p>
        {installState === "error" && installMessage && (
          <p className="text-[10px] text-rose-500 break-all">
            {installMessage}
          </p>
        )}
      </div>

      <div className="space-y-1">
        <p className="text-[11px] text-slate-500">瀏覽器擴充（選配）</p>
        <p className="text-[10px] text-slate-400 leading-relaxed">
          在瀏覽器上用 claude.ai 也想讓數字馬上動，就裝專案附的擴充（只看你有沒有送出訊息，不讀內容）。
          安裝步驟在 <code className="font-mono">extension/README.md</code>，裝好把上面的密碼牌貼進去。
        </p>
      </div>
    </div>
  );
}

/**
 * v1.8.2（主人驗收追加）：素人看不懂 log，給一顆「打包診斷資料」——後端把
 * collector.log、設定（密碼牌遮掉）、健康度、環境資訊壓成一個 zip 放進 exports
 * 並在檔案總管選取，整包丟給開發者就好。
 */
function DiagnosticsBlock() {
  const [packState, setPackState] = useState<"idle" | "packing" | "done" | "error">("idle");
  const [packMessage, setPackMessage] = useState("");

  const pack = async () => {
    setPackState("packing");
    setPackMessage("");
    try {
      const r = await cmd.packDiagnostics();
      if (r.cancelled) {
        setPackState("idle");
        return;
      }
      setPackState("done");
      setPackMessage(`已存到 ${r.path}（${(r.bytes / 1024).toFixed(0)} KB）`);
      toast("診斷包已存好，已在檔案總管選取");
      window.setTimeout(() => setPackState("idle"), 4000);
    } catch (e) {
      setPackState("error");
      setPackMessage(String(e));
    }
  };

  return (
    <div className="border-t border-slate-100 pt-3 mt-1">
      <p className="text-xs text-slate-500 mb-2">診斷</p>
      <div className="flex items-center gap-2 flex-wrap">
        <button
          onClick={pack}
          disabled={packState === "packing"}
          className={cn(
            "flex items-center gap-1.5 rounded-full px-3 py-1.5 text-xs font-medium transition-colors",
            packState === "done"
              ? "bg-emerald-500 text-white"
              : packState === "error"
              ? "bg-rose-500 text-white"
              : "bg-claude-500 text-white hover:bg-claude-600 disabled:opacity-60",
          )}
        >
          {packState === "done" ? (
            <>
              <CheckCircle2 size={12} /> 已打包
            </>
          ) : packState === "error" ? (
            <>
              <AlertCircle size={12} /> 打包失敗，重試
            </>
          ) : packState === "packing" ? (
            <>
              <RefreshCw size={12} className="animate-spin" /> 打包中…
            </>
          ) : (
            <>
              <Package size={12} /> 打包診斷資料
            </>
          )}
        </button>
        <button
          onClick={() => cmd.revealLogFolder()}
          className="rounded-full border border-slate-200 bg-white px-3 py-1.5 text-xs text-slate-700 hover:bg-slate-50 transition-colors"
        >
          打開 log 資料夾
        </button>
      </div>
      <p className="text-[10px] text-slate-400 mt-2">
        App 怪怪的時候按「打包診斷資料」，選個地方存（預設桌面），會得到一個 zip，整包傳給開發者就好。裡面是採集紀錄、你的設定（密碼牌已遮掉）和資料健康度；沒有對話內容、沒有帳本本身。
      </p>
      {packMessage && (
        <p className={cn("text-[10px] mt-1 break-all", packState === "error" ? "text-rose-500" : "text-slate-500")}>
          {packMessage}
        </p>
      )}
    </div>
  );
}

function SmallIconButton({
  title,
  onClick,
  done,
  children,
}: {
  title: string;
  onClick: () => void;
  /** E14（D77）：完成的那 1.6 秒，按鈕本身變綠——「就是你按的這顆完成了」。 */
  done?: boolean;
  children: React.ReactNode;
}) {
  return (
    <button
      title={title}
      onClick={onClick}
      className={cn(
        "flex h-6 w-6 items-center justify-center rounded-md text-slate-500 hover:bg-slate-100 hover:text-slate-700 transition-colors",
        done && "btn-done",
      )}
    >
      {children}
    </button>
  );
}
