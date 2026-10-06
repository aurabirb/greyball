import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  server: { proxy: { "/ws": {
        target: "ws://127.0.0.1:7878",
        ws: true,
        changeOrigin: true,
        configure: (proxy) => proxy.on("proxyReqWs", (req) => req.setHeader("origin", "http://127.0.0.1:7878")),
      } } },
});
