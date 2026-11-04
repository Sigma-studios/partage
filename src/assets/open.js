// The recipient's page of a one-time secret.
const box = document.getElementById('secret-box');
if (box) (async () => {
  const {id, kind, check} = box.dataset, size = +box.dataset.size;
  const btn = box.querySelector('#open'), status = box.querySelector('.status'), out = box.querySelector('.out');
  const bar = box.querySelector('.bar');
  const say = (t, error) => { status.textContent = t; status.className = 'status' + (error ? ' error' : ''); };
  const progress = f => { bar.hidden = f == null; if (f != null) bar.firstElementChild.style.width = (100 * f).toFixed(1) + '%'; };
  const keyText = location.hash.slice(1);
  let raw = null;
  try { raw = unb64u(keyText); } catch (e) {}
  if (!raw || raw.length !== 32 || await keyCheck(raw) !== check) {
    btn.remove();
    return say(keyText
      ? 'This link is damaged: part of it is missing or changed. Ask for the complete link (it ends with a # and a long code). The secret was not opened.'
      : 'This link is incomplete: the part after # is missing. Ask for the complete link. The secret was not opened.', true);
  }
  // Without a service worker the whole file is kept in memory first.
  const canStream = 'serviceWorker' in navigator && window.isSecureContext;
  if (kind === 'file' && !canStream && size > 500e6)
    say('This browser will hold the whole file (' + fmtBytes(size) + ') in memory while decrypting it: on a phone this may fail, and the secret would be lost. A computer is safer.', true);
  const forget = () => history.replaceState(null, '', location.pathname);

  btn.onclick = async () => {
    btn.disabled = true;
    try {
      if (kind === 'text') await openText();
      else if (!(canStream && await viaWorker())) await inMemory();
    } catch (err) {
      progress(null); say(err.message || String(err), true);
    }
  };

  async function fetchSecret() {
    const res = await fetch('/s/' + id + '/open', {method: 'POST'});
    if (!res.ok) throw new Error(await res.text() || ('Error ' + res.status));
    btn.remove();
    return res;
  }
  async function openText() {
    say('Opening…');
    const res = await fetchSecret();
    const {first, rest} = await openSecretStream(await importKey(raw), res.body.getReader());
    const parts = [first];
    for await (const r of rest) parts.push(r);
    const text = new TextDecoder().decode(await new Blob(parts).arrayBuffer());
    forget();
    const area = el('textarea', {readonly: '', rows: Math.min(20, Math.max(4, text.split('\n').length + 1))});
    area.value = text;
    const copy = el('button', {type: 'button', class: 'plain', 'data-copy': text}, 'Copy');
    copyButton(copy);
    out.append(area, el('div', {class: 'actions'}, copy));
    say('This secret is now destroyed on the server. Copy it before leaving this page.');
  }
  async function inMemory() {
    say('Downloading…');
    const res = await fetchSecret();
    let got = 0;
    const reader = res.body.getReader();
    const counting = {read: async () => {
      const r = await reader.read();
      if (!r.done) { got += r.value.length; progress(got / size); say('Decrypting… ' + fmtBytes(got) + ' of ' + fmtBytes(size)); }
      return r;
    }};
    const {meta, first, rest} = await openSecretStream(await importKey(raw), counting);
    const parts = [first];
    for await (const r of rest) parts.push(r);
    const blob = new Blob(parts, {type: meta.type || 'application/octet-stream'});
    forget(); progress(null);
    const a = el('a', {class: 'button', href: URL.createObjectURL(blob), download: meta.name || 'secret'}, 'Save ' + (meta.name || 'file') + ' again');
    out.append(el('div', {class: 'actions'}, a));
    a.click();
    say('Downloaded ' + (meta.name || 'the file') + ' (' + fmtBytes(blob.size) + '). It is now destroyed on the server: keep this page open until it is saved.');
  }
  function activeWorker(reg) {
    return new Promise(res => {
      if (reg.active) return res(reg.active);
      const w = reg.installing || reg.waiting;
      if (!w) return res(null);
      w.addEventListener('statechange', () => {
        if (w.state === 'activated') res(w); else if (w.state === 'redundant') res(null);
      });
    });
  }
  const within = (p, ms) => Promise.race([p, new Promise(r => setTimeout(() => r(null), ms))]);
  // Streams the file to disk through the service worker. False if the
  // worker could not be used, before anything was opened.
  async function viaWorker() {
    let reg;
    try { reg = await navigator.serviceWorker.register('/s/sw.js', {scope: '/s/dl/'}); } catch (e) { return false; }
    if (!reg) return false;
    const sw = await within(activeWorker(reg), 5000);
    if (!sw) return false;
    const ch = new MessageChannel(), dl = b64u(crypto.getRandomValues(new Uint8Array(16)));
    let fetching, ended;
    const ready = new Promise(r => ch.port1.onmessage = e => e.data.type === 'ready' && r(true));
    sw.postMessage({type: 'job', dl, id, key: keyText}, [ch.port2]);
    if (!await within(ready, 3000)) return false;
    const started = new Promise(r => fetching = r), finished = new Promise(r => ended = r);
    ch.port1.onmessage = e => {
      const m = e.data;
      if (m.type === 'fetching') { fetching(true); say('Opening…'); }
      else if (m.type === 'start') { btn.remove(); forget(); say('Downloading ' + m.name + '… keep this page open until it finishes.'); }
      else if (m.type === 'progress') { progress(m.sent / Math.max(1, m.total)); say('Downloading… ' + fmtBytes(m.sent) + ' of ' + fmtBytes(m.total) + '. Keep this page open.'); }
      else if (m.type === 'done') { progress(null); say('Downloaded (' + fmtBytes(m.sent) + '). It is now destroyed on the server.'); ended(); }
      else if (m.type === 'error') { progress(null); btn.remove(); say(m.message, true); ended(); }
    };
    const frame = el('iframe', {hidden: '', src: '/s/dl/' + dl});
    document.body.append(frame);
    if (!await within(started, 10000)) {
      // The worker never saw the request: nothing was opened.
      sw.postMessage({type: 'cancel', dl}); frame.remove();
      return false;
    }
    const ping = setInterval(() => sw.postMessage({type: 'ping'}), 10000);
    finished.then(() => clearInterval(ping));
    return true;
  }
})();
