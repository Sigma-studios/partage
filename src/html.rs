//! Server-rendered pages.

use crate::{db, now};
use axum::{
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};

const CSS: &str = r#"
:root { color-scheme: light dark; --fg: #1d1d1f; --bg: #f6f6f4; --card: #fff; --muted: #6b6b70;
  --line: #e2e2e0; --accent: #2f6fde; --bad: #c9372c; --warn: #d68a00; --ok: #2e9b5f; }
@media (prefers-color-scheme: dark) { :root { --fg: #ececec; --bg: #161617; --card: #212123;
  --muted: #9a9aa0; --line: #333336; } }
* { box-sizing: border-box; }
body { margin: 0; font: 15px/1.5 system-ui, sans-serif; color: var(--fg); background: var(--bg); }
main { max-width: 1000px; margin: 0 auto; padding: 24px 16px 64px; }
header { display: flex; align-items: center; justify-content: space-between; margin-bottom: 16px; }
h1 { font-size: 22px; margin: 0; } h2 { font-size: 17px; margin: 0 0 12px; }
section { background: var(--card); border: 1px solid var(--line); border-radius: 10px; padding: 18px; margin-bottom: 16px; }
.muted { color: var(--muted); font-size: 13px; }
.row { display: flex; flex-wrap: wrap; gap: 8px; align-items: end; }
label { display: flex; flex-direction: column; gap: 4px; font-size: 13px; color: var(--muted); }
input, select, button { font: inherit; padding: 6px 10px; border: 1px solid var(--line); border-radius: 6px;
  background: var(--bg); color: var(--fg); }
input[type=number] { width: 7em; }
button { background: var(--accent); color: #fff; border-color: var(--accent); cursor: pointer; }
button.plain { background: transparent; color: var(--fg); border-color: var(--line); }
button.danger { background: transparent; color: var(--bad); border-color: var(--line); }
button:disabled { opacity: .5; cursor: default; }
form.inline { display: inline; }
table { width: 100%; border-collapse: collapse; font-size: 14px; }
th, td { text-align: left; padding: 7px 6px; border-top: 1px solid var(--line); vertical-align: middle; }
th { color: var(--muted); font-weight: 500; font-size: 12px; border-top: none; }
td.name { word-break: break-all; }
.bar { height: 10px; background: var(--line); border-radius: 5px; overflow: hidden; margin: 8px 0; }
.bar > div { height: 100%; background: var(--ok); }
.bar.warn > div { background: var(--warn); } .bar.bad > div { background: var(--bad); }
.flash { padding: 10px 14px; border-radius: 8px; background: var(--card); border: 1px solid var(--accent); margin-bottom: 16px; }
.result { margin-top: 12px; } .result.error { color: var(--bad); }
.link { display: flex; gap: 8px; align-items: center; }
.link input { flex: 1; min-width: 0; }
.center { max-width: 460px; margin: 12vh auto; }
.tag { font-size: 12px; padding: 1px 7px; border-radius: 10px; border: 1px solid var(--line); white-space: nowrap; }
"#;

const JS: &str = r#"
for (const t of document.querySelectorAll('time[data-ts]'))
  t.textContent = new Date(t.dataset.ts * 1000).toLocaleString([], {dateStyle: 'medium', timeStyle: 'short'});
function copyButton(b) {
  b.onclick = () => navigator.clipboard.writeText(b.dataset.copy).then(() => {
    b.textContent = 'Copied'; setTimeout(() => b.textContent = 'Copy', 1500);
  });
}
document.querySelectorAll('[data-copy]').forEach(copyButton);
function fmtBytes(n) {
  const u = ['B', 'KB', 'MB', 'GB', 'TB']; let i = 0;
  while (n >= 1000 && i < u.length - 1) { n /= 1000; i++; }
  return (i ? n.toFixed(n < 10 ? 2 : 1) : n) + ' ' + u[i];
}
const up = document.getElementById('upload');
if (up) up.onsubmit = e => {
  e.preventDefault();
  const file = up.querySelector('input[type=file]').files[0];
  const result = up.querySelector('.result'), bar = up.querySelector('.bar'), btn = up.querySelector('button');
  result.className = 'result'; result.textContent = '';
  if (!file) return;
  const max = +up.dataset.max || 0;
  if (max && file.size > max) {
    result.className = 'result error';
    result.textContent = 'This file is ' + fmtBytes(file.size) + ', over the ' + fmtBytes(max) + ' allowed.';
    return;
  }
  const form = new FormData();
  const exp = up.querySelector('[name=expires]');
  if (exp && exp.value) {
    const at = Math.floor(new Date(exp.value).getTime() / 1000);
    if (at <= Date.now() / 1000) { result.className = 'result error'; result.textContent = 'The expiry date is in the past.'; return; }
    form.append('expires_at', at);
  }
  form.append('file', file);
  const xhr = new XMLHttpRequest();
  xhr.open('POST', up.action);
  bar.hidden = false; btn.disabled = true;
  const started = Date.now();
  xhr.upload.onprogress = p => {
    if (!p.lengthComputable) return;
    bar.firstElementChild.style.width = (100 * p.loaded / p.total) + '%';
    const rate = p.loaded / Math.max(1, (Date.now() - started) / 1000);
    result.textContent = fmtBytes(p.loaded) + ' of ' + fmtBytes(p.total) + ' · ' + fmtBytes(rate) + '/s';
  };
  xhr.onload = () => {
    btn.disabled = false; bar.hidden = true;
    if (xhr.status !== 200) { result.className = 'result error'; result.textContent = xhr.responseText || ('Upload failed (' + xhr.status + ')'); return; }
    const r = JSON.parse(xhr.responseText);
    result.innerHTML = '<p>Uploaded. Anyone with this link can download it:</p><div class="link"><input readonly><button type="button" class="plain">Copy</button></div>';
    result.querySelector('input').value = r.url;
    const b = result.querySelector('button'); b.dataset.copy = r.url; copyButton(b);
    if (up.dataset.done === 'hide') up.querySelector('.fields').hidden = true;
  };
  xhr.onerror = () => {
    btn.disabled = false; bar.hidden = true; result.className = 'result error';
    result.textContent = 'The upload was cut off. The file may be over the space left, or the connection dropped.';
  };
  xhr.send(form);
};
"#;

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    match i {
        0 => format!("{n} B"),
        _ if v < 10.0 => format!("{v:.2} {}", UNITS[i]),
        _ => format!("{v:.1} {}", UNITS[i]),
    }
}

/// A date in UTC, replaced by the browser's local time by the page script.
pub fn time(ts: i64) -> String {
    let days = ts.div_euclid(86400);
    let secs = ts.rem_euclid(86400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!(
        "<time data-ts=\"{ts}\">{y:04}-{m:02}-{d:02} {:02}:{:02} UTC</time>",
        secs / 3600,
        secs % 3600 / 60
    )
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex\">\
         <title>{} · partage</title><style>{CSS}</style></head>\
         <body><main>{body}</main><script>{JS}</script></body></html>",
        esc(title)
    )
}

pub fn message(status: StatusCode, title: &str, text: &str) -> Response {
    let body = format!(
        "<section class=\"center\"><h2>{}</h2><p>{}</p></section>",
        esc(title),
        esc(text)
    );
    (status, Html(page(title, &body))).into_response()
}

pub fn login(error: Option<&str>) -> String {
    let error = error
        .map(|e| format!("<p class=\"result error\">{}</p>", esc(e)))
        .unwrap_or_default();
    page(
        "Log in",
        &format!(
            "<section class=\"center\"><h2>partage</h2>\
             <form method=\"post\" action=\"/login\" class=\"row\">\
             <label style=\"flex:1\">Admin password\
             <input type=\"password\" name=\"password\" autofocus required autocomplete=\"current-password\"></label>\
             <button>Log in</button></form>{error}</section>"
        ),
    )
}

fn copy_link(url: &str) -> String {
    let url = esc(url);
    format!("<button type=\"button\" class=\"plain\" data-copy=\"{url}\">Copy</button>")
}

fn upload_form(action: &str, max: Option<u64>, with_expiry: bool, hide_when_done: bool) -> String {
    let max_attr = max
        .map(|m| format!(" data-max=\"{m}\""))
        .unwrap_or_default();
    let done = if hide_when_done {
        " data-done=\"hide\""
    } else {
        ""
    };
    let expiry = if with_expiry {
        "<label>Expires (empty: never)<input type=\"datetime-local\" name=\"expires\"></label>"
    } else {
        ""
    };
    format!(
        "<form id=\"upload\" action=\"{}\"{max_attr}{done}>\
         <div class=\"row fields\"><label style=\"flex:1\">File<input type=\"file\" required></label>\
         {expiry}<button>Upload</button></div>\
         <div class=\"bar\" hidden><div style=\"width:0\"></div></div>\
         <div class=\"result\"></div></form>",
        esc(action)
    )
}

const SIZE_UNITS: &str = "<option>MB</option><option selected>GB</option><option>TB</option>";

pub struct Dashboard<'a> {
    pub base: &'a str,
    pub flash: Option<&'a str>,
    pub used: u64,
    pub limit: u64,
    pub webhook: bool,
    pub files: &'a [db::File],
    pub invites: &'a [db::Invite],
}

pub fn dashboard(d: &Dashboard) -> String {
    let mut b = String::new();
    b += "<header><h1>partage</h1><form method=\"post\" action=\"/logout\" class=\"inline\">\
          <button class=\"plain\">Log out</button></form></header>";
    if let Some(flash) = d.flash {
        b += &format!("<div class=\"flash\">{}</div>", esc(flash));
    }

    // Storage
    let percent = d.used as f64 * 100.0 / d.limit.max(1) as f64;
    let class = match percent {
        p if p >= 90.0 => "bar bad",
        p if p >= 80.0 => "bar warn",
        _ => "bar",
    };
    b += &format!(
        "<section><h2>Storage</h2>\
         <div>{} of {} used ({percent:.1}%)</div>\
         <div class=\"{class}\"><div style=\"width:{:.1}%\"></div></div>\
         <p class=\"muted\">Counts files as stored, after compression. Uploads that would go over the limit are refused.</p>\
         <div class=\"row\">\
         <form method=\"post\" action=\"/admin/limit\" class=\"row\">\
         <label>Storage limit<input type=\"number\" name=\"amount\" min=\"0.001\" step=\"any\" required value=\"{}\"></label>\
         <label>&nbsp;<select name=\"unit\">{SIZE_UNITS}</select></label><button>Set limit</button></form>\
         </div></section>",
        fmt_bytes(d.used),
        fmt_bytes(d.limit),
        percent.min(100.0),
        d.limit as f64 / 1e9,
    );

    // Alerts
    let (state, placeholder) = if d.webhook {
        (
            "A webhook is set: alerts go to its channel.",
            "Paste a new URL to replace it",
        )
    } else {
        (
            "<strong>No webhook set: no alerts are sent.</strong>",
            "https://discord.com/api/webhooks/…",
        )
    };
    b += &format!(
        "<section><h2>Discord alerts</h2>\
         <p class=\"muted\">Tags @everyone when storage passes 80% and 90%, and when an upload is blocked for lack of space. {state}</p>\
         <div class=\"row\">\
         <form method=\"post\" action=\"/admin/webhook\" class=\"row\" style=\"flex:1\">\
         <label style=\"flex:1\">Webhook URL<input type=\"url\" name=\"url\" required placeholder=\"{placeholder}\"></label>\
         <button>Save</button></form>{}</div></section>",
        if d.webhook {
            "<form method=\"post\" action=\"/admin/test-alert\" class=\"inline\"><button class=\"plain\">Send test alert</button></form>\
             <form method=\"post\" action=\"/admin/webhook\" class=\"inline\"><input type=\"hidden\" name=\"url\">\
             <button class=\"danger\">Remove</button></form>"
        } else {
            ""
        }
    );

    // Upload
    b += "<section><h2>Upload a file</h2>";
    b += &upload_form("/admin/upload", None, true, false);
    b += "<p class=\"muted\">Links never change; once a file expires, its link says so. Reload to see new uploads below.</p></section>";

    // Files
    b += &format!("<section><h2>Files ({})</h2>", d.files.len());
    if d.files.is_empty() {
        b += "<p class=\"muted\">No files yet.</p>";
    } else {
        b += "<table><tr><th>Name</th><th>Size</th><th>Uploaded</th><th>Expires</th>\
              <th>Downloads</th><th></th><th></th></tr>";
        for f in d.files {
            let url = format!("{}/f/{}", d.base, f.id);
            let stored = if f.compressed {
                format!("{} stored", fmt_bytes(f.stored))
            } else {
                "not compressed".into()
            };
            let from = if f.invite.is_some() {
                " <span class=\"tag\">via link</span>"
            } else {
                ""
            };
            b += &format!(
                "<tr><td class=\"name\"><a href=\"{}\">{}</a>{from}</td>\
                 <td>{}<div class=\"muted\">{stored}</div></td><td>{}</td><td>{}</td><td>{}</td>\
                 <td>{}</td><td><form method=\"post\" action=\"/admin/files/{}/delete\" class=\"inline\" \
                 onsubmit=\"return confirm('Delete this file? Its link will stop working.')\">\
                 <button class=\"danger\">Delete</button></form></td></tr>",
                esc(&url),
                esc(&f.name),
                fmt_bytes(f.size),
                time(f.created_at),
                f.expires_at.map(time).unwrap_or_else(|| "never".into()),
                f.downloads,
                copy_link(&url),
                esc(&f.id),
            );
        }
        b += "</table>";
    }
    b += "</section>";

    // Upload links
    b += &format!(
        "<section><h2>Upload links</h2>\
         <p class=\"muted\">A link someone can use once to upload one file, up to a size you choose.</p>\
         <form method=\"post\" action=\"/admin/invites\" class=\"row\">\
         <label style=\"flex:1\">For (note)<input name=\"note\" placeholder=\"e.g. Camille's photos\"></label>\
         <label>Up to<input type=\"number\" name=\"amount\" min=\"0.001\" step=\"any\" required value=\"1\"></label>\
         <label>&nbsp;<select name=\"unit\">{SIZE_UNITS}</select></label>\
         <label>Link valid for<input type=\"number\" name=\"valid_for\" min=\"0.01\" step=\"any\" required value=\"7\"></label>\
         <label>&nbsp;<select name=\"valid_unit\"><option>hours</option><option selected>days</option></select></label>\
         <label>Keep file (days, empty: forever)<input type=\"number\" name=\"file_days\" min=\"0.01\" step=\"any\"></label>\
         <button>Create link</button></form>"
    );
    if !d.invites.is_empty() {
        b += "<table style=\"margin-top:12px\"><tr><th>For</th><th>Up to</th><th>Valid until</th>\
              <th>Keeps file</th><th>Status</th><th></th><th></th></tr>";
        let now = now();
        for i in d.invites {
            let url = format!("{}/u/{}", d.base, i.token);
            let usable = i.used_at.is_none() && !i.revoked && i.expires_at > now;
            let status = if let Some(id) = &i.file_id {
                format!("<a href=\"{}/f/{}\">used</a>", esc(d.base), esc(id))
            } else if i.revoked {
                "revoked".into()
            } else if i.claimed {
                "uploading…".into()
            } else if i.expires_at <= now {
                "expired".into()
            } else {
                "waiting".into()
            };
            let keeps = match i.file_ttl {
                Some(ttl) => format!("{:.3} days", ttl as f64 / 86400.0)
                    .trim_end_matches('0')
                    .trim_end_matches('.')
                    .to_string(),
                None => "forever".into(),
            };
            let actions = if usable {
                format!(
                    "<td>{}</td><td><form method=\"post\" action=\"/admin/invites/{}/revoke\" class=\"inline\">\
                     <button class=\"danger\">Revoke</button></form></td>",
                    copy_link(&url),
                    esc(&i.token)
                )
            } else {
                "<td></td><td></td>".into()
            };
            b += &format!(
                "<tr><td class=\"name\">{}</td><td>{}</td><td>{}</td><td>{keeps}</td><td>{status}</td>{actions}</tr>",
                if i.note.is_empty() {
                    "—".into()
                } else {
                    esc(&i.note)
                },
                fmt_bytes(i.max_bytes),
                time(i.expires_at),
            );
        }
        b += "</table>";
    }
    b += "</section>";
    page("Admin", &b)
}

pub fn invite(i: &db::Invite) -> String {
    let keep = match i.file_ttl {
        Some(_) => "The file is deleted after a while; its download link then stops working.",
        None => "",
    };
    let note = if i.note.is_empty() {
        String::new()
    } else {
        format!("<p><strong>{}</strong></p>", esc(&i.note))
    };
    let body = format!(
        "<section class=\"center\" style=\"max-width:640px\"><h2>Upload a file</h2>{note}\
         <p class=\"muted\">One file, up to {}. This link works once, until {}. {keep}</p>{}</section>",
        fmt_bytes(i.max_bytes),
        time(i.expires_at),
        upload_form(&format!("/u/{}", i.token), Some(i.max_bytes), false, true),
    );
    page("Upload", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(fmt_bytes(999), "999 B");
        assert_eq!(fmt_bytes(1_500_000), "1.50 MB");
        assert_eq!(fmt_bytes(80_000_000_000), "80.0 GB");
        assert!(time(0).contains("1970-01-01 00:00 UTC"));
        assert!(time(951_782_400).contains("2000-02-29 00:00 UTC"));
        assert!(time(1_793_000_000).contains("2026-10-26 07:33 UTC"));
        assert_eq!(
            esc("<a href=\"x\">'&"),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;"
        );
    }
}
