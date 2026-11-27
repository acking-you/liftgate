import { fileURLToPath, URL } from "node:url";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const gatewayTarget = process.env.VITE_GATEWAY_PROXY ?? "http://127.0.0.1:8787";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: {
    // One bundle (both UI languages included) embedded in the gateway binary and served
    // from loopback, so the default 500 kB warning is noise here.
    chunkSizeWarningLimit: 800,
  },
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
  server: {
    host: "127.0.0.1",
    port: 5173,
    proxy: {
      "/api": {
        target: gatewayTarget,
        changeOrigin: true,
      },
      "/v1": {
        target: gatewayTarget,
        changeOrigin: true,
      },
      "/healthz": {
        target: gatewayTarget,
        changeOrigin: true,
      },
    },
  },
});
