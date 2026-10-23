/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  // demo.html (fake backend for browser review) is dev-server only; the app bundles index.html
  build: { target: "es2022", sourcemap: false, rollupOptions: { input: "index.html" } },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    css: false,
  },
});
