//! SQLite: files, upload links, sessions and settings.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::{path::Path, sync::Mutex};

pub struct Db(Mutex<Connection>);

pub struct File {
    pub id: String,
    pub name: String,
    pub content_type: String,
    /// Size of the original file.
    pub size: u64,
    /// Size on disk, compressed or not.
    pub stored: u64,
    pub compressed: bool,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub downloads: u64,
    pub deleted_at: Option<i64>,
    /// The upload link it came through, if any.
    pub invite: Option<String>,
}

pub struct Invite {
    pub token: String,
    pub note: String,
    pub max_bytes: u64,
    pub created_at: i64,
    /// Until when the link can be used.
    pub expires_at: i64,
    /// How long the uploaded file is kept, in seconds; `None` for ever.
    pub file_ttl: Option<i64>,
    /// An upload through it is in progress.
    pub claimed: bool,
    pub used_at: Option<i64>,
    pub revoked: bool,
    pub file_id: Option<String>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size INTEGER NOT NULL,
    stored INTEGER NOT NULL,
    compressed INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    downloads INTEGER NOT NULL DEFAULT 0,
    deleted_at INTEGER,
    invite TEXT
);
CREATE TABLE IF NOT EXISTS invites (
    token TEXT PRIMARY KEY,
    note TEXT NOT NULL,
    max_bytes INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    file_ttl INTEGER,
    claimed INTEGER NOT NULL DEFAULT 0,
    used_at INTEGER,
    revoked INTEGER NOT NULL DEFAULT 0,
    file_id TEXT
);
CREATE TABLE IF NOT EXISTS sessions (
    token TEXT PRIMARY KEY,
    expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

const FILE_COLS: &str = "id, name, content_type, size, stored, compressed, created_at, \
                         expires_at, downloads, deleted_at, invite";

const INVITE_COLS: &str = "token, note, max_bytes, created_at, expires_at, file_ttl, claimed, \
                           used_at, revoked, file_id";

fn file(r: &Row) -> rusqlite::Result<File> {
    Ok(File {
        id: r.get(0)?,
        name: r.get(1)?,
        content_type: r.get(2)?,
        size: r.get(3)?,
        stored: r.get(4)?,
        compressed: r.get(5)?,
        created_at: r.get(6)?,
        expires_at: r.get(7)?,
        downloads: r.get(8)?,
        deleted_at: r.get(9)?,
        invite: r.get(10)?,
    })
}

fn invite(r: &Row) -> rusqlite::Result<Invite> {
    Ok(Invite {
        token: r.get(0)?,
        note: r.get(1)?,
        max_bytes: r.get(2)?,
        created_at: r.get(3)?,
        expires_at: r.get(4)?,
        file_ttl: r.get(5)?,
        claimed: r.get(6)?,
        used_at: r.get(7)?,
        revoked: r.get(8)?,
        file_id: r.get(9)?,
    })
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        // Uploads in progress died with the previous process.
        conn.execute("UPDATE invites SET claimed = 0 WHERE used_at IS NULL", [])?;
        Ok(Self(Mutex::new(conn)))
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn insert_file(&self, f: &File) -> Result<()> {
        self.conn().execute(
            &format!("INSERT INTO files ({FILE_COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"),
            params![
                f.id,
                f.name,
                f.content_type,
                f.size,
                f.stored,
                f.compressed,
                f.created_at,
                f.expires_at,
                f.downloads,
                f.deleted_at,
                f.invite
            ],
        )?;
        Ok(())
    }

    pub fn file(&self, id: &str) -> Result<Option<File>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {FILE_COLS} FROM files WHERE id = ?"),
                [id],
                file,
            )
            .optional()?)
    }

    /// Files still downloadable, newest first.
    pub fn live_files(&self) -> Result<Vec<File>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {FILE_COLS} FROM files WHERE deleted_at IS NULL ORDER BY created_at DESC"
        ))?;
        Ok(stmt.query_map([], file)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn expired_files(&self, now: i64) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id FROM files WHERE deleted_at IS NULL AND expires_at IS NOT NULL AND expires_at <= ?",
        )?;
        Ok(stmt
            .query_map([now], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Keeps the row, so the link can say the file is gone.
    pub fn mark_deleted(&self, id: &str, now: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE files SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL",
            params![now, id],
        )?;
        Ok(())
    }

    pub fn count_download(&self, id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE files SET downloads = downloads + 1 WHERE id = ?",
            [id],
        )?;
        Ok(())
    }

    /// Bytes on disk taken by files.
    pub fn usage(&self) -> Result<u64> {
        Ok(self.conn().query_row(
            "SELECT COALESCE(SUM(stored), 0) FROM files WHERE deleted_at IS NULL",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT value FROM settings WHERE key = ?", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO settings (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    pub fn delete_setting(&self, key: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM settings WHERE key = ?", [key])?;
        Ok(())
    }

    pub fn create_session(&self, token: &str, expires_at: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO sessions (token, expires_at) VALUES (?, ?)",
            params![token, expires_at],
        )?;
        Ok(())
    }

    pub fn session_valid(&self, token: &str, now: i64) -> bool {
        self.conn()
            .query_row(
                "SELECT 1 FROM sessions WHERE token = ? AND expires_at > ?",
                params![token, now],
                |_| Ok(()),
            )
            .is_ok()
    }

    pub fn delete_session(&self, token: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM sessions WHERE token = ?", [token])?;
        Ok(())
    }

    pub fn purge_sessions(&self, now: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM sessions WHERE expires_at <= ?", [now])?;
        Ok(())
    }

    pub fn insert_invite(&self, i: &Invite) -> Result<()> {
        self.conn().execute(
            &format!("INSERT INTO invites ({INVITE_COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"),
            params![
                i.token,
                i.note,
                i.max_bytes,
                i.created_at,
                i.expires_at,
                i.file_ttl,
                i.claimed,
                i.used_at,
                i.revoked,
                i.file_id
            ],
        )?;
        Ok(())
    }

    pub fn invite(&self, token: &str) -> Result<Option<Invite>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {INVITE_COLS} FROM invites WHERE token = ?"),
                [token],
                invite,
            )
            .optional()?)
    }

    pub fn recent_invites(&self) -> Result<Vec<Invite>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {INVITE_COLS} FROM invites ORDER BY created_at DESC LIMIT 50"
        ))?;
        Ok(stmt
            .query_map([], invite)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Reserves the link for one upload; false if it is not usable (or
    /// already in use).
    pub fn claim_invite(&self, token: &str, now: i64) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE invites SET claimed = 1 WHERE token = ? AND claimed = 0
             AND used_at IS NULL AND revoked = 0 AND expires_at > ?",
            params![token, now],
        )? == 1)
    }

    /// Frees the link again after a failed upload.
    pub fn release_invite(&self, token: &str) -> Result<()> {
        self.conn()
            .execute("UPDATE invites SET claimed = 0 WHERE token = ?", [token])?;
        Ok(())
    }

    pub fn complete_invite(&self, token: &str, file_id: &str, now: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE invites SET claimed = 0, used_at = ?, file_id = ? WHERE token = ?",
            params![now, file_id, token],
        )?;
        Ok(())
    }

    pub fn revoke_invite(&self, token: &str) -> Result<()> {
        self.conn()
            .execute("UPDATE invites SET revoked = 1 WHERE token = ?", [token])?;
        Ok(())
    }
}
