// The sandboxed bridge (U4, KTD4): the renderer sees exactly this surface —
// invoke (name-identical commands), event subscription, pane byte streams —
// and nothing else. U5's ipc.ts re-transport targets `window.fly`.
'use strict';

const { contextBridge, ipcRenderer } = require('electron');

// Close-request ack (renderer-crash recovery): main must not wait forever on
// a renderer that can't answer. Every `fly:close-request` is acknowledged
// from here — registered before any page script runs, so a live event loop
// always acks — carrying whether the app has a close handler at all. No ack
// within main's deadline ⇒ the renderer is dead/hung ⇒ main closes anyway;
// an ack with `false` (the crash or no-frontend page, a frontend that
// failed before wiring its handler) ⇒ nobody will ever send `fly:close-now`
// ⇒ main closes at once. `true` ⇒ the app's flow decides, on its own time.
let closeHandlers = 0;
ipcRenderer.on('fly:close-request', () => {
  ipcRenderer.send('fly:close-ack', closeHandlers > 0);
});

contextBridge.exposeInMainWorld('fly', {
  /** invoke(cmd, args) — the KTD1 command surface, exact Tauri names/shapes. */
  invoke: (cmd, args) => ipcRenderer.invoke('fly:invoke', cmd, args),
  /** Subscribe to backend events (`pane://…`, `automation://…`, …). */
  onEvent: (cb) => {
    const h = (_e, event, payload) => cb(event, payload);
    ipcRenderer.on('fly:event', h);
    return () => ipcRenderer.removeListener('fly:event', h);
  },
  /** Subscribe to pane output bytes (Uint8Array, exact bytes — KTD3). */
  onPaneOutput: (cb) => {
    const h = (_e, paneId, bytes) => cb(paneId, new Uint8Array(bytes));
    ipcRenderer.on('fly:pane-output', h);
    return () => ipcRenderer.removeListener('fly:pane-output', h);
  },
  /** Keystrokes down the 0x03 path (Uint8Array in, exact bytes). */
  paneInput: (paneId, bytes) => ipcRenderer.send('fly:pane-input', paneId, bytes),
  /** Quit-confirm flow (U5): main intercepts close and asks the renderer… */
  onCloseRequested: (cb) => {
    const h = () => cb();
    closeHandlers += 1;
    ipcRenderer.on('fly:close-request', h);
    return () => {
      closeHandlers -= 1;
      ipcRenderer.removeListener('fly:close-request', h);
    };
  },
  /** …and the renderer finishes the job once its flow decides. */
  closeNow: () => ipcRenderer.send('fly:close-now'),
});
