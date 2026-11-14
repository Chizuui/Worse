import { fileURLToPath } from "node:url";
import { resolve } from "node:path";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";
import { pdfJsCMapsPlugin } from "../../../packages/ui/vite/pdfJsCMapsPlugin.ts";

const appRoot = fileURLToPath(new URL(".", import.meta.url));
const repoRoot = resolve(appRoot, "../../..");

const buildTarget = process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome105" : "safari13";

export default defineConfig({
  plugins: [pdfJsCMapsPlugin(), react(), tailwindcss()],
  clearScreen: false,
  envPrefix: ["VITE_", "TAURI_ENV_"],
  resolve: {
    alias: {
      "@": resolve(repoRoot, "packages/ui/src"),
      "d3-path": resolve(repoRoot, "node_modules/d3-path/src/index.js"),
    },
  },
  server: {
    port: 1430,
    strictPort: true,
  },
  optimizeDeps: {
    include: ["react", "react-dom", "react/jsx-runtime", "react/jsx-dev-runtime"],
  },
  worker: {
    rollupOptions: {
      treeshake: false,
    },
  },
  build: {
    target: buildTarget,
    sourcemap: process.env.TAURI_ENV_DEBUG === "true",
  },
});