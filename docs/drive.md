# Drive: how it works

A personal drive in partage: folders, uploads (folders dropped whole keep their
tree), versions, a trash, and share links for any file or folder. One admin
password reads and writes everything.

```
 browser ──▶ partage (VPS) ──SFTP──▶ Storage Box sub-account (encrypted objects)
             database, and copies                │
             while they fit                      │ read-only twin
                                                 ▼
                                    PC: Réplique mirror job (verified copy)
```

* **Files are kept on the VPS while they fit** in the space allowed for them
  (Settings → Drive, "Kept on this server up to"), and every file is also copied
  to the **Storage Box**. Once on the box, the VPS copies that do not fit are
  freed, least recently read first; downloads then come from the box. A file
  bigger than the room left on the VPS goes straight to the box.
* **The PC backs up the box** with Réplique's mirror job, through a read-only
  login: two copies (box and PC), plus the VPS's while files fit there.
* **Everything on the box is encrypted** with the Drive key, which only partage
  and your password manager have. Hetzner sees how many objects there are, their
  sizes and when they were written; not names, folders or contents.
* **The box holds the whole tree** (manifests), so the PC's copy and the key are
  enough to get every file back with its name (`partage drive-restore`), without
  partage or its database.

Formats: [drive-format.md](drive-format.md).

## Limits and alerts

* **The Drive's limit** (Settings → Drive) counts everything on the box: every
  version, the trash and the manifests. An upload that would go over is refused;
  Discord hears at 80%, 90% and 100%, and when an upload is refused (at most once
  an hour).
* **A folder's limit** caps what share links can add to it: links that add need
  one, set when the link is created. Uploads through any link stop once the
  folder (with everything under it) holds that much; deleting frees room. The
  admin is not limited by it. Uploads in progress count, so parallel uploads
  cannot overshoot.
* Discord also hears when the box has been unreachable for 6 hours, and when the
  daily check finds a problem.

## Share links

| Link | Can |
| --- | --- |
| file | download (or open, for images, video, audio, PDF) |
| read | browse the folder and download |
| read + add | also upload files and folders, create folders |
| read + add + delete | also delete (to the admin's trash) |
| add only | upload and create folders without seeing what is there |

Links have an optional expiry, can be revoked (item details or Track), and count
their uses. People adding never overwrite anything: a taken name gets a number
(`name (2).ext`). The admin dropping a file with a taken name makes a new version
of it.

## Deleting and versions

1. Deleting moves a file or folder to the trash, with everything under it.
   Replacing a file keeps the previous version.
2. After 30 days, trashed items and replaced versions go for good.
3. Manifests are kept on a schedule: all from the last 7 days, the newest of each
   day for 30 days before that, the newest of each 30-day stretch for a year.
4. An object is deleted from the box (and the VPS) only once no kept manifest
   lists it.
5. The PC's mirror then moves it aside for its `keepRemovedDays` (30 by default).

## The sync loop and Réplique

`src/sync.rs` runs every 30 seconds, or sooner after a change:

* once the tree has been quiet for 30 seconds (or after 5 minutes of changes), a
  pass copies the objects not on the box yet (checking each against its name),
  writes a manifest, and once an hour expires the trash, old manifests and the
  objects nothing lists any more;
* frees VPS copies beyond the space allowed;
* once a day, checks the box: every object the database puts there is listed
  with the right size, up to 32 objects (2 GB) are read back against their names
  and opened with the key, and the newest manifest opens. A copy found wrong is
  sent again from the VPS when the VPS still has it; otherwise it is a problem.

Paired with Réplique (Settings → Drive → With Réplique; on Réplique's website,
a device of kind *partage*), partage reports each pass as a `backup` run of the
job `drive` (one at least daily even when nothing changed, so a silent partage is
noticed), uploads straight to the box as runs too, and each daily check as a
`check` run. Reports that cannot be sent wait in the database. On the PC, the
mirror job copies the store; see Réplique's docs/client.md, "Mirroring
partage".

## Uploads and downloads that survive a bad connection

* **Uploads** go in pieces of 16 MiB: the page starts an upload (the space is
  held from then on), then sends `PUT …/uploads/{id}?offset=N`. A piece cut
  off still counts what arrived; the page asks how much did and goes on from
  there, after a pause that grows (up to 30 seconds), and at once when the
  browser comes back online. An upload nobody continues for a day is dropped,
  and so is every unfinished upload when partage restarts: the page then sends
  that file again from the start.
* **Downloads** of files and of folder zips answer ranges, with an ETag, so a
  browser or `curl -C -` resumes where it stopped (and starts over if the file or
  folder changed meanwhile).
* **Folder zips** (Download as zip, for the admin and for links that can read)
  are built on the fly. Files are stored as they are, with the CRC-32 kept from
  their upload, so the whole zip is known before it is sent: it has a length,
  any range of it can be served, and zip64 takes over past 4 GB or 65,535
  entries.

## From a shell

`partage drive-pair <réplique url> <code>` pairs the Drive with Réplique like
Settings → Drive → With Réplique (`facade exec partage partage drive-pair …`);
the running server picks the new box up within a minute.

## Not done yet

* Rebuilding the database from the newest manifest (`drive-restore` writes the
  files out; a new partage would start from an empty Drive).
* Moving to another Storage Box: objects only on the old one are not copied over.
* The existing drop zone's files and one-time secrets stay on the VPS.
