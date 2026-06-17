import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

export default defineConfig({
  plugins: [
    svelte(),
    {
      // WebKitGTK fails to load `crossorigin` module scripts from Tauri's
      // custom asset protocol, leaving a blank window in release builds.
      name: "fly-strip-crossorigin",
      enforce: "post",
      transformIndexHtml(html) {
        return html.replace(/\s+crossorigin/g, "");
      },
    },
  ],
  // Tauri shows its own errors; don't let Vite wipe them.
  clearScreen: false,
  server: {
    // Tauri expects a fixed dev port.
    port: 1420,
    strictPort: true,
    // Don't watch the Rust side — cargo handles that.
    watch: { ignored: ["**/src-tauri/**"] },
  },
});
