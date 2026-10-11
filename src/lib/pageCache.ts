/**
 * v1.8.2 效能：統計視窗各頁「上一次拿到的 payload」。
 *
 * 頁面元件切頁就 unmount，再回來 state 是空的，只能先畫骨架等 IPC——即使後端
 * 已經有快取、IPC 只要幾十 ms，骨架還是會閃一下。這裡把 payload 按 key 留在
 * 模組層：回到頁面先用舊的畫，IPC 回來再換新的（stale-while-revalidate）。
 * 只活在這個 webview 的生命週期內，不落盤。
 */
const store = new Map<string, unknown>();

export const pageCache = {
  get<T>(key: string): T | null {
    return (store.get(key) as T | undefined) ?? null;
  },
  set<T>(key: string, value: T) {
    store.set(key, value);
  },
  /** v1.8.13（D91 T-03）：頁面 render 出錯時整個清掉——不然「重新載入」會先拿回同一份
   *  把頁面弄壞的舊 payload 再畫一次、又壞，永遠等不到新資料。 */
  clear() {
    store.clear();
  },
};
