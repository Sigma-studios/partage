// Streams one-time secret files to disk as they are decrypted, so their
// size is not limited by memory. The page hands over the key with a
// message, then loads /s/dl/<id> in a hidden frame; that request never
// reaches the server, this worker answers it.
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', e => e.waitUntil(self.clients.claim()));

const jobs = new Map();
self.addEventListener('message', e => {
  const m = e.data || {};
  if (m.type === 'job' && e.ports[0]) {
    jobs.set(m.dl, {id: m.id, key: m.key, port: e.ports[0]});
    e.ports[0].postMessage({type: 'ready'});
  } else if (m.type === 'cancel') {
    jobs.delete(m.dl);
  }
  // 'ping' messages only keep this worker alive during long downloads.
});

self.addEventListener('fetch', e => {
  const m = new URL(e.request.url).pathname.match(/^\/s\/dl\/([A-Za-z0-9_-]+)$/);
  const job = m && jobs.get(m[1]);
  if (!job) return;
  jobs.delete(m[1]);
  job.port.postMessage({type: 'fetching'});
  e.respondWith(serve(job));
});

function disposition(name) {
  const ascii = name.replace(/[^\x20-\x7e]|["\\]/g, '_');
  const enc = encodeURIComponent(name).replace(/['()*]/g, c => '%' + c.charCodeAt(0).toString(16).toUpperCase());
  return `attachment; filename="${ascii}"; filename*=UTF-8''${enc}`;
}

async function serve(job) {
  const port = job.port;
  try {
    const res = await fetch('/s/' + job.id + '/open', {method: 'POST'});
    if (!res.ok) {
      const text = await res.text();
      port.postMessage({type: 'error', message: text || ('Error ' + res.status)});
      return new Response(text, {status: res.status});
    }
    const key = await importKey(unb64u(job.key));
    const {meta, first, rest} = await openSecretStream(key, res.body.getReader());
    let sent = 0;
    const body = new ReadableStream({
      start(c) { if (first.length) { c.enqueue(first); sent += first.length; } },
      async pull(c) {
        try {
          const r = await rest.next();
          if (r.done) { c.close(); port.postMessage({type: 'done', sent}); return; }
          c.enqueue(r.value); sent += r.value.length;
          port.postMessage({type: 'progress', sent, total: meta.size});
        } catch (err) {
          c.error(err); port.postMessage({type: 'error', message: err.message});
        }
      },
      cancel() { port.postMessage({type: 'error', message: 'The download was cancelled.'}); },
    }, {highWaterMark: 1});
    port.postMessage({type: 'start', name: meta.name, size: meta.size});
    return new Response(body, {headers: {
      'Content-Type': 'application/octet-stream',
      'Content-Disposition': disposition(meta.name || 'secret'),
      'Content-Length': String(meta.size),
      'X-Content-Type-Options': 'nosniff',
    }});
  } catch (err) {
    port.postMessage({type: 'error', message: err.message || String(err)});
    return new Response(String(err), {status: 500});
  }
}
