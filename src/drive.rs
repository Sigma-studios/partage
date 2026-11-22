//! The Drive's storage: uploads are sealed as they stream in (see `crypt`),
//! kept on this server while they fit in the space allowed for it, and copied
//! to the Storage Box by the sync loop (`sync`). A file too big for this
//! server goes straight to the box. Downloads open only the records a request
//! needs, from wherever the object is.

use crate::{
    App, alert,
    crypt::{self, Key, Opener, RECORD, Sealer},
    drive_db::{self, NewFile, Node},
    html, now,
    remote::{BoxConfig, Remote},
    replique,
};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use russh::keys::ssh_key::PrivateKey;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::SeekFrom,
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::Instant,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWriteExt};

pub const DEFAULT_BOX_LIMIT: u64 = 100_000_000_000;
pub const DEFAULT_LOCAL_LIMIT: u64 = 5_000_000_000;

#[derive(Default)]
pub struct Status {
    pub last_error: Option<(i64, String)>,
    /// When, how many problems, what was checked.
    pub last_check: Option<(i64, usize, String)>,
    pub last_manifest: Option<i64>,
}

pub struct Drive {
    pub key: Key,
    pub ssh_key: PrivateKey,
    /// `PARTAGE_DATA/drive`.
    pub dir: PathBuf,
    pub remote: RwLock<Option<Arc<Remote>>>,
    /// Wakes the sync loop.
    pub wake: tokio::sync::Notify,
    /// The last change to the tree, for the manifest's quiet period.
    pub last_change: Mutex<Option<Instant>>,
    pub box_limit: AtomicU64,
    pub local_limit: AtomicU64,
    /// Sealed bytes of uploads in progress.
    pub inflight: AtomicU64,
    /// Uploads in progress through links: the folders they count against.
    pending: Mutex<HashMap<u64, (Vec<String>, u64)>>,
    pub runs: replique::Runs,
    pub status: Mutex<Status>,
}

impl Drive {
    pub fn open(data: &std::path::Path, db: &crate::db::Db) -> anyhow::Result<Self> {
        let dir = data.join("drive");
        std::fs::create_dir_all(dir.join("objects"))?;
        let key = Key::load_or_create(&dir.join("key"))?;
        let ssh_key = crate::remote::load_or_create_key(&dir.join("ssh_ed25519"))?;
        let setting = |k: &str, default: u64| {
            db.setting(k)
                .ok()
                .flatten()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let remote = match db.setting("drive_box")? {
            Some(json) => match serde_json::from_str::<BoxConfig>(&json) {
                Ok(cfg) => Some(Arc::new(Remote::new(cfg, ssh_key.clone()))),
                Err(e) => {
                    tracing::error!("the saved Storage Box settings do not read: {e}");
                    None
                }
            },
            None => None,
        };
        let dirty = db.setting("drive_dirty")?.is_some();
        Ok(Self {
            key,
            ssh_key,
            dir,
            remote: RwLock::new(remote),
            wake: Default::default(),
            last_change: Mutex::new(dirty.then(Instant::now)),
            box_limit: AtomicU64::new(setting("drive_box_limit", DEFAULT_BOX_LIMIT)),
            local_limit: AtomicU64::new(setting("drive_local_limit", DEFAULT_LOCAL_LIMIT)),
            inflight: AtomicU64::new(0),
            pending: Default::default(),
            runs: Default::default(),
            status: Default::default(),
        })
    }

    pub fn remote(&self) -> Option<Arc<Remote>> {
        self.remote.read().unwrap().clone()
    }

    pub fn object_path(&self, hash: &str) -> PathBuf {
        self.dir.join("objects").join(&hash[..2]).join(hash)
    }

    pub fn box_limit(&self) -> u64 {
        self.box_limit.load(Relaxed)
    }

    pub fn local_limit(&self) -> u64 {
        self.local_limit.load(Relaxed)
    }

    pub fn error(&self, msg: String) {
        tracing::error!("drive: {msg}");
        self.status.lock().unwrap().last_error = Some((now(), msg));
    }
}

/// The tree changed: a manifest is due, and the sync loop should look.
pub fn changed(app: &App) {
    *app.drive.last_change.lock().unwrap() = Some(Instant::now());
    if let Err(e) = app.db.set_setting("drive_dirty", "1") {
        tracing::error!("marking the Drive changed: {e:#}");
    }
    app.drive.wake.notify_one();
}

/// A name for a file or folder, or why it cannot be one.
pub fn check_name(raw: &str) -> Result<String, String> {
    let name: String = raw.trim().chars().filter(|c| !c.is_control()).collect();
    if name.is_empty() || name == "." || name == ".." {
        return Err("A name cannot be empty, “.” or “..”.".into());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("A name cannot contain / or \\.".into());
    }
    if name.len() > 255 {
        return Err("That name is too long (255 bytes at most).".into());
    }
    Ok(name)
}

pub enum DriveError {
    Bad(String),
    Full(String),
    Gone(String),
    Unavailable(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for DriveError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

impl From<std::io::Error> for DriveError {
    fn from(e: std::io::Error) -> Self {
        Self::Internal(e.into())
    }
}

impl IntoResponse for DriveError {
    fn into_response(self) -> Response {
        match self {
            Self::Bad(m) => (StatusCode::BAD_REQUEST, m).into_response(),
            Self::Full(m) => (StatusCode::INSUFFICIENT_STORAGE, m).into_response(),
            Self::Gone(m) => (StatusCode::GONE, m).into_response(),
            Self::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m).into_response(),
            Self::Internal(e) => {
                tracing::error!("drive: {e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Something went wrong on the server.",
                )
                    .into_response()
            }
        }
    }
}

/// An upload into the Drive.
pub struct Upload {
    pub folder: String,
    /// The file's path under `folder`: `name`, or `a/b/name` for a folder
    /// dropped whole.
    pub path: String,
    /// Announced by the browser, so the space is checked before any byte.
    pub size: u64,
    pub content_type: String,
    /// A new version of a file of the same name (the admin), or the next free
    /// name (links).
    pub replace: bool,
    pub by: String,
    /// Folder size limits apply (uploads through links).
    pub limited: bool,
}

/// Removes an upload from the counts when it ends, however it ends.
struct InFlight<'a> {
    app: &'a App,
    sealed: u64,
    id: u64,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.app.drive.inflight.fetch_sub(self.sealed, Relaxed);
        self.app.drive.pending.lock().unwrap().remove(&self.id);
    }
}

enum Sink {
    Local(tokio::fs::File, PathBuf),
    Remote(Box<crate::remote::RemoteWriter>),
}

impl Sink {
    async fn write(&mut self, buf: &[u8]) -> anyhow::Result<()> {
        match self {
            Self::Local(f, _) => Ok(f.write_all(buf).await?),
            Self::Remote(w) => w.write(buf).await,
        }
    }

    async fn abort(self) {
        match self {
            Self::Local(f, p) => {
                drop(f);
                let _ = tokio::fs::remove_file(p).await;
            }
            Self::Remote(w) => w.abort().await,
        }
    }
}

pub async fn receive(app: &Arc<App>, up: Upload, body: Body) -> Result<Node, DriveError> {
    let Some(folder) = app.db.node(&up.folder)? else {
        return Err(DriveError::Gone("That folder no longer exists.".into()));
    };
    if !folder.dir || folder.trashed_at.is_some() {
        return Err(DriveError::Gone("That folder no longer exists.".into()));
    }
    let mut parts = Vec::new();
    for p in up.path.split('/').filter(|p| !p.is_empty()) {
        parts.push(check_name(p).map_err(DriveError::Bad)?);
    }
    let Some(name) = parts.pop() else {
        return Err(DriveError::Bad("The file has no name.".into()));
    };
    let sealed = crypt::sealed_size(up.size);
    let d = &app.drive;

    // The box's limit, counting uploads in progress.
    let box_used = app.db.box_bytes()? + d.inflight.load(Relaxed);
    if box_used + sealed > d.box_limit() {
        alert::drive_blocked(app.clone());
        return Err(DriveError::Full(format!(
            "The Drive is full: {} of {} used.",
            html::fmt_bytes(box_used),
            html::fmt_bytes(d.box_limit())
        )));
    }
    let ancestry = app.db.ancestry(&folder.id)?;
    if up.limited {
        let pending = d.pending.lock().unwrap();
        for a in ancestry.iter().filter(|a| a.limit_bytes.is_some()) {
            let limit = a.limit_bytes.unwrap_or(0);
            let busy: u64 = pending
                .values()
                .filter(|(folders, _)| folders.contains(&a.id))
                .map(|(_, n)| n)
                .sum();
            let used = app.db.tree_size(&a.id)? + busy;
            if used + up.size > limit {
                return Err(DriveError::Full(format!(
                    "Not enough room in this folder: {} of {} used, {} left.",
                    html::fmt_bytes(used),
                    html::fmt_bytes(limit),
                    html::fmt_bytes(limit.saturating_sub(used))
                )));
            }
        }
    }
    let id = rand::random();
    d.pending.lock().unwrap().insert(
        id,
        (ancestry.iter().map(|a| a.id.clone()).collect(), up.size),
    );
    d.inflight.fetch_add(sealed, Relaxed);
    let _inflight = InFlight { app, sealed, id };

    // Here while it fits, counting what is not on the box yet; otherwise
    // straight to the box.
    let remote = d.remote();
    let (_, unsynced) = app.db.local_bytes()?;
    let fits_here = unsynced + sealed <= d.local_limit();
    let (mut sink, _run) = match (&remote, fits_here) {
        (_, true) => {
            let tmp = crate::store::tmp_dir(app).join(crate::token(16));
            (Sink::Local(tokio::fs::File::create(&tmp).await?, tmp), None)
        }
        (Some(r), false) => {
            let run = replique::begin(app);
            match r.writer().await {
                Ok(w) => (Sink::Remote(Box::new(w)), Some(run)),
                Err(e) => {
                    d.error(format!("the Storage Box is unreachable: {e:#}"));
                    return Err(DriveError::Unavailable(
                        "This file is too big for the server and the Storage Box cannot be reached right now: try again later.".into(),
                    ));
                }
            }
        }
        (None, false) => {
            return Err(DriveError::Full(format!(
                "Without a Storage Box, the Drive keeps files on this server only, and this one does not fit in the {} allowed for it.",
                html::fmt_bytes(d.local_limit())
            )));
        }
    };

    let mut sealer_out = Vec::with_capacity(2 << 20);
    let mut sealer = Sealer::new(&d.key, &mut sealer_out);
    let mut plain_hash = Sha256::new();
    let mut stream = body.into_data_stream();
    let result: Result<(), DriveError> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| DriveError::Bad(format!("upload interrupted: {e}")))?;
            if sealer.plain_len() + chunk.len() as u64 > up.size {
                return Err(DriveError::Bad("The file is bigger than announced.".into()));
            }
            plain_hash.update(&chunk);
            sealer.push(&chunk, &mut sealer_out);
            if sealer_out.len() >= 1 << 20 {
                sink.write(&sealer_out).await?;
                sealer_out.clear();
            }
        }
        if sealer.plain_len() != up.size {
            return Err(DriveError::Bad("The upload was cut off.".into()));
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        sink.abort().await;
        return Err(e);
    }
    let hash = sealer.finish(&mut sealer_out);
    let written = async {
        sink.write(&sealer_out).await?;
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = written {
        sink.abort().await;
        return Err(e.into());
    }
    let (local, remote_ok) = match sink {
        Sink::Local(f, tmp) => {
            f.sync_all().await?;
            drop(f);
            let dest = d.object_path(&hash);
            tokio::fs::create_dir_all(dest.parent().unwrap()).await?;
            tokio::fs::rename(&tmp, &dest).await?;
            (true, false)
        }
        Sink::Remote(w) => {
            let r = remote.as_ref().expect("a remote sink has a remote");
            w.commit(&r.cfg.object_path(&hash)).await?;
            if let Some(run) = &_run {
                run.added(1, sealed);
            }
            (false, true)
        }
    };
    let parent = app.db.ensure_dirs(
        &folder.id,
        &parts.iter().map(String::as_str).collect::<Vec<_>>(),
        now(),
    )?;
    let node = app.db.add_file(&NewFile {
        parent: &parent,
        name: &name,
        object: &hash,
        sealed,
        size: up.size,
        sha256: &crypt::hex(&plain_hash.finalize()),
        content_type: &up.content_type,
        local,
        remote: remote_ok,
        replace: up.replace,
        added_by: &up.by,
        now: now(),
    })?;
    tracing::info!(
        "drive: stored {} ({}, {})",
        node.name,
        html::fmt_bytes(up.size),
        if local {
            "on this server"
        } else {
            "on the Storage Box"
        }
    );
    changed(app);
    let app2 = app.clone();
    tokio::spawn(async move { alert::check_drive_usage(&app2).await });
    Ok(node)
}

/// Types a browser shows itself without running anything.
fn viewable(content_type: &str) -> bool {
    let t = content_type.split(';').next().unwrap_or("").trim();
    matches!(
        t,
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/avif"
            | "application/pdf"
            | "text/plain"
    ) || t.starts_with("video/")
        || t.starts_with("audio/")
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n`, one range only.
fn parse_range(h: &str, size: u64) -> Option<(u64, u64)> {
    let spec = h.strip_prefix("bytes=")?;
    if spec.contains(',') || size == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            (size.saturating_sub(n), size)
        }
        (a, "") => (a.parse().ok()?, size),
        (a, b) => (
            a.parse().ok()?,
            b.parse::<u64>().ok()?.saturating_add(1).min(size),
        ),
    };
    (start < end && start < size).then_some((start, end))
}

/// A file (or one of its versions) for a browser, ranges included.
pub async fn serve(
    app: &Arc<App>,
    version: &drive_db::Version,
    name: &str,
    headers: &HeaderMap,
    head: bool,
    inline: bool,
) -> Response {
    let size = version.size;
    let range = headers
        .get(header::RANGE)
        .and_then(|h| h.to_str().ok())
        .map(|h| parse_range(h, size));
    let (start, end, partial) = match range {
        Some(Some((s, e))) => (s, e, true),
        Some(None) if size > 0 => {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{size}"))],
            )
                .into_response();
        }
        _ => (0, size, false),
    };
    let body = if head || start == end {
        Body::empty()
    } else {
        match open_range(app, &version.object, start, end).await {
            Ok(b) => b,
            Err(e) => {
                app.drive
                    .error(format!("reading {}: {e:#}", version.object));
                return html::message(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Not available right now",
                    "This file is on the Storage Box, which cannot be reached right now. Try again in a moment.",
                );
            }
        }
    };
    let mut res = Response::new(body);
    if partial {
        *res.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    let h = res.headers_mut();
    let ct = if version.content_type.is_empty() {
        "application/octet-stream"
    } else {
        &version.content_type
    };
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(ct).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(header::CONTENT_LENGTH, (end - start).into());
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if partial && let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{}/{size}", end - 1)) {
        h.insert(header::CONTENT_RANGE, v);
    }
    let inline = inline && viewable(ct);
    if let Ok(v) = HeaderValue::from_str(&crate::store::disposition(name, inline)) {
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

/// The plain bytes `start..end` of an object, from this server's copy if
/// there is one, else from the box.
async fn open_range(app: &Arc<App>, hash: &str, start: u64, end: u64) -> anyhow::Result<Body> {
    let d = &app.drive;
    let obj = app
        .db
        .object(hash)?
        .ok_or_else(|| anyhow::anyhow!("unknown object"))?;
    d_touch(app, hash);
    if obj.local {
        match tokio::fs::File::open(d.object_path(hash)).await {
            Ok(f) => return Ok(stream_range(d.key.clone(), f, obj.sealed, start, end)),
            Err(e) => {
                tracing::error!("drive: local copy of {hash} missing: {e}");
                app.db.set_object_place(hash, false, obj.remote)?;
            }
        }
    }
    let r = d
        .remote()
        .ok_or_else(|| anyhow::anyhow!("no Storage Box and no local copy"))?;
    let f = r.open(&r.cfg.object_path(hash)).await?;
    Ok(stream_range(d.key.clone(), f, obj.sealed, start, end))
}

fn d_touch(app: &App, hash: &str) {
    app.db.touch_object(hash, now());
}

fn stream_range<R>(key: Key, reader: R, sealed: u64, start: u64, end: u64) -> Body
where
    R: AsyncRead + AsyncSeek + Unpin + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(4);
    tokio::spawn(async move {
        if let Err(e) = pump(key, reader, sealed, start, end, &tx).await {
            tracing::error!("drive: download failed: {e:#}");
            let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
        }
    });
    Body::from_stream(tokio_stream_from(rx))
}

fn tokio_stream_from(
    mut rx: tokio::sync::mpsc::Receiver<std::io::Result<Bytes>>,
) -> impl futures_util::Stream<Item = std::io::Result<Bytes>> {
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

async fn pump<R: AsyncRead + AsyncSeek + Unpin>(
    key: Key,
    mut r: R,
    sealed: u64,
    start: u64,
    end: u64,
    tx: &tokio::sync::mpsc::Sender<std::io::Result<Bytes>>,
) -> anyhow::Result<()> {
    let mut header = [0u8; crypt::HEADER as usize];
    r.read_exact(&mut header).await?;
    let o = Opener::new(&key, &header, sealed)?;
    let first = start / RECORD;
    let last = (end - 1) / RECORD;
    let (pos, _) = o.record_span(first);
    r.seek(SeekFrom::Start(pos)).await?;
    let mut buf = Vec::with_capacity((RECORD + crypt::TAG) as usize);
    for i in first..=last {
        let (_, len) = o.record_span(i);
        buf.resize(len, 0);
        r.read_exact(&mut buf).await?;
        o.open(i, &mut buf)?;
        let rs = i * RECORD;
        let a = (start.max(rs) - rs) as usize;
        let b = (end.min(rs + buf.len() as u64) - rs) as usize;
        if tx
            .send(Ok(Bytes::copy_from_slice(&buf[a..b])))
            .await
            .is_err()
        {
            // The browser went away.
            return Ok(());
        }
    }
    Ok(())
}

/// Saves the box settings and starts using them.
pub fn set_box(app: &App, cfg: Option<BoxConfig>) -> anyhow::Result<()> {
    match &cfg {
        Some(c) => app
            .db
            .set_setting("drive_box", &serde_json::to_string(c)?)?,
        None => app.db.delete_setting("drive_box")?,
    }
    *app.drive.remote.write().unwrap() =
        cfg.map(|c| Arc::new(Remote::new(c, app.drive.ssh_key.clone())));
    app.drive.wake.notify_one();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 100)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 1000)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 1000)));
        assert_eq!(parse_range("bytes=990-2000", 1000), Some((990, 1000)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
    }

    #[test]
    fn names() {
        assert_eq!(check_name("  a b.txt "), Ok("a b.txt".into()));
        assert!(check_name("..").is_err());
        assert!(check_name("a/b").is_err());
        assert!(check_name("").is_err());
    }
}
