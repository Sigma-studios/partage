//! Files on disk: compressed uploads, streamed downloads, expiry.

use crate::{App, alert, db, html, now, token};
use async_compression::{
    Level,
    tokio::{bufread::ZstdDecoder, write::ZstdEncoder},
};
use axum::{
    Json,
    body::Body,
    extract::multipart::{Field, Multipart, MultipartError},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use std::{
    io,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::io::ReaderStream;

/// How much of an upload is test-compressed to decide whether compressing
/// it is worth it.
const SAMPLE: usize = 1 << 20;

pub fn blob_path(app: &App, id: &str) -> PathBuf {
    app.cfg.data.join("files").join(id)
}

pub fn tmp_dir(app: &App) -> PathBuf {
    app.cfg.data.join("tmp")
}

/// Where an upload goes and what it may be.
pub struct Target {
    pub expires_at: Option<i64>,
    /// Takes the expiry from the form's `expires_at` field (Unix seconds).
    pub expiry_from_form: bool,
    /// Largest original size accepted.
    pub max_bytes: Option<u64>,
    pub invite: Option<String>,
}

pub enum UploadError {
    TooLarge(u64),
    StorageFull,
    BadRequest(String),
    Internal(anyhow::Error),
}

impl From<MultipartError> for UploadError {
    fn from(e: MultipartError) -> Self {
        Self::BadRequest(format!("upload interrupted: {e}"))
    }
}

impl From<io::Error> for UploadError {
    fn from(e: io::Error) -> Self {
        Self::Internal(e.into())
    }
}

impl From<anyhow::Error> for UploadError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

impl IntoResponse for UploadError {
    fn into_response(self) -> Response {
        match self {
            Self::TooLarge(max) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "This file is larger than the {} allowed.",
                    html::fmt_bytes(max)
                ),
            ),
            Self::StorageFull => (
                StatusCode::INSUFFICIENT_STORAGE,
                "Storage is full: this upload would go over the limit.".to_string(),
            ),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            Self::Internal(e) => {
                tracing::error!("upload failed: {e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Upload failed on the server.".to_string(),
                )
            }
        }
        .into_response()
    }
}

/// Reads the upload form and stores its file.
pub async fn receive(
    app: &Arc<App>,
    mut form: Multipart,
    mut target: Target,
) -> Result<db::File, UploadError> {
    while let Some(field) = form.next_field().await? {
        match field.name() {
            Some("expires_at") if target.expiry_from_form => {
                let text = field.text().await?;
                let text = text.trim();
                if !text.is_empty() {
                    let at: i64 = text
                        .parse()
                        .map_err(|_| UploadError::BadRequest("Invalid expiry date.".into()))?;
                    if at <= now() {
                        return Err(UploadError::BadRequest(
                            "The expiry date is in the past.".into(),
                        ));
                    }
                    target.expires_at = Some(at);
                }
            }
            Some("file") => return store(app, field, &target).await,
            _ => {}
        }
    }
    Err(UploadError::BadRequest("The upload has no file.".into()))
}

async fn store(
    app: &Arc<App>,
    mut field: Field<'_>,
    target: &Target,
) -> Result<db::File, UploadError> {
    let name = clean_name(field.file_name().unwrap_or_default());
    let content_type = field
        .content_type()
        .filter(|t| !t.is_empty())
        .unwrap_or("application/octet-stream")
        .to_string();

    let mut sample = Vec::new();
    let mut more = true;
    while sample.len() < SAMPLE {
        match field.chunk().await? {
            Some(chunk) => sample.extend_from_slice(&chunk),
            None => {
                more = false;
                break;
            }
        }
    }
    let compressed = worth_compressing(&sample);

    let tmp = TmpFile(tmp_dir(app).join(token(16)));
    let stored = Arc::new(AtomicU64::new(0));
    let file = Counting {
        inner: tokio::fs::File::create(&tmp.0).await?,
        count: stored.clone(),
    };
    let sink: Pin<Box<dyn AsyncWrite + Send>> = if compressed {
        Box::pin(ZstdEncoder::with_quality(file, Level::Default))
    } else {
        Box::pin(file)
    };
    let mut w = Writer {
        app: app.clone(),
        sink,
        stored,
        reserved: 0,
        size: 0,
        max: target.max_bytes,
    };

    w.write(&sample).await?;
    drop(sample);
    while more && let Some(chunk) = field.chunk().await? {
        w.write(&chunk).await?;
    }
    w.sink.shutdown().await?;
    w.account()?;

    let id = token(12);
    tokio::fs::rename(&tmp.0, blob_path(app, &id)).await?;
    std::mem::forget(tmp);
    let row = db::File {
        id,
        name,
        content_type,
        size: w.size,
        stored: w.reserved,
        compressed,
        created_at: now(),
        expires_at: target.expires_at,
        downloads: 0,
        deleted_at: None,
        invite: target.invite.clone(),
    };
    app.db.insert_file(&row)?;
    // Counted as used before the writer stops counting it as in flight.
    app.refresh_usage();
    drop(w);
    tracing::info!(
        "stored {} ({}, {} on disk)",
        row.name,
        html::fmt_bytes(row.size),
        html::fmt_bytes(row.stored)
    );
    let app = app.clone();
    tokio::spawn(async move { alert::check_usage(&app).await });
    Ok(row)
}

/// Compresses unless a test on the first megabyte saves under 5%, as with
/// archives, videos and most images.
fn worth_compressing(sample: &[u8]) -> bool {
    match zstd::bulk::compress(sample, 3) {
        Ok(out) => !sample.is_empty() && out.len() * 100 < sample.len() * 95,
        Err(_) => false,
    }
}

fn clean_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let name: String = base.chars().filter(|c| !c.is_control()).take(200).collect();
    match name.trim() {
        "" | "." | ".." => "file".to_string(),
        n => n.to_string(),
    }
}

/// Writes an upload while checking it against its own limit and the
/// storage limit.
struct Writer {
    app: Arc<App>,
    sink: Pin<Box<dyn AsyncWrite + Send>>,
    /// Bytes written to disk so far.
    stored: Arc<AtomicU64>,
    /// The part of them counted in `App::inflight`.
    reserved: u64,
    /// Original bytes received.
    size: u64,
    max: Option<u64>,
}

impl Writer {
    async fn write(&mut self, chunk: &[u8]) -> Result<(), UploadError> {
        self.size += chunk.len() as u64;
        if let Some(max) = self.max
            && self.size > max
        {
            return Err(UploadError::TooLarge(max));
        }
        self.sink.write_all(chunk).await?;
        self.account()
    }

    fn account(&mut self) -> Result<(), UploadError> {
        let stored = self.stored.load(Relaxed);
        self.app.inflight.fetch_add(stored - self.reserved, Relaxed);
        self.reserved = stored;
        if self.app.used() + self.app.inflight.load(Relaxed) > self.app.limit() {
            alert::upload_blocked(self.app.clone());
            return Err(UploadError::StorageFull);
        }
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.app.inflight.fetch_sub(self.reserved, Relaxed);
    }
}

/// Removed when dropped, unless forgotten.
struct TmpFile(PathBuf);

impl Drop for TmpFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct Counting<W> {
    inner: W,
    count: Arc<AtomicU64>,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for Counting<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = poll {
            self.count.fetch_add(n as u64, Relaxed);
        }
        poll
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub fn upload_response(f: &db::File, base: &str) -> Response {
    Json(serde_json::json!({
        "url": format!("{base}/f/{}", f.id),
        "name": f.name,
        "size": f.size,
    }))
    .into_response()
}

/// The original file, decompressed on the way out.
pub async fn download(app: &App, id: &str, head: bool) -> Response {
    let gone = || {
        html::message(
            StatusCode::GONE,
            "File expired",
            "This file has expired or was removed, and can no longer be downloaded.",
        )
    };
    let f = match app.db.file(id) {
        Ok(Some(f)) => f,
        Ok(None) => {
            return html::message(
                StatusCode::NOT_FOUND,
                "Not found",
                "There is no file at this link.",
            );
        }
        Err(e) => {
            tracing::error!("looking up {id}: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if f.deleted_at.is_some() {
        return gone();
    }
    if f.expires_at.is_some_and(|t| t <= now()) {
        remove(app, id).await;
        return gone();
    }
    let file = match tokio::fs::File::open(blob_path(app, id)).await {
        Ok(file) => file,
        Err(e) => {
            tracing::error!("opening {id}: {e}");
            return gone();
        }
    };
    if !head && let Err(e) = app.db.count_download(id) {
        tracing::error!("counting download of {id}: {e:#}");
    }
    let body = if f.compressed {
        Body::from_stream(ReaderStream::with_capacity(
            ZstdDecoder::new(BufReader::with_capacity(1 << 16, file)),
            1 << 16,
        ))
    } else {
        Body::from_stream(ReaderStream::with_capacity(file, 1 << 16))
    };
    let mut res = Response::new(body);
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&f.content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(header::CONTENT_LENGTH, f.size.into());
    if let Ok(v) = HeaderValue::from_str(&content_disposition(&f.name)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    res
}

/// `attachment`, with the name in ASCII for old clients and UTF-8 for the rest.
fn content_disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

/// Deletes a file's data; its link then says it expired.
pub async fn remove(app: &App, id: &str) {
    if let Err(e) = tokio::fs::remove_file(blob_path(app, id)).await
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::error!("deleting {id}: {e}");
    }
    if let Err(e) = app.db.mark_deleted(id, now()) {
        tracing::error!("marking {id} deleted: {e:#}");
    }
    app.refresh_usage();
}

/// Deletes expired files and sessions every minute.
pub async fn sweep(app: Arc<App>) {
    loop {
        match app.db.expired_files(now()) {
            Ok(ids) => {
                for id in &ids {
                    tracing::info!("{id} expired");
                    remove(&app, id).await;
                }
            }
            Err(e) => tracing::error!("listing expired files: {e:#}"),
        }
        if let Err(e) = app.db.purge_sessions(now()) {
            tracing::error!("purging sessions: {e:#}");
        }
        alert::check_usage(&app).await;
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(clean_name("C:\\Users\\me\\report.pdf"), "report.pdf");
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_name(".."), "file");
        assert_eq!(clean_name(""), "file");
        assert_eq!(
            content_disposition("été \"x\".txt"),
            "attachment; filename=\"_t_ _x_.txt\"; filename*=UTF-8''%C3%A9t%C3%A9%20%22x%22.txt"
        );
    }

    #[test]
    fn compression_choice() {
        assert!(worth_compressing(&b"hello world ".repeat(1000)));
        let noise: Vec<u8> = (0..100_000).map(|_| rand::random()).collect();
        assert!(!worth_compressing(&noise));
        assert!(!worth_compressing(b""));
    }
}
