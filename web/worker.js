// c3emu web worker: runs the emulator (c3emu_web.wasm) off the page's main thread.
// Keeps the firmware files and the phone's flash images in IndexedDB, so the phone
// (settings, installed games, files) survives reloads.
//
// page -> worker: {type:'files', files:[{kind, name, buf}]}, {type:'boot'},
//   {type:'key', idx, down}, {type:'fs', id, op, path, files?}, {type:'factory'}
// worker -> page: {type:'ready', names}, {type:'frame', buf}, {type:'audio', buf},
//   {type:'state', running, msg}, {type:'fs', id, ok, data}, {type:'error', msg}

'use strict';

let x = null;        // wasm exports
let db = null;
let running = false;
let loopDone = Promise.resolve();
let lastSerial = -1, lastFrame = 0, lastSave = 0, lastStats = 0;
const names = {};    // kind -> file name
const enc = new TextEncoder(), dec = new TextDecoder();
const KIND = { 0: 'firmware', 1: 'ppm', 2: 'content' };

function bytes(ptr, len) { return new Uint8Array(x.memory.buffer, ptr, len); }
function result() { return bytes(x.c3_result_ptr(), x.c3_result_len()).slice(); }
function resultText() { return dec.decode(result()); }

function give(kind, buf) {
  const d = new Uint8Array(buf);
  const p = x.c3_alloc(d.length);
  bytes(p, d.length).set(d);
  x.c3_set_file(kind, p, d.length); // wasm owns it now
}

// call f(ptr, len, ...) with a temporary copy of `data` (string or bytes)
function withBuf(data, f) {
  const d = typeof data === 'string' ? enc.encode(data) : new Uint8Array(data);
  const p = x.c3_alloc(Math.max(d.length, 1));
  bytes(p, d.length).set(d);
  try { return f(p, d.length); } finally { x.c3_free(p, Math.max(d.length, 1)); }
}

// ---- IndexedDB -------------------------------------------------------------------

function openDb() {
  return new Promise((ok, err) => {
    const r = indexedDB.open('c3emu', 1);
    r.onupgradeneeded = () => {
      r.result.createObjectStore('files');
      r.result.createObjectStore('chunks');
    };
    r.onsuccess = () => ok(r.result);
    r.onerror = () => err(r.error);
  });
}

function store(name, mode) { return db.transaction(name, mode).objectStore(name); }

function idbAll(name) {
  return new Promise((ok, err) => {
    const out = [];
    const r = store(name, 'readonly').openCursor();
    r.onsuccess = () => {
      const c = r.result;
      if (c) { out.push([c.key, c.value]); c.continue(); } else ok(out);
    };
    r.onerror = () => err(r.error);
  });
}

function idbTx(name, f) {
  return new Promise((ok, err) => {
    const t = db.transaction(name, 'readwrite');
    f(t.objectStore(name));
    t.oncomplete = () => ok();
    t.onerror = () => err(t.error);
  });
}

// Save the flash chunks written since the last save.
async function save() {
  if (!x) return;
  lastSave = performance.now();
  const n = x.c3_disks_dirty();
  if (!n) return;
  const r = result();
  const v = new DataView(r.buffer);
  const recs = [];
  let o = 0;
  for (let i = 0; i < n; i++) {
    const dev = v.getUint32(o, true), off = v.getFloat64(o + 4, true);
    const has = v.getUint32(o + 12, true), size = Number(v.getBigUint64(o + 16, true));
    o += 24;
    let data = null;
    if (has) { data = r.slice(o, o + 65536); o += 65536; }
    recs.push({ dev, off, size, data });
  }
  await idbTx('chunks', s => {
    for (const c of recs) {
      const k = c.dev + ':' + c.off;
      if (c.data) s.put(c, k); else s.delete(k);
    }
  });
}

// ---- emulator loop ---------------------------------------------------------------

const ch = new MessageChannel();
const yieldNow = () => new Promise(r => { ch.port1.onmessage = r; ch.port2.postMessage(0); });
const sleep = ms => new Promise(r => setTimeout(r, ms));

function pushFrame() {
  const s = x.c3_frame_serial();
  const now = performance.now();
  if (s === lastSerial || now - lastFrame < 15) return;
  lastSerial = s;
  lastFrame = now;
  const p = x.c3_frame();
  const buf = bytes(p, 320 * 240 * 4).slice().buffer;
  postMessage({ type: 'frame', buf }, [buf]);
}

function pushAudio() {
  const n = x.c3_audio();
  if (!n) return;
  const p = x.c3_audio_ptr();
  const buf = new Int16Array(x.memory.buffer, p, n).slice().buffer;
  postMessage({ type: 'audio', buf }, [buf]);
}

async function loop() {
  while (running) {
    const r = x.c3_run(16);
    if (r < 0) {
      running = false;
      postMessage({ type: 'state', running: false, msg: 'The phone stopped: ' + resultText() });
      break;
    }
    pushFrame();
    pushAudio();
    if (performance.now() - lastSave > 5000) await save();
    if (performance.now() - lastStats > 2000) { lastStats = performance.now(); x.c3_stats(); postMessage({ type: 'stats', s: JSON.parse(resultText()) }); }
    if (r > 0) await sleep(Math.min(r, 20)); else await yieldNow();
  }
  await save();
}

function start() {
  x.c3_set_time(Date.now() / 1000, -new Date().getTimezoneOffset() * 60);
  if (x.c3_boot() !== 0) {
    postMessage({ type: 'state', running: false, msg: 'Could not start: ' + resultText() });
    return false;
  }
  running = true;
  lastSerial = -1;
  postMessage({ type: 'state', running: true, msg: '' });
  loopDone = loop();
  return true;
}

async function stop() {
  running = false;
  await loopDone;
  x.c3_stop();
}

// ---- file panel ------------------------------------------------------------------

function fsCall(fn, path, data) {
  return withBuf(path, (p, l) => data === undefined ? fn(p, l) : withBuf(data, (d, dl) => fn(p, l, d, dl)));
}

async function fsOp(m) {
  const mutating = m.op !== 'list' && m.op !== 'read';
  const wasRunning = running;
  if (mutating && running) await stop(); // the OS caches its file system: reboot after changes
  let ok = true, data = null;
  try {
    if (m.op === 'list') {
      ok = fsCall(x.c3_fs_list, m.path) === 0;
      data = ok ? JSON.parse(resultText()) : resultText();
    } else if (m.op === 'read') {
      ok = fsCall(x.c3_fs_read, m.path) === 0;
      data = ok ? result().buffer : resultText();
    } else if (m.op === 'write') {
      for (const f of m.files) {
        if (fsCall(x.c3_fs_write, f.path, f.buf) !== 0) { ok = false; data = f.path + ': ' + resultText(); break; }
      }
    } else if (m.op === 'mkdir') {
      ok = fsCall(x.c3_fs_mkdir, m.path) === 0;
      if (!ok) data = resultText();
    } else if (m.op === 'remove') {
      ok = fsCall(x.c3_fs_remove, m.path) === 0;
      if (!ok) data = resultText();
    }
  } catch (e) {
    ok = false;
    data = String(e);
  }
  if (mutating) {
    await save();
    if (wasRunning) start();
  }
  postMessage({ type: 'fs', id: m.id, ok, data }, data instanceof ArrayBuffer ? [data] : []);
}

// ---- messages --------------------------------------------------------------------

let queue = Promise.resolve();
onmessage = e => { queue = queue.then(() => handle(e.data)).catch(err => postMessage({ type: 'error', msg: String(err) })); };

async function handle(m) {
  if (!x) await ready;
  switch (m.type) {
    case 'files':
      for (const f of m.files) {
        give(f.kind, f.buf.slice(0));
        names[f.kind] = f.name;
        await idbTx('files', s => s.put({ name: f.name, buf: f.buf }, KIND[f.kind]));
      }
      postMessage({ type: 'ready', names });
      break;
    case 'boot':
      if (running) await stop();
      start();
      break;
    case 'key':
      if (running) x.c3_key(m.idx, m.down ? 1 : 0);
      break;
    case 'fs':
      await fsOp(m);
      break;
    case 'factory': {
      const wasRunning = running;
      if (running) await stop();
      x.c3_disks_clear();
      await idbTx('chunks', s => s.clear());
      if (wasRunning) start();
      postMessage({ type: 'state', running, msg: 'Phone memory erased (factory state).' });
      break;
    }
  }
}

const ready = (async () => {
  const wasm = await (await fetch('c3emu_web.wasm')).arrayBuffer();
  const { instance } = await WebAssembly.instantiate(wasm, { env: { c3_now_ms: () => performance.now() } });
  x = instance.exports;
  db = await openDb();
  for (const [k, v] of await idbAll('files')) {
    const kind = +Object.keys(KIND).find(i => KIND[i] === k);
    give(kind, v.buf);
    names[kind] = v.name;
  }
  const sizes = {};
  for (const [, c] of await idbAll('chunks')) {
    withBuf(c.data, (p, l) => x.c3_disk_load_chunk(c.dev, c.off, p, l));
    sizes[c.dev] = Math.max(sizes[c.dev] || 0, c.size);
  }
  for (const d in sizes) x.c3_disk_set_size(+d, sizes[d]);
  postMessage({ type: 'ready', names });
})();
ready.catch(err => postMessage({ type: 'error', msg: 'Could not load the emulator: ' + err }));
