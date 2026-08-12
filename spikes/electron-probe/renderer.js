// KTD1: same terminal stack as fly — @xterm/xterm 5.5 + WebGL, fontSize 15.
// KTD2: control socket calls pty.write() directly (parity with send-keys entry).
const { Terminal } = require('@xterm/xterm');
const { WebglAddon } = require('@xterm/addon-webgl');
const { FitAddon } = require('@xterm/addon-fit');
const { Unicode11Addon } = require('@xterm/addon-unicode11');
const pty = require('node-pty');
const net = require('net');
const fs = require('fs');
const path = require('path');

const params = new URLSearchParams(location.search);
const NPANES = parseInt(params.get('panes') || '1', 10);

const grid = document.getElementById('grid');
if (NPANES === 1) {
  grid.style.gridTemplate = '1fr / 1fr';
} else {
  // 5-pane: 2 columns x 3 rows, pane 0 spans the bottom row (the "focused" one)
  grid.style.gridTemplate = '1fr 1fr 1fr / 1fr 1fr';
}

const terms = [];
const ptys = [];

for (let i = 0; i < NPANES; i++) {
  const el = document.createElement('div');
  el.className = 'pane';
  if (NPANES > 1 && i === 0) el.style.gridArea = '3 / 1 / 4 / 3';
  grid.appendChild(el);

  const term = new Terminal({
    fontSize: 15,
    scrollback: 10000,
    theme: { background: '#0d1117', foreground: '#c9d1d9' },
    allowProposedApi: true,
  });
  term.loadAddon(new Unicode11Addon());
  term.unicode.activeVersion = '11';
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.open(el);
  term.loadAddon(new WebglAddon());
  fit.fit();

  const p = pty.spawn('bash', ['--norc'], {
    name: 'xterm-256color',
    cols: term.cols,
    rows: term.rows,
    cwd: process.env.HOME,
    env: process.env,
  });
  p.onData((d) => term.write(d));
  term.onData((d) => p.write(d));
  window.addEventListener('resize', () => {
    fit.fit();
    p.resize(term.cols, term.rows);
  });

  terms.push(term);
  ptys.push(p);
}

terms[0].focus();

// Control socket: line protocol `TYPE <idx> <hex>\n`; replies `OK\n` after the
// pty write returns, so the probe can timestamp precisely at entry.
const sock = path.join(
  process.env.PROBE_SOCK_DIR || '/tmp',
  'electron-probe.sock'
);
try { fs.unlinkSync(sock); } catch {}
const server = net.createServer((c) => {
  let buf = '';
  c.on('data', (d) => {
    buf += d.toString('utf8');
    let nl;
    while ((nl = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, nl).trim();
      buf = buf.slice(nl + 1);
      const [cmd, idxs, hex] = line.split(' ');
      if (cmd === 'TYPE') {
        ptys[parseInt(idxs, 10)].write(Buffer.from(hex, 'hex').toString('binary'));
        c.write('OK\n');
      } else if (cmd === 'PING') {
        c.write('OK\n');
      }
    }
  });
});
server.listen(sock);
