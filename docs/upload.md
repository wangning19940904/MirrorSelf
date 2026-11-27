# Phone upload

Send files from a phone to your hub: a photo you just took, a checkup PDF, a
bill CSV, a Health export. There is no app to install. The hub serves a small
web page, and the phone opens it in Safari or Chrome over the local network.

Each file is stored in the encrypted vault through the same path as
`mirrorself import auto`: a source that recognises the file claims it (an
Apple Health export becomes `apple_health_export`, an Alipay bill
`alipay_bill_csv`), and anything else is kept under a generic kind
(`file_pdf`, `file_image`, `file_text`, `file_other`). The pipeline runs
right after each upload.

## Turn on LAN access

By default the hub listens on loopback only (`127.0.0.1:12500`), so a phone
cannot reach it. To allow uploads from the local network, add `lan_addr` to
`[server]` in `mirrorself.toml`:

```toml
[server]
addr = "127.0.0.1:12500"
lan_addr = "0.0.0.0:12501"   # all interfaces; or this computer's LAN IP, e.g. "192.168.1.20:12501"
```

Then start the hub:

```bash
mirrorself serve
```

It prints the addresses and a pairing code:

```text
warning: the upload page is reachable from the local network at 0.0.0.0:12501 over plain HTTP; ...
mirrorself REST API listening on http://127.0.0.1:12500/v1
upload page on this computer: http://127.0.0.1:12500/upload
upload page for your phone:   http://192.168.1.20:12501/upload
pairing code: 4831-0927  (enter it on the upload page; it works once, and a new one is printed after each use)
```

Only the upload page, pairing and the upload route exist on `lan_addr`. The
read API (`/v1/overview`, `/v1/timeline`, `/v1/query/sql`, `/v1/raw/…` and
the rest) stays on loopback and answers 404 on the LAN address.

`lan_addr` must use a different port from `addr`; 12501 is the port reserved
for it in MirrorSelf's block (12500–12599). If the port is taken, `serve`
fails instead of picking another one. Remove `lan_addr` to go back to
loopback only.

## Pair the phone

1. Open the "upload page for your phone" URL on the phone (opening the bare
   address redirects to `/upload`).
2. Type the 8-digit pairing code that `mirrorself serve` printed.
3. Take a photo or choose files. Each file shows its progress, then what it
   was stored as ("Stored as PDF document", "Already on the hub (Apple Health
   export)") or the error, with a Retry button for failures.

How pairing works:

- The long-lived API token (`api.token`) is never typed on the phone, put in
  a URL, or accepted on the LAN address.
- The pairing code is single-use. It is replaced after a successful pairing,
  after five wrong attempts, and when it has gone unused for 10 minutes (the
  next attempt with it then fails); each new code is printed on the hub's
  console. To pair a second phone, use the
  newest code.
- Pairing attempts are limited to 10 a minute from all clients together
  (HTTP 429 with `Retry-After` beyond that).
- A pairing gives the page a session credential that can only upload. It
  cannot read anything: the read API answers 403 for it. It expires after
  `server.upload_session_minutes` (12 hours by default). Sessions are held in
  the hub's memory only, so restarting `mirrorself serve` signs every phone
  out; the page notices and asks for a new code.
- "Unpair this phone" ends the session on the hub.

## Security boundary

The LAN listener speaks **plain HTTP**. Anyone on the same network can watch
the traffic: they can read the files you upload and could capture a session
credential and upload files of their own until it expires. They cannot read
your data through the hub, because the LAN address serves no read routes and
a session cannot read. Pairing codes are short-lived and single-use.

Use LAN access on a network you trust, such as your home Wi-Fi. For anything
else (a shared office, a café, access from outside the house), do not open
`lan_addr` to that network. Instead, reach the hub through a tailnet (for
example Tailscale, with `lan_addr` set to the tailnet address) or put a
TLS-terminating reverse proxy in front of it. TLS, tunnels and remote access
are not part of MirrorSelf itself.

Every pairing and upload is written to the audit log (`mirrorself audit`):

| Field | Value |
|---|---|
| `actor` | `upload-page` for phone sessions, `rest` for the API token |
| `client_id` | `session-<id>@<phone IP>`, `unpaired@<IP>` for refused pairings, or `bearer-token` |
| `action` | `pair`, `upload`, `unpair` |
| `scope` | `upload:write` |
| `rows` | for `upload`, the number of bytes; 1 or 0 for `pair` |
| `outcome` | `ok`, `denied` (wrong or expired pairing code) or `error` (too large, empty, failed import) |

File names can be personal, so the log stores only a digest of the request,
never the name, and never the file contents.

## Limits

- One file per request, up to `server.upload_max_bytes` (256 MiB by
  default). The page refuses larger files before sending them; the server
  answers 413 when `Content-Length` is over the limit and stops reading a
  body without one once it passes the limit. The file is held in memory while
  it is imported, so peak memory use is a few times the file size.
- Uploads are processed before the response returns. A multi-year Health
  export can take a few minutes; keep the page open until it reports the
  result. If the pipeline reports a problem, the file is still stored and the
  page shows the error next to it.
- Re-uploading identical bytes stores nothing new (`created: false`, "Already
  on the hub").
- No QR code, native app, TLS, or remote access.

## API

For scripts on the hub itself, the API token works on the loopback address:

```bash
curl -H "Authorization: Bearer $(cat ~/.mirrorself/api.token)" \
     -H "X-File-Name: checkup.pdf" \
     --data-binary @checkup.pdf http://127.0.0.1:12500/v1/upload
```

| Route | Credential | Purpose |
|---|---|---|
| `GET /upload` | none | The upload page |
| `POST /v1/pair` | none (`{"code": "48310927"}`) | Returns `{session, expires_in, max_bytes}` |
| `GET /v1/session` | session | Remaining lifetime and the current limit |
| `POST /v1/unpair` | session | Ends the session |
| `POST /v1/upload` | session, or the API token on loopback | Stores one file |

`POST /v1/upload` takes the file as the raw request body. The name goes in
`X-File-Name` (percent-encoded UTF-8, as `encodeURIComponent` produces) or in
`?name=`. Only its last path component is kept. The response (201 when the
file is new, 200 when it was already stored) is:

```json
{
  "raw_id": "1b233f70…",
  "created": true,
  "bytes": 200036,
  "source": "file",
  "kind": "file_pdf",
  "name": "体检报告 2026.pdf",
  "errors": []
}
```

`errors` lists processing problems for this file, as
`{processor, status, error}`. Error responses are `{"error": "…", "code": "…"}`
with `code` one of `pair_required`, `token_not_allowed`, `wrong_code`,
`code_expired`, `rate_limited`, `too_large`, `empty`, `bad_name`,
`interrupted`, `import_failed`.
