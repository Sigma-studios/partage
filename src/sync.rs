//! The loop that keeps the Storage Box current: copies new objects there,
//! writes a manifest after changes, deletes what nothing needs any more,
//! frees this server's copies beyond the space allowed for them, and checks
//! the box once a day. Each pass that writes to the box is one backup run
//! reported to Réplique; a pass happens at least daily so Réplique hears
//! from partage even when nothing changes.

use crate::{
    App, alert, crypt,
    manifest::{self, Manifest},
    now,
    remote::Remote,
    replique,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long the tree must stay unchanged before a manifest is written…
const QUIET: Duration = Duration::from_secs(30);
/// …unless changes keep coming for this long.
const MAX_WAIT: Duration = Duration::from_secs(300);
const TRASH_DAYS: i64 = 30;
const GC_EVERY: i64 = 3600;
const CHECK_EVERY: i64 = 86400;
/// At most this much is read back from the box by a daily check.
const CHECK_BYTES: u64 = 2_000_000_000;
const CHECK_OBJECTS: usize = 32;

pub async fn run(app: Arc<App>) {
    replique::declare_job(&app).await;
    let mut dirty_since: Option<Instant> = None;
    let mut last_gc = 0i64;
    let mut failing_since: Option<Instant> = None;
    let mut alerted = false;
    // Which remote was last prepared, by address.
    let mut prepared: Option<usize> = None;
    loop {
        tokio::select! {
            _ = app.drive.wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
        reload_box(&app);
        replique::replay_spool(&app).await;
        evict(&app).await;
        let Some(remote) = app.drive.remote() else {
            continue;
        };
        let last_change = *app.drive.last_change.lock().unwrap();
        let pending = app
            .db
            .pending_objects()
            .map(|p| !p.is_empty())
            .unwrap_or(false);
        if (pending || last_change.is_some()) && dirty_since.is_none() {
            dirty_since = Some(Instant::now());
        }
        let quiet = last_change.is_none_or(|t| t.elapsed() >= QUIET);
        let waited = dirty_since.is_some_and(|t| t.elapsed() >= MAX_WAIT);
        let work = dirty_since.is_some() && (quiet || waited);
        let heartbeat = replique::server(&app).is_some()
            && app
                .drive
                .runs
                .last
                .lock()
                .unwrap()
                .is_none_or(|(t, _)| now() - t > 23 * 3600);
        let gc = now() - last_gc >= GC_EVERY;
        if work || heartbeat || gc {
            let fresh = prepared != Some(Arc::as_ptr(&remote) as usize);
            match session(&app, &remote, fresh, gc).await {
                Ok(()) => {
                    prepared = Some(Arc::as_ptr(&remote) as usize);
                    if app.drive.last_change.lock().unwrap().is_none()
                        && app
                            .db
                            .pending_objects()
                            .map(|p| p.is_empty())
                            .unwrap_or(false)
                    {
                        dirty_since = None;
                    }
                    if gc {
                        last_gc = now();
                    }
                    failing_since = None;
                    alerted = false;
                }
                Err(e) => {
                    prepared = None;
                    app.drive.error(format!("sync with the Storage Box: {e:#}"));
                    let since = *failing_since.get_or_insert_with(Instant::now);
                    if !alerted && since.elapsed() > Duration::from_secs(6 * 3600) {
                        alerted = true;
                        alert::drive_problem(
                            app.clone(),
                            format!(
                                "the Storage Box has been unreachable for 6 hours, so new files are only on the server ({e:#})."
                            ),
                        );
                    }
                    // Not every 30 seconds while the box is away.
                    tokio::time::sleep(Duration::from_secs(120)).await;
                }
            }
        }
        let last_check: i64 = app
            .db
            .setting("drive_last_check")
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if now() - last_check >= CHECK_EVERY && prepared.is_some() {
            check(&app, &remote).await;
        }
    }
}

async fn session(
    app: &Arc<App>,
    remote: &Arc<Remote>,
    fresh: bool,
    gc_due: bool,
) -> anyhow::Result<()> {
    let run = replique::begin(app);
    let result = async {
        if fresh {
            remote.prepare().await?;
            remember_host_key(app, remote);
        }
        for obj in app.db.pending_objects()? {
            upload(app, remote, &obj.hash).await?;
            run.added(1, obj.sealed);
        }
        let dirty = app.drive.last_change.lock().unwrap().take();
        if dirty.is_some() {
            if let Err(e) = write_manifest(app, remote, &run).await {
                // Due again.
                app.drive
                    .last_change
                    .lock()
                    .unwrap()
                    .get_or_insert(Instant::now());
                return Err(e);
            }
            if app.drive.last_change.lock().unwrap().is_none() {
                let _ = app.db.delete_setting("drive_dirty");
            }
        }
        if gc_due {
            gc(app, remote).await?;
        }
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = &result {
        run.error(format!("{e:#}"));
    }
    result
}

/// Picks up box settings saved by another process (`partage drive-pair`).
fn reload_box(app: &App) {
    let saved: Option<crate::remote::BoxConfig> = app
        .db
        .setting("drive_box")
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok());
    let current = app.drive.remote().map(|r| r.cfg.clone());
    // The host key filled in after the first connection is not a change.
    let differs = match (&saved, &current) {
        (Some(a), Some(b)) => {
            (&a.host, a.port, &a.user, &a.path) != (&b.host, b.port, &b.user, &b.path)
        }
        (None, None) => false,
        _ => true,
    };
    if differs {
        tracing::info!("drive: Storage Box settings changed, using them");
        if let Err(e) = crate::drive::set_box(app, saved) {
            tracing::error!("drive: {e:#}");
        }
    }
}

fn remember_host_key(app: &App, remote: &Remote) {
    if !remote.cfg.host_key.is_empty() {
        return;
    }
    let Some(seen) = remote.seen_host_key.lock().unwrap().clone() else {
        return;
    };
    let mut cfg = remote.cfg.clone();
    cfg.host_key = seen;
    if let Ok(json) = serde_json::to_string(&cfg) {
        let _ = app.db.set_setting("drive_box", &json);
    }
}

/// Copies one object from this server to the box, checking it against its
/// name on the way (a damaged local copy is not spread).
async fn upload(app: &App, remote: &Remote, hash: &str) -> anyhow::Result<()> {
    let path = app.drive.object_path(hash);
    let mut f = tokio::fs::File::open(&path).await?;
    let mut w = remote.writer().await?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let copied = async {
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            w.write(&buf[..n]).await?;
        }
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = copied {
        w.abort().await;
        return Err(e);
    }
    if crypt::hex(&h.finalize()) != hash {
        w.abort().await;
        anyhow::bail!("this server's copy of {hash} is damaged: not copied");
    }
    w.commit(&remote.cfg.object_path(hash)).await?;
    app.db.set_object_place(hash, true, true)?;
    Ok(())
}

async fn write_manifest(
    app: &App,
    remote: &Remote,
    run: &replique::RunGuard,
) -> anyhow::Result<()> {
    let (nodes, versions) = app.db.snapshot()?;
    let at = now();
    let m = Manifest::build(&nodes, &versions, at);
    let (sealed, hash) = m.seal(&app.drive.key)?;
    let mut w = remote.writer().await?;
    if let Err(e) = w.write(&sealed).await {
        w.abort().await;
        return Err(e);
    }
    w.commit(&remote.cfg.manifest_path(&hash)).await?;
    app.db
        .add_manifest(&hash, at, sealed.len() as u64, &m.objects())?;
    run.manifest(&hash);
    app.drive.status.lock().unwrap().last_manifest = Some(at);
    tracing::info!("drive: manifest {} ({} items)", &hash[..12], m.nodes.len());
    Ok(())
}

/// Old trash and versions, then manifests past their keep, then objects no
/// kept manifest lists, then leftovers of cut-off uploads.
async fn gc(app: &App, remote: &Remote) -> anyhow::Result<()> {
    let expired = app
        .db
        .expire_trash_and_versions(now() - TRASH_DAYS * 86400)?;
    if expired > 0 {
        tracing::info!("drive: {expired} items left the trash or the versions for good");
        crate::drive::changed(app);
    }
    let manifests: Vec<(String, i64)> = app
        .db
        .manifests()?
        .into_iter()
        .map(|(h, t, _)| (h, t))
        .collect();
    let drop = manifest::to_drop(&manifests, now());
    for hash in &drop {
        remote.remove(&remote.cfg.manifest_path(hash)).await?;
        app.db.delete_manifest(hash)?;
    }
    let kept: Vec<i64> = manifests
        .iter()
        .filter(|(h, _)| !drop.contains(h))
        .map(|(_, t)| *t)
        .collect();
    let mut deleted = 0;
    for (obj, span) in app.db.unused_objects()? {
        let listed = span.is_some_and(|(a, b)| kept.iter().any(|t| (a..=b).contains(t)));
        if listed {
            continue;
        }
        if obj.remote {
            remote.remove(&remote.cfg.object_path(&obj.hash)).await?;
        }
        if obj.local {
            let _ = tokio::fs::remove_file(app.drive.object_path(&obj.hash)).await;
        }
        app.db.delete_object(&obj.hash)?;
        deleted += 1;
    }
    if deleted > 0 {
        tracing::info!("drive: deleted {deleted} objects nothing needs any more");
    }
    let cutoff = (now() - 86400) as u32;
    for (name, _, mtime) in remote.list("tmp").await.unwrap_or_default() {
        if name.ends_with(".partial") && mtime < cutoff && !name.contains('/') {
            let _ = remote
                .remove(&format!("{}/tmp/{name}", remote.cfg.path))
                .await;
        }
    }
    Ok(())
}

/// Frees this server's copies of objects already on the box, least recently
/// read first, while they take more than the space allowed.
async fn evict(app: &App) {
    let limit = app.drive.local_limit();
    let Ok((mut used, _)) = app.db.local_bytes() else {
        return;
    };
    if used <= limit {
        return;
    }
    let Ok(candidates) = app.db.evictable() else {
        return;
    };
    for obj in candidates {
        if used <= limit {
            break;
        }
        match tokio::fs::remove_file(app.drive.object_path(&obj.hash)).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::error!("drive: freeing {}: {e}", obj.hash);
                continue;
            }
        }
        if app.db.set_object_place(&obj.hash, false, true).is_ok() {
            used = used.saturating_sub(obj.sealed);
        }
    }
}

/// The daily check: every object the database puts on the box is there with
/// the right size, a sample is read back whole against its name and opened
/// with the key, and the newest manifest opens. A copy found wrong on the box
/// is sent again from this server when this server still has it.
pub async fn check(app: &Arc<App>, remote: &Remote) {
    let started = now();
    let mut errors = Vec::new();
    let mut resend = Vec::new();
    let result = async {
        let listed: HashMap<String, u64> = remote
            .list("objects")
            .await?
            .into_iter()
            .filter_map(|(rel, size, _)| {
                let name = rel.rsplit('/').next()?.to_string();
                crypt::is_hash(&name).then_some((name, size))
            })
            .collect();
        let objects = app.db.remote_objects()?;
        for o in &objects {
            match listed.get(&o.hash) {
                Some(&size) if size == o.sealed => {}
                found => {
                    let what = if found.is_some() {
                        "has the wrong size"
                    } else {
                        "is missing"
                    };
                    if o.local {
                        resend.push(o.hash.clone());
                    } else {
                        errors.push(format!(
                            "object {} {what} on the box, and this server has no copy",
                            o.hash
                        ));
                    }
                }
            }
        }
        // The sample: random objects, up to a number and a total size.
        let mut sample: Vec<&crate::drive_db::Object> = objects
            .iter()
            .filter(|o| !resend.contains(&o.hash))
            .collect();
        for i in (1..sample.len()).rev() {
            sample.swap(i, rand::random_range(0..=i));
        }
        let mut budget = CHECK_BYTES;
        let mut read = 0;
        for o in sample.into_iter().take(CHECK_OBJECTS) {
            if o.sealed > budget {
                continue;
            }
            budget -= o.sealed;
            read += 1;
            if let Err(e) = verify_remote(app, remote, &o.hash, o.sealed).await {
                if o.local {
                    let _ = remote.remove(&remote.cfg.object_path(&o.hash)).await;
                    resend.push(o.hash.clone());
                } else {
                    errors.push(format!("object {}: {e:#}", o.hash));
                }
            }
        }
        let on_box: Vec<String> = remote
            .list("manifests")
            .await?
            .into_iter()
            .map(|(n, _, _)| n)
            .filter(|n| crypt::is_hash(n))
            .collect();
        let known = app.db.manifests()?;
        for (h, _, _) in &known {
            if !on_box.contains(h) {
                errors.push(format!("manifest {h} is missing on the box"));
            }
        }
        if let Some((newest, _, _)) = known.last() {
            let bytes = remote.read(&remote.cfg.manifest_path(newest)).await?;
            if crypt::hex(&Sha256::digest(&bytes)) != *newest {
                errors.push(format!("manifest {newest} does not match its name"));
            } else if let Err(e) = Manifest::open(&app.drive.key, &bytes) {
                errors.push(format!("manifest {newest} does not open: {e:#}"));
            }
        }
        let verified: Vec<String> = known
            .into_iter()
            .map(|(h, _, _)| h)
            .filter(|h| on_box.contains(h))
            .collect();
        anyhow::Ok((objects.len(), read, verified))
    }
    .await;
    let (subset, verified) = match result {
        Ok((total, read, verified)) => (
            format!("{total} objects listed, {read} read back"),
            verified,
        ),
        Err(e) => {
            errors.push(format!("{e:#}"));
            ("not finished".to_string(), Vec::new())
        }
    };
    for hash in &resend {
        let _ = app.db.set_object_place(hash, true, false);
    }
    if !resend.is_empty() {
        tracing::warn!(
            "drive: {} objects will be copied to the box again",
            resend.len()
        );
        app.drive.wake.notify_one();
    }
    tracing::info!("drive check: {subset}, {} problems", errors.len());
    app.drive.status.lock().unwrap().last_check = Some((now(), errors.len(), subset.clone()));
    let _ = app.db.set_setting("drive_last_check", &now().to_string());
    if !errors.is_empty() {
        alert::drive_problem(
            app.clone(),
            format!(
                "the daily check of the Storage Box found {} problems: {}",
                errors.len(),
                errors[0]
            ),
        );
    }
    replique::report_check(app, started, verified, errors, subset).await;
}

async fn verify_remote(app: &App, remote: &Remote, hash: &str, sealed: u64) -> anyhow::Result<()> {
    let mut f = remote.open(&remote.cfg.object_path(hash)).await?;
    let mut h = Sha256::new();
    let mut header = vec![0u8; crypt::HEADER as usize];
    f.read_exact(&mut header).await?;
    h.update(&header);
    let opener = crypt::Opener::new(&app.drive.key, &header, sealed)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut first = Vec::new();
    let (_, first_len) = opener.record_span(0);
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if first.len() < first_len {
            let take = (first_len - first.len()).min(n);
            first.extend_from_slice(&buf[..take]);
        }
        h.update(&buf[..n]);
    }
    let _ = f.shutdown().await;
    anyhow::ensure!(crypt::hex(&h.finalize()) == hash, "does not match its name");
    opener.open(0, &mut first)?;
    Ok(())
}
