import { defineConfig } from "vite";
import { sveltekit } from "@sveltejs/kit/vite";

// Deno + Node both expose env; Tauri sets TAURI_DEV_HOST for remote HMR.
const host =
  (typeof Deno !== "undefined" && Deno.env.get("TAURI_DEV_HOST")) ||
  (typeof process !== "undefined" && process.env.TAURI_DEV_HOST) ||
  false;

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [sveltekit()],

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
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
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
