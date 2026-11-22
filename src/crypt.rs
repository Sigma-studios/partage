//! The Drive's object format, the same on this server, on the Storage Box and
//! in the PC's copy:
//!
//! ```text
//! "partage1" (8 bytes) | salt (32 bytes) | record 0 | record 1 | …
//! ```
//!
//! Each record is up to 1 MiB of the file sealed with AES-256-GCM (16-byte tag
//! after it), under a key derived from the Drive key and the object's salt
//! (HKDF-SHA256). The nonce is the record's index (8 bytes, big-endian) and a
//! last-record flag (4 bytes), so records cannot be reordered, dropped or cut
//! off without failing to open. An empty file is one empty last record.
//!
//! An object is named by the SHA-256 of all its bytes: anyone holding a copy
//! can check it without the key. See docs/drive-format.md.

use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{AeadInPlace, generic_array::GenericArray},
};
use anyhow::{Context, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

pub const MAGIC: &[u8; 8] = b"partage1";
pub const HEADER: u64 = 40;
pub const RECORD: u64 = 1 << 20;
pub const TAG: u64 = 16;
const SEALED_RECORD: u64 = RECORD + TAG;

/// The Drive key: 32 random bytes, written as 43 characters of base64url.
#[derive(Clone)]
pub struct Key([u8; 32]);

impl Key {
    pub fn generate() -> Self {
        Self(rand::random())
    }

    pub fn from_text(s: &str) -> anyhow::Result<Self> {
        let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = URL_SAFE_NO_PAD
            .decode(clean.trim_end_matches('='))
            .context("the key is not base64url")?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("the key must be 32 bytes (43 characters)"))?;
        Ok(Self(bytes))
    }

    pub fn to_text(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    /// Reads the key file, or creates it (owner only) when missing.
    pub fn load_or_create(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::from_text(&s).with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::generate();
                write_private(path, key.to_text().as_bytes())?;
                Ok(key)
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn cipher(&self, salt: &[u8]) -> Aes256Gcm {
        let mut okm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(salt), &self.0)
            .expand(b"partage drive object", &mut okm)
            .expect("32 bytes is a valid HKDF length");
        Aes256Gcm::new(GenericArray::from_slice(&okm))
    }
}

/// Writes a file only its owner can read.
pub fn write_private(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(content)?;
    f.sync_all()?;
    Ok(())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `s` looks like an object or manifest name.
pub fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn records(plain: u64) -> u64 {
    plain.div_ceil(RECORD).max(1)
}

pub fn sealed_size(plain: u64) -> u64 {
    HEADER + plain + records(plain) * TAG
}

/// The file's size from its object's, or `None` if no file seals to it.
pub fn plain_size(sealed: u64) -> Option<u64> {
    let body = sealed.checked_sub(HEADER)?;
    let n = body.div_ceil(SEALED_RECORD).max(1);
    let plain = body.checked_sub(n * TAG)?;
    (sealed_size(plain) == sealed).then_some(plain)
}

fn nonce(index: u64, last: bool) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&index.to_be_bytes());
    n[11] = last as u8;
    n
}

/// Seals a file as it streams through. `push` and `finish` append sealed
/// bytes to the buffer they are given; the object's name comes out of
/// `finish`.
pub struct Sealer {
    cipher: Aes256Gcm,
    buf: Vec<u8>,
    index: u64,
    hash: Sha256,
    plain: u64,
}

impl Sealer {
    /// Starts an object; its header goes into `out`.
    pub fn new(key: &Key, out: &mut Vec<u8>) -> Self {
        let salt: [u8; 32] = rand::random();
        let start = out.len();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&salt);
        let mut hash = Sha256::new();
        hash.update(&out[start..]);
        Self {
            cipher: key.cipher(&salt),
            buf: Vec::with_capacity(RECORD as usize + TAG as usize),
            index: 0,
            hash,
            plain: 0,
        }
    }

    /// The record being filled is held back until more data shows it is not
    /// the last one.
    pub fn push(&mut self, mut data: &[u8], out: &mut Vec<u8>) {
        self.plain += data.len() as u64;
        while !data.is_empty() {
            if self.buf.len() == RECORD as usize {
                self.seal(false, out);
            }
            let room = RECORD as usize - self.buf.len();
            let n = room.min(data.len());
            self.buf.extend_from_slice(&data[..n]);
            data = &data[n..];
        }
    }

    fn seal(&mut self, last: bool, out: &mut Vec<u8>) {
        let n = nonce(self.index, last);
        self.cipher
            .encrypt_in_place(GenericArray::from_slice(&n), b"", &mut self.buf)
            .expect("a record is far below AES-GCM's limit");
        self.hash.update(&self.buf);
        out.extend_from_slice(&self.buf);
        self.buf.clear();
        self.index += 1;
    }

    /// Bytes of the file pushed so far.
    pub fn plain_len(&self) -> u64 {
        self.plain
    }

    /// Seals the last record and returns the object's name.
    pub fn finish(mut self, out: &mut Vec<u8>) -> String {
        self.seal(true, out);
        hex(&self.hash.finalize())
    }
}

/// Opens records of one object, given its header.
pub struct Opener {
    cipher: Aes256Gcm,
    /// The file's size.
    pub plain: u64,
    last: u64,
}

impl Opener {
    pub fn new(key: &Key, header: &[u8], sealed: u64) -> anyhow::Result<Self> {
        ensure!(
            header.len() as u64 >= HEADER && &header[..8] == MAGIC,
            "not a partage object"
        );
        let Some(plain) = plain_size(sealed) else {
            bail!("an object of {sealed} bytes cannot be whole");
        };
        Ok(Self {
            cipher: key.cipher(&header[8..40]),
            plain,
            last: records(plain) - 1,
        })
    }

    /// Where record `index` starts in the object, and how long it is sealed.
    pub fn record_span(&self, index: u64) -> (u64, usize) {
        let start = HEADER + index * SEALED_RECORD;
        let len = if index == self.last {
            self.plain - index * RECORD + TAG
        } else {
            SEALED_RECORD
        };
        (start, len as usize)
    }

    pub fn records(&self) -> u64 {
        self.last + 1
    }

    /// Opens record `index` in place: `buf` holds it sealed, then the plain
    /// bytes.
    pub fn open(&self, index: u64, buf: &mut Vec<u8>) -> anyhow::Result<()> {
        let n = nonce(index, index == self.last);
        self.cipher
            .decrypt_in_place(GenericArray::from_slice(&n), b"", buf)
            .map_err(|_| {
                anyhow::anyhow!("record {index} does not open: wrong key or damaged object")
            })
    }
}

/// Seals a whole buffer (manifests). Returns the object and its name.
pub fn seal_all(key: &Key, data: &[u8]) -> (Vec<u8>, String) {
    let mut out = Vec::with_capacity(sealed_size(data.len() as u64) as usize);
    let mut s = Sealer::new(key, &mut out);
    s.push(data, &mut out);
    let name = s.finish(&mut out);
    (out, name)
}

/// Opens a whole object held in memory (manifests).
pub fn open_all(key: &Key, object: &[u8]) -> anyhow::Result<Vec<u8>> {
    let o = Opener::new(key, object, object.len() as u64)?;
    let mut plain = Vec::with_capacity(o.plain as usize);
    for i in 0..o.records() {
        let (start, len) = o.record_span(i);
        let mut buf = object[start as usize..start as usize + len].to_vec();
        o.open(i, &mut buf)?;
        plain.extend_from_slice(&buf);
    }
    Ok(plain)
}

/// Opens an object from a file into `out`, checking its name on the way.
/// Returns the file's size.
pub fn open_file(
    key: &Key,
    mut src: impl Read + Seek,
    expected_name: Option<&str>,
    mut out: impl Write,
) -> anyhow::Result<u64> {
    let sealed = src.seek(SeekFrom::End(0))?;
    src.seek(SeekFrom::Start(0))?;
    let mut header = [0u8; HEADER as usize];
    src.read_exact(&mut header)?;
    let o = Opener::new(key, &header, sealed)?;
    let mut hash = Sha256::new();
    hash.update(header);
    let mut buf = Vec::with_capacity(SEALED_RECORD as usize);
    for i in 0..o.records() {
        let (_, len) = o.record_span(i);
        buf.resize(len, 0);
        src.read_exact(&mut buf)?;
        hash.update(&buf);
        o.open(i, &mut buf)?;
        out.write_all(&buf)?;
    }
    if let Some(name) = expected_name {
        ensure!(
            hex(&hash.finalize()) == name,
            "the object's contents do not match its name"
        );
    }
    Ok(o.plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed(key: &Key, data: &[u8], piece: usize) -> (Vec<u8>, String) {
        let mut out = Vec::new();
        let mut s = Sealer::new(key, &mut out);
        for chunk in data.chunks(piece.max(1)) {
            s.push(chunk, &mut out);
        }
        let name = s.finish(&mut out);
        (out, name)
    }

    #[test]
    fn round_trips_at_record_edges() {
        let key = Key::generate();
        for len in [
            0,
            1,
            RECORD as usize - 1,
            RECORD as usize,
            RECORD as usize + 1,
            3 * RECORD as usize,
        ] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
            let (obj, name) = sealed(&key, &data, 300_000);
            assert_eq!(obj.len() as u64, sealed_size(len as u64), "len {len}");
            assert_eq!(plain_size(obj.len() as u64), Some(len as u64));
            assert_eq!(hex(&Sha256::digest(&obj)), name);
            assert_eq!(open_all(&key, &obj).unwrap(), data);
            let mut out = Vec::new();
            let n = open_file(&key, std::io::Cursor::new(&obj), Some(&name), &mut out).unwrap();
            assert_eq!((n, out), (len as u64, data));
        }
    }

    #[test]
    fn refuses_wrong_key_damage_and_truncation() {
        let key = Key::generate();
        let data = vec![42u8; 2 * RECORD as usize + 10];
        let (obj, _) = seal_all(&key, &data);
        assert!(open_all(&Key::generate(), &obj).is_err());
        let mut bad = obj.clone();
        bad[100] ^= 1;
        assert!(open_all(&key, &bad).is_err());
        // Cut after two whole records: the second was not sealed as the last.
        let cut = &obj[..(HEADER + 2 * SEALED_RECORD) as usize];
        assert!(open_all(&key, cut).is_err());
    }

    #[test]
    fn keys_as_text() {
        let k = Key::generate();
        let t = k.to_text();
        assert_eq!(t.len(), 43);
        assert_eq!(Key::from_text(&format!(" {t}\n")).unwrap().0, k.0);
        assert!(Key::from_text("short").is_err());
        assert!(plain_size(10).is_none());
    }
}
