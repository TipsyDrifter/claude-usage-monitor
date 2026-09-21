import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { TrendingUp, Gauge, HeartPulse, History, Settings as SettingsIcon } from "lucide-react";
import { useStore } from "@/store/usageStore";
import { seatLabel } from "@/lib/seatLabel";
import { cmd } from "@/lib/tauri";
import { cn } from "@/lib/cn";
import { HalfYearPage } from "./pages/HalfYear";
import { TodayPage } from "./pages/Today";
import { HealthPage } from "./pages/Health";
import { HistoryPage } from "./pages/History";
import { ToastHost, TipHost, hideTip } from "./Motion";
import "./h5.css";

// =============================================================================
// Statistics window — D55 全站 H5 統一版。
//
// 三頁制：這半年（研究層・主頁）／今天撞牆嗎（決策層）／資料健康度（帳本）。
// 爬蟲時代的六個舊頁已退役（讀舊庫、M0 後零新資料）——功能由新頁取代，
// 分佈／歷史檢視待接新 schema 後回歸。
// Chrome 遵循 §8.6：中性白畫布、Noto Sans TC 900 標題、方角 highlight＋
// 左緣 3px 實槓（D50 強調語言——不用圓角膠囊當 active 態）。
// =============================================================================

export type PageKey = "halfyear" | "today" | "history" | "health";

interface NavItem {
  key: PageKey;
  label: string;
  en: string;
  icon: typeof Gauge;
}

// D60：今日頁是首頁（決策層天生是入口）；靈魂不必守大門。
const NAV_ITEMS: NavItem[] = [
  { key: "today", label: "今天撞牆嗎", en: "TODAY", icon: Gauge },
  // v1.4.1（D78 主人拍板）：「這半年」改名「趨勢」——期間可選 30／90／180／全部，不只半年。
  { key: "halfyear", label: "趨勢", en: "TREND", icon: TrendingUp },
  { key: "history", label: "歷史檢視", en: "HISTORY", icon: History },
  { key: "health", label: "資料健康度", en: "HEALTH", icon: HeartPulse },
];

const VALID_KEYS = new Set<PageKey>(NAV_ITEMS.map((n) => n.key));

function isValidPageKey(s: string | undefined | null): s is PageKey {
  return !!s && VALID_KEYS.has(s as PageKey);
}

export function Statistics() {
  const init = useStore((s) => s.init);
  const settings = useStore((s) => s.settings);
  const updateSettings = useStore((s) => s.updateSettings);
  // v1.3 切換器（D75 決策點 8）：頂欄下拉選座位，三頁跟著走。
  const seats = useStore((s) => s.seats);
  const currentSeatId = useStore((s) => s.currentSeatId);
  const viewSeatId = useStore((s) => s.viewSeatId);
  const setViewSeat = useStore((s) => s.setViewSeat);

  useEffect(() => {
    init();
  }, [init]);

  // 記住上次看的頁；舊 key（overview 等已退役頁）自動落回主頁「這半年」。
  const activePage: PageKey = useMemo(() => {
    const stored = settings.statisticsWindow.lastPage;
    return isValidPageKey(stored) ? stored : "today";
  }, [settings.statisticsWindow.lastPage]);

  const switchPage = (key: PageKey) => {
    if (key === activePage) return;
    void updateSettings("statisticsWindow", { lastPage: key });
  };

  const activeItem = NAV_ITEMS.find((n) => n.key === activePage) ?? NAV_ITEMS[0];

  // E20（D77）：左緣 3px 靛藍槓改成一個絕對定位元素，切頁時滑到新項目——
  // 連續位移比「舊處消失、新處出現」更容易被眼角餘光跟上。位置量真實的
  // offsetTop／offsetHeight，不硬編行高（字級或 padding 日後改也不會歪）。
  const navRef = useRef<HTMLElement | null>(null);
  const [ind, setInd] = useState<{ top: number; height: number } | null>(null);
  useLayoutEffect(() => {
    const el = navRef.current?.querySelector<HTMLButtonElement>(".nav-btn.on");
    if (el) setInd({ top: el.offsetTop, height: el.offsetHeight });
    // 換頁時被 hover 的圖表會整個卸載，mouseleave 不會來——小標要自己收掉。
    hideTip();
  }, [activePage]);

  return (
    <div className="h5 flex h-screen w-screen overflow-hidden" style={{ background: "var(--h5-bg)" }}>
      {/* ===== 側欄 ===== */}
      <aside
        className="flex w-[196px] shrink-0 flex-col"
        style={{ background: "var(--h5-card)", borderRight: "1px solid var(--h5-line)" }}
      >
        <div className="px-4 pt-5 pb-4">
          {/* 3×3 網點品牌標（H5 基準檔的 logo 語言） */}
          <svg width="24" height="24" viewBox="0 0 26 26" style={{ marginBottom: 8 }}>
            {[0, 1, 2].flatMap((r) =>
              [0, 1, 2].map((c) => (
                <rect
                  key={`${r}${c}`}
                  x={c * (26 / 3) + 0.8}
                  y={r * (26 / 3) + 0.8}
                  width={26 / 3 - 2.2}
                  height={26 / 3 - 2.2}
                  rx="2.4"
                  fill={r === 2 && c === 2 ? "var(--h5-ink)" : "var(--h5-accent)"}
                />
              )),
            )}
          </svg>
          <h1 className="zh" style={{ fontWeight: 900, fontSize: 14.5, lineHeight: 1.2 }}>
            Claude Usage Monitor
          </h1>
          <p style={{ fontSize: 8.5, letterSpacing: "0.14em", color: "var(--h5-ink3)", fontWeight: 600, marginTop: 2 }}>
            長期帳本 DASHBOARD
          </p>
        </div>

        <nav ref={navRef} className="nav flex flex-1 flex-col" style={{ padding: "0 0 8px" }}>
          {ind && <span className="ind" style={{ top: ind.top, height: ind.height }} />}
          {NAV_ITEMS.map((item) => (
            <NavButton
              key={item.key}
              item={item}
              active={item.key === activePage}
              onClick={() => switchPage(item.key)}
            />
          ))}
        </nav>

        <div style={{ borderTop: "1px solid var(--h5-line-soft)", padding: "10px 8px" }}>
          <button
            onClick={() => cmd.showSettings()}
            className="zh flex w-full items-center gap-2"
            style={{
              padding: "8px 12px",
              fontSize: 12,
              color: "var(--h5-ink2)",
              background: "none",
              border: "none",
              cursor: "pointer",
            }}
          >
            <SettingsIcon size={13} />
            <span>設定</span>
          </button>
        </div>
      </aside>

      {/* ===== 主內容 ===== */}
      <main className="flex flex-1 flex-col overflow-hidden">
        <header
          className="flex items-center gap-2"
          style={{
            background: "var(--h5-card)",
            borderBottom: "1px solid var(--h5-line)",
            padding: "13px 24px",
          }}
        >
          <activeItem.icon size={16} style={{ color: "var(--h5-accent)" }} />
          <h2 className="zh" style={{ fontWeight: 900, fontSize: 15.5 }}>
            {activeItem.label}
          </h2>
          <span style={{ fontSize: 9, letterSpacing: "0.16em", color: "var(--h5-ink3)", fontWeight: 600 }}>
            {activeItem.en}
          </span>
          {seats.length > 0 && (
            <label className="zh" style={{ marginLeft: "auto", display: "flex", alignItems: "center", gap: 6, fontSize: 11, color: "var(--h5-ink2)" }}>
              <span style={{ fontSize: 9, letterSpacing: "0.14em", color: "var(--h5-ink3)", fontWeight: 600 }}>帳號</span>
              <select
                value={viewSeatId ?? currentSeatId ?? ""}
                onChange={(e) => void setViewSeat(e.target.value || null)}
                style={{
                  font: "inherit",
                  fontSize: 11.5,
                  padding: "3px 8px",
                  border: "1px solid var(--h5-line)",
                  borderRadius: 99,
                  background: "var(--h5-bg)",
                  color: "var(--h5-ink)",
                }}
              >
                {seats.map((s) => (
                  <option key={s.id} value={s.id}>
                    {seatLabel(s, settings.accounts.aliases)}
                    {s.id === currentSeatId ? "（目前登入）" : ""}
                  </option>
                ))}
              </select>
              {viewSeatId != null && (
                <span style={{ fontSize: 10.5, color: "var(--h5-amber-ink)" }}>最後已知值</span>
              )}
            </label>
          )}
        </header>

        <div className="flex-1 overflow-auto" style={{ padding: 20 }}>
          <AnimatePresence mode="wait">
            <motion.div
              key={activePage}
              /* E18（D77，主人二選一選了純淡入）：切頁 150ms 淡入，不上滑——
                 上滑是「東西從某處來」的隱喻，但分頁內容不是從下面來的。 */
              initial={{ opacity: 0 }}
              animate={{ opacity: 1 }}
              exit={{ opacity: 0 }}
              transition={{ duration: 0.15 }}
              className="mx-auto"
              style={{ maxWidth: 1120 }}
            >
              <PageBody page={activePage} />
            </motion.div>
          </AnimatePresence>
        </div>
      </main>

      {/* v1.4：操作完成的 toast（E13）與圖表 hover 小標（E21–E24）的家。
          兩個都 position: fixed，掛在 .h5 根底下才吃得到 h5 的色彩 token。 */}
      <ToastHost />
      <TipHost />
    </div>
  );
}

function NavButton({
  item,
  active,
  onClick,
}: {
  item: NavItem;
  active: boolean;
  onClick: () => void;
}) {
  const Icon = item.icon;
  // E01（D77）：hover／按下要有感覺，所以 inline style 改成 class `nav-btn`
  // （CSS 在 h5.css）。active 的顏色語意照舊；左緣 3px 槓交給 nav 裡的 .ind
  // 滑動（E20），這裡不再各自畫 inset boxShadow。
  return (
    <button
      onClick={onClick}
      className={cn("nav-btn zh", active && "on")}
    >
      <Icon size={13} style={{ color: active ? "var(--h5-accent)" : "var(--h5-ink3)" }} />
      <span className="flex-1">{item.label}</span>
      <span style={{ fontSize: 8, letterSpacing: "0.12em", color: "var(--h5-ink3)", fontFamily: "inherit" }}>
        {item.en}
      </span>
    </button>
  );
}

function PageBody({ page }: { page: PageKey }) {
  switch (page) {
    case "halfyear":
      return <HalfYearPage />;
    case "today":
      return <TodayPage />;
    case "history":
      return <HistoryPage />;
    case "health":
      return <HealthPage />;
  }
}
