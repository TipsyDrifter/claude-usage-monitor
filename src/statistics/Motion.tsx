import { useEffect, useState } from "react";

// =============================================================================
// v1.4 · 統計視窗的三個共用小元件（D77）
//   ToastHost / toast()  — E13：右下角一條深色小條，一次只留一條（新的取代舊的）
//   TipHost / showTip()  — E21–E24：圖表 hover 用的深色小標，跟游標、不吃滑鼠
//   PageSkeleton         — E28：資料還沒回來時的 2–3 條灰條（取代「載入中…」）
//
// 都刻意不引新依賴（D77 規則：不加新依賴）：module scope 存一個 setter，
// 任何頁面直接呼叫函式即可，不必拉 context 或 store。
// =============================================================================

/* ---------------------------------------------------------------- E13 toast */

interface ToastMsg {
  id: number;
  text: string;
}

let setToast: ((t: ToastMsg | null) => void) | null = null;
let toastSeq = 0;

/** 操作完成的一句話。一次只留一條——新的直接取代舊的，不堆疊。 */
export function toast(text: string) {
  setToast?.({ id: ++toastSeq, text });
}

export function ToastHost() {
  const [msg, setMsg] = useState<ToastMsg | null>(null);

  useEffect(() => {
    setToast = setMsg;
    return () => {
      setToast = null;
    };
  }, []);

  const id = msg?.id;
  useEffect(() => {
    if (id == null) return;
    const t = window.setTimeout(() => setMsg((cur) => (cur?.id === id ? null : cur)), 2700);
    return () => window.clearTimeout(t);
  }, [id]);

  if (!msg) return null;
  return (
    <div className="toasts">
      {/* key＝流水號：換內容時重播滑入，才看得出「這是新的一條」 */}
      <div className="toast zh" key={msg.id}>
        <span className="ck">✓</span>
        {msg.text}
      </div>
    </div>
  );
}

/* ------------------------------------------------------------ E21–E24 tooltip */

interface TipState {
  x: number;
  y: number;
  text: string;
}

let setTipState: ((t: TipState | null) => void) | null = null;

/** 圖表 hover 小標：跟著游標走。text 是圖上本來讀不出來的東西才值得叫它。 */
export function showTip(ev: { clientX: number; clientY: number }, text: string) {
  setTipState?.({ x: ev.clientX, y: ev.clientY, text });
}

export function hideTip() {
  setTipState?.(null);
}

export function TipHost() {
  const [tip, setTip] = useState<TipState | null>(null);

  useEffect(() => {
    setTipState = setTip;
    const hide = () => setTip(null);

    // 小標只該在「游標正停在那個東西上」時存在。各圖表的 onMouseLeave 管得住
    // 「滑到旁邊」，切頁由 Statistics 的 hideTip() 管；剩下這幾種情況沒有任何
    // mouseleave 會來，小標會孤零零留在畫面上（實測全都會卡住）：
    //   scroll     捲動時滑鼠沒動，但 position: fixed 的小標會飄到錯的位置
    //   blur       alt-tab 切走——回來時一張小標浮在那裡，底下沒有游標
    //   hidden     懸浮窗收進匣／統計視窗最小化，再打開小標還在
    //   mouseout   游標整個離開視窗（relatedTarget === null 就是離開文件）
    window.addEventListener("scroll", hide, true);
    window.addEventListener("blur", hide);
    document.addEventListener("visibilitychange", hide);
    const onOut = (ev: MouseEvent) => {
      if (ev.relatedTarget == null) hide();
    };
    document.addEventListener("mouseout", onOut);
    return () => {
      setTipState = null;
      window.removeEventListener("scroll", hide, true);
      window.removeEventListener("blur", hide);
      document.removeEventListener("visibilitychange", hide);
      document.removeEventListener("mouseout", onOut);
    };
  }, []);

  if (!tip) return null;
  // position: fixed → 直接吃 clientX/Y，不必補捲動位移。
  return (
    <div className="tip" style={{ left: tip.x, top: tip.y - 6 }}>
      {tip.text}
    </div>
  );
}

/* --------------------------------------------------------------- E28 骨架 */

/** 等資料的那幾百毫秒：灰條＋掃光，回答「這裡會有東西，正在拿」
    （跟「這裡沒東西」區分開）。資料到了就換真內容。 */
export function PageSkeleton() {
  return (
    <div className="h5" aria-busy="true" style={{ padding: "8px 2px" }}>
      <div className="skel" style={{ width: "100%" }} />
      <div className="skel" style={{ width: "78%" }} />
      <div className="skel" style={{ width: "44%" }} />
    </div>
  );
}
