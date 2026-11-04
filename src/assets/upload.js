// The drop zone: files and folders (sent as one zip each), plain or as
// one-time secrets, one after the other.
const PIECE = 8 << 20;

const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xEDB88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();
function crcUpdate(crc, buf) {
  crc = ~crc;
  for (let i = 0; i < buf.length; i++) crc = CRC_TABLE[(crc ^ buf[i]) & 0xff] ^ (crc >>> 8);
  return ~crc >>> 0;
}
function dosTime(ms) {
  const d = new Date(ms);
  if (d.getFullYear() < 1980) return {time: 0, date: (1 << 5) | 1};
  return {
    time: (d.getHours() << 11) | (d.getMinutes() << 5) | (d.getSeconds() >> 1),
    date: ((d.getFullYear() - 1980) << 9) | ((d.getMonth() + 1) << 5) | d.getDate(),
  };
}
const U32 = 0xFFFFFFFF;
// A zip of `entries` ({path, file} or {path, dir: true}), stored without
// compression (the server compresses). The blob refers to the files on
// disk instead of copying them; only their checksums are computed here.
// Uses zip64 fields when sizes or offsets need them.
async function zip(entries, onProgress) {
  const enc = new TextEncoder(), parts = [], central = [];
  const total = entries.reduce((s, e) => s + (e.file ? e.file.size : 0), 0);
  let offset = 0, done = 0;
  for (const e of entries) {
    const name = enc.encode(e.path), size = e.file ? e.file.size : 0;
    let crc = 0;
    if (e.file) for (let p = 0; p < size; p += 4 << 20) {
      const b = new Uint8Array(await e.file.slice(p, p + (4 << 20)).arrayBuffer());
      crc = crcUpdate(crc, b); done += b.length; onProgress(done, total);
    }
    const big = size >= U32, {time, date} = dosTime(e.file ? e.file.lastModified : Date.now());
    const h = new DataView(new ArrayBuffer(30 + name.length + (big ? 20 : 0)));
    h.setUint32(0, 0x04034b50, true); h.setUint16(4, big ? 45 : 20, true); h.setUint16(6, 0x0800, true);
    h.setUint16(8, 0, true); h.setUint16(10, time, true); h.setUint16(12, date, true); h.setUint32(14, crc, true);
    h.setUint32(18, big ? U32 : size, true); h.setUint32(22, big ? U32 : size, true);
    h.setUint16(26, name.length, true); h.setUint16(28, big ? 20 : 0, true);
    new Uint8Array(h.buffer).set(name, 30);
    if (big) {
      const x = 30 + name.length;
      h.setUint16(x, 1, true); h.setUint16(x + 2, 16, true);
      h.setBigUint64(x + 4, BigInt(size), true); h.setBigUint64(x + 12, BigInt(size), true);
    }
    parts.push(h.buffer); if (e.file) parts.push(e.file);
    central.push({name, crc, size, offset, time, date, dir: !e.file});
    offset += h.byteLength + size;
  }
  const cdStart = offset;
  for (const c of central) {
    const z = [];
    if (c.size >= U32) z.push(c.size, c.size);
    if (c.offset >= U32) z.push(c.offset);
    const extra = z.length ? 4 + 8 * z.length : 0;
    const h = new DataView(new ArrayBuffer(46 + c.name.length + extra));
    h.setUint32(0, 0x02014b50, true); h.setUint16(4, (3 << 8) | 45, true); h.setUint16(6, z.length ? 45 : 20, true);
    h.setUint16(8, 0x0800, true); h.setUint16(10, 0, true); h.setUint16(12, c.time, true); h.setUint16(14, c.date, true);
    h.setUint32(16, c.crc, true);
    h.setUint32(20, c.size >= U32 ? U32 : c.size, true); h.setUint32(24, c.size >= U32 ? U32 : c.size, true);
    h.setUint16(28, c.name.length, true); h.setUint16(30, extra, true);
    h.setUint16(32, 0, true); h.setUint16(34, 0, true); h.setUint16(36, 0, true);
    h.setUint32(38, c.dir ? 0o40755 * 65536 + 0x10 : 0o100644 * 65536, true);
    h.setUint32(42, c.offset >= U32 ? U32 : c.offset, true);
    new Uint8Array(h.buffer).set(c.name, 46);
    if (extra) {
      let x = 46 + c.name.length;
      h.setUint16(x, 1, true); h.setUint16(x + 2, 8 * z.length, true); x += 4;
      for (const v of z) { h.setBigUint64(x, BigInt(v), true); x += 8; }
    }
    parts.push(h.buffer); offset += h.byteLength;
  }
  const cdSize = offset - cdStart, count = central.length;
  if (count >= 0xFFFF || cdStart >= U32 || cdSize >= U32) {
    const r = new DataView(new ArrayBuffer(56 + 20));
    r.setUint32(0, 0x06064b50, true); r.setBigUint64(4, 44n, true); r.setUint16(12, 45, true); r.setUint16(14, 45, true);
    r.setUint32(16, 0, true); r.setUint32(20, 0, true);
    r.setBigUint64(24, BigInt(count), true); r.setBigUint64(32, BigInt(count), true);
    r.setBigUint64(40, BigInt(cdSize), true); r.setBigUint64(48, BigInt(cdStart), true);
    r.setUint32(56, 0x07064b50, true); r.setUint32(60, 0, true); r.setBigUint64(64, BigInt(offset), true); r.setUint32(72, 1, true);
    parts.push(r.buffer);
  }
  const end = new DataView(new ArrayBuffer(22));
  end.setUint32(0, 0x06054b50, true);
  end.setUint16(8, Math.min(count, 0xFFFF), true); end.setUint16(10, Math.min(count, 0xFFFF), true);
  end.setUint32(12, Math.min(cdSize, U32), true); end.setUint32(16, Math.min(cdStart, U32), true);
  parts.push(end.buffer);
  return new Blob(parts, {type: 'application/zip'});
}

const SKIP = new Set(['.DS_Store', 'Thumbs.db', 'desktop.ini']);
function entryFile(entry) { return new Promise((res, rej) => entry.file(res, rej)); }
async function walk(dir, prefix, out) {
  const path = prefix + dir.name + '/';
  out.push({path, dir: true});
  const reader = dir.createReader();
  for (;;) {
    const batch = await new Promise((res, rej) => reader.readEntries(res, rej));
    if (!batch.length) break;
    for (const e of batch) {
      if (e.isDirectory) await walk(e, path, out);
      else if (!SKIP.has(e.name)) out.push({path: path + e.name, file: await entryFile(e)});
    }
  }
  return out;
}

function request(method, url, body, onUp, headers) {
  return new Promise((resolve, reject) => {
    const x = new XMLHttpRequest();
    x.open(method, url);
    for (const [k, v] of Object.entries(headers || {})) x.setRequestHeader(k, v);
    if (onUp) x.upload.onprogress = p => p.lengthComputable && onUp(p.loaded);
    x.onload = () => resolve(x);
    x.onerror = () => reject(new Error('The upload was cut off: the connection dropped, or it is over the space left.'));
    x.send(body);
  });
}
function failure(x) { return new Error(x.responseText || ('Upload failed (' + x.status + ')')); }

// One piece of an encrypted upload, sent again if the connection drops.
async function putPiece(up, offset, blob, onUp) {
  for (let attempt = 0; ; attempt++) {
    let x = null;
    try { x = await request('PUT', up + '?offset=' + offset, blob, onUp); } catch (e) {}
    if (x && x.status === 200) return;
    if (x && x.status === 409) {
      try { if (JSON.parse(x.responseText).received === offset + blob.size) return; } catch (e) {}
    } else if (x && x.status >= 400 && x.status < 500) throw failure(x);
    if (attempt >= 4) throw x ? failure(x) : new Error('The upload was cut off: the connection dropped.');
    await new Promise(r => setTimeout(r, 1000 * 2 ** attempt));
  }
}

const drop = document.getElementById('drop');
if (drop) {
  const cfg = drop.dataset, single = cfg.mode !== 'admin', max = +cfg.max || 0;
  const secretBox = document.getElementById('secret'), textPanel = document.getElementById('text-panel');
  const expires = document.getElementById('expires'), list = document.getElementById('uploads');
  const pickFiles = document.getElementById('pick-files'), pickFolder = document.getElementById('pick-folder');
  const isSecret = () => cfg.mode === 'invite-secret' || !!(secretBox && secretBox.checked);
  if (secretBox) {
    const sync = () => { textPanel.hidden = !secretBox.checked; };
    secretBox.onchange = sync; sync();
  }
  const queue = []; let running = false;

  function row(it) {
    const status = el('div', {class: 'status'}, 'Waiting…');
    const bar = el('div', {class: 'bar', hidden: ''}, el('div', {style: 'width:0'}));
    const size = el('span', {class: 'muted'}, it.blob ? fmtBytes(it.blob.size) : '');
    const li = el('li', {},
      el('div', {class: 'title'}, el('strong', {}, it.name),
        el('span', {}, it.secret ? el('span', {class: 'tag'}, '🔒 one-time') : null, ' ', size)),
      status, bar);
    list.prepend(li);
    return {
      li,
      status(t, error) { status.textContent = t; status.className = 'status' + (error ? ' error' : ''); },
      progress(f) { bar.hidden = f == null; if (f != null) bar.firstElementChild.style.width = (100 * f).toFixed(1) + '%'; },
      size(n) { size.textContent = fmtBytes(n); },
    };
  }
  function lock(on) {
    if (!single) return;
    drop.classList.toggle('done', on);
    if (textPanel) textPanel.hidden = on;
  }
  function add(items) {
    items = items.filter(Boolean);
    if (!items.length) return;
    if (single) {
      if (running || items.length > 1) {
        alert('This link takes one file. Drop a folder to send several files as one zip.');
        return;
      }
      lock(true);
    }
    for (const it of items) {
      it.secret = it.secret || isSecret();
      it.expiresIn = expires ? expires.value : '';
      it.ui = row(it);
      queue.push(it);
    }
    run();
  }
  async function run() {
    if (running) return;
    running = true;
    while (queue.length) {
      const it = queue.shift();
      try {
        await send(it);
      } catch (err) {
        it.ui.progress(null); it.ui.status(err.message || String(err), true);
        lock(false);
      }
    }
    running = false;
  }
  function rate(ui, started, sent, total) {
    ui.progress(total ? sent / total : 1);
    const r = sent / Math.max(1, (Date.now() - started) / 1000);
    ui.status(fmtBytes(sent) + ' of ' + fmtBytes(total) + ' · ' + fmtBytes(r) + '/s');
  }
  async function send(it) {
    const ui = it.ui;
    let blob = it.blob, type = blob ? blob.type : '';
    if (it.entries) {
      ui.status('Zipping…');
      blob = await zip(it.entries, (d, t) => {
        ui.progress(t ? d / t : 1); ui.status('Zipping… ' + Math.floor(100 * d / Math.max(1, t)) + '%');
      });
      type = 'application/zip'; ui.size(blob.size);
    }
    if (it.text != null) blob = new Blob([new TextEncoder().encode(it.text)]);
    if (max && it.text == null && blob.size > max)
      throw new Error('This is ' + fmtBytes(blob.size) + ', over the ' + fmtBytes(max) + ' allowed.');
    const r = it.secret ? await sendSecret(it, blob, type) : await sendPlain(it, blob);
    ui.progress(null);
    done(it, r);
    refreshStorage();
  }
  async function sendPlain(it, blob) {
    const form = new FormData();
    if (it.expiresIn) form.append('expires_in', it.expiresIn);
    form.append('file', blob, it.name);
    const started = Date.now();
    const x = await request('POST', cfg.plain, form, n => rate(it.ui, started, n, blob.size));
    if (x.status !== 200) throw failure(x);
    return JSON.parse(x.responseText);
  }
  async function sendSecret(it, blob, type) {
    const ui = it.ui;
    ui.status('Encrypting…');
    const raw = crypto.getRandomValues(new Uint8Array(32)), key = await importKey(raw);
    const meta = it.text != null ? {kind: 'text'}
      : {kind: 'file', name: it.name.slice(0, 255), type: type || 'application/octet-stream', size: blob.size};
    const header = secretHeader(meta), total = encryptedSize(header.length + blob.size);
    let x = await request('POST', cfg.secret, JSON.stringify({
      kind: meta.kind, size: total, check: await keyCheck(raw), expires_in: it.expiresIn ? +it.expiresIn : null,
    }), null, {'Content-Type': 'application/json'});
    if (x.status !== 200) throw failure(x);
    const up = '/s/up/' + JSON.parse(x.responseText).upload;
    try {
      let batch = [], size = 0, sent = 0;
      const started = Date.now();
      const flush = async () => {
        await putPiece(up, sent, new Blob(batch), n => rate(ui, started, sent + n, total));
        sent += size; batch = []; size = 0;
      };
      for await (const rec of encryptRecords(key, header, blob)) {
        batch.push(rec); size += rec.length;
        if (size >= PIECE) await flush();
      }
      if (size) await flush();
      x = await request('POST', up + '/done');
      if (x.status !== 200) throw failure(x);
    } catch (err) {
      fetch(up + '/abort', {method: 'POST'}).catch(() => {});
      throw err;
    }
    const r = JSON.parse(x.responseText);
    return {url: r.url + '#' + b64u(raw), receipt: r.receipt};
  }
  function done(it, r) {
    const ui = it.ui;
    if (!r.receipt) {
      ui.status('Uploaded. Anyone with this link can download it:');
      ui.li.append(linkRow(r.url));
      return;
    }
    ui.status(cfg.mode === 'admin'
      ? 'Works once. Copy it now: the key is only in this link, not on the server.'
      : 'Send this link to the person who should read it (for example whoever gave you the upload link). It works once. Copy it now: the key is only in this link.');
    const link = linkRow(r.url);
    const burn = el('button', {type: 'button', class: 'danger'}, 'Burn');
    burn.onclick = async () => {
      if (!confirm('Destroy this secret now? Its link will stop working.')) return;
      const res = await fetch(r.receipt + '/burn', {method: 'POST'}).catch(() => null);
      if (!res || !res.ok) return alert('Could not burn it: open the receipt and try again.');
      link.remove(); burn.remove(); ui.status('Burned: the link no longer opens anything.');
    };
    ui.li.append(link, el('div', {class: 'actions'},
      el('a', {class: 'button', href: r.receipt, target: '_blank'}, 'Receipt'),
      linkRow(r.receipt, 'Copy receipt link').querySelector('button'), burn));
  }
  function refreshStorage() {
    const s = document.getElementById('storage');
    if (!s) return;
    fetch('/admin/usage').then(r => r.json()).then(u => paintStorage(s, u.used, u.limit)).catch(() => {});
  }

  drop.onclick = e => { if (!e.target.closest('button')) pickFiles.click(); };
  drop.querySelector('[data-pick=folder]').onclick = () => pickFolder.click();
  drop.querySelector('[data-pick=files]').onclick = () => pickFiles.click();
  pickFiles.onchange = () => { add([...pickFiles.files].map(f => ({name: f.name, blob: f}))); pickFiles.value = ''; };
  pickFolder.onchange = () => {
    const groups = new Map();
    for (const f of pickFolder.files) {
      if (SKIP.has(f.name)) continue;
      const top = f.webkitRelativePath.split('/')[0];
      if (!groups.has(top)) groups.set(top, []);
      groups.get(top).push({path: f.webkitRelativePath, file: f});
    }
    add([...groups].map(([name, entries]) => ({name: name + '.zip', entries})));
    pickFolder.value = '';
  };
  drop.ondragover = e => { e.preventDefault(); drop.classList.add('over'); };
  drop.ondragleave = () => drop.classList.remove('over');
  drop.ondrop = async e => {
    e.preventDefault(); drop.classList.remove('over');
    // Read before any await: the drop's data goes away after the event.
    const files = [...e.dataTransfer.files];
    const entries = [...e.dataTransfer.items].filter(i => i.kind === 'file')
      .map(i => i.webkitGetAsEntry ? i.webkitGetAsEntry() : null);
    if (!entries.length || !entries.every(Boolean)) return add(files.map(f => ({name: f.name, blob: f})));
    const items = [];
    for (const en of entries) {
      if (en.isDirectory) items.push({name: en.name + '.zip', entries: await walk(en, '', [])});
      else items.push({name: en.name, blob: await entryFile(en)});
    }
    add(items);
  };
  const sendText = document.getElementById('send-text');
  if (sendText) sendText.onclick = () => {
    const t = document.getElementById('secret-text');
    if (!t.value) return t.focus();
    add([{name: 'Secret text', text: t.value, secret: true}]);
    t.value = '';
  };
}

function paintStorage(s, used, limit) {
  const pct = 100 * used / Math.max(1, limit);
  s.querySelector('.left').textContent = fmtBytes(Math.max(0, limit - used)) + ' left';
  s.querySelector('.of').textContent = 'of ' + fmtBytes(limit) + ' · ' + pct.toFixed(1) + '% used';
  const bar = s.querySelector('.bar');
  bar.className = 'bar' + (pct >= 90 ? ' bad' : pct >= 80 ? ' warn' : '');
  bar.firstElementChild.style.width = Math.min(100, pct).toFixed(1) + '%';
}
