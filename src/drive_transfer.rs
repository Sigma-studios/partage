//! Resumable uploads and folder zips, for the admin and share links.
//!
//! An upload is started with its folder, path and size (the space is held from
//! then on), then sent in pieces: `PUT …/uploads/{id}?offset=N`. A piece cut
//! off still counts what arrived; the page asks how much did
//! (`GET …/uploads/{id}`) and goes on from there. The last byte finishes it.
//! An upload nobody continues for a day is dropped, and so is any upload when
//! partage restarts: the page then starts that file again.

use crate::{
    App,
    drive::{self, DriveError, Session, Upload},
    drive_db::{Mode, ROOT},
    drive_web::{internal, link_actor, link_gone, live, not_found, reachable, resolve, room},
    web::Admin,
    zip,
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

type AppState = Arc<App>;

pub fn router() -> Router<AppState> {
    let pieces = Router::new()
        .route(
            "/drive/uploads/{id}",
            get(admin_status).put(admin_piece).delete(admin_cancel),
        )
        .route(
            "/d/{token}/uploads/{id}",
            get(link_status).put(link_piece).delete(link_cancel),
        )
        .layer(DefaultBodyLimit::disable());
    Router::new()
        .route("/drive/uploads", post(admin_start))
        .route("/d/{token}/uploads", post(link_start))
        .route("/drive/zip/{id}", get(admin_zip))
        .route("/d/{token}/zip", get(link_zip))
        .merge(pieces)
}

#[derive(Deserialize)]
struct Start {
    folder: String,
    path: String,
    size: u64,
    #[serde(rename = "type", default)]
    content_type: String,
}

fn err_json(status: StatusCode, msg: &str, received: Option<u64>) -> Response {
    (status, Json(json!({ "error": msg, "received": received }))).into_response()
}

fn drive_err(e: DriveError, received: Option<u64>) -> Response {
    let (status, msg) = e.parts();
    err_json(status, &msg, received)
}

async fn start(app: &Arc<App>, up: Upload, owner: String) -> Response {
    let size = up.size;
    match Session::start(app, up, owner).await {
        Ok(s) => {
            let id = drive::keep(app, s);
            Json(json!({ "id": id, "received": 0, "size": size })).into_response()
        }
        Err(e) => drive_err(e, None),
    }
}

fn status(app: &App, id: &str, owner: &str) -> Response {
    let Some(shared) = drive::session(app, id) else {
        return err_json(
            StatusCode::NOT_FOUND,
            "This upload is gone: start it again.",
            None,
        );
    };
    let Ok(s) = shared.try_lock() else {
        return err_json(
            StatusCode::CONFLICT,
            "A piece of this upload is still arriving.",
            None,
        );
    };
    if s.owner != owner {
        return err_json(
            StatusCode::NOT_FOUND,
            "This upload is gone: start it again.",
            None,
        );
    }
    Json(json!({ "received": s.received, "size": s.size() })).into_response()
}

#[derive(Deserialize)]
struct Offset {
    offset: u64,
}

async fn piece(app: &Arc<App>, id: &str, owner: &str, offset: u64, body: Body) -> Response {
    let Some(shared) = drive::session(app, id) else {
        return err_json(
            StatusCode::NOT_FOUND,
            "This upload is gone: start it again.",
            None,
        );
    };
    let complete = {
        let Ok(mut s) = shared.try_lock() else {
            return err_json(
                StatusCode::CONFLICT,
                "A piece of this upload is still arriving.",
                None,
            );
        };
        if s.owner != owner {
            return err_json(
                StatusCode::NOT_FOUND,
                "This upload is gone: start it again.",
                None,
            );
        }
        if offset != s.received {
            return err_json(
                StatusCode::CONFLICT,
                "Continue from what arrived.",
                Some(s.received),
            );
        }
        let result = s.push_body(body).await;
        let received = s.received;
        if let Err(e) = result {
            let gone = matches!(e, DriveError::Gone(_) | DriveError::Internal(_));
            let bigger = matches!(&e, DriveError::Bad(m) if m.contains("bigger"));
            drop(s);
            if (gone || bigger)
                && let Some(sh) = drive::forget(app, id)
            {
                abort(sh).await;
            }
            return drive_err(e, Some(received));
        }
        if !s.complete() {
            return Json(json!({ "received": received })).into_response();
        }
        received
    };
    drop(shared);
    let Some(sh) = drive::forget(app, id) else {
        return err_json(
            StatusCode::NOT_FOUND,
            "This upload is gone: start it again.",
            None,
        );
    };
    let session = match unwrap(sh).await {
        Some(s) => s,
        None => return err_json(StatusCode::CONFLICT, "This upload is busy.", Some(complete)),
    };
    match session.finish().await {
        Ok(node) => Json(json!({
            "received": complete,
            "done": true,
            "id": node.id,
            "name": node.name,
        }))
        .into_response(),
        Err(e) => drive_err(e, None),
    }
}

/// The session out of its shared handle, once nobody else holds it.
async fn unwrap(mut sh: drive::Shared) -> Option<Session> {
    for _ in 0..50 {
        match Arc::try_unwrap(sh) {
            Ok(m) => return Some(m.into_inner()),
            Err(back) => {
                sh = back;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    }
    None
}

async fn abort(sh: drive::Shared) {
    if let Some(s) = unwrap(sh).await {
        s.abort().await;
    }
}

async fn cancel(app: &App, id: &str, owner: &str) -> Response {
    let mine =
        drive::session(app, id).is_some_and(|s| s.try_lock().is_ok_and(|s| s.owner == owner));
    if mine && let Some(sh) = drive::forget(app, id) {
        abort(sh).await;
    }
    Json(json!({ "ok": true })).into_response()
}

// The admin.

async fn admin_start(_: Admin, State(app): State<AppState>, Json(s): Json<Start>) -> Response {
    let up = Upload {
        folder: s.folder,
        path: s.path,
        size: s.size,
        content_type: clean_type(s.content_type),
        replace: true,
        by: "admin".into(),
        limited: false,
    };
    start(&app, up, "admin".into()).await
}

fn clean_type(t: String) -> String {
    if t.is_empty() || t.len() > 200 || t.chars().any(|c| c.is_control()) {
        "application/octet-stream".into()
    } else {
        t
    }
}

async fn admin_status(_: Admin, State(app): State<AppState>, Path(id): Path<String>) -> Response {
    status(&app, &id, "admin")
}

async fn admin_piece(
    _: Admin,
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<Offset>,
    body: Body,
) -> Response {
    piece(&app, &id, "admin", q.offset, body).await
}

async fn admin_cancel(_: Admin, State(app): State<AppState>, Path(id): Path<String>) -> Response {
    cancel(&app, &id, "admin").await
}

// Share links.

fn link_owner(token: &str) -> String {
    format!("link:{token}")
}

/// The link, if it can still add files.
fn adding_link(
    app: &App,
    token: &str,
) -> Result<(crate::drive_db::Link, crate::drive_db::Node), Response> {
    let (link, root) = resolve(app, token)?;
    if !link.mode.can_add() || !root.dir {
        return Err(err_json(
            StatusCode::FORBIDDEN,
            "This link cannot add files.",
            None,
        ));
    }
    Ok((link, root))
}

async fn link_start(
    State(app): State<AppState>,
    Path(token): Path<String>,
    Json(s): Json<Start>,
) -> Response {
    let (link, root) = match adding_link(&app, &token) {
        Ok(v) => v,
        Err(r) => return r,
    };
    match room(&app, &root) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return err_json(
                StatusCode::FORBIDDEN,
                "This folder has no size limit any more, so nothing can be added through links. Ask the owner.",
                None,
            );
        }
        Err(e) => return internal(e),
    }
    let folder = match reachable(&app, &root, Some(&s.folder)) {
        Ok(f) if f.dir => f,
        Ok(_) => return not_found(),
        Err(r) => return r,
    };
    app.db.count_link_use(&token);
    let up = Upload {
        folder: folder.id,
        path: s.path,
        size: s.size,
        content_type: clean_type(s.content_type),
        replace: false,
        by: link_actor(&link),
        limited: true,
    };
    start(&app, up, link_owner(&token)).await
}

async fn link_status(
    State(app): State<AppState>,
    Path((token, id)): Path<(String, String)>,
) -> Response {
    if let Err(r) = adding_link(&app, &token) {
        return r;
    }
    status(&app, &id, &link_owner(&token))
}

async fn link_piece(
    State(app): State<AppState>,
    Path((token, id)): Path<(String, String)>,
    Query(q): Query<Offset>,
    body: Body,
) -> Response {
    // A link revoked or expired meanwhile stops its uploads.
    if let Err(r) = adding_link(&app, &token) {
        if let Some(sh) = drive::forget(&app, &id) {
            abort(sh).await;
        }
        return r;
    }
    piece(&app, &id, &link_owner(&token), q.offset, body).await
}

async fn link_cancel(
    State(app): State<AppState>,
    Path((token, id)): Path<(String, String)>,
) -> Response {
    cancel(&app, &id, &link_owner(&token)).await
}

// Zips.

async fn admin_zip(
    _: Admin,
    State(app): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let folder = match live(&app, &id) {
        Ok(Some(n)) if n.dir => n,
        Ok(_) => return not_found(),
        Err(e) => return internal(e),
    };
    let top = if folder.id == ROOT {
        "Drive".to_string()
    } else {
        folder.name.clone()
    };
    match zip::entries(&app, &folder, &top).await {
        Ok(entries) => zip::serve(&app, &top, entries, &headers, method == Method::HEAD).await,
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct ZipQuery {
    f: Option<String>,
}

async fn link_zip(
    State(app): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(token): Path<String>,
    Query(q): Query<ZipQuery>,
) -> Response {
    let (link, root) = match resolve(&app, &token) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !link.mode.can_read() || link.mode == Mode::File || !root.dir {
        return link_gone();
    }
    let folder = match reachable(&app, &root, q.f.as_deref()) {
        Ok(n) if n.dir => n,
        Ok(_) => return not_found(),
        Err(r) => return r,
    };
    let head = method == Method::HEAD;
    if !head && headers.get(axum::http::header::RANGE).is_none() {
        app.db.count_link_use(&token);
    }
    let top = folder.name.clone();
    match zip::entries(&app, &folder, &top).await {
        Ok(entries) => zip::serve(&app, &top, entries, &headers, head).await,
        Err(e) => internal(e),
    }
}
