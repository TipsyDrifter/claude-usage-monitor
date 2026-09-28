import React from "react";
import ReactDOM from "react-dom/client";
import { Statistics } from "./Statistics";
import { useStore } from "@/store/usageStore";
import { isDemoMode } from "@/lib/mock";
import "@/styles/global.css";

if (isDemoMode()) {
  useStore.getState().loadDemo();
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <Statistics />
  </React.StrictMode>,
);
