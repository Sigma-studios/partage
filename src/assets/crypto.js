// One-time secrets are encrypted in the browser with AES-256-GCM. The key is
// only ever in the link's #fragment, which browsers never send to servers.
//
// The plaintext is a 4-byte big-endian length, a JSON header of that length
// ({kind: 'text'} or {kind: 'file', name, type, size}), then the content.
// It is cut into records of REC bytes, each encrypted on its own with the
// nonce 0000 | record number (7 bytes) | 1 if it is the last record, else 0,
// so records cannot be reordered, dropped or cut off unnoticed.
const REC = 1 << 20, TAG = 16;

function b64u(bytes) {
  let s = ''; for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}
function unb64u(s) {
  const bin = atob(s.replace(/-/g, '+').replace(/_/g, '/'));
  return Uint8Array.from(bin, c => c.charCodeAt(0));
}
function nonce(i, last) {
  const n = new Uint8Array(12), v = new DataView(n.buffer);
  v.setUint32(3, Math.floor(i / 4294967296)); v.setUint32(7, i >>> 0); n[11] = last ? 1 : 0;
  return n;
}
function importKey(raw) {
  return crypto.subtle.importKey('raw', raw, 'AES-GCM', false, ['encrypt', 'decrypt']);
}
// Stored by the server so the recipient's page can tell a damaged link
// before opening the secret. Says nothing about the key.
async function keyCheck(raw) {
  const label = new TextEncoder().encode('partage key check');
  const buf = new Uint8Array(label.length + raw.length); buf.set(label); buf.set(raw, label.length);
  return b64u(new Uint8Array(await crypto.subtle.digest('SHA-256', buf)).subarray(0, 16));
}
function secretHeader(meta) {
  const json = new TextEncoder().encode(JSON.stringify(meta));
  const h = new Uint8Array(4 + json.length); new DataView(h.buffer).setUint32(0, json.length); h.set(json, 4);
  return h;
}
function encryptedSize(plain) { return plain + TAG * Math.max(1, Math.ceil(plain / REC)); }

// Encrypted records of header + blob, read a record at a time.
async function* encryptRecords(key, header, blob) {
  const total = header.length + blob.size, n = Math.max(1, Math.ceil(total / REC));
  for (let i = 0; i < n; i++) {
    const s = i * REC, e = Math.min(total, s + REC), buf = new Uint8Array(e - s);
    if (s < header.length) buf.set(header.subarray(s, Math.min(e, header.length)));
    const from = Math.max(s, header.length) - header.length, to = e - header.length;
    if (to > from) buf.set(new Uint8Array(await blob.slice(from, to).arrayBuffer()), from + header.length - s);
    yield new Uint8Array(await crypto.subtle.encrypt({name: 'AES-GCM', iv: nonce(i, i === n - 1)}, key, buf));
  }
}

class ByteQueue {
  constructor() { this.chunks = []; this.length = 0; }
  push(c) { this.chunks.push(c); this.length += c.length; }
  take(n) {
    const out = new Uint8Array(n); let o = 0;
    while (o < n) {
      const c = this.chunks[0], k = Math.min(c.length, n - o);
      out.set(c.subarray(0, k), o); o += k;
      if (k === c.length) this.chunks.shift(); else this.chunks[0] = c.subarray(k);
    }
    this.length -= n;
    return out;
  }
}

// Decrypted records from a stream reader of the encrypted data.
async function* decryptRecords(key, reader) {
  const q = new ByteQueue(), R = REC + TAG; let done = false, i = 0;
  for (;;) {
    while (!done && q.length <= R) {
      const r = await reader.read();
      if (r.done) done = true; else q.push(r.value);
    }
    const last = q.length <= R;
    if (last && q.length < TAG) throw new Error('The secret is incomplete.');
    const c = q.take(last ? q.length : R);
    try {
      yield new Uint8Array(await crypto.subtle.decrypt({name: 'AES-GCM', iv: nonce(i++, last)}, key, c));
    } catch (e) {
      throw new Error('The secret could not be decrypted: the link or the data is damaged.');
    }
    if (last) return;
  }
}

// Opens a decrypted stream: {meta, first: the content in the first record, rest: iterator}.
async function openSecretStream(key, reader) {
  const rest = decryptRecords(key, reader);
  const r = await rest.next();
  if (r.done) throw new Error('The secret is empty.');
  const rec = r.value, len = new DataView(rec.buffer, rec.byteOffset).getUint32(0);
  const meta = JSON.parse(new TextDecoder().decode(rec.subarray(4, 4 + len)));
  return {meta, first: rec.subarray(4 + len), rest};
}
