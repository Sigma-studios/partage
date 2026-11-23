// The Drive: uploads (files, or folders dropped whole, keeping their tree)
// with progress, new folders and deletes, for the admin and share links.
(() => {
  const root = document.getElementById('drive');
  // The share form: a size limit only for links that can add.
  const mode = document.getElementById('share-mode');
  if (mode) {
    const limit = document.getElementById('share-limit');
    const sync = () => {
      const adds = mode.value !== 'read';
      limit.hidden = !adds;
      limit.querySelector('input').required = adds && !limit.querySelector('input').value;
    };
    mode.onchange = sync; sync();
  }
  if (!root) return;
  const base = root.dataset.base, folder = root.dataset.folder, reload = !!root.dataset.reload;
  const list = document.getElementById('uploads');
  const queue = [];
  let active = 0, failed = 0, finished = 0;

  async function post(path, fields) {
    const r = await fetch(base + path, {
      method: 'POST', headers: {'X-Fetch': '1'}, body: new URLSearchParams(fields),
    });
    const text = await r.text();
    if (!r.ok) throw new Error(text || r.statusText);
    return text;
  }

  for (const b of root.querySelectorAll('[data-delete]')) {
    b.onclick = async () => {
      if (!confirm(`Delete “${b.dataset.name}”? It goes to the trash.`)) return;
      try { await post('/delete', {node: b.dataset.delete}); location.reload(); }
      catch (e) { alert(e.message); }
    };
  }
  const mk = root.querySelector('[data-mkdir]');
  if (mk) mk.onclick = async () => {
    const name = prompt('Name of the new folder');
    if (!name) return;
    try { await post('/mkdir', {folder, name}); location.reload(); }
    catch (e) { alert(e.message); }
  };

  const pickFiles = document.getElementById('pick-files');
  const pickFolder = document.getElementById('pick-folder');
  for (const b of root.querySelectorAll('[data-pick]'))
    b.onclick = () => (b.dataset.pick === 'folder' ? pickFolder : pickFiles).click();
  if (pickFiles) pickFiles.onchange = () => { add([...pickFiles.files].map(f => ({file: f, path: f.name}))); pickFiles.value = ''; };
  if (pickFolder) pickFolder.onchange = () => {
    add([...pickFolder.files].map(f => ({file: f, path: f.webkitRelativePath || f.name})));
    pickFolder.value = '';
  };
  if (!pickFiles) return;

  // Dropping anywhere on the page.
  let depth = 0;
  document.addEventListener('dragenter', e => { e.preventDefault(); depth++; root.classList.add('over'); });
  document.addEventListener('dragleave', () => { if (--depth <= 0) { depth = 0; root.classList.remove('over'); } });
  document.addEventListener('dragover', e => e.preventDefault());
  document.addEventListener('drop', async e => {
    e.preventDefault(); depth = 0; root.classList.remove('over');
    const entries = [...e.dataTransfer.items].map(i => i.webkitGetAsEntry && i.webkitGetAsEntry()).filter(Boolean);
    if (!entries.length) { add([...e.dataTransfer.files].map(f => ({file: f, path: f.name}))); return; }
    const files = [];
    for (const entry of entries) await walk(entry, '', files);
    add(files);
  });

  // A dropped folder, read in batches as the browser hands them out.
  async function walk(entry, prefix, out) {
    if (entry.isFile) {
      const file = await new Promise((ok, ko) => entry.file(ok, ko));
      out.push({file, path: prefix + file.name});
    } else if (entry.isDirectory) {
      const reader = entry.createReader();
      for (;;) {
        const batch = await new Promise((ok, ko) => reader.readEntries(ok, ko));
        if (!batch.length) break;
        for (const child of batch) await walk(child, prefix + entry.name + '/', out);
      }
    }
  }

  function add(items) {
    for (const it of items) {
      const bar = el('div', {class: 'bar'}, el('div', {style: 'width:0%'}));
      const status = el('div', {class: 'status'}, 'waiting · ' + fmtBytes(it.file.size));
      it.li = el('li', {}, el('div', {class: 'title'}, el('strong', {}, it.path)), bar, status);
      it.bar = bar.firstChild; it.status = status;
      list.append(it.li);
      queue.push(it);
    }
    pump();
  }

  function pump() {
    while (active < 3 && queue.length) send(queue.shift());
    if (!active && !queue.length && finished) {
      if (reload && !failed) setTimeout(() => location.reload(), 600);
    }
  }

  // Each file is sent in pieces; a piece that fails is resent from what
  // the server says arrived, after a pause that grows, and sooner when the
  // browser comes back online. If the server dropped the upload (it
  // restarted), the file starts over.
  const PIECE = 16 << 20;
  let wake = null;
  window.addEventListener('online', () => wake && wake());
  const pause = ms => new Promise(ok => { const t = setTimeout(ok, ms); wake = () => { clearTimeout(t); ok(); }; });

  async function api(method, url, body) {
    const r = await fetch(url, {
      method, headers: {'Content-Type': 'application/json', 'X-Fetch': '1'},
      body: body ? JSON.stringify(body) : undefined,
    });
    let j = {};
    try { j = await r.json(); } catch (_) {}
    return {status: r.status, ok: r.ok, j};
  }

  function putPiece(it, url, blob) {
    return new Promise(ok => {
      const x = new XMLHttpRequest();
      x.open('PUT', url);
      x.setRequestHeader('X-Fetch', '1');
      x.upload.onprogress = e => show(it, it.received + e.loaded);
      x.onload = () => {
        let j = {};
        try { j = JSON.parse(x.responseText); } catch (_) {}
        ok({status: x.status, j});
      };
      x.onerror = x.ontimeout = () => ok({status: 0, j: {}});
      x.send(blob);
    });
  }

  function show(it, sent) {
    const total = it.file.size || 1;
    it.bar.style.width = (100 * Math.min(sent, total) / total).toFixed(1) + '%';
    it.status.textContent = `${fmtBytes(Math.min(sent, it.file.size))} of ${fmtBytes(it.file.size)}`;
  }

  class Fatal extends Error {}

  async function send(it) {
    active++;
    it.status.classList.remove('error');
    it.bar.parentNode.classList.remove('bad');
    if (it.retry) { it.retry.remove(); it.retry = null; }
    let tries = 0;
    try {
      for (;;) {
        try {
          if (!it.id) {
            const r = await api('POST', `${base}/uploads`, {folder, path: it.path, size: it.file.size, type: it.file.type});
            if (r.status >= 400 && r.status < 500 || r.status === 507) throw new Fatal(r.j.error || 'refused');
            if (!r.ok) throw new Error(r.j.error || 'the server did not answer');
            it.id = r.j.id; it.received = 0;
          }
          for (;;) {
            const end = Math.min(it.received + PIECE, it.file.size);
            const r = await putPiece(it, `${base}/uploads/${it.id}?offset=${it.received}`, it.file.slice(it.received, end));
            if (r.status === 200) {
              it.received = r.j.received; tries = 0;
              if (r.j.done) { success(it, r.j.name); return; }
            } else if (r.status === 409 && r.j.received != null) {
              it.received = r.j.received;
            } else if (r.status === 404) {
              it.id = null; it.received = 0;
              throw new Error('the upload was lost, starting this file again');
            } else if (r.status === 0 || r.status >= 500 && r.status !== 507 || r.status === 409) {
              throw new Error(r.j.error || 'connection lost');
            } else {
              throw new Fatal(r.j.error || 'refused');
            }
          }
        } catch (e) {
          if (e instanceof Fatal || ++tries > 40) throw e;
          const wait = Math.min(30, 2 ** Math.min(tries, 5));
          it.status.textContent = `${e.message} · trying again in ${wait} s`;
          await pause(wait * 1000);
          if (it.id) {
            const st = await api('GET', `${base}/uploads/${it.id}`).catch(() => null);
            if (st && st.ok) it.received = st.j.received;
            else if (st && st.status === 404) { it.id = null; it.received = 0; }
          }
        }
      }
    } catch (e) {
      fail(it, e.message);
    } finally {
      done();
    }
  }

  function success(it, name) {
    it.bar.style.width = '100%';
    it.status.textContent = 'done' + (name && name !== it.path.split('/').pop() ? ` · saved as “${name}”` : '');
  }

  function fail(it, msg) {
    failed++;
    it.status.textContent = msg;
    it.status.classList.add('error');
    it.bar.parentNode.classList.add('bad');
    it.retry = el('button', {type: 'button', class: 'plain small', onclick: () => { failed--; queue.push(it); pump(); }}, 'Retry');
    it.li.querySelector('.title').append(it.retry);
  }

  function done() { active--; finished++; pump(); }

  window.addEventListener('beforeunload', e => { if (active || queue.length) e.preventDefault(); });
})();
