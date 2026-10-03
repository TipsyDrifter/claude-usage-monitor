import React from "react";
import ReactDOM from "react-dom/client";
import { Statistics } from "./Statistics";
import { PageErrorBoundary } from "./PageGuards";
import { useStore } from "@/store/usageStore";
import { isDemoMode } from "@/lib/mock";
import "@/styles/global.css";

if (isDemoMode()) {
  useStore.getState().loadDemo();
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    {/* v1.8.13（D91 T-03）：最後一道網——連側欄／頂欄都出錯時，至少不是整窗白 */}
    <PageErrorBoundary>
      <Statistics />
    </PageErrorBoundary>
  </React.StrictMode>,
);
