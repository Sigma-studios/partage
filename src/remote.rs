//! The Storage Box, over SFTP, in-process (no `ssh` binary in the image).
//!
//! Layout under the configured folder (`/home/restic` when paired with
//! Réplique, whose read-only twin sees that folder as its home):
//!
//! ```text
//! objects/ab/ab12…   one per file version, named by the SHA-256 of its bytes
//! manifests/cd34…    the whole tree at one moment, same naming
//! tmp/….partial      uploads in progress
//! ```

use anyhow::{Context, anyhow, bail};
use russh::client::{self, AuthResult};
use russh::keys::ssh_key::{self, LineEnding, PrivateKey, PublicKey, private::Ed25519Keypair};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::{SftpSession, fs::File};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

/// Where the Drive's objects go. Saved as JSON in the settings.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BoxConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// The folder holding `objects/`, `manifests/` and `tmp/`.
    pub path: String,
    /// The server's key (`alg base64`); empty until the first connection,
    /// which then remembers it.
    #[serde(default)]
    pub host_key: String,
}

impl BoxConfig {
    pub fn object_path(&self, hash: &str) -> String {
        format!("{}/objects/{}/{hash}", self.path, &hash[..2])
    }

    pub fn manifest_path(&self, hash: &str) -> String {
        format!("{}/manifests/{hash}", self.path)
    }

    pub fn label(&self) -> String {
        format!("{}@{}:{}{}", self.user, self.host, self.port, self.path)
    }
}

/// The SSH key partage logs in with, created on first use.
pub fn load_or_create_key(path: &Path) -> anyhow::Result<PrivateKey> {
    match std::fs::read_to_string(path) {
        Ok(s) => PrivateKey::from_openssh(s.trim()).context("reading the SSH key"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let seed: [u8; 32] = rand::random();
            let mut key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
            key.set_comment("partage");
            let text = key
                .to_openssh(LineEnding::LF)
                .map_err(|e| anyhow!("encoding the SSH key: {e}"))?;
            crate::crypt::write_private(path, text.as_bytes())?;
            Ok(key)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// A public key as one `authorized_keys` line.
pub fn public_line(key: &PrivateKey) -> String {
    key.public_key()
        .to_openssh()
        .map(|k| format!("{k} partage"))
        .unwrap_or_default()
}

fn key_line(key: &PublicKey) -> String {
    key.to_openssh()
        .unwrap_or_default()
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

struct Handler {
    expected: Option<PublicKey>,
    seen: Arc<Mutex<Option<PublicKey>>>,
}

impl client::Handler for Handler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return Ok(false);
        };
        *self.seen.lock().unwrap() = Some(key.clone());
        Ok(match &self.expected {
            None => true,
            Some(k) if k.key_data() == key.key_data() => true,
            Some(_) => {
                tracing::error!(
                    fingerprint = %key.fingerprint(ssh_key::HashAlg::Sha256),
                    "the Storage Box's host key changed: refusing to connect"
                );
                false
            }
        })
    }
}

/// One SSH connection, reused for every SFTP session until it breaks.
pub struct Remote {
    pub cfg: BoxConfig,
    key: PrivateKey,
    conn: tokio::sync::Mutex<Option<Arc<client::Handle<Handler>>>>,
    /// The host key seen at the first connection, for the caller to save.
    pub seen_host_key: Mutex<Option<String>>,
}

impl Remote {
    pub fn new(cfg: BoxConfig, key: PrivateKey) -> Self {
        Self {
            cfg,
            key,
            conn: Default::default(),
            seen_host_key: Default::default(),
        }
    }

    async fn connect(&self) -> anyhow::Result<Arc<client::Handle<Handler>>> {
        let expected = if self.cfg.host_key.trim().is_empty() {
            None
        } else {
            Some(PublicKey::from_openssh(self.cfg.host_key.trim()).context("the saved host key")?)
        };
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(300)),
            keepalive_interval: Some(Duration::from_secs(30)),
            ..Default::default()
        });
        let seen = Arc::new(Mutex::new(None));
        let handler = Handler {
            expected,
            seen: seen.clone(),
        };
        let addr = (self.cfg.host.as_str(), self.cfg.port);
        let mut handle = tokio::time::timeout(
            Duration::from_secs(30),
            client::connect(config, addr, handler),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "connecting to {}:{} timed out",
                self.cfg.host,
                self.cfg.port
            )
        })?
        .with_context(|| format!("SSH to {}:{}", self.cfg.host, self.cfg.port))?;
        let key = PrivateKeyWithHashAlg::new(Arc::new(self.key.clone()), None);
        let result = handle.authenticate_publickey(&self.cfg.user, key).await?;
        if !matches!(result, AuthResult::Success) {
            bail!("{} refused partage's key", self.cfg.label());
        }
        if let Some(k) = seen.lock().unwrap().as_ref() {
            *self.seen_host_key.lock().unwrap() = Some(key_line(k));
        }
        Ok(Arc::new(handle))
    }

    /// A fresh SFTP session on the shared connection, reconnecting once if the
    /// connection died.
    pub async fn sftp(&self) -> anyhow::Result<SftpSession> {
        let mut guard = self.conn.lock().await;
        for attempt in 0..2 {
            let handle = match guard.as_ref() {
                Some(h) if !h.is_closed() => h.clone(),
                _ => {
                    let h = self.connect().await?;
                    *guard = Some(h.clone());
                    h
                }
            };
            match open_sftp(&handle).await {
                Ok(s) => return Ok(s),
                Err(e) if attempt == 0 => {
                    tracing::warn!("SFTP session failed, reconnecting: {e:#}");
                    *guard = None;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    /// Creates the folders the Drive writes to.
    pub async fn prepare(&self) -> anyhow::Result<()> {
        let s = self.sftp().await?;
        let base = self.cfg.path.trim_end_matches('/');
        let mut path = String::new();
        for part in base.split('/').filter(|p| !p.is_empty()) {
            path = format!("{path}/{part}");
            ensure_dir(&s, &path).await?;
        }
        for sub in ["objects", "manifests", "tmp"] {
            ensure_dir(&s, &format!("{base}/{sub}")).await?;
        }
        Ok(())
    }

    /// Writes `name` (relative to the Drive's folder) from a stream of
    /// buffers, through a temporary file renamed at the end, so a name only
    /// ever holds a whole object.
    pub async fn writer(&self) -> anyhow::Result<RemoteWriter> {
        let s = self.sftp().await?;
        let tmp = format!("{}/tmp/{}.partial", self.cfg.path, crate::token(24));
        let file = s
            .create(tmp.clone())
            .await
            .map_err(|e| anyhow!("creating {tmp}: {e}"))?;
        Ok(RemoteWriter { s, file, tmp })
    }

    pub async fn open(&self, path: &str) -> anyhow::Result<File> {
        let s = self.sftp().await?;
        // The file keeps the session's channel open while it is read.
        s.open(path.to_string())
            .await
            .map_err(|e| anyhow!("opening {path}: {e}"))
    }

    pub async fn read(&self, path: &str) -> anyhow::Result<Vec<u8>> {
        let s = self.sftp().await?;
        s.read(path.to_string())
            .await
            .map_err(|e| anyhow!("reading {path}: {e}"))
    }

    /// Deletes a file; one already gone is fine.
    pub async fn remove(&self, path: &str) -> anyhow::Result<()> {
        let s = self.sftp().await?;
        match s.remove_file(path.to_string()).await {
            Ok(()) => Ok(()),
            Err(russh_sftp::client::error::Error::Status(st))
                if st.status_code == russh_sftp::protocol::StatusCode::NoSuchFile =>
            {
                Ok(())
            }
            Err(e) => Err(anyhow!("deleting {path}: {e}")),
        }
    }

    /// Files under a folder of the Drive (`objects`, `manifests`, `tmp`), with
    /// their paths relative to it, sizes and modification times.
    pub async fn list(&self, sub: &str) -> anyhow::Result<Vec<(String, u64, u32)>> {
        let s = self.sftp().await?;
        let root = format!("{}/{sub}", self.cfg.path);
        let mut out = Vec::new();
        let mut stack = vec![String::new()];
        while let Some(rel) = stack.pop() {
            let dir = if rel.is_empty() {
                root.clone()
            } else {
                format!("{root}/{rel}")
            };
            let entries = match s.read_dir(dir.clone()).await {
                Ok(e) => e,
                Err(e) if rel.is_empty() => return Err(anyhow!("listing {dir}: {e}")),
                Err(e) => {
                    tracing::warn!("listing {dir}: {e}");
                    continue;
                }
            };
            for e in entries {
                let name = e.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                let child = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                let m = e.metadata();
                if m.is_dir() {
                    stack.push(child);
                } else {
                    out.push((child, m.len(), m.mtime.unwrap_or(0)));
                }
            }
        }
        Ok(out)
    }
}

async fn open_sftp(handle: &client::Handle<Handler>) -> anyhow::Result<SftpSession> {
    let channel = handle.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| anyhow!("starting SFTP: {e}"))
}

async fn ensure_dir(s: &SftpSession, path: &str) -> anyhow::Result<()> {
    if s.try_exists(path.to_string()).await.unwrap_or(false) {
        return Ok(());
    }
    if let Err(e) = s.create_dir(path.to_string()).await {
        // Created meanwhile by another task.
        if !s.try_exists(path.to_string()).await.unwrap_or(false) {
            return Err(anyhow!("creating {path}: {e}"));
        }
    }
    Ok(())
}

/// An object being written to the box.
pub struct RemoteWriter {
    s: SftpSession,
    file: File,
    tmp: String,
}

impl RemoteWriter {
    pub async fn write(&mut self, buf: &[u8]) -> anyhow::Result<()> {
        self.file
            .write_all(buf)
            .await
            .with_context(|| format!("writing {}", self.tmp))
    }

    /// Puts the finished object at `dest` (its folder is created if needed).
    pub async fn commit(mut self, dest: &str) -> anyhow::Result<()> {
        self.file.flush().await?;
        self.file.shutdown().await?;
        if let Some((dir, _)) = dest.rsplit_once('/') {
            ensure_dir(&self.s, dir).await?;
        }
        // A name is the hash of the bytes: one already there holds the same
        // object (an upload retried after its rename went through).
        if self.s.try_exists(dest.to_string()).await.unwrap_or(false) {
            let _ = self.s.remove_file(self.tmp.clone()).await;
            return Ok(());
        }
        self.s
            .rename(self.tmp.clone(), dest.to_string())
            .await
            .map_err(|e| anyhow!("renaming {} to {dest}: {e}", self.tmp))
    }

    /// Gives up: deletes the temporary file.
    pub async fn abort(mut self) {
        let _ = self.file.shutdown().await;
        let _ = self.s.remove_file(self.tmp.clone()).await;
    }
}
