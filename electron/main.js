// The fly Electron shell (Electron-shell migration U4) — deliberately thin:
// window + single-instance, `fly core` spawn-or-adopt, and the control-socket
// bridge (this process owns the socket; the sandboxed renderer reaches it
// only through the preload's typed surface — proposal KTD4).
'use strict';

const { app, BrowserWindow, ipcMain } = require('electron');
const net = require('net');
const path = require('path');
const fs = require('fs');
const { spawn } = require('child_process');
const { encodeJson, encodePaneInput, FrameReader } = require('./protocol');
const {
  ReloadBudget,
  canDeliver,
  closePlan,
  needsRecovery,
} = require('./recovery');

// ---- flavor isolation (proposal KTD5) --------------------------------------
// FLY_APP_NAME drives everything: the core's config/session/socket roots and
// this shell's own userData/single-instance scope. The packaged app IS fly
// (default flavor); a repo checkout defaults to the fly-el dev flavor so it
// coexists with an installed release (the flavor:dev story, U7).
const FLAVOR = process.env.FLY_APP_NAME || (app.isPackaged ? 'fly' : 'fly-el');
process.env.FLY_APP_NAME = FLAVOR;
app.setPath('userData', path.join(app.getPath('appData'), `${FLAVOR}-shell`));

const RUNTIME_DIR =
  process.env.XDG_RUNTIME_DIR || process.env.TMPDIR || '/tmp';
const CONTROL_SOCK = path.join(RUNTIME_DIR, FLAVOR, 'control.sock');

// The fly binary: env override for dev; the bundled resource when packaged
// (the same Rust binary serving the CLI role — /usr/bin/fly symlinks to it,
// see deb/postinst.sh); else the repo debug build beside this checkout; else
// PATH.
function flyBinary() {
  if (process.env.FLY_CORE_BIN) return process.env.FLY_CORE_BIN;
  if (app.isPackaged) return path.join(process.resourcesPath, 'fly');
  const dev = path.join(__dirname, '..', 'src-tauri', 'target', 'debug', 'fly');
  if (require('fs').existsSync(dev)) return dev;
  return 'fly';
}

// ---- single instance -------------------------------------------------------
if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', () => {
    const win = BrowserWindow.getAllWindows()[0];
    if (win) {
      if (win.isMinimized()) win.restore();
      win.focus();
    }
  });
}

// ---- core connection --------------------------------------------------------
let coreChild = null; // set only when THIS shell spawned the core
let sock = null;
let nextId = 1;
const pending = new Map(); // id -> {resolve, reject}
let win = null;
let quitting = false; // set by before-quit; suppresses reconnect/restart
let rewireTimer = null; // single-flight guard: core-exit and socket-close can
// both observe the same crash — only one reconnect chain may run, or two
// wireCore() races overwrite `sock` and double-spawn the core.

// ---- renderer health (renderer-crash recovery, 2026-08-22) ------------------
// What the send sites and the close flow need to know about the renderer.
// The core and its panes are NOT affected by any of this: a renderer death
// is a display outage, and recovery is reload + re-attach (the frontend's
// `adopt_live_pane` on mount), never a respawn.
const renderer = { crashed: false, hung: false, loaded: false };
const reloads = new ReloadBudget({ max: 3, windowMs: 60_000 });
let onErrorPage = false; // crashed.html is up (budget exhausted)
let closePending = null; // {ackTimer} while a close waits on the renderer
let allowClose = false; // the close flow's verdict: let the next `close` through
let droppedLogged = false; // one "frames dropped" line per outage, not 2,119

function rendererCrashed() {
  if (!win || win.isDestroyed()) return true;
  const wc = win.webContents;
  return (
    renderer.crashed ||
    wc.isDestroyed() ||
    (typeof wc.isCrashed === 'function' && wc.isCrashed())
  );
}

/** The ONE path to the renderer for control-socket traffic (events and
 * pane-output frames). A crashed render frame is not a destroyed window —
 * `webContents.send` throws "Render frame was disposed" on every call until
 * the reload lands — so guard on the frame and swallow the race. */
function sendToRenderer(channel, ...args) {
  if (!win || win.isDestroyed()) return;
  const wc = win.webContents;
  if (!canDeliver({ destroyed: wc.isDestroyed(), crashed: rendererCrashed() })) {
    if (!droppedLogged) {
      droppedLogged = true;
      console.error('[shell] renderer unreachable — dropping frames until it reloads');
    }
    return;
  }
  try {
    wc.send(channel, ...args);
  } catch (e) {
    if (!droppedLogged) {
      droppedLogged = true;
      console.error(`[shell] send to renderer failed (dropping until reload): ${e}`);
    }
  }
}

function forceClose() {
  allowClose = true;
  if (closePending) {
    if (closePending.ackTimer) clearTimeout(closePending.ackTimer);
    closePending = null;
  }
  if (win && !win.isDestroyed()) win.destroy();
}

function scheduleRewire(delayMs, label) {
  if (quitting || rewireTimer) return;
  rewireTimer = setTimeout(() => {
    rewireTimer = null;
    wireCore().catch((e) => console.error(`[shell] ${label} failed: ${e}`));
  }, delayMs);
}

function connectControl(attempt = 0) {
  return new Promise((resolve, reject) => {
    const s = net.createConnection(CONTROL_SOCK);
    s.on('connect', () => resolve(s));
    s.on('error', (e) => {
      if (attempt < 40) {
        setTimeout(
          () => connectControl(attempt + 1).then(resolve, reject),
          125
        );
      } else {
        reject(e);
      }
    });
  });
}

async function ensureCore() {
  // Adopt-or-spawn: a live socket answers `core/ping`; a dead one refuses
  // connect and we spawn our own core (whose bind reclaims the residue).
  try {
    const probe = await new Promise((resolve, reject) => {
      const s = net.createConnection(CONTROL_SOCK);
      s.on('connect', () => resolve(s));
      s.on('error', reject);
    });
    probe.destroy();
    console.log(`[shell] adopting running core at ${CONTROL_SOCK}`);
    return;
  } catch {
    /* no live core — spawn one */
  }
  const bin = flyBinary();
  console.log(`[shell] starting core: ${bin} core (flavor ${FLAVOR})`);
  coreChild = spawn(bin, ['core'], {
    env: { ...process.env, FLY_APP_NAME: FLAVOR },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  coreChild.on('exit', (code, sig) => {
    console.error(`[shell] core exited (code=${code} sig=${sig})`);
    coreChild = null;
    // Crash-restart with a beat of backoff, unless we are quitting. The
    // socket-close handler sees the same crash; scheduleRewire single-flights.
    scheduleRewire(500, 'core restart');
  });
}

async function wireCore() {
  await ensureCore();
  sock = await connectControl();
  sock.setNoDelay(true);
  const reader = new FrameReader(
    (msg) => {
      if (msg.id !== undefined && pending.has(msg.id)) {
        const p = pending.get(msg.id);
        pending.delete(msg.id);
        if ('ok' in msg) p.resolve(msg.ok);
        else p.reject(new Error(msg.err ?? 'unknown error'));
      } else if (msg.event !== undefined) {
        sendToRenderer('fly:event', msg.event, msg.payload);
      }
    },
    (paneId, bytes) => {
      // Transferable-free but structured-clone efficient; exact bytes.
      sendToRenderer('fly:pane-output', paneId, bytes);
    },
    (err) => {
      console.error(`[shell] control protocol error: ${err}`);
      sock.destroy();
    }
  );
  sock.on('data', (chunk) => reader.push(chunk));
  sock.on('close', () => {
    // Fail every in-flight request; reconnect (the core may be restarting).
    for (const [, p] of pending) p.reject(new Error('core connection lost'));
    pending.clear();
    scheduleRewire(300, 'reconnect');
  });
  console.log(`[shell] control connected: ${CONTROL_SOCK}`);
}

function invoke(cmd, args) {
  return new Promise((resolve, reject) => {
    if (!sock || sock.destroyed) {
      reject(new Error('core not connected'));
      return;
    }
    const id = nextId++;
    pending.set(id, { resolve, reject });
    sock.write(encodeJson({ id, cmd, args: args ?? null }));
  });
}

// ---- the preload's surface --------------------------------------------------
// Standard Electron hardening: only the shell's own window may drive these
// channels. No remote content ever loads, so this is belt-and-suspenders.
function fromShell(e) {
  return win !== null && !win.isDestroyed() && e.sender === win.webContents;
}
ipcMain.handle('fly:invoke', (e, cmd, args) => {
  if (!fromShell(e)) throw new Error('unexpected sender');
  return invoke(cmd, args);
});
ipcMain.on('fly:pane-input', (e, paneId, bytes) => {
  if (!fromShell(e)) return;
  if (sock && !sock.destroyed) {
    sock.write(encodePaneInput(paneId, Buffer.from(bytes)));
  }
});

// ---- window -----------------------------------------------------------------
app.whenReady().then(async () => {
  try {
    await wireCore();
  } catch (e) {
    console.error(`[shell] cannot reach fly core: ${e}`);
  }
  win = new BrowserWindow({
    width: 1200,
    height: 800,
    title: `fly (${FLAVOR})`,
    backgroundColor: '#0d1117',
    webPreferences: {
      preload: path.join(__dirname, 'preload.cjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });
  win.removeMenu();
  win.on('page-title-updated', (e) => e.preventDefault());
  const wc = win.webContents;

  // Quit-confirm flow (U5): the renderer owns the busy-agents confirm (the
  // same shared destructive-confirm overlay as under Tauri); main only
  // intercepts and forwards. `fly:close-now` is the renderer's verdict —
  // but a verdict needs a renderer that can answer (recovery, bug 3): a
  // crashed/hung/never-loaded one gets no say and the close proceeds; a
  // live one must ACK the request (preload-level, before any page script)
  // within CLOSE_ACK_MS or it is treated as dead. An ack carrying "no app
  // handler" (the crash or no-frontend page, a frontend that failed before wiring)
  // closes at once; "handler present" waits for the app's flow — on the
  // user's time, since the confirm is theirs to answer.
  const CLOSE_ACK_MS = 3000;
  win.on('close', (e) => {
    if (allowClose) return;
    const plan = closePlan({
      crashed: rendererCrashed(),
      hung: renderer.hung,
      loaded: renderer.loaded,
      onErrorPage,
    });
    if (plan === 'destroy') {
      console.error('[shell] closing without the renderer (it cannot answer)');
      allowClose = true;
      return; // let this close proceed
    }
    e.preventDefault();
    if (closePending?.ackTimer) clearTimeout(closePending.ackTimer);
    closePending = {
      ackTimer: setTimeout(() => {
        console.error('[shell] renderer never acknowledged the close — closing anyway');
        forceClose();
      }, CLOSE_ACK_MS),
    };
    sendToRenderer('fly:close-request');
  });
  ipcMain.on('fly:close-ack', (e, hasHandler) => {
    if (!fromShell(e) || !closePending) return;
    if (closePending.ackTimer) clearTimeout(closePending.ackTimer);
    if (!hasHandler) {
      forceClose(); // nobody will ever send fly:close-now
      return;
    }
    closePending = { ackTimer: null }; // live + wired: its verdict decides
  });
  ipcMain.on('fly:close-now', (e) => {
    if (!fromShell(e)) return;
    forceClose();
  });

  // Renderer-crash recovery (bug 1 of the 2026-08-22 incident): reload the
  // frontend when the render process dies. The core is untouched; the
  // reloaded frontend re-attaches to the live panes (`adopt_live_pane`).
  // Bounded by `reloads` so a renderer that dies on every load lands on
  // the crash page instead of a loop; a pending close on a dead renderer
  // can never get its verdict, so it closes instead.
  wc.on('render-process-gone', (_e, details) => {
    renderer.crashed = true;
    renderer.loaded = false;
    console.error(
      `[shell] renderer gone: reason=${details.reason} exitCode=${details.exitCode}`
    );
    if (quitting || allowClose || win.isDestroyed()) return;
    if (closePending) {
      console.error('[shell] close was waiting on the renderer — closing now');
      forceClose();
      return;
    }
    if (!needsRecovery({ reason: details.reason, loading: wc.isLoading() })) return;
    if (reloads.note(Date.now())) {
      console.error('[shell] reloading the frontend (core and panes untouched)');
      loadFrontend().catch((err) => console.error(`[shell] reload failed: ${err}`));
    } else {
      console.error(
        `[shell] renderer died ${reloads.max}× within ${reloads.windowMs / 1000}s — showing the crash page`
      );
      onErrorPage = true;
      win.loadFile('crashed.html').catch((err) =>
        console.error(`[shell] crash page failed: ${err}`)
      );
    }
  });
  wc.on('did-finish-load', () => {
    renderer.crashed = false;
    renderer.loaded = true;
    droppedLogged = false;
  });
  // Hung is not dead: a renderer busy with a huge synchronous write can trip
  // this and recover. Log, let a pending close through, otherwise wait —
  // a hung renderer's state is still the user's; a reload would discard it.
  wc.on('unresponsive', () => {
    renderer.hung = true;
    console.error('[shell] renderer unresponsive');
    if (closePending) forceClose();
  });
  wc.on('responsive', () => {
    renderer.hung = false;
    console.error('[shell] renderer responsive again');
  });
  // R on the crash page: one manual retry, budget forgiven.
  wc.on('before-input-event', (_e, input) => {
    if (!onErrorPage || input.type !== 'keyDown') return;
    if ((input.key || '').toLowerCase() !== 'r') return;
    onErrorPage = false;
    reloads.reset();
    reloads.note(Date.now());
    console.error('[shell] manual reload from the crash page');
    loadFrontend().catch((err) => console.error(`[shell] reload failed: ${err}`));
  });

  await loadFrontend();
});

// Packaged: the built frontend travels inside the asar (`frontend/`, copied
// from ../dist by the dist script — relative vite base, U7). Dev:
// FLY_SHELL_URL points at the Vite dev server; otherwise a built ../dist is
// loaded straight from the tree; with neither, an inert "no frontend build"
// page says what to do. Also the renderer-crash reload path — the same
// load, so recovery can never diverge from first launch.
function loadFrontend() {
  const url = process.env.FLY_SHELL_URL;
  if (url) return win.loadURL(url);
  if (app.isPackaged) return win.loadFile('frontend/index.html');
  const built = path.join(__dirname, '..', 'dist', 'index.html');
  if (fs.existsSync(built)) return win.loadFile(built);
  return win.loadFile('no-frontend.html');
}

// Ordered core shutdown on quit (migration U6): ask the core to run the same
// teardown lifecycle.rs runs under Tauri — clean-exit marker, interrupted-run
// closes, substrate DETACH — whether we spawned it or adopted it (quitting
// fly means quitting the backend; sessions survive on the tmux server either
// way). `core/shutdown` is primary; SIGTERM lands on the same flag in the
// core; SIGKILL after a deadline is the last resort for a wedged core.
let quitFlowDone = false;
app.on('before-quit', (e) => {
  if (quitFlowDone) return;
  e.preventDefault();
  quitFlowDone = true;
  quitting = true;
  if (rewireTimer) {
    clearTimeout(rewireTimer);
    rewireTimer = null;
  }
  const finish = () => app.quit();
  const askShutdown = invoke('core/shutdown').catch(() => {
    // Socket down or command refused — signal the core we own instead.
    if (coreChild) coreChild.kill('SIGTERM');
  });
  if (coreChild) {
    // Wait for the ordered exit; SIGKILL a core that never finishes.
    const deadline = setTimeout(() => {
      try {
        coreChild.kill('SIGKILL');
      } catch {}
      finish();
    }, 10_000);
    coreChild.once('exit', () => {
      clearTimeout(deadline);
      finish();
    });
  } else {
    // Adopted core: we can't watch its pid; give the request a beat to land.
    askShutdown.then(
      () => setTimeout(finish, 500),
      () => finish()
    );
  }
});

app.on('window-all-closed', () => app.quit());
