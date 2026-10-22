import "./app.css";
import { mount } from "svelte";
import { invoke, listen, shellInfo } from "./lib/transport";
import App from "./App.svelte";

// macOS hides the native title bar (electron/main.js), so the control bar doubles
// as the title bar: `titlebar-overlay` reserves room for the traffic lights.
// The shell reports the platform (one source, no UA sniffing) and tells us when
// fullscreen hides the lights, so the gutter goes with them.
if (shellInfo().platform === "darwin") {
  const root = document.documentElement;
  root.classList.add("titlebar-overlay");
  void listen<boolean>("shell://fullscreen", (e) =>
    root.classList.toggle("titlebar-overlay", !e.payload),
  );
}

// The webview console is invisible outside devtools, so forward uncaught
// errors to the app's stderr.
const report = (msg: string) => void invoke("frontend_log", { msg }).catch(() => {});
window.addEventListener("error", (e) =>
  report(`error: ${e.message} @ ${e.filename}:${e.lineno}:${e.colno}`),
);
window.addEventListener("unhandledrejection", (e) =>
  report(`unhandledrejection: ${String(e.reason)}`),
);

const app = mount(App, {
  target: document.getElementById("app")!,
});

export default app;
