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

  function send(it) {
    active++;
    const x = new XMLHttpRequest();
    const q = new URLSearchParams({folder, path: it.path, size: it.file.size});
    x.open('PUT', `${base}/upload?${q}`);
    x.setRequestHeader('Content-Type', it.file.type || 'application/octet-stream');
    x.setRequestHeader('X-Fetch', '1');
    x.upload.onprogress = e => {
      if (!e.lengthComputable) return;
      it.bar.style.width = (100 * e.loaded / e.total).toFixed(1) + '%';
      it.status.textContent = `${fmtBytes(e.loaded)} of ${fmtBytes(e.total)}`;
    };
    x.onload = () => {
      if (x.status >= 200 && x.status < 300) {
        let name = it.path;
        try { name = JSON.parse(x.responseText).name || name; } catch (_) {}
        it.bar.style.width = '100%';
        it.status.textContent = 'done' + (name !== it.path.split('/').pop() ? ` · saved as “${name}”` : '');
      } else fail(it, x.responseText || x.statusText);
      done();
    };
    x.onerror = () => { fail(it, 'connection lost'); done(); };
    x.send(it.file);
  }

  function fail(it, msg) {
    failed++;
    it.status.textContent = msg;
    it.status.classList.add('error');
    it.bar.parentNode.classList.add('bad');
  }

  function done() { active--; finished++; pump(); }

  window.addEventListener('beforeunload', e => { if (active || queue.length) e.preventDefault(); });
})();
