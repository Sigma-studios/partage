//! The Drive's tables: folders and files, their versions, the objects holding
//! them, share links, manifests, and reports waiting for Réplique.

use crate::db::Db;
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

pub const ROOT: &str = "root";

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS drive_nodes (
    id TEXT PRIMARY KEY,
    parent TEXT,
    name TEXT NOT NULL,
    dir INTEGER NOT NULL,
    version TEXT,
    size INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    modified_at INTEGER NOT NULL,
    trashed_at INTEGER,
    trash_root INTEGER NOT NULL DEFAULT 0,
    trashed_by TEXT,
    limit_bytes INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS drive_names ON drive_nodes (parent, name) WHERE trashed_at IS NULL;
CREATE INDEX IF NOT EXISTS drive_parent ON drive_nodes (parent);
CREATE TABLE IF NOT EXISTS drive_versions (
    id TEXT PRIMARY KEY,
    node TEXT NOT NULL,
    object TEXT NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    content_type TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    replaced_at INTEGER,
    added_by TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS drive_versions_node ON drive_versions (node);
CREATE INDEX IF NOT EXISTS drive_versions_object ON drive_versions (object);
CREATE TABLE IF NOT EXISTS drive_objects (
    hash TEXT PRIMARY KEY,
    sealed INTEGER NOT NULL,
    local INTEGER NOT NULL,
    remote INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    last_access INTEGER NOT NULL,
    first_ref INTEGER,
    last_ref INTEGER
);
CREATE TABLE IF NOT EXISTS drive_manifests (
    hash TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    sealed INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS drive_links (
    token TEXT PRIMARY KEY,
    node TEXT NOT NULL,
    mode TEXT NOT NULL,
    note TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    revoked INTEGER NOT NULL DEFAULT 0,
    uses INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS replique_spool (
    id INTEGER PRIMARY KEY,
    body TEXT NOT NULL
);
";

pub fn migrate(conn: &Connection, now: i64) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    conn.execute(
        "INSERT OR IGNORE INTO drive_nodes (id, parent, name, dir, created_at, modified_at) \
         VALUES (?, NULL, '', 1, ?, ?)",
        params![ROOT, now, now],
    )?;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: String,
    pub parent: Option<String>,
    pub name: String,
    pub dir: bool,
    pub version: Option<String>,
    pub size: u64,
    pub created_at: i64,
    pub modified_at: i64,
    pub trashed_at: Option<i64>,
    pub trash_root: bool,
    pub trashed_by: Option<String>,
    pub limit_bytes: Option<u64>,
    /// From the current version.
    pub content_type: Option<String>,
}

const NODE_COLS: &str = "n.id, n.parent, n.name, n.dir, n.version, n.size, n.created_at, \
     n.modified_at, n.trashed_at, n.trash_root, n.trashed_by, n.limit_bytes, v.content_type";
const NODE_FROM: &str = "drive_nodes n LEFT JOIN drive_versions v ON v.id = n.version";

fn node(r: &Row) -> rusqlite::Result<Node> {
    Ok(Node {
        id: r.get(0)?,
        parent: r.get(1)?,
        name: r.get(2)?,
        dir: r.get(3)?,
        version: r.get(4)?,
        size: r.get(5)?,
        created_at: r.get(6)?,
        modified_at: r.get(7)?,
        trashed_at: r.get(8)?,
        trash_root: r.get(9)?,
        trashed_by: r.get(10)?,
        limit_bytes: r.get(11)?,
        content_type: r.get(12)?,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// One file, to download.
    File,
    Read,
    /// Add files without seeing what is there.
    Add,
    ReadAdd,
    /// Read, add, and delete (to the trash).
    Full,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "file" => Self::File,
            "read" => Self::Read,
            "add" => Self::Add,
            "read_add" => Self::ReadAdd,
            "full" => Self::Full,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Read => "read",
            Self::Add => "add",
            Self::ReadAdd => "read_add",
            Self::Full => "full",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::File => "download",
            Self::Read => "view and download",
            Self::Add => "add only",
            Self::ReadAdd => "view, download and add",
            Self::Full => "view, download, add and delete",
        }
    }

    pub fn can_read(self) -> bool {
        matches!(self, Self::File | Self::Read | Self::ReadAdd | Self::Full)
    }

    pub fn can_add(self) -> bool {
        matches!(self, Self::Add | Self::ReadAdd | Self::Full)
    }

    pub fn can_delete(self) -> bool {
        self == Self::Full
    }
}

#[derive(Clone, Debug)]
pub struct Link {
    pub token: String,
    pub node: String,
    pub mode: Mode,
    pub note: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub revoked: bool,
    pub uses: u64,
}

impl Link {
    pub fn usable(&self, now: i64) -> bool {
        !self.revoked && self.expires_at.is_none_or(|t| t > now)
    }
}

fn link(r: &Row) -> rusqlite::Result<Link> {
    let mode: String = r.get(2)?;
    Ok(Link {
        token: r.get(0)?,
        node: r.get(1)?,
        mode: Mode::parse(&mode).unwrap_or(Mode::Read),
        note: r.get(3)?,
        created_at: r.get(4)?,
        expires_at: r.get(5)?,
        revoked: r.get(6)?,
        uses: r.get(7)?,
    })
}

const LINK_COLS: &str = "token, node, mode, note, created_at, expires_at, revoked, uses";

#[derive(Clone, Debug)]
pub struct Version {
    pub id: String,
    pub node: String,
    pub object: String,
    pub size: u64,
    pub sha256: String,
    pub content_type: String,
    pub created_at: i64,
    pub replaced_at: Option<i64>,
    pub added_by: String,
}

fn version(r: &Row) -> rusqlite::Result<Version> {
    Ok(Version {
        id: r.get(0)?,
        node: r.get(1)?,
        object: r.get(2)?,
        size: r.get(3)?,
        sha256: r.get(4)?,
        content_type: r.get(5)?,
        created_at: r.get(6)?,
        replaced_at: r.get(7)?,
        added_by: r.get(8)?,
    })
}

const VERSION_COLS: &str =
    "id, node, object, size, sha256, content_type, created_at, replaced_at, added_by";

/// The first and last manifests (by time) that listed an object.
pub type Span = (i64, i64);

#[derive(Clone, Debug)]
pub struct Object {
    pub hash: String,
    pub sealed: u64,
    pub local: bool,
    pub remote: bool,
}

fn object(r: &Row) -> rusqlite::Result<Object> {
    Ok(Object {
        hash: r.get(0)?,
        sealed: r.get(1)?,
        local: r.get(2)?,
        remote: r.get(3)?,
    })
}

/// A finished upload, ready to enter the tree.
pub struct NewFile<'a> {
    pub parent: &'a str,
    pub name: &'a str,
    pub object: &'a str,
    pub sealed: u64,
    pub size: u64,
    pub sha256: &'a str,
    pub content_type: &'a str,
    /// Where the object is.
    pub local: bool,
    pub remote: bool,
    /// Replaces a file of the same name (a new version) rather than taking
    /// the next free name.
    pub replace: bool,
    pub added_by: &'a str,
    pub now: i64,
}

/// `name (2).ext`, `name (3).ext`… for the `n`th try.
pub fn numbered(name: &str, n: u32) -> String {
    if n <= 1 {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

fn free_name(tx: &Transaction, parent: &str, name: &str) -> Result<String> {
    for n in 1..10_000 {
        let candidate = numbered(name, n);
        let taken: bool = tx.query_row(
            "SELECT COUNT(*) FROM drive_nodes WHERE parent = ? AND name = ? AND trashed_at IS NULL",
            params![parent, candidate],
            |r| r.get(0),
        )?;
        if !taken {
            return Ok(candidate);
        }
    }
    bail!("no free name for {name}")
}

impl Db {
    pub fn node(&self, id: &str) -> Result<Option<Node>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {NODE_COLS} FROM {NODE_FROM} WHERE n.id = ?"),
                [id],
                node,
            )
            .optional()?)
    }

    /// What a folder holds (not in the trash), folders first, by name.
    pub fn children(&self, parent: &str) -> Result<Vec<Node>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {NODE_COLS} FROM {NODE_FROM} WHERE n.parent = ? AND n.trashed_at IS NULL \
             ORDER BY n.dir DESC, n.name COLLATE NOCASE"
        ))?;
        Ok(stmt
            .query_map([parent], node)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn child(&self, parent: &str, name: &str) -> Result<Option<Node>> {
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {NODE_COLS} FROM {NODE_FROM} WHERE n.parent = ? AND n.name = ? \
                     AND n.trashed_at IS NULL"
                ),
                params![parent, name],
                node,
            )
            .optional()?)
    }

    /// The folders from the root down to `id`, `id` included.
    pub fn ancestry(&self, id: &str) -> Result<Vec<Node>> {
        let mut out = Vec::new();
        let mut next = Some(id.to_string());
        while let Some(id) = next {
            let Some(n) = self.node(&id)? else { break };
            next = n.parent.clone();
            out.push(n);
            if out.len() > 1000 {
                bail!("folder loop at {id}");
            }
        }
        out.reverse();
        Ok(out)
    }

    /// Whether `id` is `ancestor` or somewhere under it.
    pub fn is_within(&self, id: &str, ancestor: &str) -> Result<bool> {
        Ok(self.ancestry(id)?.iter().any(|n| n.id == ancestor))
    }

    /// Every folder not in the trash, with its path, for "Move to".
    pub fn all_folders(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "WITH RECURSIVE t(id, path) AS (SELECT id, '' FROM drive_nodes WHERE id = 'root' \
             UNION ALL SELECT n.id, t.path || '/' || n.name FROM drive_nodes n JOIN t ON n.parent = t.id \
             WHERE n.dir = 1 AND n.trashed_at IS NULL) SELECT id, path FROM t ORDER BY path COLLATE NOCASE",
        )?;
        Ok(stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn create_dir(&self, parent: &str, name: &str, now: i64) -> Result<Node> {
        let id = crate::token(16);
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let taken: bool = tx.query_row(
            "SELECT COUNT(*) FROM drive_nodes WHERE parent = ? AND name = ? AND trashed_at IS NULL",
            params![parent, name],
            |r| r.get(0),
        )?;
        if taken {
            bail!("“{name}” already exists here");
        }
        tx.execute(
            "INSERT INTO drive_nodes (id, parent, name, dir, created_at, modified_at) VALUES (?, ?, ?, 1, ?, ?)",
            params![id, parent, name, now, now],
        )?;
        tx.commit()?;
        drop(conn);
        Ok(self.node(&id)?.expect("just created"))
    }

    /// The folder at `parts` under `parent`, created as needed. A file in the
    /// way is an error.
    pub fn ensure_dirs(&self, parent: &str, parts: &[&str], now: i64) -> Result<String> {
        let mut current = parent.to_string();
        for part in parts {
            current = match self.child(&current, part)? {
                Some(n) if n.dir => n.id,
                Some(_) => bail!("“{part}” is a file, not a folder"),
                None => match self.create_dir(&current, part, now) {
                    Ok(n) => n.id,
                    // Created by a parallel upload meanwhile.
                    Err(_) => match self.child(&current, part)? {
                        Some(n) if n.dir => n.id,
                        _ => bail!("could not create the folder “{part}”"),
                    },
                },
            };
        }
        Ok(current)
    }

    /// Puts an uploaded file into the tree, with its object, in one
    /// transaction. Returns the file's node.
    pub fn add_file(&self, f: &NewFile) -> Result<Node> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO drive_objects (hash, sealed, local, remote, created_at, last_access) \
             VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (hash) DO UPDATE SET \
             local = local OR excluded.local, remote = remote OR excluded.remote",
            params![f.object, f.sealed, f.local, f.remote, f.now, f.now],
        )?;
        let existing: Option<(String, bool)> = tx
            .query_row(
                "SELECT id, dir FROM drive_nodes WHERE parent = ? AND name = ? AND trashed_at IS NULL",
                params![f.parent, f.name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let version = crate::token(16);
        let node_id = match existing {
            Some((id, false)) if f.replace => {
                tx.execute(
                    "UPDATE drive_versions SET replaced_at = ? WHERE node = ? AND replaced_at IS NULL",
                    params![f.now, id],
                )?;
                tx.execute(
                    "UPDATE drive_nodes SET version = ?, size = ?, modified_at = ? WHERE id = ?",
                    params![version, f.size, f.now, id],
                )?;
                id
            }
            _ => {
                let name = free_name(&tx, f.parent, f.name)?;
                let id = crate::token(16);
                tx.execute(
                    "INSERT INTO drive_nodes (id, parent, name, dir, version, size, created_at, modified_at) \
                     VALUES (?, ?, ?, 0, ?, ?, ?, ?)",
                    params![id, f.parent, name, version, f.size, f.now, f.now],
                )?;
                id
            }
        };
        tx.execute(
            &format!(
                "INSERT INTO drive_versions ({VERSION_COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?)"
            ),
            params![
                version,
                node_id,
                f.object,
                f.size,
                f.sha256,
                f.content_type,
                f.now,
                f.added_by
            ],
        )?;
        touch_parent(&tx, f.parent, f.now)?;
        tx.commit()?;
        drop(conn);
        Ok(self.node(&node_id)?.expect("just written"))
    }

    pub fn rename(&self, id: &str, name: &str, now: i64) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let parent: Option<String> =
            tx.query_row("SELECT parent FROM drive_nodes WHERE id = ?", [id], |r| {
                r.get(0)
            })?;
        let Some(parent) = parent else {
            bail!("the root has no name")
        };
        let taken: bool = tx.query_row(
            "SELECT COUNT(*) FROM drive_nodes WHERE parent = ? AND name = ? AND trashed_at IS NULL AND id != ?",
            params![parent, name, id],
            |r| r.get(0),
        )?;
        if taken {
            bail!("“{name}” already exists in that folder");
        }
        tx.execute(
            "UPDATE drive_nodes SET name = ?, modified_at = ? WHERE id = ?",
            params![name, now, id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn move_node(&self, id: &str, to: &str, now: i64) -> Result<()> {
        if id == ROOT {
            bail!("the root cannot move");
        }
        if self.is_within(to, id)? {
            bail!("a folder cannot go inside itself");
        }
        let Some(dest) = self.node(to)? else {
            bail!("no such folder")
        };
        if !dest.dir || dest.trashed_at.is_some() {
            bail!("not a folder");
        }
        let Some(n) = self.node(id)? else {
            bail!("no such file")
        };
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let name = free_name(&tx, to, &n.name)?;
        tx.execute(
            "UPDATE drive_nodes SET parent = ?, name = ?, modified_at = ? WHERE id = ?",
            params![to, name, now, id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Moves a file or folder, and everything under it, to the trash.
    pub fn trash(&self, id: &str, by: &str, now: i64) -> Result<()> {
        if id == ROOT {
            bail!("the root cannot be deleted");
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "WITH RECURSIVE t(id) AS (SELECT ? UNION ALL SELECT n.id FROM drive_nodes n JOIN t ON n.parent = t.id) \
             UPDATE drive_nodes SET trashed_at = ? WHERE id IN t AND trashed_at IS NULL",
            params![id, now],
        )?;
        tx.execute(
            "UPDATE drive_nodes SET trash_root = 1, trashed_by = ? WHERE id = ?",
            params![by, id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Takes a trashed item back out, with what was trashed along with it.
    /// It goes back to its folder, or to the root if that folder is in the
    /// trash too, under a free name.
    pub fn restore(&self, id: &str) -> Result<()> {
        let Some(n) = self.node(id)? else {
            bail!("no such item")
        };
        let Some(at) = n.trashed_at.filter(|_| n.trash_root) else {
            bail!("not in the trash")
        };
        let parent_ok = match &n.parent {
            Some(p) => self.node(p)?.is_some_and(|p| p.trashed_at.is_none()),
            None => false,
        };
        let parent = if parent_ok {
            n.parent.clone().unwrap()
        } else {
            ROOT.to_string()
        };
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let name = free_name(&tx, &parent, &n.name)?;
        tx.execute(
            "WITH RECURSIVE t(id) AS (SELECT ? UNION ALL SELECT n.id FROM drive_nodes n JOIN t ON n.parent = t.id \
             WHERE n.trashed_at = ? AND n.trash_root = 0) \
             UPDATE drive_nodes SET trashed_at = NULL WHERE id IN t",
            params![id, at],
        )?;
        tx.execute(
            "UPDATE drive_nodes SET trash_root = 0, trashed_by = NULL, parent = ?, name = ? WHERE id = ?",
            params![parent, name, id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Deletes a trashed item for good (its objects go once no kept manifest
    /// holds them).
    pub fn purge(&self, id: &str) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let trashed: Option<i64> = tx
            .query_row(
                "SELECT trashed_at FROM drive_nodes WHERE id = ?",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if trashed.is_none() {
            bail!("only items in the trash can be deleted for good");
        }
        let sub = "WITH RECURSIVE t(id) AS (SELECT ? UNION ALL SELECT n.id FROM drive_nodes n JOIN t ON n.parent = t.id)";
        tx.execute(
            &format!("{sub} DELETE FROM drive_versions WHERE node IN t"),
            [id],
        )?;
        tx.execute(
            &format!("{sub} DELETE FROM drive_links WHERE node IN t"),
            [id],
        )?;
        tx.execute(
            &format!("{sub} DELETE FROM drive_nodes WHERE id IN t"),
            [id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Items deleted directly (not along with their folder), newest first.
    pub fn trash_items(&self) -> Result<Vec<Node>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {NODE_COLS} FROM {NODE_FROM} WHERE n.trash_root = 1 AND n.trashed_at IS NOT NULL \
             ORDER BY n.trashed_at DESC"
        ))?;
        Ok(stmt.query_map([], node)?.collect::<rusqlite::Result<_>>()?)
    }

    /// Size of what a trashed item holds.
    pub fn trashed_size(&self, id: &str) -> Result<u64> {
        Ok(self.conn().query_row(
            "WITH RECURSIVE t(id) AS (SELECT ? UNION ALL SELECT n.id FROM drive_nodes n JOIN t ON n.parent = t.id) \
             SELECT COALESCE(SUM(size), 0) FROM drive_nodes WHERE id IN t AND dir = 0",
            [id],
            |r| r.get(0),
        )?)
    }

    /// Trash older than `before`, and versions replaced before it, go for good.
    pub fn expire_trash_and_versions(&self, before: i64) -> Result<usize> {
        let ids: Vec<String> = {
            let conn = self.conn();
            let mut stmt = conn.prepare(
                "SELECT id FROM drive_nodes WHERE trash_root = 1 AND trashed_at IS NOT NULL AND trashed_at < ?",
            )?;
            stmt.query_map([before], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for id in &ids {
            self.purge(id)?;
        }
        let versions = self.conn().execute(
            "DELETE FROM drive_versions WHERE replaced_at IS NOT NULL AND replaced_at < ?",
            [before],
        )?;
        Ok(ids.len() + versions)
    }

    /// Current size of the files under a folder, trash excluded.
    pub fn tree_size(&self, id: &str) -> Result<u64> {
        Ok(self.conn().query_row(
            "WITH RECURSIVE t(id) AS (SELECT ? UNION ALL SELECT n.id FROM drive_nodes n JOIN t ON n.parent = t.id \
             WHERE n.trashed_at IS NULL) \
             SELECT COALESCE(SUM(size), 0) FROM drive_nodes WHERE id IN t AND dir = 0 AND trashed_at IS NULL",
            [id],
            |r| r.get(0),
        )?)
    }

    pub fn set_limit(&self, id: &str, limit: Option<u64>) -> Result<()> {
        self.conn().execute(
            "UPDATE drive_nodes SET limit_bytes = ? WHERE id = ? AND dir = 1",
            params![limit, id],
        )?;
        Ok(())
    }

    pub fn versions(&self, node: &str) -> Result<Vec<Version>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {VERSION_COLS} FROM drive_versions WHERE node = ? ORDER BY created_at DESC"
        ))?;
        Ok(stmt
            .query_map([node], version)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn version(&self, id: &str) -> Result<Option<Version>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {VERSION_COLS} FROM drive_versions WHERE id = ?"),
                [id],
                version,
            )
            .optional()?)
    }

    /// Makes an older version the current one again (as a new version, so
    /// nothing is lost).
    pub fn revert(&self, node: &str, version_id: &str, now: i64) -> Result<()> {
        let Some(v) = self.version(version_id)? else {
            bail!("no such version")
        };
        if v.node != node {
            bail!("no such version");
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE drive_versions SET replaced_at = ? WHERE node = ? AND replaced_at IS NULL",
            params![now, node],
        )?;
        let id = crate::token(16);
        tx.execute(
            &format!(
                "INSERT INTO drive_versions ({VERSION_COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?)"
            ),
            params![
                id,
                node,
                v.object,
                v.size,
                v.sha256,
                v.content_type,
                now,
                "admin"
            ],
        )?;
        tx.execute(
            "UPDATE drive_nodes SET version = ?, size = ?, modified_at = ? WHERE id = ?",
            params![id, v.size, now, node],
        )?;
        tx.commit()?;
        Ok(())
    }

    // Objects.

    pub fn object(&self, hash: &str) -> Result<Option<Object>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT hash, sealed, local, remote FROM drive_objects WHERE hash = ?",
                [hash],
                object,
            )
            .optional()?)
    }

    pub fn touch_object(&self, hash: &str, now: i64) {
        let _ = self.conn().execute(
            "UPDATE drive_objects SET last_access = ? WHERE hash = ?",
            params![now, hash],
        );
    }

    /// Objects only on this server, oldest first.
    pub fn pending_objects(&self) -> Result<Vec<Object>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT hash, sealed, local, remote FROM drive_objects WHERE remote = 0 AND local = 1 \
             ORDER BY created_at",
        )?;
        Ok(stmt
            .query_map([], object)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_object_place(&self, hash: &str, local: bool, remote: bool) -> Result<()> {
        self.conn().execute(
            "UPDATE drive_objects SET local = ?, remote = ? WHERE hash = ?",
            params![local, remote, hash],
        )?;
        Ok(())
    }

    /// Bytes of objects kept on this server, and of those not on the box yet.
    pub fn local_bytes(&self) -> Result<(u64, u64)> {
        Ok(self.conn().query_row(
            "SELECT COALESCE(SUM(sealed), 0), COALESCE(SUM(CASE WHEN remote = 0 THEN sealed END), 0) \
             FROM drive_objects WHERE local = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// What the Drive takes on the box, or will once everything is copied.
    pub fn box_bytes(&self) -> Result<u64> {
        Ok(self.conn().query_row(
            "SELECT (SELECT COALESCE(SUM(sealed), 0) FROM drive_objects) + \
             (SELECT COALESCE(SUM(sealed), 0) FROM drive_manifests)",
            [],
            |r| r.get(0),
        )?)
    }

    /// Local copies that can go (already on the box), least recently used
    /// first.
    pub fn evictable(&self) -> Result<Vec<Object>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT hash, sealed, local, remote FROM drive_objects WHERE local = 1 AND remote = 1 \
             ORDER BY last_access",
        )?;
        Ok(stmt
            .query_map([], object)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Objects no version uses any more, with the span of manifests that
    /// listed them (`None` if none did).
    pub fn unused_objects(&self) -> Result<Vec<(Object, Option<Span>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT hash, sealed, local, remote, first_ref, last_ref FROM drive_objects o \
             WHERE NOT EXISTS (SELECT 1 FROM drive_versions v WHERE v.object = o.hash)",
        )?;
        Ok(stmt
            .query_map([], |r| {
                let span = match (r.get::<_, Option<i64>>(4)?, r.get::<_, Option<i64>>(5)?) {
                    (Some(a), Some(b)) => Some((a, b)),
                    _ => None,
                };
                Ok((object(r)?, span))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_object(&self, hash: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM drive_objects WHERE hash = ?", [hash])?;
        Ok(())
    }

    /// Objects on the box, for the daily check.
    pub fn remote_objects(&self) -> Result<Vec<Object>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT hash, sealed, local, remote FROM drive_objects WHERE remote = 1")?;
        Ok(stmt
            .query_map([], object)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn object_counts(&self) -> Result<(u64, u64)> {
        Ok(self.conn().query_row(
            "SELECT COUNT(*), COALESCE(SUM(remote = 0), 0) FROM drive_objects",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    // Manifests.

    /// Every node and version, for a manifest.
    pub fn snapshot(&self) -> Result<(Vec<Node>, Vec<Version>)> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {NODE_COLS} FROM {NODE_FROM} ORDER BY n.created_at, n.id"
        ))?;
        let nodes = stmt.query_map([], node)?.collect::<rusqlite::Result<_>>()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {VERSION_COLS} FROM drive_versions ORDER BY created_at, id"
        ))?;
        let versions = stmt
            .query_map([], version)?
            .collect::<rusqlite::Result<_>>()?;
        Ok((nodes, versions))
    }

    /// Records a manifest written to the box, and that it lists `objects`.
    pub fn add_manifest(&self, hash: &str, at: i64, sealed: u64, objects: &[String]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO drive_manifests (hash, created_at, sealed) VALUES (?, ?, ?)",
            params![hash, at, sealed],
        )?;
        {
            let mut stmt = tx.prepare(
                "UPDATE drive_objects SET first_ref = COALESCE(first_ref, ?1), last_ref = ?1 WHERE hash = ?2",
            )?;
            for o in objects {
                stmt.execute(params![at, o])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Manifests, oldest first: `(hash, created_at, sealed)`.
    pub fn manifests(&self) -> Result<Vec<(String, i64, u64)>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT hash, created_at, sealed FROM drive_manifests ORDER BY created_at")?;
        Ok(stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_manifest(&self, hash: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM drive_manifests WHERE hash = ?", [hash])?;
        Ok(())
    }

    // Links.

    pub fn insert_link(&self, l: &Link) -> Result<()> {
        self.conn().execute(
            &format!("INSERT INTO drive_links ({LINK_COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"),
            params![
                l.token,
                l.node,
                l.mode.as_str(),
                l.note,
                l.created_at,
                l.expires_at,
                l.revoked,
                l.uses
            ],
        )?;
        Ok(())
    }

    pub fn link(&self, token: &str) -> Result<Option<Link>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {LINK_COLS} FROM drive_links WHERE token = ?"),
                [token],
                link,
            )
            .optional()?)
    }

    /// Links not revoked, newest first; `node` narrows them to one item.
    pub fn links(&self, node: Option<&str>) -> Result<Vec<Link>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {LINK_COLS} FROM drive_links WHERE revoked = 0 AND (?1 IS NULL OR node = ?1) \
             ORDER BY created_at DESC"
        ))?;
        Ok(stmt
            .query_map([node], link)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn revoke_link(&self, token: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE drive_links SET revoked = 1 WHERE token = ?",
            [token],
        )?;
        Ok(())
    }

    pub fn count_link_use(&self, token: &str) {
        let _ = self.conn().execute(
            "UPDATE drive_links SET uses = uses + 1 WHERE token = ?",
            [token],
        );
    }

    // Reports waiting for Réplique.

    pub fn spool_push(&self, body: &str) -> Result<()> {
        self.conn()
            .execute("INSERT INTO replique_spool (body) VALUES (?)", [body])?;
        // Keep the spool bounded if Réplique stays away for long.
        self.conn().execute(
            "DELETE FROM replique_spool WHERE id NOT IN (SELECT id FROM replique_spool ORDER BY id DESC LIMIT 500)",
            [],
        )?;
        Ok(())
    }

    pub fn spool(&self) -> Result<Vec<(i64, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, body FROM replique_spool ORDER BY id")?;
        Ok(stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn spool_delete(&self, id: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM replique_spool WHERE id = ?", [id])?;
        Ok(())
    }
}

fn touch_parent(tx: &Transaction, parent: &str, now: i64) -> Result<()> {
    tx.execute(
        "UPDATE drive_nodes SET modified_at = ? WHERE id = ?",
        params![now, parent],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        let dir = std::env::temp_dir().join(format!("partage-test-{}", crate::token(10)));
        std::fs::create_dir_all(&dir).unwrap();
        Db::open(&dir.join("t.db")).unwrap()
    }

    fn file<'a>(parent: &'a str, name: &'a str, object: &'a str, replace: bool) -> NewFile<'a> {
        NewFile {
            parent,
            name,
            object,
            sealed: 100,
            size: 44,
            sha256: "x",
            content_type: "text/plain",
            local: true,
            remote: false,
            replace,
            added_by: "admin",
            now: 1000,
        }
    }

    #[test]
    fn names_versions_trash_and_restore() {
        let db = db();
        let a = db.create_dir(ROOT, "a", 1).unwrap();
        assert!(db.create_dir(ROOT, "a", 1).is_err());
        let f1 = db.add_file(&file(&a.id, "x.txt", "o1", false)).unwrap();
        let f2 = db.add_file(&file(&a.id, "x.txt", "o2", false)).unwrap();
        assert_eq!(f2.name, "x (2).txt");
        let f3 = db.add_file(&file(&a.id, "x.txt", "o3", true)).unwrap();
        assert_eq!(f3.id, f1.id);
        assert_eq!(db.versions(&f1.id).unwrap().len(), 2);
        assert_eq!(db.tree_size(ROOT).unwrap(), 88);

        let b = db.ensure_dirs(&a.id, &["b", "c"], 2).unwrap();
        db.add_file(&file(&b, "y", "o4", false)).unwrap();
        db.trash(&f2.id, "admin", 10).unwrap();
        db.trash(&a.id, "admin", 20).unwrap();
        assert_eq!(db.tree_size(ROOT).unwrap(), 0);
        assert_eq!(db.trash_items().unwrap().len(), 2);
        db.restore(&a.id).unwrap();
        // x (2).txt was trashed on its own before: it stays in the trash.
        assert_eq!(db.tree_size(ROOT).unwrap(), 88);
        assert_eq!(db.trash_items().unwrap().len(), 1);
        db.purge(&f2.id).unwrap();
        let unused: Vec<String> = db
            .unused_objects()
            .unwrap()
            .into_iter()
            .map(|(o, _)| o.hash)
            .collect();
        assert_eq!(unused, ["o2"]);
        assert!(db.move_node(&a.id, &b, 3).is_err());
        assert!(db.is_within(&b, &a.id).unwrap());
    }

    #[test]
    fn numbering() {
        assert_eq!(numbered("a.tar.gz", 2), "a.tar (2).gz");
        assert_eq!(numbered(".env", 3), ".env (3)");
        assert_eq!(numbered("README", 2), "README (2)");
        assert_eq!(numbered("x.txt", 1), "x.txt");
    }
}
