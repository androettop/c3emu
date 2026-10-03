// c3emu web page: firmware drop, screen, keyboard, sound and the C: file panel. The
// emulator itself runs in worker.js.
'use strict';

const $ = id => document.getElementById(id);
const worker = new Worker('worker.js');
const canvas = $('screen'), ctx = canvas.getContext('2d');
const KINDS = ['firmware', 'language', 'content'];
let names = {}, running = false, cwd = '/';

// ---- keys ------------------------------------------------------------------------

// PC key (KeyboardEvent.code) -> keypad matrix index (row * 8 + column); 255 = red key
const KEYMAP = {
  KeyQ: 0, KeyW: 1, KeyE: 2, KeyR: 3, KeyT: 4, KeyY: 5, KeyU: 6, KeyI: 24, KeyO: 25, KeyP: 26,
  KeyA: 8, KeyS: 9, KeyD: 10, KeyF: 11, KeyG: 12, KeyH: 13, KeyJ: 14, KeyK: 32, KeyL: 33,
  KeyZ: 16, KeyX: 17, KeyC: 18, KeyV: 19, KeyB: 20, KeyN: 21, KeyM: 22,
  Backspace: 34, NumpadEnter: 42, Space: 49, ShiftLeft: 35, ShiftRight: 35,
  ControlLeft: 27, ControlRight: 27, Period: 28, Comma: 45, Minus: 40, Equal: 41,
  ArrowUp: 56, ArrowDown: 57, ArrowRight: 59, ArrowLeft: 58, Enter: 60,
  F1: 29, F2: 43, F3: 30, F4: 38, F5: 52, F10: 48, F12: 50, End: 255,
};

function key(idx, down) { worker.postMessage({ type: 'key', idx, down }); }

addEventListener('keydown', e => {
  if (!running || e.target.tagName === 'INPUT' || e.ctrlKey && e.code !== 'ControlLeft' && e.code !== 'ControlRight') return;
  const k = KEYMAP[e.code];
  if (k === undefined) return;
  e.preventDefault();
  if (!e.repeat) key(k, true);
});
addEventListener('keyup', e => {
  const k = KEYMAP[e.code];
  if (k === undefined || !running) return;
  e.preventDefault();
  key(k, false);
});

// Clickable keys drawn on the shell: [x, y, w, h, index] in shell.png pixels (803 x 1605).
const HOT = [
  [100, 856, 115, 42, 29], [585, 856, 115, 42, 43],       // soft keys (the short lines)
  [36, 912, 224, 56, 38], [536, 912, 232, 56, 52],        // contacts, messaging (bar + icon)
  [118, 984, 84, 48, 30], [602, 984, 84, 48, 255],        // call, end / power
  [340, 880, 120, 115, 60],                               // D-pad centre
  [330, 845, 140, 35, 56], [330, 997, 140, 42, 57],       // up, down
  [298, 880, 42, 115, 58], [460, 880, 42, 115, 59],       // left, right
];
const QW = [
  [1058, 92, [0, 1, 2, 3, 4, 5, 6, 24, 25, 26]],
  [1150, 112, [8, 9, 10, 11, 12, 13, 14, 32, 33, 34]],
  [1262, 110, [16, 17, 18, 19, 20, 21, 22, 45, 28, 42]],
  [1372, 122, [50, 35, 48, 49, 49, 49, 49, 40, 41, 27]],
];
for (const [y, h, keys] of QW) {
  for (let c = 0; c < 10; c++) {
    if (keys[c] === 49 && keys[c - 1] === 49) continue;
    const span = keys[c] === 49 ? 4 : 1;
    HOT.push([42 + c * 72.3, y, 72.3 * span, h, keys[c]]);
  }
}
for (const [x, y, w, h, k] of HOT) {
  const b = document.createElement('button');
  b.className = 'hot';
  b.tabIndex = -1;
  Object.assign(b.style, { left: x / 8.03 + '%', top: y / 16.05 + '%', width: w / 8.03 + '%', height: h / 16.05 + '%' });
  const up = () => { if (b.classList.contains('on')) { b.classList.remove('on'); key(k, false); } };
  b.addEventListener('pointerdown', e => {
    if (!running) return;
    e.preventDefault();
    b.setPointerCapture(e.pointerId);
    b.classList.add('on');
    key(k, true);
  });
  b.addEventListener('pointerup', up);
  b.addEventListener('pointercancel', up);
  $('hots').appendChild(b);
}

// ---- sound -----------------------------------------------------------------------

let audio = null, audioPort = null, gain = null;

// volume (0-100 %, default 10 %), remembered in this browser
const volEl = $('vol');
try { const v = localStorage.getItem('c3vol'); if (v !== null) volEl.value = v; } catch {}
function applyVolume() {
  const v = +volEl.value;
  $('volVal').textContent = v + '%';
  if (gain) gain.gain.value = v / 100;
  try { localStorage.setItem('c3vol', String(v)); } catch {}
}
volEl.addEventListener('input', applyVolume);
applyVolume();
const WORKLET = `
class Pcm extends AudioWorkletProcessor {
  constructor(o) {
    super();
    this.q = []; this.off = 0; this.step = 44100 / o.processorOptions.rate; this.pos = 0;
    this.port.onmessage = e => {
      this.q.push(new Int16Array(e.data));
      let n = 0; for (const b of this.q) n += b.length;
      while (n > 44100 * 2 * 0.3 && this.q.length > 1) { n -= this.q[0].length; this.q.shift(); this.off = 0; }
    };
  }
  process(_, outs) {
    const L = outs[0][0], R = outs[0][1] || L;
    for (let i = 0; i < L.length; i++) {
      let l = 0, r = 0;
      const b = this.q[0];
      if (b) {
        l = b[this.off] / 32768; r = b[this.off + 1] / 32768;
        this.pos += this.step;
        while (this.pos >= 1) {
          this.pos -= 1; this.off += 2;
          if (this.off >= this.q[0].length) { this.q.shift(); this.off = 0; if (!this.q.length) break; }
        }
      }
      L[i] = l; R[i] = r;
    }
    return true;
  }
}
registerProcessor('pcm', Pcm);`;

async function startAudio() {
  if (audio) return;
  try {
    try { audio = new AudioContext({ sampleRate: 44100 }); } catch { audio = new AudioContext(); }
    const url = URL.createObjectURL(new Blob([WORKLET], { type: 'text/javascript' }));
    await audio.audioWorklet.addModule(url);
    const node = new AudioWorkletNode(audio, 'pcm', { outputChannelCount: [2], processorOptions: { rate: audio.sampleRate } });
    gain = audio.createGain();
    node.connect(gain).connect(audio.destination);
    applyVolume();
    audioPort = node.port;
  } catch (e) {
    console.warn('no sound:', e);
  }
}

// ---- firmware --------------------------------------------------------------------

function kindOf(name) {
  const n = name.toLowerCase();
  if (n.includes('.mcusw')) return 0;
  if (n.includes('.ppm')) return 1;
  if (n.includes('.image')) return 2;
  return -1;
}

async function giveFirmware(list) {
  const files = [];
  for (const f of list) {
    const kind = kindOf(f.name);
    if (kind < 0) { status(`${f.name}: not a .mcusw, .ppm_* or .image_* file`, true); continue; }
    files.push({ kind, name: f.name, buf: await f.arrayBuffer() });
  }
  if (files.length) {
    status('Saving the firmware…');
    worker.postMessage({ type: 'files', files }, files.map(f => f.buf));
  }
}

function showFiles() {
  const ul = $('files');
  ul.innerHTML = '';
  for (let k = 0; k < 3; k++) {
    const li = document.createElement('li');
    li.innerHTML = `<span class="k">${KINDS[k]}</span><span>${names[k] ? esc(names[k]) : '<span class="muted">—</span>'}</span>`;
    ul.appendChild(li);
  }
  $('start').disabled = !names[0];
}

const drop = $('drop');
drop.addEventListener('dragover', e => { e.preventDefault(); drop.classList.add('over'); });
drop.addEventListener('dragleave', () => drop.classList.remove('over'));
drop.addEventListener('drop', e => { e.preventDefault(); drop.classList.remove('over'); giveFirmware([...e.dataTransfer.files]); });
$('pick').onclick = () => $('pickInput').click();
$('pickInput').onchange = e => { giveFirmware([...e.target.files]); e.target.value = ''; };
$('changeFw').onclick = () => { drop.style.display = ''; };

$('start').onclick = async () => {
  await startAudio();
  if (audio && audio.state === 'suspended') audio.resume();
  status('Starting…');
  worker.postMessage({ type: 'boot' });
};
$('restart').onclick = () => worker.postMessage({ type: 'boot' });
$('factory').onclick = () => {
  if (confirm('Erase the phone memory? Settings, installed games and files are lost (the factory content comes back).')) {
    worker.postMessage({ type: 'factory' });
  }
};

function status(msg, err) {
  const s = $('status');
  s.textContent = msg || '';
  s.className = err ? 'err' : 'muted';
}

// ---- file panel ------------------------------------------------------------------

let fsSeq = 0;
const fsWait = new Map();
function fs(op, path, extra = {}, transfer = []) {
  return new Promise(ok => {
    const id = ++fsSeq;
    fsWait.set(id, ok);
    worker.postMessage({ type: 'fs', id, op, path, ...extra }, transfer);
  });
}

const join = (a, b) => (a.endsWith('/') ? a : a + '/') + b;
const esc = s => String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
function size(n) {
  if (n < 1024) return n + ' B';
  if (n < 1048576) return (n / 1024).toFixed(1) + ' KB';
  return (n / 1048576).toFixed(1) + ' MB';
}
function fsMsg(t, err) { const m = $('fsMsg'); m.textContent = t; m.className = err ? 'err' : 'muted'; }

async function refresh() {
  const r = await fs('list', cwd);
  const list = $('fsList');
  const crumbs = $('crumbs');
  crumbs.innerHTML = '';
  const parts = cwd.split('/').filter(Boolean);
  const mk = (label, path) => {
    const a = document.createElement('a');
    a.textContent = label;
    a.onclick = () => { cwd = path; refresh(); };
    crumbs.appendChild(a);
  };
  mk('C:', '/');
  parts.forEach((p, i) => { crumbs.append(' / '); mk(p, '/' + parts.slice(0, i + 1).join('/')); });
  if (!r.ok) {
    list.innerHTML = `<div class="empty">${esc(r.data)}</div>`;
    $('fsInfo').textContent = '';
    return;
  }
  list.innerHTML = '';
  if (cwd !== '/') {
    const up = document.createElement('div');
    up.className = 'ent dir';
    up.innerHTML = '<span>↩</span><span class="n">..</span><span></span><span></span>';
    up.querySelector('.n').onclick = () => { cwd = '/' + parts.slice(0, -1).join('/'); refresh(); };
    list.appendChild(up);
  }
  if (!r.data.entries.length) list.insertAdjacentHTML('beforeend', '<div class="empty">Empty folder</div>');
  for (const e of r.data.entries) {
    const path = join(cwd, e.name);
    const row = document.createElement('div');
    row.className = 'ent' + (e.dir ? ' dir' : '');
    row.innerHTML = `<span>${e.dir ? '📁' : '📄'}</span><span class="n" title="${esc(e.name)}">${esc(e.name)}</span>
      <span class="s">${e.dir ? '' : size(e.size)}</span><span class="row"></span>`;
    const acts = row.lastElementChild;
    if (e.dir) {
      row.querySelector('.n').onclick = () => { cwd = path; refresh(); };
      row.addEventListener('dragover', ev => { ev.preventDefault(); ev.stopPropagation(); row.classList.add('over'); });
      row.addEventListener('dragleave', () => row.classList.remove('over'));
      row.addEventListener('drop', ev => { ev.preventDefault(); ev.stopPropagation(); row.classList.remove('over'); upload(ev.dataTransfer, path); });
    } else {
      const dl = document.createElement('button');
      dl.textContent = '↓';
      dl.title = 'Download';
      dl.onclick = async () => {
        const r = await fs('read', path);
        if (!r.ok) return fsMsg(r.data, true);
        const a = document.createElement('a');
        a.href = URL.createObjectURL(new Blob([r.data]));
        a.download = e.name;
        a.click();
        setTimeout(() => URL.revokeObjectURL(a.href), 10000);
      };
      acts.appendChild(dl);
    }
    const rm = document.createElement('button');
    rm.textContent = '✕';
    rm.title = 'Delete';
    rm.onclick = async () => {
      if (!confirm(`Delete ${e.name}${e.dir ? ' and everything in it' : ''}?`)) return;
      const r = await fs('remove', path);
      fsMsg(r.ok ? `${e.name} deleted.` : r.data, !r.ok);
      refresh();
    };
    acts.appendChild(rm);
    list.appendChild(row);
  }
  $('fsInfo').textContent = `${size(r.data.free)} free of ${size(r.data.total)}`;
}

// files (and folders, recursively) from a drop or a file picker -> [{path, buf}]
async function collect(dt, base) {
  const out = [];
  const walk = async (entry, dir) => {
    if (entry.isFile) {
      const f = await new Promise((ok, err) => entry.file(ok, err));
      out.push({ path: join(dir, entry.name), buf: await f.arrayBuffer() });
    } else if (entry.isDirectory) {
      const reader = entry.createReader();
      const sub = join(dir, entry.name);
      out.push({ dir: sub });
      for (;;) {
        const batch = await new Promise((ok, err) => reader.readEntries(ok, err));
        if (!batch.length) break;
        for (const e of batch) await walk(e, sub);
      }
    }
  };
  if (dt.items) {
    const entries = [...dt.items].map(i => i.webkitGetAsEntry && i.webkitGetAsEntry()).filter(Boolean);
    if (entries.length) {
      for (const e of entries) await walk(e, base);
      return out;
    }
  }
  for (const f of dt.files || dt) out.push({ path: join(base, f.name), buf: await f.arrayBuffer() });
  return out;
}

async function upload(dt, dir) {
  const items = await collect(dt, dir);
  const files = items.filter(i => i.buf);
  for (const d of items.filter(i => i.dir)) await fs('mkdir', d.dir);
  if (!files.length) return refresh();
  const total = files.reduce((n, f) => n + f.buf.byteLength, 0);
  fsMsg(`Copying ${files.length} file(s), ${size(total)}… (the phone restarts)`);
  const r = await fs('write', dir, { files }, files.map(f => f.buf));
  fsMsg(r.ok ? `Copied: ${files.map(f => f.path).join(', ')}` : r.data, !r.ok);
  refresh();
}

const list = $('fsList');
list.addEventListener('dragover', e => { e.preventDefault(); list.classList.add('over'); });
list.addEventListener('dragleave', e => { if (e.target === list) list.classList.remove('over'); });
list.addEventListener('drop', e => { e.preventDefault(); list.classList.remove('over'); upload(e.dataTransfer, cwd); });
$('up').onclick = () => $('upInput').click();
$('upInput').onchange = e => { upload([...e.target.files], cwd); e.target.value = ''; };
$('refresh').onclick = refresh;
$('mkdir').onclick = async () => {
  const n = prompt('Name of the new folder:');
  if (!n) return;
  const r = await fs('mkdir', join(cwd, n));
  fsMsg(r.ok ? `Folder ${n} created (the phone restarts).` : r.data, !r.ok);
  refresh();
};

// ---- worker messages -------------------------------------------------------------

const img = ctx.createImageData(320, 240);
worker.onmessage = e => {
  const m = e.data;
  switch (m.type) {
    case 'ready':
      names = m.names;
      showFiles();
      status(names[0] ? 'Firmware ready: press Start.' : 'Drop the firmware on the phone screen.');
      refresh();
      break;
    case 'frame':
      img.data.set(new Uint8Array(m.buf));
      ctx.putImageData(img, 0, 0);
      break;
    case 'audio':
      if (audioPort) audioPort.postMessage(m.buf, [m.buf]);
      break;
    case 'state':
      running = m.running;
      $('restart').disabled = !running;
      if (running) { drop.style.display = 'none'; $('shell').focus(); }
      status(m.msg || (running ? 'Running. Click the phone and use the keyboard.' : ''), !running && !!m.msg);
      if (running) setTimeout(refresh, 4000);
      break;
    case 'fs': {
      const ok = fsWait.get(m.id);
      fsWait.delete(m.id);
      if (ok) ok(m);
      break;
    }
    case 'stats':
      window.c3stats = m.s;
      break;
    case 'error':
      status(m.msg, true);
      break;
  }
};
