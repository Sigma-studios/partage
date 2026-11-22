//! Réplique: pairing (which gives the Storage Box login), the job declaration
//! and run reports. Every write to the box happens inside a reported run, so
//! Réplique's "changed outside a reported run" warning keeps its meaning;
//! reports that cannot be sent wait in the database.

use crate::{App, html::rfc3339, now, remote::BoxConfig};
use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub const JOB: &str = "drive";
const USER_AGENT: &str = concat!("partage/", env!("CARGO_PKG_VERSION"));

/// The Réplique server partage reports to.
#[derive(Clone)]
pub struct Server {
    pub url: String,
    pub token: String,
}

pub fn server(app: &App) -> Option<Server> {
    let get = |k| app.db.setting(k).ok().flatten();
    Some(Server {
        url: get("replique_url")?,
        token: get("replique_token")?,
    })
}

pub fn forget(app: &App) {
    for k in ["replique_url", "replique_token", "replique_device_id"] {
        let _ = app.db.delete_setting(k);
    }
}

enum SendError {
    /// Network trouble or a 5xx: try again later.
    Unreachable(anyhow::Error),
    /// A 4xx: retrying will not help.
    Refused(String),
}

async fn send(
    app: &App,
    method: reqwest::Method,
    url: &str,
    token: Option<&str>,
    body: &Value,
) -> Result<Value, SendError> {
    let mut req = app
        .http
        .request(method, url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .json(body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req
        .send()
        .await
        .map_err(|e| SendError::Unreachable(e.into()))?;
    let status = res.status();
    let value: Value = res.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        Ok(value)
    } else if status.is_server_error() {
        Err(SendError::Unreachable(anyhow!("{url}: {status}")))
    } else {
        let msg = value["message"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("{url}: {status}"));
        Err(SendError::Refused(msg))
    }
}

/// Pairs with a Réplique server using a code from its website, and returns
/// the Storage Box login it set up for partage's key.
pub async fn pair(app: &Arc<App>, url: &str, code: &str) -> anyhow::Result<BoxConfig> {
    let url = url.trim().trim_end_matches('/');
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("the Réplique address must start with https://");
    }
    let body = json!({
        "code": code.trim(),
        "device_name": "partage",
        "kind": "partage",
        "ssh_public_key": crate::remote::public_line(&app.drive.ssh_key),
        "client_version": format!("partage {}", env!("CARGO_PKG_VERSION")),
    });
    let r = match send(
        app,
        reqwest::Method::POST,
        &format!("{url}/api/v1/pair"),
        None,
        &body,
    )
    .await
    {
        Ok(v) => v,
        Err(SendError::Refused(m)) => bail!("{m}"),
        Err(SendError::Unreachable(e)) => return Err(e.context("Réplique did not answer")),
    };
    let s = &r["storage"];
    let field = |v: &Value, name: &str| {
        v[name]
            .as_str()
            .map(str::to_string)
            .with_context(|| format!("Réplique's reply has no {name}"))
    };
    let host_key = s["known_hosts"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            match f.as_slice() {
                [alg, b64] => format!("{alg} {b64}"),
                [_, alg, b64, ..] => format!("{alg} {b64}"),
                _ => String::new(),
            }
        })
        .unwrap_or_default();
    let cfg = BoxConfig {
        host: field(s, "host")?,
        port: s["port"].as_u64().unwrap_or(23) as u16,
        user: field(s, "username")?,
        path: field(s, "repo_path")?,
        host_key,
    };
    app.db.set_setting("replique_url", url)?;
    app.db
        .set_setting("replique_token", &field(&r, "report_token")?)?;
    app.db.set_setting(
        "replique_device_id",
        &field(&r, "device_id").unwrap_or_default(),
    )?;
    Ok(cfg)
}

/// Declares the `drive` job: a backup run at least once a day (the sync
/// loop makes one even when nothing changed).
pub async fn declare_job(app: &App) {
    let Some(s) = server(app) else { return };
    let body = json!({
        "kind": "backup",
        "schedule": "daily",
        "expected_interval_secs": 86400,
        "rules": { "paths": ["Drive"] },
    });
    let url = format!("{}/api/v1/jobs/{JOB}", s.url);
    match send(app, reqwest::Method::PUT, &url, Some(&s.token), &body).await {
        Ok(_) => {}
        Err(SendError::Refused(m)) => tracing::error!("Réplique refused the job: {m}"),
        Err(SendError::Unreachable(e)) => tracing::warn!("declaring the job to Réplique: {e:#}"),
    }
}

/// Where a run stands, for the reports and the settings page.
#[derive(Default)]
struct Current {
    client_run_id: String,
    started_at: i64,
    run_id: Option<String>,
    files_new: u64,
    data_added: u64,
    snapshot_id: Option<String>,
    errors: Vec<String>,
}

#[derive(Default)]
pub struct Runs {
    state: Mutex<(u32, Option<Current>)>,
    /// The last backup run's end and whether it went well.
    pub last: Mutex<Option<(i64, bool)>>,
}

/// Holding one keeps a backup run open; the run ends when the last guard
/// goes. The sync loop and uploads straight to the box hold one each.
pub struct RunGuard {
    app: Arc<App>,
}

pub fn begin(app: &Arc<App>) -> RunGuard {
    let mut st = app.drive.runs.state.lock().unwrap();
    st.0 += 1;
    if st.0 == 1 {
        let cur = Current {
            client_run_id: uuid(),
            started_at: now(),
            ..Default::default()
        };
        if let Some(s) = server(app) {
            let app = app.clone();
            let body = start_body(&cur, "backup");
            tokio::spawn(async move {
                let url = format!("{}/api/v1/runs", s.url);
                if let Ok(v) = send(&app, reqwest::Method::POST, &url, Some(&s.token), &body).await
                    && let Some(id) = v["run_id"].as_str()
                {
                    let mut st = app.drive.runs.state.lock().unwrap();
                    if let Some(c) = st.1.as_mut()
                        && body["client_run_id"] == c.client_run_id.as_str()
                    {
                        c.run_id = Some(id.to_string());
                    }
                }
            });
        }
        st.1 = Some(cur);
    }
    RunGuard { app: app.clone() }
}

impl RunGuard {
    fn with(&self, f: impl FnOnce(&mut Current)) {
        if let Some(c) = self.app.drive.runs.state.lock().unwrap().1.as_mut() {
            f(c);
        }
    }

    pub fn added(&self, files: u64, bytes: u64) {
        self.with(|c| {
            c.files_new += files;
            c.data_added += bytes;
        });
    }

    pub fn manifest(&self, hash: &str) {
        self.with(|c| c.snapshot_id = Some(hash.to_string()));
    }

    pub fn error(&self, msg: String) {
        self.with(|c| {
            if c.errors.len() < 50 {
                c.errors.push(msg);
            }
        });
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let cur = {
            let mut st = self.app.drive.runs.state.lock().unwrap();
            st.0 -= 1;
            if st.0 > 0 {
                return;
            }
            st.1.take()
        };
        let Some(cur) = cur else { return };
        let ok = cur.errors.is_empty();
        *self.app.drive.runs.last.lock().unwrap() = Some((now(), ok));
        let finished = now();
        let finish = json!({
            "status": if ok { "success" } else { "failed" },
            "exit_code": if ok { 0 } else { 1 },
            "finished_at": rfc3339(finished),
            "summary": {
                "files_new": cur.files_new,
                "data_added": cur.data_added,
                "data_added_packed": cur.data_added,
                "total_duration": (finished - cur.started_at) as f64,
                "backup_start": rfc3339(cur.started_at),
                "backup_end": rfc3339(finished),
                "snapshot_id": cur.snapshot_id,
            },
            "errors": cur.errors.iter().map(|m| json!({ "message": m })).collect::<Vec<_>>(),
            "error_count": cur.errors.len(),
            "unreadable_paths": [],
        });
        let start = start_body(&cur, "backup");
        let app = self.app.clone();
        tokio::spawn(async move { deliver(&app, start, cur.run_id, finish).await });
    }
}

fn start_body(cur: &Current, kind: &str) -> Value {
    json!({
        "client_run_id": cur.client_run_id,
        "job": JOB,
        "kind": kind,
        "started_at": rfc3339(cur.started_at),
        "client_version": format!("partage {}", env!("CARGO_PKG_VERSION")),
    })
}

/// Reports a finished check run.
pub async fn report_check(
    app: &App,
    started_at: i64,
    manifests: Vec<String>,
    errors: Vec<String>,
    subset: String,
) {
    if server(app).is_none() {
        return;
    }
    let cur = Current {
        client_run_id: uuid(),
        started_at,
        ..Default::default()
    };
    let ok = errors.is_empty();
    let finish = json!({
        "status": if ok { "success" } else { "failed" },
        "exit_code": if ok { 0 } else { 1 },
        "finished_at": rfc3339(now()),
        "check": {
            "read_data_subset": subset,
            "num_errors": errors.len(),
            "snapshot_ids": manifests,
        },
        "errors": errors.iter().take(50).map(|m| json!({ "message": m })).collect::<Vec<_>>(),
        "error_count": errors.len(),
        "unreadable_paths": [],
    });
    deliver(app, start_body(&cur, "check"), None, finish).await;
}

/// Sends a run's finish (and its start, if it never got through), or keeps
/// both for later.
async fn deliver(app: &App, start: Value, run_id: Option<String>, finish: Value) {
    let Some(s) = server(app) else { return };
    let entry = json!({ "start": start, "finish": finish });
    if let Some(id) = run_id {
        let url = format!("{}/api/v1/runs/{id}/finish", s.url);
        match send(app, reqwest::Method::POST, &url, Some(&s.token), &finish).await {
            Ok(_) => return,
            Err(SendError::Refused(m)) => {
                tracing::error!("Réplique refused a run's finish: {m}");
                return;
            }
            Err(SendError::Unreachable(_)) => {}
        }
    } else if replay_one(app, &s, &entry).await.is_ok() {
        return;
    }
    if let Err(e) = app.db.spool_push(&entry.to_string()) {
        tracing::error!("keeping a report for Réplique: {e:#}");
    }
}

/// Start (idempotent thanks to its `client_run_id`) then finish.
async fn replay_one(app: &App, s: &Server, entry: &Value) -> Result<(), ()> {
    let url = format!("{}/api/v1/runs", s.url);
    let run_id = match send(
        app,
        reqwest::Method::POST,
        &url,
        Some(&s.token),
        &entry["start"],
    )
    .await
    {
        Ok(v) => v["run_id"].as_str().unwrap_or_default().to_string(),
        Err(SendError::Refused(m)) => {
            tracing::error!("Réplique refused a run's start, dropping it: {m}");
            return Ok(());
        }
        Err(SendError::Unreachable(_)) => return Err(()),
    };
    let url = format!("{}/api/v1/runs/{run_id}/finish", s.url);
    match send(
        app,
        reqwest::Method::POST,
        &url,
        Some(&s.token),
        &entry["finish"],
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(SendError::Refused(m)) => {
            tracing::error!("Réplique refused a run's finish, dropping it: {m}");
            Ok(())
        }
        Err(SendError::Unreachable(_)) => Err(()),
    }
}

/// Sends the reports that waited, oldest first, until one fails.
pub async fn replay_spool(app: &App) {
    let Some(s) = server(app) else { return };
    let Ok(entries) = app.db.spool() else { return };
    for (id, body) in entries {
        let Ok(entry) = serde_json::from_str::<Value>(&body) else {
            let _ = app.db.spool_delete(id);
            continue;
        };
        if replay_one(app, &s, &entry).await.is_err() {
            return;
        }
        let _ = app.db.spool_delete(id);
    }
}

fn uuid() -> String {
    let mut b: [u8; 16] = rand::random();
    // Version 4, RFC 4122 variant.
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = crate::crypt::hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}
