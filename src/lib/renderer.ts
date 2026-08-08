// Pure WebGL-attachment policy — the decision half of the KTD6 renderer
// eviction (foundation plan; built as T4 of the 2026-07-23 performance audit).
//
// The 2026-08-08 typing-latency diagnosis measured the xterm DOM renderer at
// ~20 ms of webview main-thread work per coalesced output flush — with a few
// visible streaming agents that saturates the thread, and every keystroke and
// scrollback scroll waits behind it. WebGL removes that per-flush cost, but a
// live GL context per pane is what WebKitGTK historically failed to composite
// (inactive panes blanked), which is why DOM was the default while the
// eviction policy was unbuilt. The policy bounds live contexts to the panes of
// one tab: attach while visible, dispose on hide. Terminal.svelte holds the
// effectful half (attach/dispose against the xterm addon); this module holds
// the rule so it is testable without a webview.
export type RendererMode = "auto" | "webgl" | "dom";

/**
 * Whether a pane should hold a live WebGL context right now.
 *
 * - `auto` (the default): only while the pane is visible (its tab is the
 *   active tab of the active workspace) — the eviction policy.
 * - `webgl`: always — the pre-eviction force switch, kept for debugging; this
 *   is the mode with the many-context blanking caveat.
 * - `dom`: never.
 * - `failed` (construction threw, or the context was lost): never again for
 *   this pane's lifetime — parity with the pre-T4 fallback, no retry loop.
 */
export function wantsWebgl(
  mode: RendererMode,
  visible: boolean,
  failed: boolean,
): boolean {
  if (failed || mode === "dom") return false;
  return mode === "webgl" || visible;
}
