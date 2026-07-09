import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri expects a fixed dev port and no clever obfuscation of the server.
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  // Tauri serves the app from this dev server; keep the port stable and fail
  // loudly if it's taken rather than silently hopping ports (breaks tauri.conf).
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? { protocol: "ws", host, port: 1421 }
      : undefined,
    watch: {
      // src-tauri is a Rust crate; Vite has no business watching it.
      ignored: ["**/src-tauri/**"],
    },
  },
  // Produce assets Tauri can package. esbuild target matches a modern webview.
  build: {
    target: "es2021",
    minify: "esbuild",
    sourcemap: false,
  },
});
