import { resolve } from "node:path";
import { defineConfig } from "vite";

const root = import.meta.dirname;

// Each overlay window is its own page so it loads only what it needs.
export default defineConfig({
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: {
    target: "es2022",
    rollupOptions: {
      input: {
        bubble: resolve(root, "bubble.html"),
        panel: resolve(root, "panel.html"),
      },
    },
  },
});
