//! `partage drive-restore`: gets the Drive back from a copy of the box (the
//! PC's mirror folder), with real names, without this server. Needs only the
//! copy and the Drive key.

use crate::{
    crypt::{self, Key},
    manifest::Manifest,
};
use anyhow::{Context, bail};
use sha2::{Digest, Sha256};
use std::{
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

const USAGE: &str = "\
partage drive-restore <copy> <out> [--at <YYYY-MM-DD|unix time>] [--manifest <hash>] [--trash]
partage drive-restore <copy> --list

<copy> is a folder holding objects/ and manifests/ (the PC's mirror of the
Drive). The Drive key is read from PARTAGE_DRIVE_KEY, or asked for.
--at takes the newest manifest written before that time; --trash also
restores what was in the trash, under <out>/.trash.";

struct Args {
    copy: PathBuf,
    out: Option<PathBuf>,
    at: Option<i64>,
    manifest: Option<String>,
    trash: bool,
    list: bool,
}

fn parse(args: Vec<String>) -> anyhow::Result<Args> {
    let mut pos = Vec::new();
    let mut a = Args {
        copy: PathBuf::new(),
        out: None,
        at: None,
        manifest: None,
        trash: false,
        list: false,
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--at" => a.at = Some(parse_time(&it.next().context("--at needs a time")?)?),
            "--manifest" => a.manifest = Some(it.next().context("--manifest needs a hash")?),
            "--trash" => a.trash = true,
            "--list" => a.list = true,
            "-h" | "--help" => bail!("{USAGE}"),
            s if s.starts_with("--") => bail!("unknown option {s}\n\n{USAGE}"),
            _ => pos.push(arg),
        }
    }
    match (pos.len(), a.list) {
        (1, true) => {}
        (2, false) => a.out = Some(pos[1].clone().into()),
        _ => bail!("{USAGE}"),
    }
    a.copy = pos[0].clone().into();
    Ok(a)
}

fn parse_time(s: &str) -> anyhow::Result<i64> {
    if let Ok(n) = s.parse::<i64>() {
        return Ok(n);
    }
    let p: Vec<i64> = s
        .split('-')
        .map(|x| x.parse().context("dates look like 2026-11-23"))
        .collect::<anyhow::Result<_>>()?;
    let [y, m, d] = p[..] else {
        bail!("dates look like 2026-11-23")
    };
    // Howard Hinnant's days_from_civil; the end of that day.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Ok(days * 86400 + 86399)
}

fn read_key() -> anyhow::Result<Key> {
    if let Ok(k) = std::env::var("PARTAGE_DRIVE_KEY") {
        return Key::from_text(&k);
    }
    eprint!("Drive key: ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Key::from_text(&line)
}

/// A writer that hashes what goes through it.
struct Hashing<W> {
    inner: W,
    hash: Sha256,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// A path from a manifest, kept inside the output folder.
fn safe_join(out: &Path, path: &str) -> Option<PathBuf> {
    let mut p = out.to_path_buf();
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('\\') {
            return None;
        }
        p.push(part);
    }
    Some(p)
}

pub fn main(args: Vec<String>) -> anyhow::Result<()> {
    let a = parse(args)?;
    let key = read_key()?;
    let mut manifests = Vec::new();
    let dir = a.copy.join("manifests");
    for e in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if !crypt::is_hash(&name) {
            continue;
        }
        let bytes = std::fs::read(e.path())?;
        if crypt::hex(&Sha256::digest(&bytes)) != name {
            eprintln!("manifest {name} is damaged: skipped");
            continue;
        }
        match Manifest::open(&key, &bytes) {
            Ok(m) => manifests.push((name, m)),
            Err(e) => eprintln!("manifest {name} does not open ({e:#}): wrong key?"),
        }
    }
    if manifests.is_empty() {
        bail!("no manifest opens in {}", dir.display());
    }
    manifests.sort_by_key(|(_, m)| m.created_at);
    if a.list {
        for (name, m) in &manifests {
            let files = m.files(false);
            let bytes: u64 = files.iter().map(|(_, v)| v.size).sum();
            println!(
                "{}  {:>7} files  {:>14} bytes  {name}",
                crate::html::rfc3339(m.created_at),
                files.len(),
                bytes
            );
        }
        return Ok(());
    }
    let chosen = if let Some(prefix) = &a.manifest {
        manifests
            .iter()
            .find(|(n, _)| n.starts_with(prefix.as_str()))
            .with_context(|| format!("no manifest starts with {prefix}"))?
    } else if let Some(at) = a.at {
        manifests
            .iter()
            .rev()
            .find(|(_, m)| m.created_at <= at)
            .context("no manifest is that old")?
    } else {
        manifests.last().expect("not empty")
    };
    let (name, m) = chosen;
    let out = a.out.expect("checked by parse");
    if out.exists() && std::fs::read_dir(&out)?.next().is_some() {
        bail!("{} is not empty: restore into a new folder", out.display());
    }
    std::fs::create_dir_all(&out)?;
    eprintln!(
        "Restoring the Drive as it was at {} (manifest {}) into {}",
        crate::html::rfc3339(m.created_at),
        &name[..12],
        out.display()
    );
    let mut problems = 0;
    let mut restored = 0;
    let mut files: Vec<(String, &crate::manifest::MVersion, bool)> = m
        .files(false)
        .into_iter()
        .map(|(p, v)| (p, v, false))
        .collect();
    if a.trash {
        let live: std::collections::HashSet<String> = files.iter().map(|f| f.0.clone()).collect();
        for (p, v) in m.files(true) {
            if !live.contains(&p) {
                files.push((format!(".trash/{p}"), v, true));
            }
        }
    }
    for (path, v, _) in files {
        let Some(dest) = safe_join(&out, &path) else {
            eprintln!("skipped a strange path: {path:?}");
            problems += 1;
            continue;
        };
        let obj = a.copy.join("objects").join(&v.object[..2]).join(&v.object);
        let result = (|| {
            let src = std::fs::File::open(&obj).context("its object is not in the copy")?;
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let f = std::fs::File::create(&dest)?;
            let mut w = Hashing {
                inner: BufWriter::new(f),
                hash: Sha256::new(),
            };
            crypt::open_file(&key, src, Some(&v.object), &mut w)?;
            w.flush()?;
            anyhow::ensure!(
                crypt::hex(&w.hash.finalize()) == v.sha256,
                "the file does not match its checksum"
            );
            anyhow::Ok(())
        })();
        match result {
            Ok(()) => restored += 1,
            Err(e) => {
                problems += 1;
                let _ = std::fs::remove_file(&dest);
                eprintln!("{path}: {e:#}");
            }
        }
    }
    eprintln!("{restored} files restored, {problems} problems");
    if problems > 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_paths() {
        assert_eq!(parse_time("1970-01-01").unwrap(), 86399);
        assert_eq!(parse_time("2026-11-23").unwrap(), 1_795_478_399);
        assert!(safe_join(Path::new("/o"), "a/../b").is_none());
        assert_eq!(
            safe_join(Path::new("/o"), "a/b").unwrap(),
            PathBuf::from("/o/a/b")
        );
    }
}
