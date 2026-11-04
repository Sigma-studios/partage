for (const t of document.querySelectorAll('time[data-ts]'))
  t.textContent = new Date(t.dataset.ts * 1000).toLocaleString([], {dateStyle: 'medium', timeStyle: 'short'});
function copyButton(b) {
  b.onclick = () => navigator.clipboard.writeText(b.dataset.copy).then(() => {
    const was = b.textContent;
    b.textContent = 'Copied'; setTimeout(() => b.textContent = was, 1500);
  });
}
document.querySelectorAll('[data-copy]').forEach(copyButton);
function fmtBytes(n) {
  const u = ['B', 'KB', 'MB', 'GB', 'TB']; let i = 0;
  while (n >= 1000 && i < u.length - 1) { n /= 1000; i++; }
  return (i ? n.toFixed(n < 10 ? 2 : 1) : n) + ' ' + u[i];
}
function el(tag, attrs, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === 'class') e.className = v; else if (k.startsWith('on')) e[k] = v; else e.setAttribute(k, v);
  }
  e.append(...kids.filter(k => k != null));
  return e;
}
function linkRow(url, label) {
  const b = el('button', {type: 'button', class: 'plain', 'data-copy': url}, label || 'Copy');
  copyButton(b);
  const i = el('input', {readonly: ''}); i.value = url; i.onfocus = () => i.select();
  return el('div', {class: 'link'}, i, b);
}
