import { useCallback, useEffect, useRef, useState } from "react";
import {
  RefreshCw,
  Settings as SettingsIcon,
  BarChart3,
  Rows3,
  AlertCircle,
  Check,
} from "lucide-react";
import { getCurrentWindow, LogicalSize, PhysicalPosition } from "@tauri-apps/api/window";
import { useStore } from "@/store/usageStore";
import { cmd } from "@/lib/tauri";
import type { UsageItem } from "@/lib/types";
import { fmtMomentCompact, normalizeTimeMode, type TimeMode } from "@/lib/timefmt";
import { toneOf, thresholdsOf, type Thresholds, type Tone } from "@/lib/tone";
import { seatLabel } from "@/lib/seatLabel";
import { formatTimeAgo } from "@/lib/format";
import {
  DENSITY_CYCLE,
  DENSITY_SIZE,
  EtaStat,
  PANEL_SIZE,
  isExpired,
  isStale,
  normalizeTheme,
  paceOf,
  usePrefersDark,
  type BurnStats,
  type Density,
  type FaceProps,
  type FaceRow,
  type FaceTone,
} from "./shared";
import { W1Face } from "./W1Glass";
import { W4Face } from "./W4Ring";
import "./w5.css";

// =============================================================================
// W5 · 深色儀器 (M2) — 視覺基準 prototypes/widget-W5-synthesis.html 的實作。
//
// 三段密度: micro 220×32 / card 280×160 / panel 320×280（設定持久化，
// 切換時視窗跟著變形）。D34 三修正已內建：微型條無 ▼、微型數字整數、
// 零動畫（crit 單脈衝為未來 opt-in，預設不存在）。
//
// 資料對映: 5H = currentSession、7D = weeklyAllModels（皆為 CLI 帳號，D51）。
// 數字一律整數——來源實質精度就是 1%（O4），顯示小數是假精度。
//
// v1.8.1（D82）主題商店：這個檔案算資料、管視窗尺寸與 hover 控制列；W5 的畫法留在
// 這裡，W1 毛玻璃／W4 環形各自一個檔（W1Glass.tsx／W4Ring.tsx），吃同一份 FaceProps。
// 密度尺寸、配速、過期、見底預測格搬到 shared.tsx 三張臉共用。
// =============================================================================

/** 狀態類別：留意值／警戒值讀設定（v1.2 唯一來源 lib/tone.ts）。
    「懸浮窗變色」關掉時三列全程象牙色——針仍畫在留意值，刻度不是狀態色（D74 決策點 5）。 */
const W5_STATE: Record<Tone, string> = { unknown: "s-safe", ok: "s-safe", warn: "s-warn", crit: "s-crit" };
const stateOf = (p: number | null | undefined, th: Thresholds, tint: boolean) =>
  tint ? W5_STATE[toneOf(p, th)] : "s-safe";
const FACE_TONE: Record<string, FaceTone> = { "s-safe": "safe", "s-warn": "warn", "s-crit": "crit" };

/** 針軌。pace=null 就不畫 ▼。needle＝象牙針位置（留意值 %）。 */
function Bar({
  p,
  h,
  pace,
  st,
  needle,
}: {
  p: number;
  h: 3 | 6;
  pace: number | null;
  st?: string;
  needle: number;
}) {
  return (
    <div className={`bar h${h}${st ? " " + st : ""}`}>
      {pace != null && <i className="pace" style={{ left: `${pace}%` }} />}
      <div className="fill" style={{ width: `${Math.min(p, 100)}%` }} />
      <i className="rl" style={{ left: `${needle}%` }} />
    </div>
  );
}

function Chip({ st }: { st: string }) {
  if (st === "s-warn") return <span className="chip cw">注意</span>;
  if (st === "s-crit") return <span className="chip cc">警戒</span>;
  return null;
}

interface RowData {
  id: string;
  item: UsageItem | undefined;
  pace: number | null;
  mode: TimeMode;
  st: string;
  needle: number;
  /** v1.4.1（D78 #1）：看別的座位時，最後已知值的重置時間已過——之後用了多少沒人知道，
      數字改「—」、重置欄寫「已重置」，上次的值放 title。 */
  expired?: boolean;
}

/** 標準卡／面板的一列（W2 原樣：位置即身份、27px 數字）。
    D61：三列同一種時間語言（依設定走相對或絕對），不再 5H/週限混用。 */
function CardRow({ id, item, pace, mode, st, needle, expired }: RowData) {
  const last = item?.usedPercent ?? null;
  const p = expired ? null : last;
  const title = expired && last != null ? `上次已知 ${Math.round(last)}%，重置後用了多少未知` : (item?.note ?? undefined);
  return (
    <div className={expired ? "s-safe" : st} title={title}>
      <div className="l1">
        <span className="idlab">{id}</span>
        {!expired && <Chip st={st} />}
        <span className="lreset">
          <b>{fmtMomentCompact(item?.resetAt, mode)}</b>
          <span>{expired ? " 已重置" : " 重置"}</span>
        </span>
      </div>
      <div className="l2">
        <span className="num n27">
          {p == null ? "—" : Math.round(p)}
          <span className="unit">%</span>
        </span>
        <Bar p={p ?? 0} h={6} pace={expired ? null : pace} needle={needle} />
      </div>
    </div>
  );
}

export function Widget() {
  const init = useStore((s) => s.init);
  const refresh = useStore((s) => s.refresh);
  const usage = useStore((s) => s.usage);
  const settings = useStore((s) => s.settings);
  const updateSettings = useStore((s) => s.updateSettings);
  // v1.3 切換器：viewSeatId 非空＝看別的座位的最後已知值。
  const seats = useStore((s) => s.seats);
  const currentSeatId = useStore((s) => s.currentSeatId);
  const viewSeatId = useStore((s) => s.viewSeatId);
  const seatSnapshot = useStore((s) => s.seatSnapshot);
  const cycleSeat = useStore((s) => s.cycleSeat);
  const [burn, setBurn] = useState<BurnStats | null>(null);
  const [, forceTick] = useState(0);

  const density: Density = (["micro", "card", "panel"] as const).includes(
    settings.widget.density as Density,
  )
    ? (settings.widget.density as Density)
    : "card";
  const mode = normalizeTimeMode(settings.general.timeFormat);
  // v1.8.1（D82）：哪張臉。瀏覽器 demo（非 Tauri）可用 ?theme=w1|w4 直接看，方便驗證。
  const theme = normalizeTheme(
    !("__TAURI_INTERNALS__" in window) && new URLSearchParams(window.location.search).get("theme")
      ? new URLSearchParams(window.location.search).get("theme")
      : settings.widget.theme,
  );
  const prefersDark = usePrefersDark();

  useEffect(() => {
    init();
  }, [init]);

  // 倒數與 ▼ 每分鐘走一格。
  useEffect(() => {
    const id = window.setInterval(() => forceTick((n) => n + 1), 60_000);
    return () => window.clearInterval(id);
  }, []);

  // 視窗尺寸跟著密度走；變形時右下角錨定（D61：小窗通常停在螢幕右下，
  // 錨左上會越變越往外跑）。
  const appliedDensity = useRef<Density | null>(null);
  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return; // browser/demo mode
    const prev = appliedDensity.current;
    if (prev === density) return;
    appliedDensity.current = density;
    const [w, h] = DENSITY_SIZE[density];
    const win = getCurrentWindow();
    void (async () => {
      if (prev != null) {
        const [pw, ph] = DENSITY_SIZE[prev];
        try {
          const pos = await win.outerPosition();
          const factor = await win.scaleFactor();
          const dx = Math.round((pw - w) * factor);
          const dy = Math.round((ph - h) * factor);
          await win.setMinSize(new LogicalSize(w, h));
          await win.setSize(new LogicalSize(w, h));
          await win.setPosition(new PhysicalPosition(pos.x + dx, pos.y + dy));
          return;
        } catch {
          /* fall through */
        }
      }
      await win.setMinSize(new LogicalSize(w, h));
      await win.setSize(new LogicalSize(w, h));
    })();
  }, [density]);

  // D61：拖曳後記住位置（600ms 去抖），重開還原。
  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;
    let t: number | undefined;
    let unlisten: (() => void) | undefined;
    void getCurrentWindow()
      .onMoved((e) => {
        window.clearTimeout(t);
        const { x, y } = e.payload;
        t = window.setTimeout(() => {
          void updateSettings("widget", { position: { x, y } });
        }, 600);
      })
      .then((u) => {
        unlisten = u;
      });
    return () => {
      window.clearTimeout(t);
      unlisten?.();
    };
  }, [updateSettings]);

  // 燃燒統計（展開面板才需要）。
  useEffect(() => {
    if (density !== "panel") return;
    let cancelled = false;
    cmd
      .getBurnStats(viewSeatId)
      .then((b) => {
        if (!cancelled) setBurn(b as BurnStats);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [density, usage.lastSuccessAt, viewSeatId]);

  const cycleDensity = useCallback(() => {
    void updateSettings("widget", { density: DENSITY_CYCLE[density] });
  }, [density, updateSettings]);

  // E27（D77）：立即刷新鈕轉完圈打個勾 1.2s——**只有手動按的那次**，背景自動
  // 刷新不動這顆鈕。它是「數字沒變就什麼都不動」條款的配套：沒有它，「沒變」
  // 和「沒查」長得一樣。刷新失敗就不打勾（錯誤走既有的錯誤畫面）。
  const [rfOk, setRfOk] = useState<number | null>(null);
  useEffect(() => {
    if (rfOk == null) return;
    const t = window.setTimeout(() => setRfOk(null), 1200);
    return () => window.clearTimeout(t);
  }, [rfOk]);
  const manualRefresh = useCallback(async () => {
    await refresh();
    if (useStore.getState().usage.status !== "error") setRfOk(Date.now());
  }, [refresh]);

  const viewing = viewSeatId != null;
  const data = viewing ? seatSnapshot : usage.data;
  const seatInfo = seats.find((s) => s.id === (viewSeatId ?? currentSeatId));
  const acctLabel = seatInfo ? seatLabel(seatInfo, settings.accounts.aliases) : null;
  const ghost = viewing ? " ghost" : "";
  const acctLine = acctLabel && (
    <div className="acct">
      <button title={seats.length > 1 ? "點一下切換帳號" : undefined} onClick={() => void cycleSeat()}>
        {acctLabel}
      </button>
      {viewing && <span className="ago">{data?.scrapedAt ? `${formatTimeAgo(data.scrapedAt)}的值` : "無記錄"}</span>}
    </div>
  );
  const session = data?.currentSession;
  const weekly = data?.weeklyAllModels;
  const fable = data?.weeklyFable;
  const pace = paceOf(session?.resetAt);
  const pace7 = paceOf(weekly?.resetAt, 168);
  const paceF = paceOf(fable?.resetAt, 168);
  const th = thresholdsOf(settings);
  const tint = settings.notifications.widgetTint;
  const needle = th.notice;
  const x5 = isExpired(viewing, session);
  const x7 = isExpired(viewing, weekly);
  const xf = isExpired(viewing, fable);
  const s5 = stateOf(x5 ? null : session?.usedPercent, th, tint);
  const s7 = stateOf(x7 ? null : weekly?.usedPercent, th, tint);
  const sf = stateOf(xf ? null : fable?.usedPercent, th, tint);
  const isError = usage.status === "error" && !data;

  const hover = (
    <div className="w5-gutter">
      <button title="切換密度" onClick={cycleDensity}>
        <Rows3 size={11} />
      </button>
      <button
        title="立即刷新"
        className={rfOk != null ? "ok" : undefined}
        onClick={() => void manualRefresh()}
      >
        {rfOk != null ? (
          <Check size={11} />
        ) : (
          <RefreshCw size={11} className={usage.status === "loading" ? "animate-spin" : ""} />
        )}
      </button>
      <button title="統計視窗" onClick={() => cmd.showStatistics()}>
        <BarChart3 size={11} />
      </button>
      <button title="設定" onClick={() => cmd.showSettings()}>
        <SettingsIcon size={11} />
      </button>
    </div>
  );

  const [rootW, rootH] = DENSITY_SIZE[density];
  const [panelW, panelH] = PANEL_SIZE[density];

  // ---- 錯誤（無資料可显示）----
  if (isError) {
    return (
      <div className="drag w5-root relative" style={{ width: rootW, height: rootH }}>
        {hover}
        <div className="w5" style={{ width: panelW, height: panelH, display: "flex" }}>
          <div className="plain">
            <AlertCircle size={13} style={{ color: "#f0594c", flex: "none" }} />
            <span>{usage.error ?? "無資料"}</span>
          </div>
        </div>
      </div>
    );
  }

  // ---- v1.8.1：W1／W4 兩張臉吃同一份 FaceProps；W5 維持下面原本的畫法 ----
  if (theme !== "w5") {
    const row = (id: FaceRow["id"], item: typeof session, pc: number | null, st: string, expired: boolean): FaceRow => ({
      id, item, pace: expired ? null : pc, tone: expired ? "safe" : FACE_TONE[st] ?? "safe", expired,
    });
    const face: FaceProps = {
      density,
      rows: [row("5H", session, pace, s5, x5), row("7D", weekly, pace7, s7, x7), row("FABLE", fable, paceF, sf, xf)],
      needle,
      mode,
      dark: prefersDark,
      ghost: viewing,
      acctLabel,
      ago: viewing ? (data?.scrapedAt ? `${formatTimeAgo(data.scrapedAt)}的值` : "無記錄") : null,
      canCycleSeat: seats.length > 1,
      onCycleSeat: () => void cycleSeat(),
      burn,
      freshNote: session?.note ?? "來源不明",
      stale: isStale(session?.note),
    };
    return (
      <div className="drag w5-root relative" style={{ width: rootW, height: rootH }}>
        {hover}
        {theme === "w1" ? <W1Face {...face} /> : <W4Face {...face} />}
      </div>
    );
  }

  // ---- micro 220×32（D34：無 ▼、整數％）----
  if (density === "micro") {
    const p5 = x5 ? null : session?.usedPercent;
    return (
      <div className="drag w5-root relative" style={{ width: rootW, height: rootH }}>
        {hover}
        <div className={`w5 micro${ghost}`} title={session?.note ?? undefined}>
          <span className="idlab">5H</span>
          <span className={`num n17 ${s5}`}>
            {p5 == null ? "—" : Math.round(p5)}
            <span className="unit">%</span>
          </span>
          <div className="duo">
            <Bar p={p5 ?? 0} h={3} pace={null} st={s5} needle={needle} />
            <Bar p={x7 ? 0 : (weekly?.usedPercent ?? 0)} h={3} pace={null} st={s7} needle={needle} />
            <Bar p={xf ? 0 : (fable?.usedPercent ?? 0)} h={3} pace={null} st={sf} needle={needle} />
          </div>
          <div className="mcol">
            <span className="mreset">
              <span className="rsym">⟳</span>
              {fmtMomentCompact(session?.resetAt, mode)}
            </span>
            <span className="m7d">
              <span className={s7}>
                <b>7D</b>
                <span className={`mv${s7 === "s-safe" ? " quiet" : ""}`}>
                  {x7 || weekly?.usedPercent == null ? "—" : Math.round(weekly.usedPercent)}
                </span>
              </span>
              <span className="msep">·</span>
              <span className={sf}>
                <b>F</b>
                <span className={`mv${sf === "s-safe" ? " quiet" : ""}`}>
                  {xf || fable?.usedPercent == null ? "—" : Math.round(fable.usedPercent)}
                </span>
              </span>
            </span>
          </div>
        </div>
      </div>
    );
  }

  // ---- card 280×228（D53 三軌）----
  if (density === "card") {
    return (
      <div className="drag w5-root relative" style={{ width: rootW, height: rootH }}>
        {hover}
        <div className={`w5 card${ghost}`}>
          {acctLine}
          <CardRow id="5H" item={session} pace={pace} mode={mode} st={s5} needle={needle} expired={x5} />
          <div className="hair" />
          <CardRow id="7D" item={weekly} pace={pace7} mode={mode} st={s7} needle={needle} expired={x7} />
          <div className="hair" />
          <CardRow id="FABLE" item={fable} pace={paceF} mode={mode} st={sf} needle={needle} expired={xf} />
        </div>
      </div>
    );
  }

  // ---- panel 320×336（D53 三軌）----
  return (
    <div className="drag w5-root relative" style={{ width: rootW, height: rootH }}>
      {hover}
      <div className={`w5 panel${ghost}`}>
        {acctLine}
        <CardRow id="5H" item={session} pace={pace} mode={mode} st={s5} needle={needle} expired={x5} />
        <div className="hair" />
        <CardRow id="7D" item={weekly} pace={pace7} mode={mode} st={s7} needle={needle} expired={x7} />
        <div className="hair" />
        <CardRow id="FABLE" item={fable} pace={paceF} mode={mode} st={sf} needle={needle} expired={xf} />
        <div className="hair" />
        <div className="stats">
          <div className="stat">
            <span className="slab">燃燒速度</span>
            <span className="sval">
              {burn?.burnPerHour != null ? burn.burnPerHour.toFixed(1) : "—"}
              <span className="sunit"> %/h</span>
            </span>
            <span className="ssub">近期真值樣本推算</span>
          </div>
          <EtaStat burn={burn} mode={mode} />
        </div>
        <div className="hair" />
        {/* D54: 帳號警示只住 dashboard，不佔 widget 空間（主人拍板）。 */}
        <div className={`fresh${isStale(session?.note) ? " stale" : ""}`}>
          {session?.note ?? "來源不明"}
        </div>
      </div>
    </div>
  );
}
