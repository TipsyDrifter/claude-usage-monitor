import { emit } from "@tauri-apps/api/event";

/** 跟後端 `widget_pos::PAINTED_EVENT` 同一個名字。 */
export const PAINTED_EVENT = "widget://painted";

/**
 * D107：懸浮窗藏起來時後端把 WebView 設成看不見（頁面變 hidden、不再畫）。要顯示時後端先把
 * WebView 設回看得見、等這個訊號，才把視窗秀出來——不然視窗出來的第一格是藏起來前的舊畫面
 * （A／B 截圖 8 輪有 7 輪閃一格）。
 *
 * 等三個 rAF：第二個 rAF 只代表新的一格交給了合成器；`--disable-gpu` 下合成在 GPU 行程用 CPU 做，
 * 真的貼到畫面上還要再一格。只等兩個時 8 輪仍有 1 輪閃一格。
 */
export function announcePaintAfterShow(): void {
  if (!("__TAURI_INTERNALS__" in window)) return; // 瀏覽器裡的 demo／fixture 模式
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState !== "visible") return;
    let frames = 0;
    const tick = () => {
      if (++frames < 3) {
        requestAnimationFrame(tick);
        return;
      }
      void emit(PAINTED_EVENT).catch(() => {});
    };
    requestAnimationFrame(tick);
  });
}
