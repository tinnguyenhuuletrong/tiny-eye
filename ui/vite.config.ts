import { defineConfig } from "vite";
import preact from "@preact/preset-vite";

// `npm run dev` serves the UI on :5173 and proxies the API to a running tiny-eye (TINY_EYE_URL).
const target = process.env.TINY_EYE_URL || "http://127.0.0.1:7070";

export default defineConfig({
  plugins: [preact()],
  base: "/",
  build: { outDir: "dist", emptyOutDir: true, assetsDir: "assets", chunkSizeWarningLimit: 800 },
  server: { proxy: { "/api": target, "/openapi.yaml": target } },
});
