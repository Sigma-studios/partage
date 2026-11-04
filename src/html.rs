//! Server-rendered pages.

use crate::{db, now};
use axum::{
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use std::collections::HashMap;

const CSS: &str = include_str!("assets/style.css");
const COMMON: &str = include_str!("assets/common.js");
const CRYPTO: &str = include_str!("assets/crypto.js");
const UPLOAD: &str = include_str!("assets/upload.js");
const OPEN: &str = include_str!("assets/open.js");
pub const ICON_SVG: &str = include_str!("assets/icon.svg");
pub const ICON_PNG: &[u8] = include_bytes!("assets/apple-touch-icon.png");
pub const SERVICE_WORKER: &str = concat!(
    include_str!("assets/crypto.js"),
    include_str!("assets/sw.js")
);

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

fn page(title: &str, body: &str, scripts: &[&str]) -> String {
    let mut js = COMMON.to_string();
    for s in scripts {
        js += s;
    }
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex\">\
         <meta name=\"referrer\" content=\"no-referrer\">\
         <link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\">\
         <link rel=\"apple-touch-icon\" href=\"/apple-touch-icon.png\">\
         <title>{} · partage</title><style>{CSS}</style></head>\
         <body><main>{body}</main><script>{js}</script></body></html>",
        esc(title)
    )
}

pub fn message(status: StatusCode, title: &str, text: &str) -> Response {
    let body = format!(
        "<section class=\"center\"><h2>{}</h2><p>{}</p></section>",
        esc(title),
        esc(text)
    );
    (status, Html(page(title, &body, &[]))).into_response()
}

pub fn login(error: Option<&str>) -> String {
    let error = error
        .map(|e| format!("<p class=\"result error\">{}</p>", esc(e)))
        .unwrap_or_default();
    page(
        "Log in",
        &format!(
            "<section class=\"center\"><h2 class=\"brand\">{LOGO}partage</h2>\
             <form method=\"post\" action=\"/login\" class=\"row\">\
             <label style=\"flex:1\">Admin password\
             <input type=\"password\" name=\"password\" autofocus required autocomplete=\"current-password\"></label>\
             <button>Log in</button></form>{error}</section>"
        ),
        &[],
    )
}

const LOGO: &str = "<img class=\"logo\" src=\"/favicon.svg\" alt=\"\" width=\"28\" height=\"28\">";

/// The admin pages' header: the name goes home, the buttons on the right,
/// with Upload first away from the drop zone.
fn admin_header(current: &str) -> String {
    let link = |href: &str, label: &str, class: &str| {
        if href == current {
            String::new()
        } else {
            format!("<a class=\"button{class}\" href=\"{href}\">{label}</a>")
        }
    };
    format!(
        "<header><h1><a href=\"/\">{LOGO}partage</a></h1><nav>{}{}{}\
         <form method=\"post\" action=\"/logout\" class=\"inline\"><button class=\"plain\">Log out</button></form>\
         </nav></header>",
        link("/", "↑ Upload", " primary"),
        link("/track", "Track", ""),
        link("/settings", "Settings", ""),
    )
}

fn flash_box(flash: Option<&str>) -> String {
    flash
        .map(|f| format!("<div class=\"flash\">{}</div>", esc(f)))
        .unwrap_or_default()
}

fn copy_link(url: &str) -> String {
    let url = esc(url);
    format!("<button type=\"button\" class=\"plain\" data-copy=\"{url}\">Copy</button>")
}

fn percent(used: u64, limit: u64) -> f64 {
    used as f64 * 100.0 / limit.max(1) as f64
}

fn bar_class(percent: f64) -> &'static str {
    match percent {
        p if p >= 90.0 => "bar bad",
        p if p >= 80.0 => "bar warn",
        _ => "bar",
    }
}

/// The drop zone, its options and the list of uploads under it.
/// `mode`: `admin`, `invite` or `invite-secret`.
fn drop_zone(mode: &str, plain: &str, secret: &str, max: Option<u64>) -> String {
    let max_attr = max
        .map(|m| format!(" data-max=\"{m}\""))
        .unwrap_or_default();
    let (what, choose) = if mode == "admin" {
        ("Drop files or folders here", "choose files")
    } else {
        ("Drop a file or a folder here", "choose a file")
    };
    let options = if mode == "admin" {
        "<div class=\"options\">\
         <label>Expires <select id=\"expires\"><option value=\"\">never</option>\
         <option value=\"3600\">in 1 hour</option><option value=\"86400\">in 1 day</option>\
         <option value=\"604800\">in 7 days</option><option value=\"2592000\">in 30 days</option></select></label>\
         <label><input type=\"checkbox\" id=\"secret\"> One-time secret \
         <span class=\"muted\">end-to-end encrypted, opens once</span></label></div>"
    } else {
        ""
    };
    let text = if mode == "invite" {
        String::new()
    } else {
        let hidden = if mode == "admin" { " hidden" } else { "" };
        format!(
            "<div class=\"text-secret\" id=\"text-panel\"{hidden}>\
             <textarea id=\"secret-text\" rows=\"4\" maxlength=\"100000\" placeholder=\"…or write a secret text\"></textarea>\
             <button type=\"button\" id=\"send-text\">Create secret link</button></div>"
        )
    };
    let multiple = if mode == "admin" { " multiple" } else { "" };
    format!(
        "<div id=\"drop\" class=\"drop\" data-mode=\"{mode}\" data-plain=\"{}\" data-secret=\"{}\"{max_attr}>\
         <p class=\"big\">{what}</p>\
         <p class=\"muted\">or <button type=\"button\" class=\"pick\" data-pick=\"files\">{choose}</button> · \
         <button type=\"button\" class=\"pick\" data-pick=\"folder\">choose a folder</button> \
         (sent as a zip)</p></div>\
         <input type=\"file\" id=\"pick-files\"{multiple} hidden>\
         <input type=\"file\" id=\"pick-folder\" webkitdirectory hidden>\
         {options}{text}<ul id=\"uploads\" class=\"uploads\"></ul>",
        esc(plain),
        esc(secret),
    )
}

pub fn home(used: u64, limit: u64) -> String {
    let p = percent(used, limit);
    let body = format!(
        "{}<div id=\"storage\" class=\"storage\"><strong class=\"left\">{} left</strong> \
         <span class=\"muted of\">of {} · {p:.1}% used</span>\
         <div class=\"{}\"><div style=\"width:{:.1}%\"></div></div></div>{}",
        admin_header("/"),
        fmt_bytes(limit.saturating_sub(used)),
        fmt_bytes(limit),
        bar_class(p),
        p.min(100.0),
        drop_zone("admin", "/admin/upload", "/admin/secrets", None),
    );
    page("Upload", &body, &[CRYPTO, UPLOAD])
}

const SIZE_UNITS: &str = "<option>MB</option><option selected>GB</option><option>TB</option>";

pub struct Settings<'a> {
    pub flash: Option<&'a str>,
    pub used: u64,
    pub limit: u64,
    pub webhook: bool,
}

pub fn settings(d: &Settings) -> String {
    let mut b = admin_header("/settings");
    b += &flash_box(d.flash);
    let p = percent(d.used, d.limit);
    b += &format!(
        "<section><h2>Storage</h2>\
         <div>{} of {} used ({p:.1}%)</div>\
         <div class=\"{}\"><div style=\"width:{:.1}%\"></div></div>\
         <p class=\"muted\">Counts files as stored, after compression. Uploads that would go over the limit are refused.</p>\
         <form method=\"post\" action=\"/admin/limit\" class=\"row\">\
         <label>Storage limit<input type=\"number\" name=\"amount\" min=\"0.001\" step=\"any\" required value=\"{}\"></label>\
         <label>&nbsp;<select name=\"unit\">{SIZE_UNITS}</select></label><button>Set limit</button></form>\
         </section>",
        fmt_bytes(d.used),
        fmt_bytes(d.limit),
        bar_class(p),
        p.min(100.0),
        d.limit as f64 / 1e9,
    );
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
    page("Settings", &b, &[])
}

pub struct Track<'a> {
    pub base: &'a str,
    pub flash: Option<&'a str>,
    pub files: &'a [db::File],
    pub secrets: &'a [db::File],
    pub invites: &'a [db::Invite],
    /// Receipt tokens of the secrets sent through upload links, by file id.
    pub receipts: &'a HashMap<String, String>,
}

fn secret_kind(f: &db::File) -> &'static str {
    match f.secret.as_deref() {
        Some("text") => "Text",
        _ => "File",
    }
}

fn secret_status(f: &db::File) -> String {
    match f.secret_end() {
        Some("opened") => format!("opened {}", f.opened_at.map(time).unwrap_or_default()),
        Some(end) => end.to_string(),
        None if f.expires_at.is_some_and(|t| t <= now()) => "expired".into(),
        None => "waiting".into(),
    }
}

fn delete_button(id: &str, label: &str, question: &str) -> String {
    format!(
        "<form method=\"post\" action=\"/admin/files/{}/delete\" class=\"inline\" \
         onsubmit=\"return confirm('{question}')\"><button class=\"danger\">{label}</button></form>",
        esc(id)
    )
}

pub fn track(d: &Track) -> String {
    let mut b = admin_header("/track");
    b += &flash_box(d.flash);

    // Files
    b += &format!("<section><h2>Files ({})</h2>", d.files.len());
    if d.files.is_empty() {
        b += "<p class=\"muted\">No files yet: drop some on the <a href=\"/\">upload page</a>.</p>";
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
                 <td>{}</td><td>{}</td></tr>",
                esc(&url),
                esc(&f.name),
                fmt_bytes(f.size),
                time(f.created_at),
                f.expires_at.map(time).unwrap_or_else(|| "never".into()),
                f.downloads,
                copy_link(&url),
                delete_button(
                    &f.id,
                    "Delete",
                    "Delete this file? Its link will stop working."
                ),
            );
        }
        b += "</table>";
    }
    b += "</section>";

    // Secrets
    b += "<section><h2>One-time secrets</h2>\
          <p class=\"muted\">Encrypted in the browser: their links (with the key) were only shown to whoever \
          created them. Receipts show when they were opened, and from where.</p>";
    if d.secrets.is_empty() {
        b += "<p class=\"muted\">None yet: tick “One-time secret” on the <a href=\"/\">upload page</a>.</p>";
    } else {
        b += "<table><tr><th>Kind</th><th>Size</th><th>Created</th><th>Expires</th><th>Status</th>\
              <th></th><th></th></tr>";
        for f in d.secrets {
            let receipt = f
                .receipt
                .as_deref()
                .map(|r| format!("<a class=\"button\" href=\"/r/{}\">Receipt</a>", esc(r)))
                .unwrap_or_default();
            let from = if f.invite.is_some() {
                " <span class=\"tag\">via link</span>"
            } else {
                ""
            };
            let burn = if f.deleted_at.is_none() {
                delete_button(
                    &f.id,
                    "Burn",
                    "Destroy this secret now? Its link will stop working.",
                )
            } else {
                String::new()
            };
            b += &format!(
                "<tr><td>🔒 {}{from}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{receipt}</td><td>{burn}</td></tr>",
                secret_kind(f),
                fmt_bytes(f.size),
                time(f.created_at),
                f.expires_at.map(time).unwrap_or_else(|| "never".into()),
                secret_status(f),
            );
        }
        b += "</table>";
    }
    b += "</section>";

    // Upload links
    b += &format!(
        "<section><h2>Upload links</h2>\
         <p class=\"muted\">A link someone can use once to upload one file (or folder, zipped), up to a size you choose.</p>\
         <form method=\"post\" action=\"/admin/invites\" class=\"row\">\
         <label style=\"flex:1\">For (note)<input name=\"note\" placeholder=\"e.g. Camille's photos\"></label>\
         <label>Up to<input type=\"number\" name=\"amount\" min=\"0.001\" step=\"any\" required value=\"1\"></label>\
         <label>&nbsp;<select name=\"unit\">{SIZE_UNITS}</select></label>\
         <label>Link valid for<input type=\"number\" name=\"valid_for\" min=\"0.01\" step=\"any\" required value=\"7\"></label>\
         <label>&nbsp;<select name=\"valid_unit\"><option>hours</option><option selected>days</option></select></label>\
         <label>Keep file (days, empty: forever)<input type=\"number\" name=\"file_days\" min=\"0.01\" step=\"any\"></label>\
         <label title=\"Encrypted in their browser, opens once. They get the link to send back; you see its receipt here.\">\
         One-time secret<span class=\"checkbox-field\"><input type=\"checkbox\" name=\"secret\"></span></label>\
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
                match d.receipts.get(id) {
                    Some(r) => format!("used · <a href=\"/r/{}\">receipt</a>", esc(r)),
                    None if i.secret => "used".into(),
                    None => format!("<a href=\"{}/f/{}\">used</a>", esc(d.base), esc(id)),
                }
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
            let tag = if i.secret {
                " <span class=\"tag\">🔒 one-time</span>"
            } else {
                ""
            };
            b += &format!(
                "<tr><td class=\"name\">{}{tag}</td><td>{}</td><td>{}</td><td>{keeps}</td><td>{status}</td>{actions}</tr>",
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
    page("Track", &b, &[])
}

pub fn invite(i: &db::Invite) -> String {
    let keep = match i.file_ttl {
        Some(_) => " It is deleted after a while; its link then stops working.",
        None => "",
    };
    let note = if i.note.is_empty() {
        String::new()
    } else {
        format!("<p><strong>{}</strong></p>", esc(&i.note))
    };
    let (title, how, mode) = if i.secret {
        (
            "Send a secret",
            "It is encrypted in your browser before it leaves: nobody can read it, not even this \
             server. You get a link that opens it once — send it to the person who should read it.",
            "invite-secret",
        )
    } else {
        ("Upload a file", "", "invite")
    };
    let body = format!(
        "<section class=\"center\" style=\"max-width:640px\"><h2>{title}</h2>{note}\
         <p class=\"muted\">One file or folder, up to {}. This link works once, until {}.{keep} {how}</p>{}</section>",
        fmt_bytes(i.max_bytes),
        time(i.expires_at),
        drop_zone(
            mode,
            &format!("/u/{}", i.token),
            &format!("/u/{}/secret", i.token),
            Some(i.max_bytes)
        ),
    );
    page(title, &body, &[CRYPTO, UPLOAD])
}

/// The recipient's page of a secret; `end` is why it is gone, if it is.
pub fn secret(f: &db::File, end: Option<&str>) -> String {
    let body = match end {
        Some(end) => {
            let text = match end {
                "opened" => format!(
                    "This secret was already opened{}. It could be opened only once, and is now destroyed.",
                    f.opened_at
                        .map(|t| format!(", on {}", time(t)))
                        .unwrap_or_default()
                ),
                "expired" => {
                    "This secret expired before anyone opened it, and is destroyed.".into()
                }
                _ => "This secret was destroyed by its sender.".into(),
            };
            format!("<section class=\"secret-box\"><h2>Secret gone</h2><p>{text}</p></section>")
        }
        None => {
            let (what, button) = match f.secret.as_deref() {
                Some("text") => ("a secret text".to_string(), "Reveal the secret"),
                _ => (
                    format!("an encrypted file (about {})", fmt_bytes(f.size)),
                    "Download the file",
                ),
            };
            let expires = f
                .expires_at
                .map(|t| format!(" Unopened, it expires on {}.", time(t)))
                .unwrap_or_default();
            format!(
                "<section class=\"secret-box\" id=\"secret-box\" data-id=\"{}\" data-kind=\"{}\" data-check=\"{}\" data-size=\"{}\">\
                 <h2>Someone sent you {what}</h2>\
                 <p>It can be opened <strong>only once</strong>: after that it is destroyed.{expires}</p>\
                 <p class=\"muted\">It is end-to-end encrypted: the key is in your link, after the #, and never reaches the server.</p>\
                 <button id=\"open\">{button}</button>\
                 <div class=\"bar\" hidden><div style=\"width:0\"></div></div>\
                 <p class=\"status\"></p><div class=\"out\"></div></section>",
                esc(&f.id),
                esc(f.secret.as_deref().unwrap_or("file")),
                esc(f.key_check.as_deref().unwrap_or("")),
                f.size,
            )
        }
    };
    page("Secret", &body, &[CRYPTO, OPEN])
}

fn event_label(e: &db::Event) -> String {
    let d = esc(&e.detail);
    match e.kind.as_str() {
        "created" => format!("Created {d}"),
        "viewed" if d.is_empty() => "Link page viewed (nothing opened)".into(),
        "viewed" => format!("Link page viewed {d}"),
        "opened" => {
            "<strong>Opened</strong>: download started, secret destroyed on the server".into()
        }
        "completed" => "Download finished".into(),
        "interrupted" => format!("<span class=\"warn\">Download interrupted {d}</span>"),
        "refused" => format!("Someone tried to open it: refused, {d}"),
        "burned" => format!("Burned {d}"),
        "expired" => "Expired unopened".into(),
        other => esc(other),
    }
}

pub fn receipt(f: &db::File, events: &[db::Event], link: &str, flash: Option<&str>) -> String {
    let mut b = flash_box(flash);
    let live = f.deleted_at.is_none() && f.expires_at.is_none_or(|t| t > now());
    b += &format!(
        "<section><h2>Receipt: one-time secret</h2>\
         <p>🔒 {} · {} · created {} · expires {}</p>\
         <p>Status: <strong>{}</strong></p>\
         <p class=\"muted\">Link: <code>{}#…</code> (the key part is not kept here)</p>{}</section>",
        secret_kind(f),
        fmt_bytes(f.size),
        time(f.created_at),
        f.expires_at.map(time).unwrap_or_else(|| "never".into()),
        secret_status(f),
        esc(link),
        if live {
            format!(
                "<form method=\"post\" action=\"/r/{}/burn\" class=\"inline\" \
                 onsubmit=\"return confirm('Destroy this secret now? Its link will stop working.')\">\
                 <button class=\"danger\">Burn now</button></form>",
                esc(f.receipt.as_deref().unwrap_or_default())
            )
        } else {
            String::new()
        },
    );
    b += "<section><h2>Timeline</h2><p class=\"muted\">“Viewed” is someone loading the link's page, which \
          opens nothing: link previews (Discord, Slack, iMessage…) do this, their name shows under Browser. \
          IP addresses are as the server saw them.</p>\
          <table class=\"timeline\"><tr><th>When</th><th>What</th><th>IP</th><th>Browser</th></tr>";
    for e in events {
        b += &format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td class=\"agent\" title=\"{}\">{}</td></tr>",
            time(e.at),
            event_label(e),
            esc(&e.ip),
            esc(&e.agent),
            esc(&e.agent),
        );
    }
    b += "</table></section>";
    page("Receipt", &b, &[])
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
