//! Folders downloaded as one zip, built on the fly. Files are stored as they
//! are (photos and videos do not shrink), with their CRC-32 known from the
//! upload, so every byte of the zip is known before it is sent: it has a
//! length, and a download that stops can resume from where it stopped
//! (ranges, checked against an ETag of what the zip holds). Zip64 fields
//! appear where sizes, offsets or the number of entries need them.

use crate::{App, drive};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::sync::Arc;

const MAX32: u64 = 0xFFFF_FFFF;

pub struct Entry {
    /// Inside the zip; folders end with `/`.
    pub path: String,
    pub size: u64,
    pub crc: u32,
    pub mtime: i64,
    /// The object holding the file; `None` for a folder.
    pub object: Option<String>,
}

enum Seg {
    Bytes(Vec<u8>),
    File { object: String, size: u64 },
}

impl Seg {
    fn len(&self) -> u64 {
        match self {
            Seg::Bytes(b) => b.len() as u64,
            Seg::File { size, .. } => *size,
        }
    }
}

fn dos_time(ts: i64) -> (u16, u16) {
    let (y, m, d, h, min, s) = crate::html::civil(ts);
    if y < 1980 {
        return (0, (1 << 5) | 1);
    }
    let time = (h << 11) | (min << 5) | (s / 2);
    let date = ((y - 1980) << 9) | (m << 5) | d;
    (time as u16, date as u16)
}

fn layout(entries: &[Entry]) -> (Vec<Seg>, u64) {
    let mut segs = Vec::new();
    let mut central = Vec::new();
    let mut offset = 0u64;
    for e in entries {
        let name = e.path.as_bytes();
        let (time, date) = dos_time(e.mtime);
        let big = e.size >= MAX32;
        let mut h = Vec::with_capacity(30 + name.len() + 20);
        h.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        h.extend_from_slice(&(if big { 45u16 } else { 20 }).to_le_bytes());
        h.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
        h.extend_from_slice(&0u16.to_le_bytes()); // stored
        h.extend_from_slice(&time.to_le_bytes());
        h.extend_from_slice(&date.to_le_bytes());
        h.extend_from_slice(&e.crc.to_le_bytes());
        let small = if big { MAX32 as u32 } else { e.size as u32 };
        h.extend_from_slice(&small.to_le_bytes());
        h.extend_from_slice(&small.to_le_bytes());
        h.extend_from_slice(&(name.len() as u16).to_le_bytes());
        h.extend_from_slice(&(if big { 20u16 } else { 0 }).to_le_bytes());
        h.extend_from_slice(name);
        if big {
            h.extend_from_slice(&1u16.to_le_bytes());
            h.extend_from_slice(&16u16.to_le_bytes());
            h.extend_from_slice(&e.size.to_le_bytes());
            h.extend_from_slice(&e.size.to_le_bytes());
        }
        let header_len = h.len() as u64;
        segs.push(Seg::Bytes(h));
        if let Some(object) = &e.object
            && e.size > 0
        {
            segs.push(Seg::File {
                object: object.clone(),
                size: e.size,
            });
        }

        // Its central directory record, for later.
        let mut z: Vec<u64> = Vec::new();
        if big {
            z.push(e.size);
            z.push(e.size);
        }
        if offset >= MAX32 {
            z.push(offset);
        }
        let extra = if z.is_empty() { 0 } else { 4 + 8 * z.len() };
        let mut c = Vec::with_capacity(46 + name.len() + extra);
        c.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        c.extend_from_slice(&((3u16 << 8) | 45).to_le_bytes());
        c.extend_from_slice(&(if z.is_empty() { 20u16 } else { 45 }).to_le_bytes());
        c.extend_from_slice(&0x0800u16.to_le_bytes());
        c.extend_from_slice(&0u16.to_le_bytes());
        c.extend_from_slice(&time.to_le_bytes());
        c.extend_from_slice(&date.to_le_bytes());
        c.extend_from_slice(&e.crc.to_le_bytes());
        c.extend_from_slice(&small.to_le_bytes());
        c.extend_from_slice(&small.to_le_bytes());
        c.extend_from_slice(&(name.len() as u16).to_le_bytes());
        c.extend_from_slice(&(extra as u16).to_le_bytes());
        c.extend_from_slice(&0u16.to_le_bytes()); // comment
        c.extend_from_slice(&0u16.to_le_bytes()); // disk
        c.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        let external: u32 = if e.object.is_none() {
            (0o40755 << 16) | 0x10
        } else {
            0o100644 << 16
        };
        c.extend_from_slice(&external.to_le_bytes());
        c.extend_from_slice(&(offset.min(MAX32) as u32).to_le_bytes());
        c.extend_from_slice(name);
        if !z.is_empty() {
            c.extend_from_slice(&1u16.to_le_bytes());
            c.extend_from_slice(&((8 * z.len()) as u16).to_le_bytes());
            for v in &z {
                c.extend_from_slice(&v.to_le_bytes());
            }
        }
        central.extend_from_slice(&c);
        offset += header_len + if e.object.is_some() { e.size } else { 0 };
    }
    let cd_start = offset;
    let cd_size = central.len() as u64;
    let count = entries.len() as u64;
    let mut end = central;
    let zip64 = count >= 0xFFFF || cd_start >= MAX32 || cd_size >= MAX32;
    if zip64 {
        let record = cd_start + cd_size;
        end.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
        end.extend_from_slice(&44u64.to_le_bytes());
        end.extend_from_slice(&45u16.to_le_bytes());
        end.extend_from_slice(&45u16.to_le_bytes());
        end.extend_from_slice(&0u32.to_le_bytes());
        end.extend_from_slice(&0u32.to_le_bytes());
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&cd_size.to_le_bytes());
        end.extend_from_slice(&cd_start.to_le_bytes());
        end.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
        end.extend_from_slice(&0u32.to_le_bytes());
        end.extend_from_slice(&record.to_le_bytes());
        end.extend_from_slice(&1u32.to_le_bytes());
    }
    end.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    let count16 = count.min(0xFFFF) as u16;
    end.extend_from_slice(&count16.to_le_bytes());
    end.extend_from_slice(&count16.to_le_bytes());
    end.extend_from_slice(&(cd_size.min(MAX32) as u32).to_le_bytes());
    end.extend_from_slice(&(cd_start.min(MAX32) as u32).to_le_bytes());
    end.extend_from_slice(&0u16.to_le_bytes());
    segs.push(Seg::Bytes(end));
    let total = segs.iter().map(Seg::len).sum();
    (segs, total)
}

/// What the zip holds, as a validator: a resumed download only continues if
/// the folder is still the same.
fn etag(entries: &[Entry]) -> String {
    let mut h = Sha256::new();
    for e in entries {
        h.update(e.path.as_bytes());
        h.update([0]);
        h.update(e.object.as_deref().unwrap_or("").as_bytes());
        h.update(e.mtime.to_le_bytes());
    }
    format!("\"z-{}\"", &crate::crypt::hex(&h.finalize())[..24])
}

/// Sends the zip, or the part of it a range asks for.
pub async fn serve(
    app: &Arc<App>,
    name: &str,
    entries: Vec<Entry>,
    headers: &HeaderMap,
    head: bool,
) -> Response {
    let tag = etag(&entries);
    let (segs, total) = layout(&entries);
    let (start, end, partial) = match drive::wanted_range(headers, total, &tag) {
        Ok(r) => r,
        Err(res) => return res,
    };
    let body = if head || start == end {
        Body::empty()
    } else {
        let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(4);
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = pump(&app, segs, start, end, &tx).await {
                tracing::error!("drive: zip download failed: {e:#}");
                let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
            }
        });
        let mut rx = rx;
        Body::from_stream(futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)))
    };
    let mut res = Response::new(body);
    if partial {
        *res.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    h.insert(header::CONTENT_LENGTH, (end - start).into());
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(v) = HeaderValue::from_str(&tag) {
        h.insert(header::ETAG, v);
    }
    if partial && let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{}/{total}", end - 1)) {
        h.insert(header::CONTENT_RANGE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&crate::store::disposition(&format!("{name}.zip"), false))
    {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    res.into_response()
}

async fn pump(
    app: &Arc<App>,
    segs: Vec<Seg>,
    start: u64,
    end: u64,
    tx: &tokio::sync::mpsc::Sender<std::io::Result<Bytes>>,
) -> anyhow::Result<()> {
    let mut pos = 0u64;
    for seg in segs {
        let len = seg.len();
        let (a, b) = (start.max(pos), end.min(pos + len));
        pos += len;
        if a >= b {
            continue;
        }
        let (a, b) = (a - (pos - len), b - (pos - len));
        match seg {
            Seg::Bytes(bytes) => {
                if tx
                    .send(Ok(Bytes::copy_from_slice(&bytes[a as usize..b as usize])))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
            Seg::File { object, .. } => {
                let mut stream = drive::open_range(app, &object, a, b)
                    .await?
                    .into_data_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|e| anyhow::anyhow!("reading {object}: {e}"))?;
                    if tx.send(Ok(chunk)).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
        if pos >= end {
            break;
        }
    }
    Ok(())
}

/// The entries for a folder: its files under a folder of its name, with
/// their CRC-32 (computed once for versions from before it was stored).
pub async fn entries(
    app: &Arc<App>,
    folder: &crate::drive_db::Node,
    top: &str,
) -> anyhow::Result<Vec<Entry>> {
    let mut out = vec![Entry {
        path: format!("{top}/"),
        size: 0,
        crc: 0,
        mtime: folder.modified_at,
        object: None,
    }];
    for (path, n) in app.db.tree(&folder.id)? {
        if n.dir {
            out.push(Entry {
                path: format!("{top}/{path}/"),
                size: 0,
                crc: 0,
                mtime: n.modified_at,
                object: None,
            });
            continue;
        }
        let Some(v) = n
            .version
            .as_deref()
            .and_then(|v| app.db.version(v).ok().flatten())
        else {
            continue;
        };
        let crc = match v.crc32 {
            Some(c) => c,
            None => {
                let c = file_crc(app, &v.object, v.size).await?;
                app.db.set_crc32(&v.id, c)?;
                c
            }
        };
        out.push(Entry {
            path: format!("{top}/{path}"),
            size: v.size,
            crc,
            mtime: n.modified_at,
            object: Some(v.object),
        });
    }
    Ok(out)
}

async fn file_crc(app: &Arc<App>, object: &str, size: u64) -> anyhow::Result<u32> {
    let mut h = crc32fast::Hasher::new();
    if size > 0 {
        let mut stream = drive::open_range(app, object, 0, size)
            .await?
            .into_data_stream();
        while let Some(chunk) = stream.next().await {
            h.update(&chunk.map_err(|e| anyhow::anyhow!("{e}"))?);
        }
    }
    Ok(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_zip_layout() {
        let entries = vec![
            Entry {
                path: "a/".into(),
                size: 0,
                crc: 0,
                mtime: 1_795_392_000,
                object: None,
            },
            Entry {
                path: "a/x.txt".into(),
                size: 5,
                crc: 0x3610a686,
                mtime: 1_795_392_000,
                object: Some("o".into()),
            },
        ];
        let (segs, total) = layout(&entries);
        // Local header + name for each, the file's bytes, the central
        // directory (46 + name each) and the end record (22).
        let expect = (30 + 2) + (30 + 7) + 5 + (46 + 2) + (46 + 7) + 22;
        assert_eq!(total, expect);
        assert_eq!(segs.len(), 4);
    }

    #[test]
    fn zip64_when_big() {
        let entries = vec![Entry {
            path: "big.bin".into(),
            size: 5_000_000_000,
            crc: 1,
            mtime: 0,
            object: Some("o".into()),
        }];
        let (_, total) = layout(&entries);
        let expect = (30 + 7 + 20) + 5_000_000_000 + (46 + 7 + 20) + 56 + 20 + 22;
        assert_eq!(total, expect);
    }
}
