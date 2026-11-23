//! The Drive's routes: the admin's pages and actions, what share links can
//! do, and the Drive's settings.

use crate::{
    App, alert,
    drive::{self, DriveError, Upload, check_name},
    drive_db::{Link, Mode, Node, ROOT},
    drive_html, now,
    remote::BoxConfig,
    replique,
    web::{Admin, base_url},
};
use axum::{
    Form, Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post, put},
};
use serde::Deserialize;
use std::sync::Arc;

type AppState = Arc<App>;

pub fn router() -> Router<AppState> {
    let uploads = Router::new()
        .route("/drive/upload", put(admin_upload))
        .route("/d/{token}/upload", put(link_upload))
        .layer(DefaultBodyLimit::disable());
    Router::new()
        .route("/drive", get(folder_page))
        .route("/drive/item/{id}", get(item_page))
        .route("/drive/trash", get(trash_page))
        .route("/drive/file/{id}", get(admin_file))
        .route("/drive/mkdir", post(admin_mkdir))
        .route("/drive/rename", post(rename))
        .route("/drive/move", post(move_node))
        .route("/drive/delete", post(admin_delete))
        .route("/drive/restore", post(restore))
        .route("/drive/purge", post(purge))
        .route("/drive/revert", post(revert))
        .route("/drive/share", post(share))
        .route("/drive/limit", post(set_limit))
        .route("/drive/links/{token}/revoke", post(revoke))
        .route("/d/{token}", get(link_page))
        .route("/d/{token}/file/{id}", get(link_file))
        .route("/d/{token}/mkdir", post(link_mkdir))
        .route("/d/{token}/delete", post(link_delete))
        .route("/admin/drive/pair", post(pair))
        .route("/admin/drive/box", post(manual_box))
        .route("/admin/drive/disconnect", post(disconnect))
        .route("/admin/drive/limits", post(limits))
        .route("/admin/drive/key-saved", post(key_saved))
        .route("/admin/drive/sync", post(sync_now))
        .route("/admin/drive/check", post(check_now))
        .merge(uploads)
        .merge(crate::drive_transfer::router())
}

/// Requests from the page's script get plain text; forms get a redirect
/// back with a message.
fn from_script(headers: &HeaderMap) -> bool {
    headers.contains_key("x-fetch")
}

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn done(headers: &HeaderMap, back: &str, msg: &str) -> Response {
    if from_script(headers) {
        return msg.to_string().into_response();
    }
    let sep = if back.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{back}{sep}msg={}", enc(msg))).into_response()
}

fn failed(headers: &HeaderMap, back: &str, err: &str) -> Response {
    if from_script(headers) {
        return (StatusCode::BAD_REQUEST, err.to_string()).into_response();
    }
    let sep = if back.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{back}{sep}err={}", enc(err))).into_response()
}

pub(crate) fn internal(e: anyhow::Error) -> Response {
    tracing::error!("drive: {e:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong on the server.",
    )
        .into_response()
}

#[derive(Deserialize, Default)]
struct PageQuery {
    f: Option<String>,
    msg: Option<String>,
    err: Option<String>,
}

impl PageQuery {
    fn flash(&self) -> (Option<String>, bool) {
        match (&self.err, &self.msg) {
            (Some(e), _) => (Some(e.chars().take(300).collect()), true),
            (None, Some(m)) => (Some(m.chars().take(300).collect()), false),
            _ => (None, false),
        }
    }
}

fn item_href(id: &str) -> String {
    format!("/drive/item/{}", enc(id))
}

fn folder_href(id: &str) -> String {
    if id == ROOT {
        "/drive".into()
    } else {
        format!("/drive?f={}", enc(id))
    }
}

/// A live (not trashed) node.
pub(crate) fn live(app: &App, id: &str) -> anyhow::Result<Option<Node>> {
    Ok(app.db.node(id)?.filter(|n| n.trashed_at.is_none()))
}

pub(crate) fn not_found() -> Response {
    crate::html::message(
        StatusCode::NOT_FOUND,
        "Not found",
        "This file or folder does not exist, or is in the trash.",
    )
}

// Admin pages.

async fn folder_page(
    _: Admin,
    State(app): State<AppState>,
    Query(q): Query<PageQuery>,
) -> Response {
    let id = q.f.clone().unwrap_or_else(|| ROOT.into());
    let node = match live(&app, &id) {
        Ok(Some(n)) if n.dir => n,
        Ok(Some(n)) => return Redirect::to(&item_href(&n.id)).into_response(),
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    let r = (|| {
        let path = app.db.ancestry(&node.id)?;
        let children = app.db.children(&node.id)?;
        let box_used = app.db.box_bytes()?;
        let (_, pending) = app.db.object_counts()?;
        anyhow::Ok((path, children, box_used, pending))
    })();
    let (path, children, box_used, pending) = match r {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let (flash, error) = q.flash();
    Html(drive_html::folder(&drive_html::Folder {
        flash,
        error,
        path: &path,
        children: &children,
        box_used,
        box_limit: app.drive.box_limit(),
        connected: app.drive.remote().is_some(),
        key_saved: key_saved_flag(&app),
        pending,
    }))
    .into_response()
}

fn key_saved_flag(app: &App) -> bool {
    app.db.setting("drive_key_saved").ok().flatten().is_some()
}

async fn item_page(
    _: Admin,
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    let node = match live(&app, &id) {
        Ok(Some(n)) => n,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    let r = (|| {
        let mut path = app.db.ancestry(&node.id)?;
        path.pop();
        let versions = if node.dir {
            Vec::new()
        } else {
            app.db.versions(&node.id)?
        };
        let links = app.db.links(Some(&node.id))?;
        let folders = app.db.all_folders()?;
        let used = if node.dir {
            app.db.tree_size(&node.id)?
        } else {
            0
        };
        anyhow::Ok((path, versions, links, folders, used))
    })();
    let (mut path, versions, links, folders, used) = match r {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    if path.is_empty() {
        // The root's details: its own crumb.
        path.push(node.clone());
    }
    let folders: Vec<(String, String)> = folders
        .into_iter()
        .filter(|(fid, _)| !node.dir || !app.db.is_within(fid, &node.id).unwrap_or(true))
        .collect();
    let base = base_url(&app, &headers);
    let (flash, error) = q.flash();
    Html(drive_html::item(&drive_html::Item {
        base: &base,
        flash,
        error,
        node: &node,
        path: &path,
        versions: &versions,
        links: &links,
        folders: &folders,
        used,
    }))
    .into_response()
}

async fn trash_page(_: Admin, State(app): State<AppState>, Query(q): Query<PageQuery>) -> Response {
    let r = (|| {
        let mut out = Vec::new();
        for n in app.db.trash_items()? {
            let size = app.db.trashed_size(&n.id)?;
            let from = match &n.parent {
                Some(p) => {
                    let path = app.db.ancestry(p)?;
                    let names: Vec<&str> = path.iter().skip(1).map(|n| n.name.as_str()).collect();
                    format!("Drive/{}", names.join("/"))
                }
                None => String::new(),
            };
            out.push(drive_html::TrashItem {
                node: n,
                size,
                from,
            });
        }
        anyhow::Ok(out)
    })();
    match r {
        Ok(items) => {
            let (flash, error) = q.flash();
            Html(drive_html::trash(flash, error, &items)).into_response()
        }
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct FileQuery {
    v: Option<String>,
    inline: Option<String>,
}

async fn admin_file(
    _: Admin,
    State(app): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> Response {
    let node = match app.db.node(&id) {
        Ok(Some(n)) if !n.dir => n,
        Ok(_) => return not_found(),
        Err(e) => return internal(e),
    };
    let version_id = q.v.or(node.version.clone()).unwrap_or_default();
    let version = match app.db.version(&version_id) {
        Ok(Some(v)) if v.node == node.id => v,
        Ok(_) => return not_found(),
        Err(e) => return internal(e),
    };
    drive::serve(
        &app,
        &version,
        &node.name,
        &headers,
        method == Method::HEAD,
        q.inline.is_some(),
    )
    .await
}

// Admin actions.

#[derive(Deserialize)]
struct UploadQuery {
    folder: String,
    path: String,
    size: u64,
}

pub(crate) fn content_type(headers: &HeaderMap) -> String {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .filter(|t| !t.is_empty() && t.len() < 200)
        .unwrap_or("application/octet-stream")
        .to_string()
}

async fn admin_upload(
    _: Admin,
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<UploadQuery>,
    body: Body,
) -> Result<Response, DriveError> {
    let node = drive::receive(
        &app,
        Upload {
            folder: q.folder,
            path: q.path,
            size: q.size,
            content_type: content_type(&headers),
            replace: true,
            by: "admin".into(),
            limited: false,
        },
        body,
    )
    .await?;
    Ok(Json(serde_json::json!({ "id": node.id, "name": node.name })).into_response())
}

#[derive(Deserialize)]
struct MkdirForm {
    folder: String,
    name: String,
}

async fn mkdir(app: &App, headers: &HeaderMap, f: MkdirForm, back: &str) -> Response {
    let name = match check_name(&f.name) {
        Ok(n) => n,
        Err(e) => return failed(headers, back, &e),
    };
    match live(app, &f.folder) {
        Ok(Some(n)) if n.dir => {}
        Ok(_) => return failed(headers, back, "That folder no longer exists."),
        Err(e) => return internal(e),
    }
    match app.db.create_dir(&f.folder, &name, now()) {
        Ok(_) => {
            drive::changed(app);
            done(headers, back, &format!("Folder “{name}” created."))
        }
        Err(e) => failed(headers, back, &e.to_string()),
    }
}

async fn admin_mkdir(
    _: Admin,
    State(app): State<AppState>,
    headers: HeaderMap,
    Form(f): Form<MkdirForm>,
) -> Response {
    let back = folder_href(&f.folder);
    mkdir(&app, &headers, f, &back).await
}

#[derive(Deserialize)]
struct NodeForm {
    node: String,
    name: Option<String>,
    to: Option<String>,
    version: Option<String>,
}

async fn rename(
    _: Admin,
    State(app): State<AppState>,
    headers: HeaderMap,
    Form(f): Form<NodeForm>,
) -> Response {
    let back = item_href(&f.node);
    let name = match check_name(f.name.as_deref().unwrap_or("")) {
        Ok(n) => n,
        Err(e) => return failed(&headers, &back, &e),
    };
    match app.db.rename(&f.node, &name, now()) {
        Ok(()) => {
            drive::changed(&app);
            done(&headers, &back, "Renamed.")
        }
        Err(e) => failed(&headers, &back, &e.to_string()),
    }
}

async fn move_node(
    _: Admin,
    State(app): State<AppState>,
    headers: HeaderMap,
    Form(f): Form<NodeForm>,
) -> Response {
    let back = item_href(&f.node);
    match app
        .db
        .move_node(&f.node, f.to.as_deref().unwrap_or(""), now())
    {
        Ok(()) => {
            drive::changed(&app);
            done(&headers, &back, "Moved.")
        }
        Err(e) => failed(&headers, &back, &e.to_string()),
    }
}

async fn admin_delete(
    _: Admin,
    State(app): State<AppState>,
    headers: HeaderMap,
    Form(f): Form<NodeForm>,
) -> Response {
    let parent = match app.db.node(&f.node) {
        Ok(Some(n)) => n.parent.unwrap_or_else(|| ROOT.into()),
        Ok(None) => return failed(&headers, "/drive", "Already gone."),
        Err(e) => return internal(e),
    };
    let back = folder_href(&parent);
    match app.db.trash(&f.node, "admin", now()) {
        Ok(()) => {
            drive::changed(&app);
            done(&headers, &back, "Moved to the trash.")
        }
        Err(e) => failed(&headers, &back, &e.to_string()),
    }
}

async fn restore(
    _: Admin,
    State(app): State<AppState>,
    headers: HeaderMap,
    Form(f): Form<NodeForm>,
) -> Response {
    match app.db.restore(&f.node) {
        Ok(()) => {
            drive::changed(&app);
            done(&headers, "/drive/trash", "Restored.")
        }
        Err(e) => failed(&headers, "/drive/trash", &e.to_string()),
    }
}

async fn purge(
    _: Admin,
    State(app): State<AppState>,
    headers: HeaderMap,
    Form(f): Form<NodeForm>,
) -> Response {
    match app.db.purge(&f.node) {
        Ok(()) => {
            drive::changed(&app);
            done(&headers, "/drive/trash", "Deleted for good.")
        }
        Err(e) => failed(&headers, "/drive/trash", &e.to_string()),
    }
}

async fn revert(
    _: Admin,
    State(app): State<AppState>,
    headers: HeaderMap,
    Form(f): Form<NodeForm>,
) -> Response {
    let back = item_href(&f.node);
    match app
        .db
        .revert(&f.node, f.version.as_deref().unwrap_or(""), now())
    {
        Ok(()) => {
            drive::changed(&app);
            done(&headers, &back, "That version is current again.")
        }
        Err(e) => failed(&headers, &back, &e.to_string()),
    }
}

/// An amount and a unit from a form, in bytes.
fn bytes(amount: &str, unit: &str) -> Option<u64> {
    let amount: f64 = amount.trim().parse().ok()?;
    let scale = match unit {
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => return None,
    };
    let n = amount * scale;
    (n.is_finite() && n >= 1e6).then_some(n as u64)
}

#[derive(Deserialize)]
struct ShareForm {
    node: String,
    mode: String,
    note: Option<String>,
    expires: Option<String>,
    limit_amount: Option<String>,
    limit_unit: Option<String>,
}

async fn share(
    _: Admin,
    State(app): State<AppState>,
    headers: HeaderMap,
    Form(f): Form<ShareForm>,
) -> Response {
    let back = item_href(&f.node);
    let node = match live(&app, &f.node) {
        Ok(Some(n)) => n,
        Ok(None) => return failed(&headers, "/drive", "That item no longer exists."),
        Err(e) => return internal(e),
    };
    let Some(mode) = Mode::parse(&f.mode) else {
        return failed(&headers, &back, "Unknown kind of link.");
    };
    if node.dir == (mode == Mode::File) {
        return failed(&headers, &back, "That kind of link does not fit this item.");
    }
    if mode.can_add() {
        let amount = f.limit_amount.as_deref().unwrap_or("");
        match bytes(amount, f.limit_unit.as_deref().unwrap_or("GB")) {
            Some(limit) => {
                if let Err(e) = app.db.set_limit(&node.id, Some(limit)) {
                    return internal(e);
                }
            }
            None if amount.trim().is_empty() && node.limit_bytes.is_some() => {}
            None => {
                return failed(
                    &headers,
                    &back,
                    "A link that can add files needs a size limit for the folder (1 MB or more).",
                );
            }
        }
    }
    let expires_at = match f.expires.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(s) => match s.parse::<i64>() {
            Ok(secs) if secs > 0 => Some(now() + secs),
            _ => return failed(&headers, &back, "That expiry is not valid."),
        },
    };
    let link = Link {
        token: crate::token(24),
        node: node.id.clone(),
        mode,
        note: f
            .note
            .unwrap_or_default()
            .trim()
            .chars()
            .take(200)
            .collect(),
        created_at: now(),
        expires_at,
        revoked: false,
        uses: 0,
    };
    if let Err(e) = app.db.insert_link(&link) {
        return internal(e);
    }
    done(&headers, &back, "Link created: copy it from the list.")
}

#[derive(Deserialize)]
struct LimitForm {
    node: String,
    amount: String,
    unit: String,
}

async fn set_limit(
    _: Admin,
    State(app): State<AppState>,
    headers: HeaderMap,
    Form(f): Form<LimitForm>,
) -> Response {
    let back = item_href(&f.node);
    let limit = if f.amount.trim().is_empty() || f.amount.trim() == "0" {
        None
    } else {
        match bytes(&f.amount, &f.unit) {
            Some(l) => Some(l),
            None => return failed(&headers, &back, "That size is not valid (1 MB or more)."),
        }
    };
    match app.db.set_limit(&f.node, limit) {
        Ok(()) => done(
            &headers,
            &back,
            if limit.is_some() {
                "Size limit saved."
            } else {
                "Size limit removed."
            },
        ),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct BackForm {
    back: Option<String>,
}

async fn revoke(
    _: Admin,
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Form(f): Form<BackForm>,
) -> Response {
    let back = f
        .back
        .filter(|b| b.starts_with('/') && !b.starts_with("//"))
        .unwrap_or_else(|| "/track".into());
    match app.db.revoke_link(&token) {
        Ok(()) => done(&headers, &back, "Link revoked."),
        Err(e) => internal(e),
    }
}

// Share links.

pub(crate) fn link_gone() -> Response {
    crate::html::message(
        StatusCode::GONE,
        "Link not usable",
        "This link has expired or was revoked, or what it shared was deleted.",
    )
}

/// The link and the item it shares, if it can still be used.
#[allow(clippy::result_large_err)]
pub(crate) fn resolve(app: &App, token: &str) -> Result<(Link, Node), Response> {
    let link = match app.db.link(token) {
        Ok(Some(l)) if l.usable(now()) => l,
        Ok(_) => return Err(link_gone()),
        Err(e) => return Err(internal(e)),
    };
    match live(app, &link.node) {
        Ok(Some(n)) => Ok((link, n)),
        Ok(None) => Err(link_gone()),
        Err(e) => Err(internal(e)),
    }
}

/// A folder the link reaches: its own, or one under it.
#[allow(clippy::result_large_err)]
pub(crate) fn reachable(app: &App, root: &Node, id: Option<&str>) -> Result<Node, Response> {
    let Some(id) = id.filter(|i| *i != root.id) else {
        return Ok(root.clone());
    };
    match live(app, id) {
        Ok(Some(n)) if app.db.is_within(&n.id, &root.id).unwrap_or(false) => {
            // Nothing on the way may be in the trash.
            match app.db.ancestry(&n.id) {
                Ok(path) if path.iter().all(|p| p.trashed_at.is_none()) => Ok(n),
                Ok(_) => Err(not_found()),
                Err(e) => Err(internal(e)),
            }
        }
        Ok(_) => Err(not_found()),
        Err(e) => Err(internal(e)),
    }
}

/// Room left under the tightest limit between the link's folder and the
/// root, and that limit.
pub(crate) fn room(app: &App, folder: &Node) -> anyhow::Result<Option<(u64, u64)>> {
    let mut best: Option<(u64, u64)> = None;
    for a in app.db.ancestry(&folder.id)? {
        if let Some(limit) = a.limit_bytes {
            let left = limit.saturating_sub(app.db.tree_size(&a.id)?);
            if best.is_none_or(|(l, _)| left < l) {
                best = Some((left, limit));
            }
        }
    }
    Ok(best)
}

async fn link_page(
    State(app): State<AppState>,
    Path(token): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    let (link, root) = match resolve(&app, &token) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if link.mode == Mode::File {
        if root.dir {
            return link_gone();
        }
        let viewable = root.content_type.as_deref().is_some_and(|t| {
            t.starts_with("image/")
                || t.starts_with("video/")
                || t.starts_with("audio/")
                || t == "application/pdf"
        });
        return Html(drive_html::shared_file(&token, &root, viewable)).into_response();
    }
    let here = if link.mode.can_read() {
        match reachable(&app, &root, q.f.as_deref()) {
            Ok(n) if n.dir => n,
            Ok(_) => return not_found(),
            Err(r) => return r,
        }
    } else {
        root.clone()
    };
    let r = (|| {
        let full = app.db.ancestry(&here.id)?;
        let start = full.iter().position(|n| n.id == root.id).unwrap_or(0);
        let path = full[start..].to_vec();
        let children = if link.mode.can_read() {
            app.db.children(&here.id)?
        } else {
            Vec::new()
        };
        let room = if link.mode.can_add() {
            room(&app, &here)?
        } else {
            None
        };
        anyhow::Ok((path, children, room))
    })();
    let (path, children, room) = match r {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let (flash, _) = q.flash();
    Html(drive_html::shared_folder(&drive_html::Shared {
        token: &token,
        link: &link,
        path: &path,
        children: &children,
        room,
        flash,
    }))
    .into_response()
}

async fn link_file(
    State(app): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path((token, id)): Path<(String, String)>,
    Query(q): Query<FileQuery>,
) -> Response {
    let (link, root) = match resolve(&app, &token) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !link.mode.can_read() {
        return not_found();
    }
    let node = if link.mode == Mode::File {
        if id != root.id {
            return not_found();
        }
        root
    } else {
        match reachable(&app, &root, Some(&id)) {
            Ok(n) if !n.dir => n,
            Ok(_) => return not_found(),
            Err(r) => return r,
        }
    };
    let version = match node.version.as_deref().map(|v| app.db.version(v)) {
        Some(Ok(Some(v))) => v,
        Some(Err(e)) => return internal(e),
        _ => return not_found(),
    };
    let head = method == Method::HEAD;
    if !head && headers.get(header::RANGE).is_none() {
        app.db.count_link_use(&token);
    }
    drive::serve(
        &app,
        &version,
        &node.name,
        &headers,
        head,
        q.inline.is_some(),
    )
    .await
}

pub(crate) fn link_actor(link: &Link) -> String {
    let who = if link.note.is_empty() {
        &link.token[..8.min(link.token.len())]
    } else {
        &link.note
    };
    format!("link “{who}”")
}

async fn link_upload(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Query(q): Query<UploadQuery>,
    body: Body,
) -> Result<Response, Response> {
    let (link, root) = resolve(&app, &token)?;
    if !link.mode.can_add() || !root.dir {
        return Err((StatusCode::FORBIDDEN, "This link cannot add files.").into_response());
    }
    if root.limit_bytes.is_none() && room(&app, &root).map_err(internal)?.is_none() {
        return Err((
            StatusCode::FORBIDDEN,
            "This folder has no size limit any more, so nothing can be added through links. Ask the owner.",
        )
            .into_response());
    }
    let folder = reachable(&app, &root, Some(&q.folder))?;
    if !folder.dir {
        return Err(not_found());
    }
    let node = drive::receive(
        &app,
        Upload {
            folder: folder.id,
            path: q.path,
            size: q.size,
            content_type: content_type(&headers),
            replace: false,
            by: link_actor(&link),
            limited: true,
        },
        body,
    )
    .await
    .map_err(IntoResponse::into_response)?;
    app.db.count_link_use(&token);
    Ok(Json(serde_json::json!({ "name": node.name })).into_response())
}

async fn link_mkdir(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Form(f): Form<MkdirForm>,
) -> Response {
    let (link, root) = match resolve(&app, &token) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !link.mode.can_add() || !root.dir {
        return (StatusCode::FORBIDDEN, "This link cannot add folders.").into_response();
    }
    if let Err(r) = reachable(&app, &root, Some(&f.folder)) {
        return r;
    }
    let back = format!("/d/{}?f={}", enc(&token), enc(&f.folder));
    mkdir(&app, &headers, f, &back).await
}

async fn link_delete(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Form(f): Form<NodeForm>,
) -> Response {
    let (link, root) = match resolve(&app, &token) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !link.mode.can_delete() {
        return (StatusCode::FORBIDDEN, "This link cannot delete.").into_response();
    }
    if f.node == root.id {
        return (
            StatusCode::FORBIDDEN,
            "The shared folder itself cannot be deleted.",
        )
            .into_response();
    }
    let node = match reachable(&app, &root, Some(&f.node)) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let back = format!(
        "/d/{}?f={}",
        enc(&token),
        enc(node.parent.as_deref().unwrap_or(""))
    );
    match app.db.trash(&node.id, &link_actor(&link), now()) {
        Ok(()) => {
            drive::changed(&app);
            done(&headers, &back, "Deleted.")
        }
        Err(e) => failed(&headers, &back, &e.to_string()),
    }
}

// Settings.

pub fn settings_part(app: &App, show_key: bool) -> String {
    let remote = app.drive.remote();
    let (local_used, _) = app.db.local_bytes().unwrap_or((0, 0));
    let (_, pending) = app.db.object_counts().unwrap_or((0, 0));
    let key_saved = key_saved_flag(app);
    let key = app.drive.key.to_text();
    let status = app.drive.status.lock().unwrap();
    drive_html::settings_part(&drive_html::SettingsPart {
        connected: remote.as_ref().map(|r| r.cfg.label()),
        via_replique: replique::server(app).map(|s| s.url),
        public_key: &crate::remote::public_line(&app.drive.ssh_key),
        box_used: app.db.box_bytes().unwrap_or(0),
        box_limit: app.drive.box_limit(),
        local_used,
        local_limit: app.drive.local_limit(),
        pending,
        key: (!key_saved || show_key).then_some(key.as_str()),
        key_saved,
        last_error: status.last_error.clone(),
        last_check: status.last_check.clone(),
        last_manifest: status.last_manifest,
        last_run: *app.drive.runs.last.lock().unwrap(),
    })
}

fn settings_ok(msg: &str) -> Response {
    Redirect::to(&format!("/settings?msg={}#drive", enc(msg))).into_response()
}

fn settings_err(msg: &str) -> Response {
    Redirect::to(&format!("/settings?err={}#drive", enc(msg))).into_response()
}

#[derive(Deserialize)]
struct PairForm {
    url: String,
    code: String,
}

/// Tries a configuration and keeps it if partage can write there.
pub(crate) async fn connect(app: &Arc<App>, cfg: BoxConfig) -> Result<(), String> {
    let remote = crate::remote::Remote::new(cfg.clone(), app.drive.ssh_key.clone());
    if let Err(e) = remote.prepare().await {
        return Err(format!("{e:#}"));
    }
    let mut cfg = cfg;
    if cfg.host_key.is_empty()
        && let Some(k) = remote.seen_host_key.lock().unwrap().clone()
    {
        cfg.host_key = k;
    }
    drive::set_box(app, Some(cfg)).map_err(|e| format!("{e:#}"))?;
    drive::changed(app);
    Ok(())
}

async fn pair(_: Admin, State(app): State<AppState>, Form(f): Form<PairForm>) -> Response {
    let cfg = match replique::pair(&app, &f.url, &f.code).await {
        Ok(c) => c,
        Err(e) => return settings_err(&format!("Pairing failed: {e:#}")),
    };
    if let Err(e) = connect(&app, cfg).await {
        return settings_err(&format!(
            "Paired with Réplique, but the Storage Box refused partage: {e}"
        ));
    }
    replique::declare_job(&app).await;
    tracing::info!("drive: paired with Réplique at {}", f.url.trim());
    settings_ok("Paired: the Drive now copies to the Storage Box.")
}

#[derive(Deserialize)]
struct BoxForm {
    host: String,
    port: u16,
    user: String,
    path: String,
}

async fn manual_box(_: Admin, State(app): State<AppState>, Form(f): Form<BoxForm>) -> Response {
    let path = format!("/{}", f.path.trim().trim_matches('/'));
    let cfg = BoxConfig {
        host: f.host.trim().to_string(),
        port: f.port,
        user: f.user.trim().to_string(),
        path,
        host_key: String::new(),
    };
    match connect(&app, cfg).await {
        Ok(()) => settings_ok("Connected: the Drive now copies to the Storage Box."),
        Err(e) => settings_err(&format!("Could not use that account: {e}")),
    }
}

async fn disconnect(_: Admin, State(app): State<AppState>) -> Response {
    if let Err(e) = drive::set_box(&app, None) {
        return internal(e);
    }
    replique::forget(&app);
    settings_ok("Disconnected from the Storage Box.")
}

#[derive(Deserialize)]
struct LimitsForm {
    box_amount: String,
    box_unit: String,
    local_amount: String,
    local_unit: String,
}

async fn limits(_: Admin, State(app): State<AppState>, Form(f): Form<LimitsForm>) -> Response {
    let Some(box_limit) = bytes(&f.box_amount, &f.box_unit) else {
        return settings_err("That Drive limit is not valid.");
    };
    let local = if f.local_amount.trim() == "0" {
        Some(0)
    } else {
        bytes(&f.local_amount, &f.local_unit)
    };
    let Some(local) = local else {
        return settings_err("That size for this server is not valid.");
    };
    let saved = app
        .db
        .set_setting("drive_box_limit", &box_limit.to_string())
        .and_then(|_| app.db.set_setting("drive_local_limit", &local.to_string()));
    if let Err(e) = saved {
        return internal(e);
    }
    app.drive
        .box_limit
        .store(box_limit, std::sync::atomic::Ordering::Relaxed);
    app.drive
        .local_limit
        .store(local, std::sync::atomic::Ordering::Relaxed);
    app.drive.wake.notify_one();
    alert::check_drive_usage(&app).await;
    settings_ok("Drive limits saved.")
}

async fn key_saved(_: Admin, State(app): State<AppState>) -> Response {
    if let Err(e) = app.db.set_setting("drive_key_saved", "1") {
        return internal(e);
    }
    settings_ok("Noted: the key is no longer shown.")
}

async fn sync_now(_: Admin, State(app): State<AppState>) -> Response {
    drive::changed(&app);
    settings_ok("Syncing: reload in a moment to see how it went.")
}

async fn check_now(_: Admin, State(app): State<AppState>) -> Response {
    let Some(remote) = app.drive.remote() else {
        return settings_err("No Storage Box connected.");
    };
    crate::sync::check(&app, &remote).await;
    settings_ok("Check finished.")
}
