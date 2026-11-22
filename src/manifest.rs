//! Manifests: the whole Drive tree at one moment, written to the box so that
//! a copy of the box (the PC's) is enough to get every file back with its
//! name, without this server's database. JSON, zstd-compressed, then sealed
//! like an object. See docs/drive-format.md.

use crate::{
    crypt::{self, Key},
    drive_db::{Node, Version},
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const FORMAT: u32 = 1;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Manifest {
    pub format: u32,
    /// Unix seconds.
    pub created_at: i64,
    pub nodes: Vec<MNode>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MNode {
    pub id: String,
    /// `None` for the root, whose name is empty.
    pub parent: Option<String>,
    pub name: String,
    pub dir: bool,
    pub created_at: i64,
    pub modified_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trashed_at: Option<i64>,
    /// Newest first; the current one has `current`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub versions: Vec<MVersion>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MVersion {
    pub object: String,
    pub size: u64,
    /// Of the file itself, not of its object.
    pub sha256: String,
    pub content_type: String,
    pub created_at: i64,
    pub current: bool,
}

impl Manifest {
    pub fn build(nodes: &[Node], versions: &[Version], now: i64) -> Self {
        let mut by_node: HashMap<&str, Vec<&Version>> = HashMap::new();
        for v in versions {
            by_node.entry(v.node.as_str()).or_default().push(v);
        }
        let nodes = nodes
            .iter()
            .map(|n| {
                let mut vs: Vec<MVersion> = by_node
                    .get(n.id.as_str())
                    .map(|vs| {
                        vs.iter()
                            .map(|v| MVersion {
                                object: v.object.clone(),
                                size: v.size,
                                sha256: v.sha256.clone(),
                                content_type: v.content_type.clone(),
                                created_at: v.created_at,
                                current: n.version.as_deref() == Some(v.id.as_str()),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                vs.sort_by_key(|v| std::cmp::Reverse(v.created_at));
                MNode {
                    id: n.id.clone(),
                    parent: n.parent.clone(),
                    name: n.name.clone(),
                    dir: n.dir,
                    created_at: n.created_at,
                    modified_at: n.modified_at,
                    trashed_at: n.trashed_at,
                    versions: vs,
                }
            })
            .collect();
        Self {
            format: FORMAT,
            created_at: now,
            nodes,
        }
    }

    /// Every object it lists.
    pub fn objects(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .nodes
            .iter()
            .flat_map(|n| n.versions.iter().map(|v| v.object.clone()))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Sealed, with its name.
    pub fn seal(&self, key: &Key) -> anyhow::Result<(Vec<u8>, String)> {
        let json = serde_json::to_vec(self)?;
        let packed = zstd::encode_all(&json[..], 9)?;
        Ok(crypt::seal_all(key, &packed))
    }

    pub fn open(key: &Key, sealed: &[u8]) -> anyhow::Result<Self> {
        let packed = crypt::open_all(key, sealed)?;
        let json = zstd::decode_all(&packed[..])?;
        let m: Self = serde_json::from_slice(&json)?;
        anyhow::ensure!(
            m.format == FORMAT,
            "manifest format {} is newer than this partage",
            m.format
        );
        Ok(m)
    }

    /// The files in the tree (trash left out unless `trash`), as paths from
    /// the root with their current version.
    pub fn files(&self, trash: bool) -> Vec<(String, &MVersion)> {
        let by_id: HashMap<&str, &MNode> = self.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let mut out = Vec::new();
        for n in &self.nodes {
            if n.dir || (!trash && n.trashed_at.is_some()) {
                continue;
            }
            let Some(v) = n.versions.iter().find(|v| v.current) else {
                continue;
            };
            let mut parts = vec![n.name.as_str()];
            let mut parent = n.parent.as_deref();
            let mut ok = true;
            while let Some(p) = parent {
                let Some(pn) = by_id.get(p) else {
                    ok = false;
                    break;
                };
                if pn.parent.is_some() {
                    parts.push(pn.name.as_str());
                }
                parent = pn.parent.as_deref();
                if parts.len() > 1000 {
                    ok = false;
                    break;
                }
            }
            if ok {
                parts.reverse();
                out.push((parts.join("/"), v));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// Which manifests to keep: all from the last 7 days, the newest of each day
/// for 30 days before that, the newest of each 30-day stretch for a year,
/// and always the newest. `manifests` is `(hash, created_at)`, oldest first.
pub fn to_drop(manifests: &[(String, i64)], now: i64) -> Vec<String> {
    const DAY: i64 = 86400;
    let mut kept: HashSet<(u8, i64)> = HashSet::new();
    let mut drop = Vec::new();
    // Newest first, so the first one seen in a bucket is the one kept.
    for (i, (hash, at)) in manifests.iter().enumerate().rev() {
        let age = now - at;
        let newest = i == manifests.len() - 1;
        let bucket = if newest || age < 7 * DAY {
            None
        } else if age < 37 * DAY {
            Some((1, at.div_euclid(DAY)))
        } else if age < 372 * DAY {
            Some((2, at.div_euclid(30 * DAY)))
        } else {
            drop.push(hash.clone());
            continue;
        };
        if let Some(b) = bucket
            && !kept.insert(b)
        {
            drop.push(hash.clone());
        }
    }
    drop
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention() {
        let now = 1000 * 86400;
        let mut ms = Vec::new();
        // Every 6 hours for 400 days.
        for i in 0..1600 {
            let at = now - (1600 - i) * 6 * 3600;
            ms.push((format!("m{i}"), at));
        }
        let drop = to_drop(&ms, now);
        let kept = ms.len() - drop.len();
        // 7 days × 4, about 30 daily, about 12 monthly.
        assert!((65..=75).contains(&kept), "kept {kept}");
        assert!(!drop.contains(&"m1599".to_string()));
        assert!(to_drop(&[("only".into(), 0)], now).is_empty());
    }

    #[test]
    fn round_trip_and_paths() {
        let m = Manifest {
            format: FORMAT,
            created_at: 5,
            nodes: vec![
                MNode {
                    id: "root".into(),
                    parent: None,
                    name: String::new(),
                    dir: true,
                    created_at: 0,
                    modified_at: 0,
                    trashed_at: None,
                    versions: vec![],
                },
                MNode {
                    id: "d".into(),
                    parent: Some("root".into()),
                    name: "Photos".into(),
                    dir: true,
                    created_at: 0,
                    modified_at: 0,
                    trashed_at: None,
                    versions: vec![],
                },
                MNode {
                    id: "f".into(),
                    parent: Some("d".into()),
                    name: "a.jpg".into(),
                    dir: false,
                    created_at: 0,
                    modified_at: 0,
                    trashed_at: None,
                    versions: vec![MVersion {
                        object: "o".into(),
                        size: 3,
                        sha256: "s".into(),
                        content_type: "image/jpeg".into(),
                        created_at: 1,
                        current: true,
                    }],
                },
            ],
        };
        let key = Key::generate();
        let (sealed, _) = m.seal(&key).unwrap();
        let back = Manifest::open(&key, &sealed).unwrap();
        let files = back.files(false);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "Photos/a.jpg");
        assert_eq!(back.objects(), ["o"]);
    }
}
