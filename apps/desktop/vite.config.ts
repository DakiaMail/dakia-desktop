import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { resolve } from "node:path";
import { readFileSync } from "node:fs";

const bergamotDirectory = resolve(
  __dirname,
  "../../node_modules/@browsermt/bergamot-translator/worker",
);
const bergamotAssets = [
  "translator-worker.js",
  "bergamot-translator-worker.js",
  "bergamot-translator-worker.wasm",
];

function bergamotWorkerAssets() {
  return {
    name: "bergamot-worker-assets",
    configureServer(server) {
      server.middlewares.use("/bergamot", (request, response, next) => {
        const filename = request.url?.replace(/^\/+/, "");
        if (!filename || !bergamotAssets.includes(filename)) return next();
        response.setHeader(
          "Content-Type",
          filename.endsWith(".wasm")
            ? "application/wasm"
            : "text/javascript; charset=utf-8",
        );
        response.end(readFileSync(resolve(bergamotDirectory, filename)));
      });
    },
    generateBundle() {
      for (const filename of bergamotAssets) {
        this.emitFile({
          type: "asset",
          fileName: `bergamot/${filename}`,
          source: readFileSync(resolve(bergamotDirectory, filename)),
        });
      }
    },
  };
}

export default defineConfig({
  root: resolve(__dirname),
  plugins: [react(), bergamotWorkerAssets()],
  clearScreen: false,
  server: { port: 1420, strictPort: true, host: "127.0.0.1" },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: ["es2021", "safari13"],
    minify: !process.env.TAURI_DEBUG,
    sourcemap: !!process.env.TAURI_DEBUG,
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (id.includes("@mantine")) return "mantine";
          if (id.includes("@tabler")) return "icons";
          if (id.includes("i18next")) return "i18n";
          if (id.includes("@tauri-apps")) return "tauri";
          if (id.includes("date-fns")) return "dates";
          if (id.includes("react")) return "react";
          return undefined;
        },
      },
    },
  },
});
