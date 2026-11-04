# partage

Share files through links. An admin logs in with one password, drops files on
the main page (optionally with an expiry) and gets a link anyone can download
from. The link never changes; once the file expires or is deleted, the link
says so.

The main page is a drop zone with the space left above it. Several files give
one link each; a **folder** is zipped in the browser (stored, not compressed,
zip64 when needed) and gives one link. **Track** lists files, one-time secrets
and upload links; **Settings** holds the storage limit and Discord alerts.

* **One-time secrets**: tick *One-time secret* under the drop zone to send files
  or a text that open **once**. They are encrypted in the browser
  (AES-256-GCM, 1 MiB records); the key is only in the link's `#fragment`,
  which browsers never send, so the server stores data it cannot read, not
  even the file name. The link opens a page with a button: link previews
  (Discord, Slack…) load the page but open nothing. Opening deletes the data
  before the first byte is sent, so a cut-off download still counts as
  opened. Files are decrypted by a service worker straight to disk (no size
  limit; browsers without one hold the file in memory, and the page warns
  above 500 MB). A damaged or truncated link is detected before opening.
* **Receipts**: each secret has a receipt page, shown to whoever created it and
  listed under Track. Its timeline shows when it was created, every view of
  its page (with IP and browser, so previews show up), when it was opened,
  whether the download finished or was interrupted, attempts after it was
  gone, and expiry. **Burn** destroys a secret before it is opened, from the
  receipt, the upload result, or Track.

* **Upload links**: the admin creates a link someone else can use **once** to
  upload one file (or folder), up to a size the admin chooses, until a date.
  The admin also sets how long that file is kept (or forever). The person
  uploading gets the download link, and the file shows up under Track. A link
  in **one-time secret** mode takes an encrypted file or text instead: the
  person gets the one-time link to send on (the server never has its key) and
  its receipt; the admin sees the receipt under Track.
* **Storage limit**: the admin sets a total size for everything stored. When
  usage crosses **80%** and **90%**, an `@everyone` alert goes to a Discord
  webhook. An upload that would go **over 100%** is refused, with an alert at
  most once an hour. A level alerts again only after usage drops back below it.
* **Compression**: uploads are compressed with zstd as they arrive. A test on
  the first megabyte skips files that would shrink by less than 5% (zip, video,
  JPEG…). Downloads are decompressed on the fly: people always get the original
  file. The limit counts the size on disk, after compression.
* No size limit apart from the storage limit. Uploads and downloads are
  streamed, so memory use stays flat.

Data (SQLite database and files) lives in `PARTAGE_DATA`.

## Deploying on facade

No Dockerfile: facade's scan finds the Rust binary, its port (8080) and the
variables it reads, and builds it.

```sh
facade app create partage --git https://github.com/Sigma-studios/partage \
  --domain files.example.com --volume partage-data:/data --health "partage healthcheck"
facade env set partage PARTAGE_DATA=/data PARTAGE_PUBLIC_URL=https://files.example.com --public
facade env set partage PARTAGE_ADMIN_PASSWORD           # prompts; sensitive
facade deploy partage
```

Then log in at `https://files.example.com`, open **Settings**, set the storage limit (10 GB until
you do) and paste the Discord channel's webhook URL under **Discord alerts**;
**Send test alert** checks it. Alerts wait until a webhook is set.

| variable | default | |
|---|---|---|
| `PARTAGE_ADMIN_PASSWORD` | required | the admin password |
| `PARTAGE_PUBLIC_URL` | from each request | origin of the links handed out |
| `PARTAGE_DATA` | `data` | database and files: put it on the volume |
| `PARTAGE_ADDR` | `0.0.0.0:8080` | listen address |
| `RUST_LOG` | `partage=info` | log level |

`partage healthcheck` exits with success when the server answers. Admin sessions last 30
days. After 10 wrong passwords from one address (facade's `X-Real-IP`), login
is locked for 15 minutes.

## Running locally

```sh
PARTAGE_ADMIN_PASSWORD=change-me-please cargo run    # http://localhost:8080
```

## Routes

| | |
|---|---|
| `/` | drop zone (redirects to `/login`) |
| `/track`, `/settings` | admin pages |
| `/f/{id}` | download a file |
| `/u/{token}` | one-time upload page |
| `/s/{id}#key` | open a one-time secret |
| `/r/{token}` | a secret's receipt |
| `/health` | `ok` |
