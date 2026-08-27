import { defineConfig } from "vitest/config";
import { svelte } from "@sveltejs/vite-plugin-svelte";

export default defineConfig({
  // Relative asset URLs: the packaged Electron shell loads the same dist/
  // over file:// (migration U7), where absolute '/' paths break.
  base: "./",
  plugins: [
    svelte(),
    {
      // A `crossorigin` module script forces CORS mode, and file:// is an
      // opaque origin — the packaged shell would show a blank window. Strip
      // the attribute Vite adds so the module loads under file://.
      name: "fly-strip-crossorigin",
      enforce: "post",
      transformIndexHtml(html) {
        return html.replace(/\s+crossorigin/g, "");
      },
    },
  ],
  // Keep the shell's own log lines visible in the dev terminal.
  clearScreen: false,
  server: {
    // The Electron dev loop points FLY_SHELL_URL at this fixed port.
    port: 1420,
    strictPort: true,
    // Don't watch the Rust side — cargo handles that.
    watch: { ignored: ["**/src-tauri/**"] },
  },
  test: {
    // Explicit (2026-08-27-001 KTD12): the shell's codec/recovery tests ride
    // the same run as the frontend's; nothing else in the tree is a test.
    include: ["src/**/*.test.ts", "electron/**/*.test.js"],
    exclude: ["**/node_modules/**", "**/dist/**", "electron/dist-el/**", "electron/frontend/**"],
  },
});
