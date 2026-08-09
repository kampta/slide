import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const DAEMON = process.env.SLIDE_DAEMON ?? "http://127.0.0.1:7777";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    strictPort: true,
    proxy: {
      "/api": { target: DAEMON, changeOrigin: true },
      "/ws": {
        target: DAEMON,
        changeOrigin: true,
        ws: true,
        configure: (proxy) => {
          proxy.on("error", () => {});
        },
      },
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
});
