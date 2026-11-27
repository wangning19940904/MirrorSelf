# Scheduled sync

`mirrorself sync` pulls data from MCP servers and command-line tools on a
schedule and keeps what they return. A sync is a saved, deterministic
recipe: it names one tool or one program, its arguments, and how often to
run. No model decides what to call.

Each run:

1. computes a time window from the recipe's cursor;
2. calls the one tool or program the recipe names, with the window filled
   into its arguments;
3. stores the response unchanged as an immutable raw object in the vault
   (identical responses are stored once, since the vault is
   content-addressed);
4. advances the cursor and runs the normal pipeline, which parses the
   response like any other raw object.

Fetching only happens in `mirrorself sync …`. `mirrorself process` (and
`process --rebuild`) never calls out; it re-parses what is in the vault.

## Commands

```bash
mirrorself sync list                  # recipes, cursor, last success or error, pause, missing secrets
mirrorself sync run <name>            # run one recipe now
mirrorself sync run --due             # run every recipe whose interval has passed
mirrorself sync daemon                # run due recipes in a loop until Ctrl-C
mirrorself sync resume <name>         # clear a pause
mirrorself sync history [<name>]      # recent runs
mirrorself sync secret set <name>     # store a credential, read from stdin
mirrorself sync secret set <name> --from-env VAR
mirrorself sync secret list           # names only
mirrorself sync secret remove <name>
```

All of them accept `--json`. `sync run` exits non-zero when a recipe failed
(1) or is paused or refused (6). `sync run --due` from cron or launchd works
as well as the daemon; both use the same state.

## Recipe reference

Recipes are `[[sync]]` tables in `mirrorself.toml`. They are checked when
the file is read, and a bad recipe stops `mirrorself` with an error naming
the recipe and the field.

| Key | Required | Meaning |
|---|---|---|
| `name` | yes | Unique; `a-z`, `0-9`, `_`, `-`. |
| `adapter` | yes | `mcp` or `cli`. |
| `source` | yes | `raw_objects.source` of what is stored, e.g. `github`. |
| `kind` | yes | `raw_objects.kind`: `mcp_response`, `cli_response`, or a typed kind such as `feishu_calendar_events`. The kind picks the parser. |
| `interval` | yes | How often the recipe is due: `30s`, `15m`, `1h`, `1d`, `1w`. At least `1m`. |
| `overlap` | no | How far each window reaches back before the cursor (default `0s`). Records in the overlap come back again; see de-duplication below. |
| `lookback` | no | Length of the first window, before there is a cursor (default `7d`). |
| `cursor_format` | no | strftime format for `{{since}}` and `{{until}}` (default `%Y-%m-%dT%H:%M:%SZ`). |
| `utc_offset` | no | Offset the window is rendered in, and the local day of records whose timestamps are UTC or carry no offset (default `+00:00`). |
| `timeout` | no | Per call (default `120s`, at most `1h`). |
| `max_bytes` | no | Largest response kept (default 16 MiB, at most 256 MiB). Larger responses fail the run. |

### Windows and placeholders

A run at time *now* covers `since .. until`, where `until` is *now* and
`since` is the cursor minus `overlap`, or *now* minus `lookback` on the first
run. After a successful run the cursor becomes `until`. A failed run leaves
the cursor alone, so the next run covers the gap.

String values in tool arguments and CLI arguments may use:

| Placeholder | Value |
|---|---|
| `{{since}}`, `{{until}}` | The window ends, formatted with `cursor_format` in `utc_offset`. |
| `{{since_date}}`, `{{until_date}}` | `YYYY-MM-DD` in `utc_offset`. |
| `{{since_unix}}`, `{{until_unix}}` | Unix seconds. |

A recipe is due when it never ran or `interval` has passed since its last
attempt, successful or not. Paused recipes are never due.

### `[sync.mcp]`

| Key | Meaning |
|---|---|
| `url` | Streamable HTTP endpoint. `https://`, or `http://` for loopback only. |
| `bearer_secret` | Name of a stored secret sent as `Authorization: Bearer …`. |
| `headers` | Extra HTTP headers. Values may use `${secret:<name>}`. |
| `command`, `args` | A stdio server, run directly (no shell). |
| `env` | Extra environment for a stdio server. Values may use `${secret:<name>}`. |
| `tool` | The one tool this recipe calls. |
| `arguments` | Tool arguments (a TOML table, sent as JSON). |

Set exactly one of `url` and `command`.

### `[sync.cli]`

| Key | Meaning |
|---|---|
| `program` | Run directly, never through a shell. A bare name is looked up on `PATH`. |
| `args` | Arguments; may use window placeholders. |
| `env` | Extra environment. Values may use `${secret:<name>}`. |

stdout is the response. A non-zero exit, a timeout or more than `max_bytes`
of output fails the run; the end of stderr goes into the error message.

### `[sync.items]`: the generic tier

`mcp_response` and `cli_response` objects become `items` rows of type
`record` (`v_items`). For an MCP result, the payload is its single JSON text
block if it has one (that is how most servers return structured data), else
`structuredContent`, else its text. For CLI output, the payload is the JSON
it printed, else its text.

| Key | Meaning |
|---|---|
| `subtype` | `items.subtype`, e.g. `github_commit`. |
| `category` | `items.category_id`, e.g. `work`. |
| `records` | Path to the array of records (e.g. `items`). Without it, a top-level array is split into records and anything else is one item. |
| `key_field` | Identifies a record across responses. With it, a record that comes back in a later (overlapping) response replaces the earlier item instead of adding a second one. |
| `title_field`, `text_field` | Where the title (first line, 200 characters) and text come from. Defaults: the first of `title`, `name`, `subject`, `summary`, `message`, and of `body`, `description`, `text`, `content`. |
| `time_field`, `end_field` | When the record happened: RFC 3339, `YYYY-MM-DD`, or Unix seconds or milliseconds. |
| `updated_field` | The source's own last-modified time → `items.source_updated_at`. |

Paths are dot-separated object keys, e.g. `commit.author.date`. The rules are
read when a response is processed; after changing them, run
`mirrorself process --rebuild`.

## The GitHub recipe

GitHub activity comes from the official GitHub MCP server
([github/github-mcp-server](https://github.com/github/github-mcp-server)),
read-only tools only. Two recipes: commits via `search_commits`, pull
requests via `search_pull_requests`. Both search across all repositories, so
one recipe covers everything the user authored.

First create a fine-grained personal access token with read access to the
repositories you want, and store it:

```bash
mirrorself sync secret set github          # paste the token, press Enter
```

Remote server (read-only endpoint):

```toml
[[sync]]
name = "github-commits"
adapter = "mcp"
source = "github"
kind = "mcp_response"
interval = "1h"
overlap = "1d"
lookback = "30d"
cursor_format = "%Y-%m-%d"
utc_offset = "+08:00"
[sync.mcp]
url = "https://api.githubcopilot.com/mcp/readonly"
bearer_secret = "github"
tool = "search_commits"
[sync.mcp.headers]
X-MCP-Toolsets = "repos"
[sync.mcp.arguments]
query = "author:YOUR_LOGIN author-date:{{since}}..{{until}}"
sort = "author-date"
order = "desc"
perPage = 100
[sync.items]
subtype = "github_commit"
category = "work"
records = "items"
key_field = "sha"
title_field = "commit.message"
time_field = "commit.author.date"
[sync.github]
login = "YOUR_LOGIN"

[[sync]]
name = "github-prs"
adapter = "mcp"
source = "github"
kind = "mcp_response"
interval = "1h"
overlap = "1d"
lookback = "30d"
cursor_format = "%Y-%m-%d"
utc_offset = "+08:00"
[sync.mcp]
url = "https://api.githubcopilot.com/mcp/readonly"
bearer_secret = "github"
tool = "search_pull_requests"
[sync.mcp.headers]
X-MCP-Toolsets = "pull_requests"
[sync.mcp.arguments]
query = "author:YOUR_LOGIN updated:{{since}}..{{until}}"
sort = "updated"
order = "desc"
perPage = 100
[sync.items]
subtype = "github_pull_request"
category = "work"
records = "items"
key_field = "html_url"
time_field = "created_at"
updated_field = "updated_at"
[sync.github]
login = "YOUR_LOGIN"
```

Local server over stdio instead: replace the `[sync.mcp]` table's `url`,
`bearer_secret` and `headers` with

```toml
command = "github-mcp-server"
args = ["stdio", "--read-only", "--toolsets", "repos,pull_requests"]
[sync.mcp.env]
GITHUB_PERSONAL_ACCESS_TOKEN = "${secret:github}"
```

(or `command = "docker"` with `args = ["run", "-i", "--rm", "-e",
"GITHUB_PERSONAL_ACCESS_TOKEN", "-e", "GITHUB_READ_ONLY=1",
"ghcr.io/github/github-mcp-server"]`).

The typed mapping (processor `sync.github`) runs on `mcp_response` objects
whose source is `github` and reads the response shape, not the tool name:

| Response | Becomes |
|---|---|
| `search_commits` (`{total_count, incomplete_results, items}`) or `list_commits` (an array) | One `events` row per commit: type `commit`, category `work`, at `commit.author.date`, title `owner/repo: first line of the message`, body the full message, `data` with `repo`, `sha`, `html_url`, `author_login`. |
| `search_pull_requests` (issue-search items) or `list_pull_requests` (an array) | One `events` row per pull request: type `pull_request`, category `work`, at `created_at`, title `owner/repo#N title`, `data` with `repo`, `number`, `state` (`open`, `closed` or `merged`), `merged_at`, `updated_at`, `html_url`, `author_login`. |
| Anything else (`get_me`, …) | Only the generic tier. |

With `[sync.github] login` set, records by anyone else are skipped (the
generic tier still keeps them as items). One event is kept per commit and
per pull request; when a later response reports a newer `updated_at` (for
example a pull request that has since merged), the event is updated.
Repositories become `repository` entities linked from their events
(`in_repository`), so `mirrorself entity owner/repo` works.

Limits: one call per run, so only the first page (up to `perPage` results)
of each window is kept. Keep the interval short enough that a window holds
fewer than 100 commits. GitHub's search index can lag by a few minutes; the
overlap covers that.

## Adding W2's Feishu calendar recipe

The Feishu calendar comes from `lark-cli` on a schedule, stored as kind
`feishu_calendar_events` and parsed by W2's processor (`pipeline/feishu_calendar.rs`).
W2 documents the exact command in `docs/sources/feishu.md`; take the program
and arguments from there and put the window placeholders where the date
range goes. The shape of the recipe:

```toml
[[sync]]
name = "feishu-calendar"
adapter = "cli"
source = "feishu"
kind = "feishu_calendar_events"
interval = "30m"
overlap = "1d"
lookback = "30d"
cursor_format = "%Y-%m-%dT%H:%M:%S%:z"
utc_offset = "+08:00"
timeout = "120s"
[sync.cli]
program = "lark-cli"
# The exact subcommand and flags are in docs/sources/feishu.md.
args = ["calendar", "…", "--start", "{{since}}", "--end", "{{until}}"]
```

`lark-cli` keeps its own login under the user's home directory, so no
secret is needed here. If a future command needs a token, put it in
`[sync.cli.env]` as `"${secret:<name>}"`, never in `args`.

## Safety rules

- **Only the named tool.** A recipe calls exactly one tool; the client calls
  `tools/list` to check it and `tools/call` for that tool, nothing else.
  Nothing in a response chooses what is called next.
- **Read-only tools only.** The tool must be annotated `readOnlyHint: true`
  (and not `destructiveHint: true`) in `tools/list`. Otherwise the run is
  refused, nothing is called, and the recipe is paused. There is no recipe
  option to override this. Annotations are the server's claim; prefer
  servers and endpoints that are read-only themselves (GitHub's
  `/readonly` endpoint or `--read-only` flag).
- **Pinned input schema.** The first successful `tools/list` pins a SHA-256
  of the tool's input schema (keys sorted, so ordering does not matter). If
  the schema later differs, the run calls nothing, the recipe is paused, and
  `sync list` shows why. After checking the recipe's arguments against the
  new schema, `mirrorself sync resume <name>` clears the pause and the next
  run pins the new schema.
- **Responses are data.** They are stored and parsed, never executed, never
  used to pick a tool, never put into a tool description. Responses larger
  than `max_bytes` fail the run. Server errors and stderr are shown with
  control characters removed and length capped.
- **No shell.** Programs and stdio servers are started directly with their
  argument list.
- **Secrets stay sealed.** Tokens are set with `sync secret set` (stdin or an
  environment variable) and stored in `<home>/sync-secrets.json`, each value
  AES-256-GCM-sealed under a key derived from the vault key, bound to its
  name. `mirrorself.toml` only holds `${secret:<name>}` references; a
  credential-looking header or environment variable with a literal value is
  rejected. Secrets are only allowed in headers and environment variables,
  never in arguments (other processes can read arguments). HTTP requests go
  through `curl` with the URL, headers and body written to its stdin, not
  its command line. Secret values are scrubbed from stored error messages.

## State

Sync state is not derived from the vault, so `process --rebuild` keeps it.
It lives in the index (`0004_sync.sql`):

- `sync_state`: per recipe: cursor, last attempt, last success, last error,
  pause flag and reason, and the pinned tool and schema hash
  (`v_sync_status`).
- `sync_runs`: one row per attempt: window, status (`ok`, `error`,
  `refused`, `paused`), the stored raw object and whether it was new
  (`v_sync_runs`).

Deleting `index.db` loses this state (the vault still rebuilds every
derived row). The next run then starts from `lookback` and re-pins schemas;
overlapping responses de-duplicate as usual.

## MCP protocol support

The client is small and blocking (no async runtime in the core crate):

- **stdio**: newline-delimited JSON-RPC over the server's stdin and stdout;
  stderr is kept only for error messages. Requests the server sends to the
  client (sampling, roots) are declined.
- **Streamable HTTP**: one POST per message with
  `Accept: application/json, text/event-stream`, JSON or SSE replies, via the
  system `curl` (installed on macOS and most Linux systems).
- **Revisions**: it first sends `server/discover` in the 2026-07-28 form
  (stateless; `_meta` carries `io.modelcontextprotocol/protocolVersion`,
  `clientInfo` and `clientCapabilities`; HTTP adds `MCP-Protocol-Version`,
  `Mcp-Method` and `Mcp-Name`). If the server does not answer with a result,
  it falls back to `initialize` with 2025-11-25, accepts 2025-11-25,
  2025-06-18, 2025-03-26 or 2024-11-05, sends `notifications/initialized`,
  and keeps any `Mcp-Session-Id` the server hands out.
- Not supported: OAuth browser flows (use a bearer token or a stdio server),
  servers that ask for user input mid-call (`input_required`), and
  paginating a tool's own results.
