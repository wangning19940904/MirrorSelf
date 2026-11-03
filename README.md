# MirrorSelf

A user-owned personal data layer. MirrorSelf keeps every original file in an
encrypted vault, derives a linked, queryable index from it, and lets you and
the agents you choose (Claude, ChatGPT, local coding agents) ask questions
through a CLI, a REST API and MCP.

This is the MVP: milestones **M0 (foundations)** and **M1 (server, Apple
Health, CLI, local MCP)** from *MirrorSelf — System Architecture v1*. It runs
as a personal server (deployment mode B) on macOS or Linux.

## What works

- **Encrypted raw vault.** Each file gets its own AES-256-GCM key, wrapped by
  your vault key. Files are addressed by HMAC-SHA256, so importing the same
  file twice stores it once. Storage is local disk or NAS (`local:/path`).
- **Encrypted index.** SQLCipher, keyed from the vault key. Delete it at any
  time and run `mirrorself process --rebuild` to get identical results back.
- **Apple Health import.** `export.zip` or `export.xml`, streamed. Units are
  normalized to UCUM, timestamps are stored as UTC plus the original offset,
  and a watch's and a phone's overlapping samples count once.
- **Derived data.** Daily aggregates, 28-day personal baselines and z-scores,
  sleep sessions, workouts, notable deviations on a timeline, device and
  activity entities with links, and full-text search.
- **Provenance.** Every value carries `raw_id`, `run_id` and confidence.
  `processor_runs` records every processor version that ran.
- **One query layer, three surfaces.** The CLI (`--json` on every read), REST
  (`/v1`, bearer token, loopback only) and MCP over stdio (9 read-only tools,
  the `mirrorself://schema` resource and 2 prompts). Every agent and API read
  is written to the audit log.

## Quick start

```bash
cargo build --release
```

```bash
./target/release/mirrorself init
```

```bash
./target/release/mirrorself import apple-health ~/Downloads/export.zip
```

```bash
./target/release/mirrorself overview
```

```bash
./target/release/mirrorself metrics ms:resting_hr --from 90d
```

To try it without real data, generate a synthetic export (fake, deterministic):

```bash
./target/release/mirrorself dev synth-apple-health --years 5 --out /tmp/synthetic-export.zip
```

Back up `~/.mirrorself/vault.key`. Without it the vault and index cannot be
decrypted. Set `MIRRORSELF_HOME` or pass `--home` to use another directory.

## Connect an agent (local MCP)

Claude Code:

```bash
claude mcp add mirrorself -- /absolute/path/to/mirrorself mcp serve --stdio
```

Claude Desktop (`claude_desktop_config.json`):

```json
{ "mcpServers": { "mirrorself": { "command": "/absolute/path/to/mirrorself", "args": ["mcp", "serve", "--stdio"] } } }
```

Then ask: *"How did my resting heart rate change over the last 90 days?"* The
answer comes from `get_metric_series`, which returns the record ids behind
each day.

## REST API

`mirrorself serve` listens on `127.0.0.1:12500` (MirrorSelf's reserved port
block is 12500–12599) and exits if the port is taken. Send the token from
`~/.mirrorself/api.token` as `Authorization: Bearer …`.

| Method and path | Purpose |
| --- | --- |
| `GET /v1/health` | Liveness, no auth |
| `GET /v1/overview` | Coverage, sources, latest key values |
| `GET /v1/timeline?from&to&types&limit&offset` | Events and notable deviations |
| `GET /v1/metrics` | Metric codes |
| `GET /v1/metrics/{code}/series?from&to&bucket&stat` | Series with baseline and z |
| `GET /v1/metrics/{code}/compare?a_from&a_to&b_from&b_to` | Two periods compared |
| `GET /v1/labs?code&from&to` | Lab results (empty until M2) |
| `GET /v1/documents/{id}` | Documents (from M2) |
| `GET /v1/entities?q` · `GET /v1/entities/{id}/links` | Entities and linked records |
| `GET /v1/search?q&from&to&kinds` | Full-text search |
| `POST /v1/query/sql` | Read-only SQL over `v_*` views |
| `GET /v1/audit` | Access log |
| `GET /v1/raw/{id}` | The original file |

Dates accept `YYYY-MM-DD`, `today`, or look-backs such as `90d`, `12w`, `6m`, `1y`.

## Not in this MVP

Checkup-report OCR and extraction with a review queue (M2), the iOS app and
sync (M3), remote MCP with OAuth 2.1 grants, scopes and redaction (M4), device
mode (M5), Android and Huawei import (M6), cloud storage adapters, FHIR and
Parquet export, and vector search.

## Layout

```text
crates/core     schema, vault, storage, Apple Health importer, pipeline, QueryService
crates/server   REST API (axum) and MCP server (stdio)
crates/cli      the mirrorself binary
fixtures/       synthetic Apple Health fixture used by the tests
```

See `AGENTS.md` for build and test commands and project conventions.
