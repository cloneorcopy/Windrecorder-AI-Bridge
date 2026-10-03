import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";
import process from "node:process";

const host = process.env.TAURI_DEV_HOST;

// Port 1421 rather than Tauri's customary 1420: the ServicePulse dashboard this design follows runs
// its dev server on 1420, and `strictPort` means a collision is a hard failure rather than a silent
// move to a port the Rust shell was not told about.
export default defineConfig(() => ({
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  server: {
    port: 1421,
    strictPort: true,
    host: host || false,
    hmr: host
      ? { protocol: "ws", host, port: 1422 }
      : undefined,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
}));
