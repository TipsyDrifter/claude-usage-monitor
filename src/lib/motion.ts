import { useEffect, useRef, useState } from "react";

// =============================================================================
// v1.4 · 有回應的畫面（D77）——「只有值真的變了才動」的共用零件。
//
// 規則 1（D77，不准破）：E07 E09 E10 E11 E12 E26 E27 全部要記上一幀比較；
// 第一次載入不算「變」——不做入場動畫（D67-Q13 ⑤）。這裡的每個 hook 都
// 把「上一幀」存在 useRef 裡，第一次拿到值時只寫 ref、不啟動動畫。
//
// 規則 4：prefers-reduced-motion 下 JS 動畫直接跳終值（CSS 那半在 h5.css／
// w5.css 的 @media 區塊歸零）。
// =============================================================================

/** 使用者要求減少動態？（每次問即時值——系統設定可能中途改） */
export function reduceMotion(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches
  );
}

/** v1.10.4（D105）：多久沒有任何操作就把無限循環的動畫停下來。 */
export const MOTION_IDLE_MS = 60_000;

/** v1.10.4（D105）：這個視窗現在有沒有人在看——沒有的話，無限循環的動畫要停。
    今日頁點陣條有 202 個無限動畫，視窗只要開著就每秒重畫 60 次（renderer 約七成顆核心、
    GPU 行程再六成），縮到工作列也一樣。被別的視窗蓋住時 WebView2 會不會自己停沒測過，所以由頁面判斷：
    切到別的程式（blur）或一分鐘沒有任何滑鼠／鍵盤動作就算沒人看；滑鼠移進來、按鍵、
    拿回焦點就醒來。最小化另外由後端把 WebView 設成看不見（windows.rs）。
    暫停是用 CSS `animation-play-state`（h5.css `.motion-asleep`）——醒來時從停住的那一格接著流。 */
export function useMotionAwake(idleMs = MOTION_IDLE_MS): boolean {
  const [awake, setAwake] = useState(true);
  useEffect(() => {
    let t: number | undefined;
    const sleep = () => {
      window.clearTimeout(t);
      setAwake(false);
    };
    const wake = () => {
      setAwake(true);
      window.clearTimeout(t);
      t = window.setTimeout(sleep, idleMs);
    };
    const onVisibility = () => (document.visibilityState === "hidden" ? sleep() : wake());
    const wakeOn = ["pointermove", "pointerdown", "keydown", "wheel", "focus"] as const;
    for (const ev of wakeOn) window.addEventListener(ev, wake, { passive: true });
    window.addEventListener("blur", sleep);
    document.addEventListener("visibilitychange", onVisibility);
    // 剛打開的視窗就是有人要看（就算焦點沒搶到，例如從懸浮窗打開時）——先醒著，一分鐘沒動靜再睡。
    wake();
    return () => {
      window.clearTimeout(t);
      for (const ev of wakeOn) window.removeEventListener(ev, wake);
      window.removeEventListener("blur", sleep);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [idleMs]);
  return awake;
}

/** 上一幀的值（第一次回 undefined——呼叫端據此判斷「這不是變化，是初值」）。 */
export function usePrev<T>(value: T): T | undefined {
  const ref = useRef<T | undefined>(undefined);
  useEffect(() => {
    ref.current = value;
  }, [value]);
  return ref.current;
}

/** E07 大數字補間：值變了才從舊值滾到新值（0.6s，先快後慢）。
    第一次拿到值直接落定；rAF 在背景分頁會停，所以另有終值保險。 */
export function useTweenNumber(target: number | null, dur = 600): number | null {
  const [shown, setShown] = useState<number | null>(target);
  const fromRef = useRef<number | null>(target);

  useEffect(() => {
    const from = fromRef.current;
    fromRef.current = target;
    // 初值、消失、沒變、或使用者要求減少動態 → 直接落定，不動。
    if (target == null || from == null || from === target || reduceMotion()) {
      setShown(target);
      return;
    }
    let raf = 0;
    const t0 = performance.now();
    const step = (t: number) => {
      const k = Math.min(1, (t - t0) / dur);
      const e = 1 - Math.pow(1 - k, 3);
      setShown(Math.round(from + (target - from) * e));
      if (k < 1) raf = requestAnimationFrame(step);
    };
    raf = requestAnimationFrame(step);
    const guard = window.setTimeout(() => setShown(target), dur + 60);
    return () => {
      cancelAnimationFrame(raf);
      window.clearTimeout(guard);
    };
  }, [target, dur]);

  return shown;
}

/** E09 變化閃琥珀：把回傳的 ref 掛在要閃的元素上，值真的變了才加 `.flash`，
    animationend 自己拿掉（之後查 DOM 才看得出「這次有沒有動」）。 */
export function useFlashOnChange<T extends HTMLElement>(
  value: number | string | null | undefined,
): React.RefObject<T | null> {
  const ref = useRef<T | null>(null);
  const prev = useRef(value);

  useEffect(() => {
    const p = prev.current;
    prev.current = value;
    if (p == null || value == null || p === value) return; // 初值／消失／沒變
    const el = ref.current;
    if (!el || reduceMotion()) return;
    el.classList.remove("flash");
    void el.offsetWidth; // 強制 reflow，同一個值連續變也會重播
    el.classList.add("flash");
    const done = () => el.classList.remove("flash");
    el.addEventListener("animationend", done, { once: true });
    // 保險：視窗被藏起來（懸浮窗縮到匣、統計視窗切到背景）時瀏覽器會凍住
    // CSS 動畫，animationend 永遠不來——沒有這個 timer，class 會黏在 DOM 上。
    const guard = window.setTimeout(done, 1200);
    return () => {
      el.removeEventListener("animationend", done);
      window.clearTimeout(guard);
    };
  }, [value]);

  return ref;
}
