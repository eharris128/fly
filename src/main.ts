import "./app.css";
import { mount } from "svelte";
import { invoke } from "./lib/transport";
import App from "./App.svelte";

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
