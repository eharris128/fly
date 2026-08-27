// Window urgency on an attention raise (docs/plans/2026-08-27-001 KTD4) —
// the pure rule. The OS banner is the core's (`notify::banner` →
// notify-send); the window-urgency hint the pre-Electron shell also set on
// every banner is the shell's, and this decides when. Flash only for a
// `raised` attention event while the window is unfocused: a focused user
// is looking already (and `flashFrame(true)` on a focused window is a
// no-op or a distraction depending on the WM). Suppression policy already
// ran in the core — a `raised` that reaches the shell is one the user has
// not seen. main.js clears the flash on focus.
'use strict';

const ATTENTION_EVENT = 'pane://attention';

/** Whether an incoming backend event should set the window urgency hint. */
function shouldFlash(event, payload, focused) {
  if (event !== ATTENTION_EVENT) return false;
  if (!payload || payload.state !== 'raised') return false;
  return !focused;
}

module.exports = { shouldFlash, ATTENTION_EVENT };
