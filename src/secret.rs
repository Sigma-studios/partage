//! One-time secrets: files or texts encrypted in the browser (the key stays
//! in the link's `#fragment`, never sent here), uploaded in pieces, and
//! handed out once. Each has a receipt page with its timeline.
//!
//! An upload is created with its final size, which is reserved against the
//! storage limit, then sent in pieces at increasing offsets, then finished.

use crate::{App, alert, db, html, now, store, token, web::base_url};
use axum::{
    Json,
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex, atomic::Ordering::Relaxed},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

/// Uploads left unfinished this long are dropped.
const IDLE: Duration = Duration::from_secs(15 * 60);

/// Largest text secret, encrypted.
const MAX_TEXT: u64 = 2 << 20;

/// Encryption overhead, as written by the browser: a header of at most
/// this many bytes, and a 16-byte tag per mebibyte record.
const MAX_HEADER: u64 = 4 + 4096;
const RECORD: u64 = 1 << 20;
const TAG: u64 = 16;

/// The most an encrypted upload of a file of up to `max` bytes can weigh.
fn encrypted_max(max: u64) -> u64 {
    let plain = max.saturating_add(MAX_HEADER);
    plain.saturating_add(plain.div_ceil(RECORD) * TAG)
}

#[derive(Default)]
pub struct Uploads(Mutex<HashMap<String, Arc<tokio::sync::Mutex<Pending>>>>);

struct Pending {
    tmp: PathBuf,
    file: tokio::fs::File,
    /// Declared size, reserved in `App::inflight`.
    size: u64,
    written: u64,
    kind: &'static str,
    key_check: String,
    expires_at: Option<i64>,
    invite: Option<db::Invite>,
    from: Client,
    touched: Instant,
    /// Finished or aborted: the reservation is released.
    closed: bool,
}

/// Who did something, for the timeline.
#[derive(Clone)]
pub struct Client {
    pub ip: String,
    pub agent: String,
}

impl Client {
    pub fn from(headers: &HeaderMap) -> Self {
        let agent = headers
            .get(header::USER_AGENT)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .chars()
            .take(300)
            .collect();
        Self {
            ip: crate::web::client_ip(headers),
            agent,
        }
    }
}

fn log(app: &App, id: &str, kind: &str, who: Option<&Client>, detail: &str) {
    let (ip, agent) = who.map_or(("", ""), |c| (c.ip.as_str(), c.agent.as_str()));
    app.db.add_event(id, kind, ip, agent, detail);
}

fn fail(status: StatusCode, msg: &str) -> Response {
    (status, msg.to_string()).into_response()
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!("secret: {e:#}");
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong on the server.",
    )
}

#[derive(Deserialize)]
pub struct CreateReq {
    kind: String,
    /// Encrypted size.
    size: u64,
    check: String,
    /// Seconds; only for the admin, upload links set their own.
    expires_in: Option<i64>,
}

/// Starts an upload; for upload links, the link is already claimed and
/// released again if this fails.
pub async fn create(
    app: &Arc<App>,
    req: CreateReq,
    invite: Option<db::Invite>,
    from: Client,
) -> Response {
    let kind = match req.kind.as_str() {
        "file" => "file",
        "text" => "text",
        _ => return fail(StatusCode::BAD_REQUEST, "Unknown kind of secret."),
    };
    let check_ok = req.check.len() == 22
        && req
            .check
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !check_ok || req.size <= TAG {
        return fail(StatusCode::BAD_REQUEST, "Invalid upload.");
    }
    let max = match (&invite, kind) {
        (_, "text") => MAX_TEXT,
        (Some(i), _) => encrypted_max(i.max_bytes),
        (None, _) => u64::MAX,
    };
    if req.size > max {
        let shown = invite.as_ref().map_or(max, |i| i.max_bytes);
        return fail(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!(
                "This is larger than the {} allowed.",
                html::fmt_bytes(shown)
            ),
        );
    }
    let expires_at = match &invite {
        Some(i) => i.file_ttl.map(|ttl| now().saturating_add(ttl)),
        None => match req.expires_in {
            Some(s) if s > 0 => Some(now().saturating_add(s)),
            Some(_) => return fail(StatusCode::BAD_REQUEST, "Invalid expiry."),
            None => None,
        },
    };
    app.inflight.fetch_add(req.size, Relaxed);
    if app.used() + app.inflight.load(Relaxed) > app.limit() {
        app.inflight.fetch_sub(req.size, Relaxed);
        alert::upload_blocked(app.clone());
        return fail(
            StatusCode::INSUFFICIENT_STORAGE,
            "Storage is full: this upload would go over the limit.",
        );
    }
    let tmp = store::tmp_dir(app).join(token(16));
    let file = match tokio::fs::File::create(&tmp).await {
        Ok(f) => f,
        Err(e) => {
            app.inflight.fetch_sub(req.size, Relaxed);
            return internal(e);
        }
    };
    let upload = token(24);
    let pending = Pending {
        tmp,
        file,
        size: req.size,
        written: 0,
        kind,
        key_check: req.check,
        expires_at,
        invite,
        from,
        touched: Instant::now(),
        closed: false,
    };
    app.uploads
        .0
        .lock()
        .unwrap()
        .insert(upload.clone(), Arc::new(tokio::sync::Mutex::new(pending)));
    Json(serde_json::json!({ "upload": upload })).into_response()
}

/// Releases what an upload holds. Callers hold its lock.
fn close(app: &App, p: &mut Pending) {
    if std::mem::replace(&mut p.closed, true) {
        return;
    }
    app.inflight.fetch_sub(p.size, Relaxed);
    let _ = std::fs::remove_file(&p.tmp);
    if let Some(i) = &p.invite
        && let Err(e) = app.db.release_invite(&i.token)
    {
        tracing::error!("releasing upload link: {e:#}");
    }
}

fn pending(app: &App, upload: &str) -> Option<Arc<tokio::sync::Mutex<Pending>>> {
    app.uploads.0.lock().unwrap().get(upload).cloned()
}

fn gone_upload() -> Response {
    fail(
        StatusCode::NOT_FOUND,
        "This upload was interrupted for too long or cancelled: start again.",
    )
}

pub async fn abort(app: &App, upload: &str) -> Response {
    let Some(p) = app.uploads.0.lock().unwrap().remove(upload) else {
        return StatusCode::NO_CONTENT.into_response();
    };
    close(app, &mut *p.lock().await);
    StatusCode::NO_CONTENT.into_response()
}

/// Drops uploads nobody has sent to in a while.
pub async fn sweep(app: &App) {
    let all: Vec<_> = app.uploads.0.lock().unwrap().clone().into_iter().collect();
    for (upload, p) in all {
        let Ok(mut p) = p.try_lock() else { continue };
        if p.touched.elapsed() > IDLE {
            tracing::info!("dropping abandoned secret upload");
            close(app, &mut p);
            app.uploads.0.lock().unwrap().remove(&upload);
        }
    }
}

/// Appends a piece; `offset` must be what was received so far. A piece
/// already received (its response was lost) is acknowledged again.
pub async fn piece(
    app: &App,
    upload: &str,
    offset: u64,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let Some(p) = pending(app, upload) else {
        return gone_upload();
    };
    let Ok(mut p) = p.try_lock() else {
        return fail(StatusCode::CONFLICT, "This upload is busy.");
    };
    if p.closed {
        return gone_upload();
    }
    p.touched = Instant::now();
    let received = |n: u64| Json(serde_json::json!({ "received": n })).into_response();
    if offset != p.written {
        let len: Option<u64> = headers
            .get(header::CONTENT_LENGTH)
            .and_then(|h| h.to_str().ok()?.parse().ok());
        if len.is_some_and(|l| offset.checked_add(l) == Some(p.written)) {
            return received(p.written);
        }
        return (StatusCode::CONFLICT, received(p.written)).into_response();
    }
    let mut stream = body.into_data_stream();
    let mut res = Ok(());
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                res = Err(fail(
                    StatusCode::BAD_REQUEST,
                    &format!("upload interrupted: {e}"),
                ));
                break;
            }
        };
        if p.written + chunk.len() as u64 > p.size {
            close(app, &mut p);
            app.uploads.0.lock().unwrap().remove(upload);
            return fail(
                StatusCode::BAD_REQUEST,
                "The upload is larger than announced.",
            );
        }
        if let Err(e) = p.file.write_all(&chunk).await {
            res = Err(internal(e));
            break;
        }
        p.written += chunk.len() as u64;
    }
    if let Err(res) = res {
        // Back to the start of the piece, so it can be sent again.
        p.written = offset;
        if let Err(e) = p.file.set_len(offset).await {
            tracing::error!("truncating secret upload: {e}");
        }
        use tokio::io::AsyncSeekExt;
        let _ = p.file.seek(std::io::SeekFrom::Start(offset)).await;
        return res;
    }
    p.touched = Instant::now();
    received(p.written)
}

/// Stores a complete upload and returns its links (without the key).
pub async fn finish(app: &Arc<App>, upload: &str, headers: &HeaderMap) -> Response {
    let Some(arc) = pending(app, upload) else {
        return gone_upload();
    };
    let mut p = arc.lock().await;
    if p.closed {
        return gone_upload();
    }
    if p.written != p.size {
        return fail(StatusCode::BAD_REQUEST, "The upload is incomplete.");
    }
    if let Err(e) = p.file.flush().await {
        return internal(e);
    }
    let id = token(12);
    if let Err(e) = tokio::fs::rename(&p.tmp, store::blob_path(app, &id)).await {
        return internal(e);
    }
    let receipt = token(24);
    let row = db::File {
        id: id.clone(),
        name: String::new(),
        content_type: "application/octet-stream".into(),
        size: p.size,
        stored: p.size,
        compressed: false,
        created_at: now(),
        expires_at: p.expires_at,
        downloads: 0,
        deleted_at: None,
        invite: p.invite.as_ref().map(|i| i.token.clone()),
        secret: Some(p.kind.into()),
        key_check: Some(p.key_check.clone()),
        receipt: Some(receipt.clone()),
        opened_at: None,
    };
    if let Err(e) = app.db.insert_file(&row) {
        let _ = std::fs::rename(store::blob_path(app, &id), &p.tmp);
        return internal(e);
    }
    app.refresh_usage();
    let invite = p.invite.take();
    close(app, &mut p);
    app.uploads.0.lock().unwrap().remove(upload);
    let detail = match &invite {
        Some(i) => {
            if let Err(e) = app.db.complete_invite(&i.token, &id, now()) {
                tracing::error!("completing upload link: {e:#}");
            }
            if i.note.is_empty() {
                "through an upload link".to_string()
            } else {
                format!("through the upload link “{}”", i.note)
            }
        }
        None => "by the admin".to_string(),
    };
    log(app, &id, "created", Some(&p.from), &detail);
    tracing::info!("stored a one-time secret ({})", html::fmt_bytes(p.size));
    let app2 = app.clone();
    tokio::spawn(async move { alert::check_usage(&app2).await });
    let base = base_url(app, headers);
    Json(serde_json::json!({
        "url": format!("{base}/s/{id}"),
        "receipt": format!("{base}/r/{receipt}"),
    }))
    .into_response()
}

fn not_found() -> Response {
    html::message(
        StatusCode::NOT_FOUND,
        "Not found",
        "There is no secret at this link.",
    )
}

/// The recipient's page: nothing is opened until they press the button, so
/// link previews do not use the secret up.
pub fn page(app: &App, id: &str, from: &Client) -> Response {
    let f = match app.db.file(id) {
        Ok(Some(f)) if f.secret.is_some() => f,
        Ok(_) => return not_found(),
        Err(e) => return internal(e),
    };
    let end = f.secret_end().or_else(|| {
        f.expires_at
            .is_some_and(|t| t <= now())
            .then_some("expired")
    });
    log(
        app,
        id,
        "viewed",
        Some(from),
        if end.is_some() {
            "after it was destroyed"
        } else {
            ""
        },
    );
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::response::Html(html::secret(&f, end)),
    )
        .into_response()
}

/// Hands the encrypted data out once: the secret is marked opened and its
/// file removed before the first byte is sent.
pub async fn open(app: &Arc<App>, id: &str, from: Client) -> Response {
    let f = match app.db.file(id) {
        Ok(Some(f)) if f.secret.is_some() => f,
        Ok(_) => return fail(StatusCode::NOT_FOUND, "There is no secret at this link."),
        Err(e) => return internal(e),
    };
    let path = store::blob_path(app, id);
    // Opened first: the data stays readable through it once unlinked.
    let file = tokio::fs::File::open(&path).await;
    match app.db.open_secret(id, now()) {
        Ok(true) => {}
        Ok(false) => {
            // Read again: it may have been opened since.
            let f = match app.db.file(id) {
                Ok(Some(f)) => f,
                Ok(None) => return not_found(),
                Err(e) => return internal(e),
            };
            let why = match f.secret_end() {
                Some(end) => end,
                None => {
                    // Expired, not swept yet.
                    log(app, id, "expired", None, "");
                    store::remove(app, id).await;
                    "expired"
                }
            };
            log(app, id, "refused", Some(&from), why);
            let msg = match why {
                "opened" => "This secret was already opened. It can be opened only once.",
                "expired" => "This secret expired before anyone opened it.",
                _ => "This secret was destroyed by its sender.",
            };
            return fail(StatusCode::GONE, msg);
        }
        Err(e) => return internal(e),
    }
    log(app, id, "opened", Some(&from), "");
    let file = match file {
        Ok(f) => f,
        Err(e) => {
            log(
                app,
                id,
                "interrupted",
                None,
                "its data was missing on the server",
            );
            return internal(e);
        }
    };
    if let Err(e) = tokio::fs::remove_file(&path).await {
        tracing::error!("removing opened secret {id}: {e}");
    }
    app.refresh_usage();
    tracing::info!("one-time secret {id} opened");
    let body = Tracked {
        inner: ReaderStream::with_capacity(file, 1 << 16),
        app: app.clone(),
        id: id.to_string(),
        sent: 0,
        total: f.stored,
        from,
    };
    let mut res = Response::new(Body::from_stream(body));
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(header::CONTENT_LENGTH, f.stored.into());
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// The download of a secret, recording on the timeline whether it went
/// through to the end.
struct Tracked {
    inner: ReaderStream<tokio::fs::File>,
    app: Arc<App>,
    id: String,
    sent: u64,
    total: u64,
    from: Client,
}

impl Stream for Tracked {
    type Item = std::io::Result<axum::body::Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let poll = Pin::new(&mut self.inner).poll_next(cx);
        if let Poll::Ready(Some(Ok(b))) = &poll {
            self.sent += b.len() as u64;
        }
        poll
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        if self.sent >= self.total {
            log(&self.app, &self.id, "completed", Some(&self.from), "");
        } else {
            let detail = format!(
                "after {} of {}",
                html::fmt_bytes(self.sent),
                html::fmt_bytes(self.total)
            );
            log(
                &self.app,
                &self.id,
                "interrupted",
                Some(&self.from),
                &detail,
            );
        }
    }
}

/// Destroys a secret before it is opened.
pub async fn burn(app: &App, f: &db::File, by: &str) -> bool {
    if f.secret.is_none() || f.deleted_at.is_some() {
        return false;
    }
    store::remove(app, &f.id).await;
    log(app, &f.id, "burned", None, by);
    tracing::info!("one-time secret {} burned {by}", f.id);
    true
}

pub fn receipt(app: &App, token: &str, headers: &HeaderMap, flash: Option<&str>) -> Response {
    let f = match app.db.file_by_receipt(token) {
        Ok(Some(f)) => f,
        Ok(None) => {
            return html::message(
                StatusCode::NOT_FOUND,
                "Not found",
                "There is no receipt at this link.",
            );
        }
        Err(e) => return internal(e),
    };
    let events = match app.db.events(&f.id) {
        Ok(e) => e,
        Err(e) => return internal(e),
    };
    let link = format!("{}/s/{}", base_url(app, headers), f.id);
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::response::Html(html::receipt(&f, &events, &link, flash)),
    )
        .into_response()
}

pub async fn burn_from_receipt(app: &App, token: &str) -> Response {
    let f = match app.db.file_by_receipt(token) {
        Ok(Some(f)) => f,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    let msg = if burn(app, &f, "by the sender").await {
        "burned"
    } else {
        "too-late"
    };
    alert::check_usage(app).await;
    axum::response::Redirect::to(&format!("/r/{token}?msg={msg}")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overhead() {
        // 1 byte + the largest header fits in one record.
        assert_eq!(encrypted_max(1), 1 + MAX_HEADER + TAG);
        // 1 GB: 954 full records and a partial one.
        let plain = 1_000_000_000 + MAX_HEADER;
        assert_eq!(encrypted_max(1_000_000_000), plain + 954 * TAG);
    }
}
