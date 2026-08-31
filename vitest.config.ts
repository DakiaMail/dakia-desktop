import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    include: ["apps/desktop/src/**/*.{test,spec}.{ts,tsx}"],
    setupFiles: ["./apps/desktop/src/test/setup.ts"],
  },
});
