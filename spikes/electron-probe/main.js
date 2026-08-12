// Lab bench, not a product: nodeIntegration on, no isolation (KTD5).
const { app, BrowserWindow } = require('electron');

const panes = parseInt(process.env.PROBE_PANES || '1', 10) || 1;

app.whenReady().then(() => {
  const win = new BrowserWindow({
    width: 927,
    height: 1131,
    title: 'electron-probe',
    backgroundColor: '#0d1117',
    webPreferences: { nodeIntegration: true, contextIsolation: false },
  });
  win.removeMenu();
  win.loadFile('index.html', { query: { panes: String(panes) } });
  // keep the title stable (xterm focus etc. must not retitle)
  win.on('page-title-updated', (e) => e.preventDefault());
});

app.on('window-all-closed', () => app.quit());
