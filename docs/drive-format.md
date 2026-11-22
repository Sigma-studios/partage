# The Drive's formats

Enough to read a copy of the Drive without partage, or to rewrite
`partage drive-restore`. The code is in `src/crypt.rs` and `src/manifest.rs`.

## Layout

On the Storage Box (under the configured folder, `/home/restic` when paired with
Réplique), and in the PC's copy:

```
objects/ab/ab12…ef     one per file version
manifests/cd34…01      the whole tree at one moment
tmp/<random>.partial   uploads in progress (never copied)
.ssh/                  Réplique's, for the read-only login; not partage's
```

Every name under `objects/` and `manifests/` is the lowercase hex SHA-256 of the
file's bytes. Files there are never modified; partage deletes them only once no
manifest it keeps lists them.

## Key

The Drive key is 32 random bytes, written as 43 characters of unpadded base64url.
partage keeps it in `PARTAGE_DATA/drive/key` and shows it in Settings.

## Objects

```
"partage1" (8 bytes) | salt (32 random bytes) | record 0 | record 1 | …
```

* Record key: HKDF-SHA256 with the Drive key as input, the salt as salt and
  `partage drive object` as info, 32 bytes.
* The file is cut into records of 1 MiB (1,048,576 bytes); the last one holds the
  rest, and an empty file is one empty record. Each record is sealed with
  AES-256-GCM, no associated data, and stored as ciphertext followed by its
  16-byte tag.
* Nonce (12 bytes): the record's index, 8 bytes big-endian, then 3 zero bytes,
  then 1 if this is the last record, else 0. A record moved, dropped or cut off
  fails to open.
* Size: `40 + size + 16 × max(1, ceil(size / 1 MiB))`.

## Manifests

A manifest is sealed exactly like an object. What it seals is a
zstd-compressed JSON document:

```json
{
  "format": 1,
  "created_at": 1795392000,
  "nodes": [
    { "id": "root", "parent": null, "name": "", "dir": true, "created_at": 0, "modified_at": 0 },
    { "id": "k3…", "parent": "root", "name": "Photos", "dir": true, "created_at": 0, "modified_at": 0 },
    {
      "id": "x9…", "parent": "k3…", "name": "a.jpg", "dir": false,
      "created_at": 0, "modified_at": 0,
      "versions": [
        { "object": "ab12…", "size": 2048, "sha256": "…of the file itself…",
          "content_type": "image/jpeg", "created_at": 0, "current": true }
      ]
    }
  ]
}
```

* Times are Unix seconds.
* A file's path is the names from the root (whose name is empty) down to it.
* `trashed_at` (absent otherwise) is set on items in the trash and on everything under them.
* `versions` are newest first; replaced ones stay listed for 30 days.

partage writes a manifest about 30 seconds after the tree last changed, once
every object it lists is on the box. It keeps every manifest from the last 7
days, the newest of each day for the 30 days before, the newest of each 30-day
stretch for a year, and always the newest.
