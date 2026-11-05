import { getCurrentWindow } from "@tauri-apps/api/window";

// =============================================================================
// v1.10.5（D106）：懸浮窗的拖曳由這裡處理，不用 CSS 的 app-region。
//
// 以前根節點掛 `-webkit-app-region` drag。wry 對每個 WebView2 都打開
// IsNonClientRegionSupportEnabled，WebView2 就把拖曳區做成 App 視窗裡的一組透明子視窗。
// 拖曳區裡只要有東西在動，例如「更新中」時轉圈的刷新圖示，Blink 每一幀都會重算一次拖曳區、
// 送一則 DraggableRegionsChanged，App 主執行緒也跟著每秒重排那些子視窗幾百次。
// 2026-10-21 現場：主執行緒處理不完，WebView2 主程序排著幾十萬則還沒處理的訊息，
// 長到 3.5 GB；同一個主執行緒也是托盤重畫要等的，刷新收不了尾，圖示就一直轉。
//
// 現在只在按下滑鼠的那一刻呼叫 `startDragging()`：Tauri 的 drag_window 會把
// WM_NCLBUTTONDOWN（HTCAPTION）交給系統，拖曳由 Windows 接手，手感跟以前一樣；
// 畫面上的東西怎麼動，都不會再產生任何視窗層的工作。
// =============================================================================

/** 按在這些東西上是「點」，不是「拖」：按鈕（控制列、切帳號的標籤）、連結、輸入框，
    以及標了 data-no-drag 的元素（⚠ 提示，游標是「?」）。 */
export const NO_DRAG_SELECTOR = "button, a, input, select, textarea, [data-no-drag]";

/** 拖曳只從標了 `drag` class 的根節點裡開始（懸浮窗五種畫面的最外層都有）。 */
export const DRAG_ROOT_SELECTOR = ".drag";

/** 這一下要不要拖視窗：左鍵、在拖曳根節點裡、不在可以點的東西上。 */
export function shouldStartDrag(target: EventTarget | null, button: number): boolean {
  if (button !== 0) return false;
  if (!(target instanceof Element)) return false;
  if (!target.closest(DRAG_ROOT_SELECTOR)) return false;
  return !target.closest(NO_DRAG_SELECTOR);
}

/** 掛上懸浮窗的拖曳；回傳拆掉的函式。 */
export function installWidgetDrag(): () => void {
  const onMouseDown = (e: MouseEvent) => {
    if (!shouldStartDrag(e.target, e.button)) return;
    // 不讓按下去變成選字；拖曳交給系統。
    e.preventDefault();
    void getCurrentWindow()
      .startDragging()
      .catch(() => {
        /* 視窗正在關、或權限被拿掉：這一下就不拖 */
      });
  };
  // 以前右鍵落在拖曳區會叫出系統選單；現在落在網頁上，不讓 WebView2 的「上一頁／重新整理」選單跑出來。
  const onContextMenu = (e: MouseEvent) => e.preventDefault();
  document.addEventListener("mousedown", onMouseDown);
  document.addEventListener("contextmenu", onContextMenu);
  return () => {
    document.removeEventListener("mousedown", onMouseDown);
    document.removeEventListener("contextmenu", onContextMenu);
  };
}
