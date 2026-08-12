// The sandboxed bridge (U4, KTD4): the renderer sees exactly this surface —
// invoke (name-identical commands), event subscription, pane byte streams —
// and nothing else. U5's ipc.ts re-transport targets `window.fly`.
'use strict';

const { contextBridge, ipcRenderer } = require('electron');

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
    ipcRenderer.on('fly:close-request', h);
    return () => ipcRenderer.removeListener('fly:close-request', h);
  },
  /** …and the renderer finishes the job once its flow decides. */
  closeNow: () => ipcRenderer.send('fly:close-now'),
});
