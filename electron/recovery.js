// Renderer-crash recovery — the pure half (tested in recovery.test.js; main.js
// wires it to Electron). Born of the 2026-08-22 incident: the Chromium
// renderer OOM-died under memory pressure and the shell sat on a blank
// window for over an hour while the core and nine agents kept working —
// nothing reloaded the frontend, every control-socket frame threw "Render
// frame was disposed" (2,119 times in 77 s), and the window could not even
// be closed because the close flow waited forever for a dead renderer's
// verdict. Three decisions, each a function here.
'use strict';

/**
 * Bounds the auto-reload: at most `max` reloads inside any `windowMs`
 * window, after which the shell shows the crash page instead of looping
 * (a renderer that dies on every load — a broken frontend, a box with no
 * memory left — would otherwise spin forever). `note(nowMs)` records an
 * attempt and says whether it is allowed; `reset()` forgives (a manual
 * reload from the crash page).
 */
class ReloadBudget {
  constructor({ max = 3, windowMs = 60_000 } = {}) {
    this.max = max;
    this.windowMs = windowMs;
    this.stamps = [];
  }
  note(nowMs) {
    this.stamps = this.stamps.filter((t) => nowMs - t < this.windowMs);
    if (this.stamps.length >= this.max) return false;
    this.stamps.push(nowMs);
    return true;
  }
  reset() {
    this.stamps = [];
  }
}

/**
 * May a frame be handed to the renderer? A crashed render frame is NOT a
 * destroyed BrowserWindow — `win.isDestroyed()` stays false while
 * `webContents.send` throws on every call — so the send sites must check
 * the frame, not the window.
 */
function canDeliver({ destroyed, crashed }) {
  return !destroyed && !crashed;
}

/**
 * What the window's `close` should do. 'ask' forwards the close to the
 * renderer (the busy-agents confirm, U5) and waits for its verdict;
 * 'destroy' lets the close proceed because no verdict can ever arrive —
 * the renderer is crashed, hung, never finished loading, or is the crash
 * page (which carries no app handler).
 */
function closePlan({ crashed, hung, loaded, onErrorPage }) {
  if (crashed || hung || !loaded || onErrorPage) return 'destroy';
  return 'ask';
}

/**
 * A `render-process-gone` that is part of a navigation process swap
 * (`clean-exit` while another load is already in flight) needs no
 * recovery — reloading on top of it would double-load. Everything else —
 * crashed, oom, killed, abnormal-exit, launch-failed, a clean-exit with
 * nothing replacing it — leaves a blank window that must be recovered.
 */
function needsRecovery({ reason, loading }) {
  return !(reason === 'clean-exit' && loading);
}

module.exports = { ReloadBudget, canDeliver, closePlan, needsRecovery };
