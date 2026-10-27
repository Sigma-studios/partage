# partage

Share files through links. An admin logs in with one password, uploads files
(optionally with an expiry date) and gets a link anyone can download from. The
link never changes; once the file expires or is deleted, the link says so.

* **Upload links**: the admin creates a link someone else can use **once** to
  upload one file, up to a size the admin chooses, until a date. The admin also
  sets how long that file is kept (or forever). The person uploading gets the
  download link, and the file shows up in the admin's list.
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

Then log in at `https://files.example.com`, set the storage limit (10 GB until
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
| `/` | admin dashboard (redirects to `/login`) |
| `/f/{id}` | download a file |
| `/u/{token}` | one-time upload page |
| `/health` | `ok` |
