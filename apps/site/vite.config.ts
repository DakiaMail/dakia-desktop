import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { resolve } from "node:path";

const workspaceRoot = resolve(__dirname, "../..");

export default defineConfig({
  root: resolve(__dirname),
  plugins: [react()],
  server: {
    host: "127.0.0.1",
    port: 4173,
    strictPort: true,
    fs: { allow: [workspaceRoot] },
  },
  preview: { host: "127.0.0.1", port: 4174, strictPort: true },
  build: { target: "es2022" },
});
