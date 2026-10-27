//! partage: share files through links that do not change, with a storage
//! limit and Discord alerts. See README.md.

mod alert;
mod db;
mod html;
mod store;
mod web;

use anyhow::{Context, bail};
use rand::{Rng, distr::Alphanumeric};
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

const DEFAULT_ADDR: &str = "0.0.0.0:8080";
const DEFAULT_LIMIT: u64 = 10_000_000_000;

pub struct Config {
    password: String,
    data: PathBuf,
    addr: String,
    /// Origin of the links handed out, like `https://files.example.com`;
    /// taken from each request when unset.
    public_url: Option<String>,
}

/// Named `var` so facade's scan lists the variables read with it.
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    fn from_env() -> anyhow::Result<Self> {
        let Some(password) = var("PARTAGE_ADMIN_PASSWORD") else {
            bail!("PARTAGE_ADMIN_PASSWORD is not set");
        };
        if password.len() < 12 {
            tracing::warn!("PARTAGE_ADMIN_PASSWORD is short: use 12 characters or more");
        }
        Ok(Self {
            password,
            data: var("PARTAGE_DATA").unwrap_or_else(|| "data".into()).into(),
            addr: var("PARTAGE_ADDR").unwrap_or_else(|| DEFAULT_ADDR.into()),
            public_url: var("PARTAGE_PUBLIC_URL").map(|u| u.trim_end_matches('/').to_string()),
        })
    }
}

pub struct App {
    cfg: Config,
    db: db::Db,
    http: reqwest::Client,
    /// Bytes stored by finished uploads.
    used: AtomicU64,
    /// Bytes written by uploads in progress.
    inflight: AtomicU64,
    limit: AtomicU64,
    /// Discord webhook URL, set from the dashboard.
    webhook: RwLock<Option<String>>,
    alert_lock: tokio::sync::Mutex<()>,
    last_blocked_alert: Mutex<Option<Instant>>,
    login_failures: Mutex<HashMap<String, (u32, Instant)>>,
}

impl App {
    fn used(&self) -> u64 {
        self.used.load(Relaxed)
    }

    fn limit(&self) -> u64 {
        self.limit.load(Relaxed)
    }

    fn webhook(&self) -> Option<String> {
        self.webhook.read().unwrap().clone()
    }

    fn refresh_usage(&self) {
        match self.db.usage() {
            Ok(n) => self.used.store(n, Relaxed),
            Err(e) => tracing::error!("reading usage: {e:#}"),
        }
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn token(len: usize) -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

/// `partage healthcheck`: exits with success when the server answers.
fn healthcheck() -> anyhow::Result<()> {
    let addr = var("PARTAGE_ADDR").unwrap_or_else(|| DEFAULT_ADDR.into());
    let port: u16 = addr.rsplit(':').next().unwrap_or_default().parse()?;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut reply = String::new();
    s.read_to_string(&mut reply)?;
    anyhow::ensure!(
        reply.split_whitespace().nth(1) == Some("200"),
        "unhealthy: {reply}"
    );
    Ok(())
}

async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck();
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "partage=info".into()),
        )
        .init();
    let cfg = Config::from_env()?;
    std::fs::create_dir_all(cfg.data.join("files"))
        .with_context(|| format!("creating {}", cfg.data.display()))?;
    // Leftovers of uploads cut off by a restart.
    let _ = std::fs::remove_dir_all(cfg.data.join("tmp"));
    std::fs::create_dir_all(cfg.data.join("tmp"))?;
    let db = db::Db::open(&cfg.data.join("partage.db"))?;
    let limit = db
        .setting("storage_limit")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_LIMIT);
    let webhook = db.setting("discord_webhook")?;
    let addr = cfg.addr.clone();
    let app = Arc::new(App {
        cfg,
        db,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?,
        used: AtomicU64::new(0),
        inflight: AtomicU64::new(0),
        limit: AtomicU64::new(limit),
        webhook: RwLock::new(webhook),
        alert_lock: Default::default(),
        last_blocked_alert: Default::default(),
        login_failures: Default::default(),
    });
    app.refresh_usage();
    tracing::info!(
        "{} of {} used, data in {}",
        html::fmt_bytes(app.used()),
        html::fmt_bytes(app.limit()),
        app.cfg.data.display()
    );
    tokio::spawn(store::sweep(app.clone()));

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("listening on {addr}"))?;
    tracing::info!("listening on {addr}");
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}
