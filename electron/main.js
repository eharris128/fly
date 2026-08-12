// The fly Electron shell (Electron-shell migration U4) — deliberately thin:
// window + single-instance, `fly core` spawn-or-adopt, and the control-socket
// bridge (this process owns the socket; the sandboxed renderer reaches it
// only through the preload's typed surface — proposal KTD4).
'use strict';

const { app, BrowserWindow, ipcMain, Notification } = require('electron');
const net = require('net');
const path = require('path');
const { spawn } = require('child_process');
const { encodeJson, encodePaneInput, FrameReader } = require('./protocol');

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
    // Crash-restart with a beat of backoff, unless we are quitting.
    if (!app.isQuittingFly) {
      setTimeout(() => {
        wireCore().catch((e) =>
          console.error(`[shell] core restart failed: ${e}`)
        );
      }, 500);
    }
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
        if (win && !win.isDestroyed()) {
          win.webContents.send('fly:event', msg.event, msg.payload);
        }
      }
    },
    (paneId, bytes) => {
      if (win && !win.isDestroyed()) {
        // Transferable-free but structured-clone efficient; exact bytes.
        win.webContents.send('fly:pane-output', paneId, bytes);
      }
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
    if (!app.isQuittingFly) {
      setTimeout(() => {
        wireCore().catch((e) => console.error(`[shell] reconnect failed: ${e}`));
      }, 300);
    }
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
ipcMain.handle('fly:invoke', (_e, cmd, args) => invoke(cmd, args));
ipcMain.on('fly:pane-input', (_e, paneId, bytes) => {
  if (sock && !sock.destroyed) {
    sock.write(encodePaneInput(paneId, Buffer.from(bytes)));
  }
});
// OS notification relay (interim: the core's own banner seam uses
// notify-send when headless; a shell that owns the desktop session may take
// over via this channel in U5+).
ipcMain.on('fly:notify', (_e, title, body) => {
  new Notification({ title, body }).show();
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
  // Quit-confirm flow (U5): the renderer owns the busy-agents confirm (the
  // same shared destructive-confirm overlay as under Tauri); main only
  // intercepts and forwards. `fly:close-now` is the renderer's verdict.
  let allowClose = false;
  win.on('close', (e) => {
    if (allowClose) return;
    e.preventDefault();
    win.webContents.send('fly:close-request');
  });
  ipcMain.on('fly:close-now', () => {
    allowClose = true;
    if (win && !win.isDestroyed()) win.destroy();
  });
  // Packaged: the built frontend travels inside the asar (`frontend/`,
  // copied from ../dist by the dist script — relative vite base, U7). Dev:
  // FLY_SHELL_URL points at the Vite dev server; bare fallback is the U4
  // bridge probe page.
  const url = process.env.FLY_SHELL_URL;
  if (url) {
    await win.loadURL(url);
  } else if (app.isPackaged) {
    await win.loadFile('frontend/index.html');
  } else {
    await win.loadFile('probe.html');
  }
});

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
  app.isQuittingFly = true;
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
