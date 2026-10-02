import { Component, type ReactNode } from "react";
import { RefreshCw } from "lucide-react";
import { pageCache } from "@/lib/pageCache";

// =============================================================================
// v1.8.13（D91 T-03）：統計視窗的兩道安全網。
//
//   EmptyLedger        帳本裡還沒有任何帳號×組織（新安裝、CLI 與 Desktop 都沒登入，
//                      或第一次採集完成前那十幾秒）——說人話告訴使用者等一下就有。
//   PageErrorBoundary  某一頁 render 丟錯時只換掉那一頁。以前全站沒有 ErrorBoundary，
//                      趨勢頁一丟 TypeError 整棵樹被卸掉、側欄也不見；統計視窗的 X 只是
//                      隱藏，不重開 App 怎麼開關都是白的。
//
// 兩個都只用既有語彙：外層 .card、說明 .foot-note、按鈕 .h5btn（同今日頁「重新整理」）。
// =============================================================================

export function EmptyLedger() {
  return (
    <article className="h5 card">
      <div className="card-head">
        <span className="card-title zh">還沒有資料</span>
      </div>
      <p className="foot-note zh" style={{ marginTop: 8, fontSize: 11.5, color: "var(--h5-ink2)" }}>
        {/* 一句一行寫在同一個字串裡——JSX 換行會變成半形空白，中文句中多一格很醜 */}
        {"帳本裡還沒有任何帳號的記錄。App 開著、第一次查完額度後（通常十幾秒）這裡就會出現；" +
          "如果一直是空的，請確認 Claude Code 或 Claude Desktop 至少有一個已經登入。"}
      </p>
    </article>
  );
}

interface BoundaryProps {
  children: ReactNode;
}
interface BoundaryState {
  error: Error | null;
  /** 每按一次「重新載入」加一，當成子樹的 key——整頁重新掛載、重新抓資料。 */
  attempt: number;
}

export class PageErrorBoundary extends Component<BoundaryProps, BoundaryState> {
  state: BoundaryState = { error: null, attempt: 0 };

  static getDerivedStateFromError(error: Error): Partial<BoundaryState> {
    return { error };
  }

  // v1.8.13（D91 T-03 審查）：安裝版看不到 devtools，前端也沒有寫 log 到後端的管道，
  // 診斷包裡不會有這個錯誤——所以錯誤訊息直接印在卡片上，讓使用者截圖回報。

  // 錯誤可能來自 pageCache 裡那份舊 payload（切頁回來先畫舊的）——重試前清掉，
  // 讓重新掛載的頁面走骨架→重新抓，而不是再畫一次同一份壞資料。
  componentDidCatch() {
    pageCache.clear();
  }

  private retry = () => {
    pageCache.clear();
    this.setState((s) => ({ error: null, attempt: s.attempt + 1 }));
  };

  render() {
    if (this.state.error) {
      return (
        <article className="h5 card">
          <div className="card-head">
            <span className="card-title zh">這一頁畫不出來</span>
          </div>
          <p className="foot-note zh" style={{ marginTop: 8, fontSize: 11.5, color: "var(--h5-ink2)" }}>
            {"這一頁在整理資料時出了錯，其他頁不受影響，可以從左邊切過去。先按「重新載入」再試一次；" +
              "一直出現的話，請把這一頁截圖（含下面那行錯誤訊息）交給開發者。"}
          </p>
          {/* 跟各頁「讀取失敗:{error}」同一種做法：原文照印、小字、可換行。 */}
          <p className="foot-note" style={{ marginTop: 6, wordBreak: "break-all" }}>
            錯誤訊息:{this.state.error.message || String(this.state.error)}
          </p>
          <div style={{ marginTop: 12 }}>
            <button className="h5btn" onClick={this.retry}>
              <RefreshCw size={11} />
              <span className="zh">重新載入</span>
            </button>
          </div>
        </article>
      );
    }
    return <div key={this.state.attempt}>{this.props.children}</div>;
  }
}
