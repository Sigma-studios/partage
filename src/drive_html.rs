//! The Drive's pages: the admin's folders, item details and trash, the pages
//! share links open, and the Drive's parts of Settings and Track.

use crate::{
    drive_db::{Link, Mode, Node, ROOT, Version},
    html::{
        LOGO, admin_header, bar_class, copy_link, esc, flash_box, fmt_bytes, page, percent, time,
    },
    now,
};

const DRIVE_JS: &str = include_str!("assets/drive.js");

fn icon(n: &Node) -> &'static str {
    if n.dir {
        return "📁";
    }
    let t = n.content_type.as_deref().unwrap_or("");
    if t.starts_with("image/") {
        "🖼️"
    } else if t.starts_with("video/") {
        "🎞️"
    } else if t.starts_with("audio/") {
        "🎵"
    } else if t == "application/pdf" {
        "📕"
    } else {
        "📄"
    }
}

/// `Drive / a / b`, each part a link but the last.
fn crumbs(path: &[Node], href: impl Fn(&Node) -> String, root_label: &str) -> String {
    let mut out = String::from("<nav class=\"crumbs\">");
    for (i, n) in path.iter().enumerate() {
        let label = if i == 0 {
            root_label.to_string()
        } else {
            esc(&n.name)
        };
        if i > 0 {
            out += "<span class=\"muted\"> / </span>";
        }
        if i + 1 == path.len() {
            out += &format!("<strong>{label}</strong>");
        } else {
            out += &format!("<a href=\"{}\">{label}</a>", href(n));
        }
    }
    out + "</nav>"
}

fn upload_tools(can_mkdir: bool) -> String {
    let mkdir = if can_mkdir {
        "<button type=\"button\" class=\"plain\" data-mkdir>New folder</button>"
    } else {
        ""
    };
    format!(
        "<div class=\"tools\">\
         <button type=\"button\" data-pick=\"files\">↑ Upload files</button>\
         <button type=\"button\" class=\"plain\" data-pick=\"folder\">↑ Upload a folder</button>{mkdir}</div>\
         <input type=\"file\" id=\"pick-files\" multiple hidden>\
         <input type=\"file\" id=\"pick-folder\" webkitdirectory hidden>"
    )
}

pub struct Folder<'a> {
    pub flash: Option<String>,
    pub error: bool,
    pub path: &'a [Node],
    pub children: &'a [Node],
    pub box_used: u64,
    pub box_limit: u64,
    pub connected: bool,
    pub key_saved: bool,
    pub pending: u64,
}

fn flash(text: &Option<String>, error: bool) -> String {
    match text {
        Some(t) if error => format!("<div class=\"flash error\">{}</div>", esc(t)),
        Some(t) => flash_box(Some(t)),
        None => String::new(),
    }
}

/// The admin's view of a folder.
pub fn folder(d: &Folder) -> String {
    let here = d.path.last().expect("a path has its folder");
    let mut b = admin_header(if here.id == ROOT { "/drive" } else { "" });
    b += &flash(&d.flash, d.error);
    if !d.connected {
        b += "<div class=\"flash warn\">No Storage Box connected: Drive files are only on this server. \
              Connect one in <a href=\"/settings#drive\">Settings</a>.</div>";
    }
    if !d.key_saved {
        b += "<div class=\"flash warn\">The Drive key is not saved anywhere else yet: without it, the copies \
              on the Storage Box and on the PC cannot be read. <a href=\"/settings#drive\">Save it now</a>.</div>";
    }
    let p = percent(d.box_used, d.box_limit);
    let pending = if d.pending > 0 {
        format!(
            " · {} not on the Storage Box yet",
            plural(d.pending, "file")
        )
    } else {
        String::new()
    };
    b += &format!(
        "<div class=\"storage\"><strong class=\"left\">{} left</strong> \
         <span class=\"muted of\">of {} · {p:.1}% used{pending}</span>\
         <div class=\"{}\"><div style=\"width:{:.1}%\"></div></div></div>",
        fmt_bytes(d.box_limit.saturating_sub(d.box_used)),
        fmt_bytes(d.box_limit),
        bar_class(p),
        p.min(100.0),
    );
    b += &format!(
        "<section id=\"drive\" class=\"drive\" data-base=\"/drive\" data-folder=\"{}\" data-reload=\"1\">\
         <div class=\"drive-head\">{}<div class=\"row\">\
         <a class=\"button\" href=\"/drive/zip/{id}\" download>Download as zip</a>\
         <a class=\"button\" href=\"/drive/item/{id}\">Share{}</a>\
         <a class=\"button\" href=\"/drive/trash\">Trash</a></div></div>{}",
        esc(&here.id),
        crumbs(d.path, |n| folder_href(&n.id), "Drive"),
        if here.id == ROOT {
            ""
        } else {
            " or edit this folder"
        },
        upload_tools(true),
        id = esc(&here.id),
    );
    b += &listing(
        d.children,
        &Listing {
            open: &|n: &Node| {
                if n.dir {
                    folder_href(&n.id)
                } else {
                    format!("/drive/file/{}?inline=1", esc(&n.id))
                }
            },
            download: &|n: &Node| format!("/drive/file/{}", esc(&n.id)),
            details: true,
            delete: true,
        },
    );
    b += "<p class=\"muted drop-hint\">Drop files or folders anywhere on this page to upload them here. \
          A file with the same name becomes a new version of it.</p>\
          <ul id=\"uploads\" class=\"uploads\"></ul></section>";
    page(
        if here.id == ROOT { "Drive" } else { &here.name },
        &b,
        &[DRIVE_JS],
    )
}

fn folder_href(id: &str) -> String {
    if id == ROOT {
        "/drive".into()
    } else {
        format!("/drive?f={}", esc(id))
    }
}

fn plural(n: u64, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

struct Listing<'a> {
    open: &'a dyn Fn(&Node) -> String,
    download: &'a dyn Fn(&Node) -> String,
    details: bool,
    delete: bool,
}

fn listing(children: &[Node], l: &Listing) -> String {
    if children.is_empty() {
        return "<p class=\"muted empty\">This folder is empty.</p>".into();
    }
    let mut b = String::from(
        "<table class=\"files\"><tr><th>Name</th><th>Size</th><th>Modified</th><th></th></tr>",
    );
    for n in children {
        let target = if n.dir {
            ""
        } else {
            " target=\"_blank\" rel=\"noopener\""
        };
        let mut actions = String::new();
        if !n.dir {
            actions += &format!(
                "<a class=\"button small\" href=\"{}\" download>Download</a>",
                (l.download)(n)
            );
        }
        if l.details {
            actions += &format!(
                "<a class=\"button small\" href=\"/drive/item/{}\">Details</a>",
                esc(&n.id)
            );
        }
        if l.delete {
            actions += &format!(
                "<button type=\"button\" class=\"danger small\" data-delete=\"{}\" data-name=\"{}\">Delete</button>",
                esc(&n.id),
                esc(&n.name)
            );
        }
        b += &format!(
            "<tr><td class=\"name\"><a href=\"{}\"{target}>{} {}</a></td><td>{}</td><td>{}</td>\
             <td class=\"actions-cell\">{actions}</td></tr>",
            (l.open)(n),
            icon(n),
            esc(&n.name),
            if n.dir {
                String::new()
            } else {
                fmt_bytes(n.size)
            },
            time(n.modified_at),
        );
    }
    b + "</table>"
}

pub struct Item<'a> {
    pub base: &'a str,
    pub flash: Option<String>,
    pub error: bool,
    pub node: &'a Node,
    pub path: &'a [Node],
    pub versions: &'a [Version],
    pub links: &'a [Link],
    pub folders: &'a [(String, String)],
    /// For folders: what is in them.
    pub used: u64,
}

const EXPIRY: &str = "<option value=\"\">never</option><option value=\"86400\">in 1 day</option>\
    <option value=\"604800\">in 7 days</option><option value=\"2592000\">in 30 days</option>\
    <option value=\"7776000\">in 90 days</option><option value=\"31536000\">in 1 year</option>";

/// A file's or folder's details: versions, share links, rename, move, delete.
pub fn item(d: &Item) -> String {
    let n = d.node;
    let mut b = admin_header("");
    b += &flash(&d.flash, d.error);
    let id = esc(&n.id);
    let back = format!("/drive/item/{id}");
    b += &format!(
        "<section>{}<h2 class=\"item-title\">{} {}</h2>",
        crumbs(d.path, |p| folder_href(&p.id), "Drive"),
        icon(n),
        if n.id == ROOT {
            "Drive".into()
        } else {
            esc(&n.name)
        }
    );
    if n.dir {
        let limit = match n.limit_bytes {
            Some(l) => format!(
                " · limit {} ({} left)",
                fmt_bytes(l),
                fmt_bytes(l.saturating_sub(d.used))
            ),
            None => String::new(),
        };
        b += &format!(
            "<p class=\"muted\">Holds {}{limit} · <a href=\"{}\">open the folder</a></p>",
            fmt_bytes(d.used),
            folder_href(&n.id)
        );
    } else {
        b += &format!(
            "<p class=\"muted\">{} · modified {} · \
             <a href=\"/drive/file/{id}?inline=1\" target=\"_blank\" rel=\"noopener\">open</a> · \
             <a href=\"/drive/file/{id}\" download>download</a></p>",
            fmt_bytes(n.size),
            time(n.modified_at),
        );
    }
    if n.id != ROOT {
        let options: String = d
            .folders
            .iter()
            .filter(|(fid, _)| Some(fid) != n.parent.as_ref())
            .map(|(fid, path)| {
                format!(
                    "<option value=\"{}\">{}</option>",
                    esc(fid),
                    if path.is_empty() {
                        "Drive".into()
                    } else {
                        format!("Drive{}", esc(path))
                    }
                )
            })
            .collect();
        b += &format!(
            "<div class=\"row\" style=\"margin-top:12px\">\
             <form method=\"post\" action=\"/drive/rename\" class=\"row\"><input type=\"hidden\" name=\"node\" value=\"{id}\">\
             <label>Name<input name=\"name\" required value=\"{}\"></label><button class=\"plain\">Rename</button></form>\
             <form method=\"post\" action=\"/drive/move\" class=\"row\"><input type=\"hidden\" name=\"node\" value=\"{id}\">\
             <label>Move to<select name=\"to\">{options}</select></label><button class=\"plain\">Move</button></form>\
             <form method=\"post\" action=\"/drive/delete\" class=\"inline\" onsubmit=\"return confirm('Move to the trash?')\">\
             <input type=\"hidden\" name=\"node\" value=\"{id}\"><button class=\"danger\">Delete</button></form></div>",
            esc(&n.name),
        );
    }
    b += "</section>";

    // Share links.
    b += "<section><h2>Share</h2>";
    if !d.links.is_empty() {
        b += "<table><tr><th>Link</th><th>Can</th><th>Note</th><th>Expires</th><th>Uses</th><th></th></tr>";
        for l in d.links {
            b += &link_row(d.base, l, &back);
        }
        b += "</table>";
    }
    if n.dir {
        let (limit_value, limit_units) = size_fields(n.limit_bytes);
        b += &format!(
            "<form method=\"post\" action=\"/drive/share\" class=\"share-form\" id=\"share-form\">\
             <input type=\"hidden\" name=\"node\" value=\"{id}\">\
             <div class=\"row\">\
             <label>People with the link can<select name=\"mode\" id=\"share-mode\">\
             <option value=\"read\">view and download</option>\
             <option value=\"read_add\">view, download and add</option>\
             <option value=\"full\">view, download, add and delete</option>\
             <option value=\"add\">only add (they see nothing)</option></select></label>\
             <label>Expires<select name=\"expires\">{EXPIRY}</select></label>\
             <label style=\"flex:1\">Note (for you)<input name=\"note\" maxlength=\"200\"></label></div>\
             <div class=\"row\" id=\"share-limit\" hidden>\
             <label>Size limit of this folder<input type=\"number\" name=\"limit_amount\" min=\"0.001\" step=\"any\" value=\"{limit_value}\"></label>\
             <label>&nbsp;<select name=\"limit_unit\">{limit_units}</select></label>\
             <p class=\"muted\" style=\"flex:1\">Required for links that can add: uploads through any link stop once the folder \
             holds this much. Deleting frees room. You are not limited.</p></div>\
             <button>Create link</button></form>"
        );
        b += &format!(
            "<h2 style=\"margin-top:18px\">Size limit</h2><form method=\"post\" action=\"/drive/limit\" class=\"row\">\
             <input type=\"hidden\" name=\"node\" value=\"{id}\">\
             <label>Limit for links adding here<input type=\"number\" name=\"amount\" min=\"0\" step=\"any\" value=\"{limit_value}\" placeholder=\"none\"></label>\
             <label>&nbsp;<select name=\"unit\">{limit_units}</select></label><button class=\"plain\">Save</button>\
             <span class=\"muted\">Empty removes it (links that add then stop working until one is set).</span></form>"
        );
    } else {
        b += &format!(
            "<form method=\"post\" action=\"/drive/share\" class=\"row\">\
             <input type=\"hidden\" name=\"node\" value=\"{id}\"><input type=\"hidden\" name=\"mode\" value=\"file\">\
             <label>Expires<select name=\"expires\">{EXPIRY}</select></label>\
             <label style=\"flex:1\">Note (for you)<input name=\"note\" maxlength=\"200\"></label>\
             <button>Create download link</button></form>"
        );
    }
    b += "</section>";

    if !n.dir && !d.versions.is_empty() {
        b += "<section><h2>Versions</h2>\
              <table><tr><th>Uploaded</th><th>Size</th><th>By</th><th>Kept until</th><th></th></tr>";
        for v in d.versions {
            let current = n.version.as_deref() == Some(v.id.as_str());
            let action = if current {
                "<span class=\"tag\">current</span>".to_string()
            } else {
                format!(
                    "<a class=\"button small\" href=\"/drive/file/{id}?v={}\" download>Download</a> \
                     <form method=\"post\" action=\"/drive/revert\" class=\"inline\">\
                     <input type=\"hidden\" name=\"node\" value=\"{id}\"><input type=\"hidden\" name=\"version\" value=\"{}\">\
                     <button class=\"plain small\">Make current</button></form>",
                    esc(&v.id),
                    esc(&v.id)
                )
            };
            let until = match v.replaced_at {
                Some(t) => time(t + 30 * 86400),
                None => String::new(),
            };
            b += &format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{until}</td><td class=\"actions-cell\">{action}</td></tr>",
                time(v.created_at),
                fmt_bytes(v.size),
                esc(&v.added_by)
            );
        }
        b += "</table></section>";
    }
    page(
        if n.id == ROOT { "Drive" } else { &n.name },
        &b,
        &[DRIVE_JS],
    )
}

fn link_row(base: &str, l: &Link, back: &str) -> String {
    let url = format!("{base}/d/{}", l.token);
    let expiry = match l.expires_at {
        Some(t) if t <= now() => "expired".to_string(),
        Some(t) => time(t),
        None => "never".into(),
    };
    format!(
        "<tr><td class=\"link-cell\"><a href=\"{}\" target=\"_blank\" rel=\"noopener\">/d/{}</a> {}</td>\
         <td>{}</td><td>{}</td><td>{expiry}</td><td>{}</td>\
         <td><form method=\"post\" action=\"/drive/links/{}/revoke\" class=\"inline\" \
         onsubmit=\"return confirm('Revoke this link? It stops working at once.')\">\
         <input type=\"hidden\" name=\"back\" value=\"{}\"><button class=\"danger small\">Revoke</button></form></td></tr>",
        esc(&url),
        esc(&l.token[..8.min(l.token.len())]),
        copy_link(&url),
        l.mode.label(),
        esc(&l.note),
        l.uses,
        esc(&l.token),
        esc(back),
    )
}

pub struct TrashItem {
    pub node: Node,
    pub size: u64,
    pub from: String,
}

pub fn trash(flash_text: Option<String>, error: bool, items: &[TrashItem]) -> String {
    let mut b = admin_header("");
    b += &flash(&flash_text, error);
    b += "<section><nav class=\"crumbs\"><a href=\"/drive\">Drive</a><span class=\"muted\"> / </span><strong>Trash</strong></nav>\
          <p class=\"muted\">Deleted files and folders stay here 30 days, then go for good. \
          Their copies on the PC are kept a while longer.</p>";
    if items.is_empty() {
        b += "<p class=\"muted empty\">The trash is empty.</p>";
    } else {
        b += "<table><tr><th>Name</th><th>Was in</th><th>Size</th><th>Deleted</th><th>By</th><th></th></tr>";
        for t in items {
            let n = &t.node;
            b += &format!(
                "<tr><td class=\"name\">{} {}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
                 <td class=\"actions-cell\"><form method=\"post\" action=\"/drive/restore\" class=\"inline\">\
                 <input type=\"hidden\" name=\"node\" value=\"{id}\"><button class=\"plain small\">Restore</button></form> \
                 <form method=\"post\" action=\"/drive/purge\" class=\"inline\" onsubmit=\"return confirm('Delete for good?')\">\
                 <input type=\"hidden\" name=\"node\" value=\"{id}\"><button class=\"danger small\">Delete for good</button></form></td></tr>",
                icon(n),
                esc(&n.name),
                esc(&t.from),
                fmt_bytes(t.size),
                n.trashed_at.map(time).unwrap_or_default(),
                esc(n.trashed_by.as_deref().unwrap_or("")),
                id = esc(&n.id),
            );
        }
        b += "</table>";
    }
    b += "</section>";
    page("Trash", &b, &[])
}

/// A size as a number and a unit for a form: the largest unit it fills.
fn size_fields(bytes: Option<u64>) -> (String, String) {
    let (scale, unit) = match bytes.unwrap_or(1_000_000_000) {
        b if b >= 1_000_000_000_000 => (1e12, "TB"),
        b if b >= 1_000_000_000 => (1e9, "GB"),
        _ => (1e6, "MB"),
    };
    let amount = bytes
        .map(|b| format!("{}", (b as f64 / scale * 1000.0).round() / 1000.0))
        .unwrap_or_default();
    let units = ["MB", "GB", "TB"]
        .iter()
        .map(|u| {
            let sel = if *u == unit { " selected" } else { "" };
            format!("<option{sel}>{u}</option>")
        })
        .collect();
    (amount, units)
}

// Pages for share links.

fn public_header() -> String {
    format!("<header><h1><span class=\"brand\">{LOGO}partage</span></h1></header>")
}

pub fn shared_file(token: &str, n: &Node, viewable: bool) -> String {
    let t = esc(token);
    let open = if viewable {
        format!(
            "<a class=\"button plain-link\" href=\"/d/{t}/file/{}?inline=1\">Open</a>",
            esc(&n.id)
        )
    } else {
        String::new()
    };
    let body = format!(
        "<section class=\"center\" style=\"max-width:560px\"><h2>{} {}</h2><p class=\"muted\">{}</p>\
         <div class=\"row\"><a class=\"button primary\" href=\"/d/{t}/file/{}\" download>Download</a>{open}</div></section>",
        icon(n),
        esc(&n.name),
        fmt_bytes(n.size),
        esc(&n.id),
    );
    page(&n.name, &body, &[])
}

pub struct Shared<'a> {
    pub token: &'a str,
    pub link: &'a Link,
    /// From the link's folder down to the one shown.
    pub path: &'a [Node],
    pub children: &'a [Node],
    /// Room left for additions, and the limit, when the link can add.
    pub room: Option<(u64, u64)>,
    pub flash: Option<String>,
}

pub fn shared_folder(d: &Shared) -> String {
    let here = d.path.last().expect("a path has its folder");
    let root = &d.path[0];
    let t = esc(d.token);
    let mode = d.link.mode;
    let mut b = public_header();
    b += &flash(&d.flash, true);
    let what = match mode {
        Mode::Add => "You can add files and folders here. You cannot see what is already there.",
        Mode::ReadAdd => "You can view, download and add files.",
        Mode::Full => {
            "You can view, download, add and delete files. Deleted files go to the owner's trash."
        }
        _ => "You can view and download these files.",
    };
    let room = match d.room {
        Some((left, limit)) => format!(" {} left of {}.", fmt_bytes(left), fmt_bytes(limit)),
        None => String::new(),
    };
    let expiry = match d.link.expires_at {
        Some(e) => format!(" This link works until {}.", time(e)),
        None => String::new(),
    };
    let zip = if mode.can_read() {
        format!(
            "<a class=\"button\" href=\"/d/{t}/zip?f={}\" download>Download as zip</a>",
            esc(&here.id)
        )
    } else {
        String::new()
    };
    b += &format!(
        "<section id=\"drive\" class=\"drive\" data-base=\"/d/{t}\" data-folder=\"{}\" data-reload=\"{}\">\
         <div class=\"drive-head\">{}{zip}</div><p class=\"muted\">{what}{room}{expiry}</p>",
        esc(&here.id),
        if mode.can_read() { "1" } else { "" },
        if mode.can_read() {
            crumbs(
                d.path,
                |n| {
                    if n.id == root.id {
                        format!("/d/{t}")
                    } else {
                        format!("/d/{t}?f={}", esc(&n.id))
                    }
                },
                &esc(&root.name),
            )
        } else {
            format!(
                "<nav class=\"crumbs\"><strong>{}</strong></nav>",
                esc(&root.name)
            )
        },
    );
    if mode.can_add() {
        b += &upload_tools(true);
    }
    if mode.can_read() {
        let tok = d.token.to_string();
        let tok2 = tok.clone();
        b += &listing(
            d.children,
            &Listing {
                open: &move |n: &Node| {
                    if n.dir {
                        format!("/d/{}?f={}", esc(&tok), esc(&n.id))
                    } else {
                        format!("/d/{}/file/{}?inline=1", esc(&tok), esc(&n.id))
                    }
                },
                download: &move |n: &Node| format!("/d/{}/file/{}", esc(&tok2), esc(&n.id)),
                details: false,
                delete: mode.can_delete(),
            },
        );
    }
    if mode.can_add() {
        b += "<p class=\"muted drop-hint\">Drop files or folders anywhere on this page to add them. \
              A file whose name is taken gets a number: nothing is overwritten.</p>";
    }
    b += "<ul id=\"uploads\" class=\"uploads\"></ul></section>";
    page(&root.name, &b, &[DRIVE_JS])
}

// Settings and Track.

pub struct SettingsPart<'a> {
    pub connected: Option<String>,
    pub via_replique: Option<String>,
    pub public_key: &'a str,
    pub box_used: u64,
    pub box_limit: u64,
    pub local_used: u64,
    pub local_limit: u64,
    pub pending: u64,
    pub key: Option<&'a str>,
    pub key_saved: bool,
    pub last_error: Option<(i64, String)>,
    pub last_check: Option<(i64, usize, String)>,
    pub last_manifest: Option<i64>,
    pub last_run: Option<(i64, bool)>,
}

pub fn settings_part(d: &SettingsPart) -> String {
    let mut b = String::from("<section id=\"drive\"><h2>Drive</h2>");
    let p = percent(d.box_used, d.box_limit);
    let (box_amount, box_units) = size_fields(Some(d.box_limit));
    let (local_amount, local_units) = size_fields(Some(d.local_limit));
    b += &format!(
        "<div>{} of {} used on the Storage Box ({p:.1}%)</div>\
         <div class=\"{}\"><div style=\"width:{:.1}%\"></div></div>\
         <p class=\"muted\">Counts every version, the trash and the manifests. Uploads that would go over are refused; \
         Discord hears at 80% and 90%. On this server: {} of {} (copies beyond that are freed once on the box){}.</p>\
         <form method=\"post\" action=\"/admin/drive/limits\" class=\"row\">\
         <label>Drive limit<input type=\"number\" name=\"box_amount\" min=\"0.001\" step=\"any\" required value=\"{box_amount}\"></label>\
         <label>&nbsp;<select name=\"box_unit\">{box_units}</select></label>\
         <label>Kept on this server up to<input type=\"number\" name=\"local_amount\" min=\"0\" step=\"any\" required value=\"{local_amount}\"></label>\
         <label>&nbsp;<select name=\"local_unit\">{local_units}</select></label><button>Save</button></form>",
        fmt_bytes(d.box_used),
        fmt_bytes(d.box_limit),
        bar_class(p),
        p.min(100.0),
        fmt_bytes(d.local_used),
        fmt_bytes(d.local_limit),
        if d.pending > 0 {
            format!(
                ", {} waiting to be copied to the box",
                plural(d.pending, "file")
            )
        } else {
            String::new()
        },
    );

    b += "<h2 style=\"margin-top:18px\">Storage Box</h2>";
    match &d.connected {
        Some(label) => {
            let via = d
                .via_replique
                .as_ref()
                .map(|u| format!(" Paired with Réplique at {}: it watches the copies and the PC backs them up.", esc(u)))
                .unwrap_or_else(|| " Not paired with Réplique: nobody is told if copying stops.".into());
            let mut status = Vec::new();
            if let Some((t, ok)) = d.last_run {
                status.push(format!(
                    "last sync {} {}",
                    time(t),
                    if ok { "✓" } else { "failed" }
                ));
            }
            if let Some(t) = d.last_manifest {
                status.push(format!("last manifest {}", time(t)));
            }
            if let Some((t, n, what)) = &d.last_check {
                status.push(format!(
                    "last check {} ({}, {})",
                    time(*t),
                    esc(what),
                    if *n == 0 {
                        "no problems".into()
                    } else {
                        format!("<strong>{n} problems</strong>")
                    }
                ));
            }
            b += &format!(
                "<p>Files are copied to <code>{}</code>.{via}</p><p class=\"muted\">{}</p>",
                esc(label),
                status.join(" · ")
            );
            if let Some((t, e)) = &d.last_error {
                b += &format!("<p class=\"result error\">{}: {}</p>", time(*t), esc(e));
            }
            b += "<div class=\"row\">\
                  <form method=\"post\" action=\"/admin/drive/sync\" class=\"inline\"><button class=\"plain\">Sync now</button></form>\
                  <form method=\"post\" action=\"/admin/drive/check\" class=\"inline\"><button class=\"plain\">Check the box now</button></form>\
                  <form method=\"post\" action=\"/admin/drive/disconnect\" class=\"inline\" \
                  onsubmit=\"return confirm('Stop copying to this Storage Box? Files only there stay there, and can no longer be downloaded until it is connected again.')\">\
                  <button class=\"danger\">Disconnect</button></form></div>";
        }
        None => {
            b += &format!(
                "<p class=\"muted\">Not connected: the Drive keeps files on this server only, up to the space above.</p>\
                 <h3>With Réplique</h3>\
                 <p class=\"muted\">On Réplique's website, add a device of kind <em>partage</em>, then paste its pairing code here. \
                 Réplique creates the Storage Box account, and the PC can then back up the Drive.</p>\
                 <form method=\"post\" action=\"/admin/drive/pair\" class=\"row\">\
                 <label style=\"flex:1\">Réplique address<input type=\"url\" name=\"url\" required placeholder=\"https://replique.example.com\"></label>\
                 <label>Pairing code<input name=\"code\" required placeholder=\"XXXX-XXXX\" autocomplete=\"off\"></label>\
                 <button>Pair</button></form>\
                 <h3>By hand</h3>\
                 <p class=\"muted\">Any SFTP account works. Install this key on it first:</p>\
                 <div class=\"link\"><input readonly value=\"{pk}\">{}</div>\
                 <form method=\"post\" action=\"/admin/drive/box\" class=\"row\" style=\"margin-top:8px\">\
                 <label style=\"flex:1\">Host<input name=\"host\" required placeholder=\"u123456.your-storagebox.de\"></label>\
                 <label>Port<input type=\"number\" name=\"port\" value=\"23\" required></label>\
                 <label>User<input name=\"user\" required placeholder=\"u123456-sub1\"></label>\
                 <label>Folder<input name=\"path\" required value=\"/home/partage\"></label>\
                 <button class=\"plain\">Connect</button></form>",
                copy_link(d.public_key),
                pk = esc(d.public_key),
            );
        }
    }

    b += "<h2 style=\"margin-top:18px\">Drive key</h2>";
    match d.key {
        Some(k) => {
            b += &format!(
                "<p>Everything the Drive sends to the Storage Box is encrypted with this key. <strong>Without it, \
                 neither the box nor the PC's copy can be read</strong>, even by you: keep it in your password manager.</p>\
                 <div class=\"link\"><input readonly value=\"{}\" class=\"mono\">{}</div>",
                esc(k),
                copy_link(k)
            );
            if !d.key_saved {
                b += "<form method=\"post\" action=\"/admin/drive/key-saved\" style=\"margin-top:8px\">\
                      <button>I saved it somewhere else</button></form>";
            }
        }
        None => {
            b += "<p class=\"muted\">Saved in your password manager. \
                  <a href=\"/settings?show_key=1#drive\">Show it again</a></p>";
        }
    }
    b + "</section>"
}

pub fn track_links(base: &str, links: &[(Link, String)]) -> String {
    let mut b = format!("<section><h2>Drive links ({})</h2>", links.len());
    if links.is_empty() {
        b += "<p class=\"muted\">None yet: share a file or folder from its details in the <a href=\"/drive\">Drive</a>.</p>";
    } else {
        b += "<table><tr><th>Link</th><th>To</th><th>Can</th><th>Note</th><th>Expires</th><th>Uses</th><th></th></tr>";
        for (l, path) in links {
            let row = link_row(base, l, "/track");
            // Insert the item's path after the link cell.
            let (first, rest) = row.split_once("</td>").unwrap_or((&row, ""));
            b += &format!(
                "{first}</td><td class=\"name\"><a href=\"/drive/item/{}\">{}</a></td>{rest}",
                esc(&l.node),
                esc(path)
            );
        }
        b += "</table>";
    }
    b + "</section>"
}
