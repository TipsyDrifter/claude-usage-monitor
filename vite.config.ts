import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { resolve } from "path";

const host = process.env.TAURI_DEV_HOST;

export default defineConfig(async () => ({
  plugins: [react(), tailwindcss()],

  resolve: {
    alias: {
      "@": resolve(__dirname, "./src"),
    },
  },

  build: {
    // 前端 bundle 出到 build/（不是 dist/）：vite build 會清空 outDir，
    // dist/ 留給安裝包（dist/installers），不然每次 build 都把安裝包掃掉。
    outDir: "build",
    rollupOptions: {
      input: {
        widget: resolve(__dirname, "widget.html"),
        settings: resolve(__dirname, "settings.html"),
        statistics: resolve(__dirname, "statistics.html"),
        demo: resolve(__dirname, "demo.html"),
      },
    },
  },

  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
}));
