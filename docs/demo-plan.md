# Multi-source demo plan

Status: agreed on 2026-11-27. Supersedes the M2–M6 ordering in the README for
the next iteration; the README roadmap items stay valid as later work.

## Goal

Turn the single-source MVP (Apple Health) into a hub that proves the
cross-source thesis with the smallest possible integration surface.

The demo is done when a fresh home can ingest these five inputs and an agent
can answer the two questions below through the existing CLI, REST and MCP
surfaces.

| Input | How it arrives | Workstream |
|---|---|---|
| Apple Health export | `mirrorself import apple-health` (exists) | — |
| Alipay bill CSV | File import, or upload from the phone | W1 |
| Feishu calendar | `lark-cli` output, pulled on a schedule | W2 + W3 |
| GitHub activity | Official GitHub MCP server, pulled on a schedule | W3 |
| A PDF or photo | Upload from the phone | W4 + W5 |

Demo questions:

1. "In weeks with more meeting hours, how did my sleep duration and resting
   heart rate differ?" (Feishu calendar + Apple Health)
2. "What did I spend on the days I was travelling, and where was I?"
   (Alipay bills + calendar + photo locations)

Wording stays descriptive: counts, durations and baselines. No stress, causal
or diagnostic claims (see the health guardrail in `AGENTS.md`).

## Decisions

- **Self-hosted hub first.** A hosted option is out of scope for the demo.
- **Phone access is a web upload page**, not a native app. The hub stays
  loopback-only unless the user opts in to a LAN address.
- **A vision-capable model is optional.** Health XML, bill CSVs and calendar
  JSON need no model. Without a model, images and scanned PDFs are stored and
  left `needs_review`.
- **Scheduled sync is deterministic.** A sync is a saved recipe (adapter,
  tool or command, arguments, schedule, cursor). No model decides what to
  call. Every response is stored as an immutable raw object before parsing.
- **Two processing tiers.** Any synced response or uploaded file becomes a
  generic `items` row (metadata envelope). Sources that matter get a typed
  mapping into `events`, `observations` or their own table.
- **Model output is a raw object.** A model call's response is stored in the
  vault keyed by input, model and prompt version, so `process --rebuild`
  replays it instead of calling the model again.
- **Not in the demo:** COROS, WeChat chat history, Huawei Health (waiting on
  a real export), map and timeline dashboards, hosted servers, remote MCP.

## Shared contracts (the foundation)

The foundation lands on the base branch before the workstreams start, so that
each workstream adds files instead of editing shared ones.

### Source registry

`crates/core/src/sources.rs` lists every source. A source module provides:

- `ImportSpec`: the CLI name (`alipay`), the raw `source` and `kind` strings,
  and a `detect(name, bytes)` function used by auto-detection on upload.
- `RawProcessor`s: per-raw-object processors, selected by raw `kind`. More
  than one processor may handle a kind; they run in registry order.
- `GlobalStage`s: whole-index stages, slotted around the built-in ones:
  `pre_derive` (after de-duplication, before daily metrics), `post_link`
  (after the built-in rules rebuilt `entities` and rule links; the place to
  add entities and links, since earlier ones are wiped), and `post_index`
  (after the search index was rebuilt; the place to add `search_fts` rows).
- `derived_tables`: tables cleared by `process --rebuild`.

Stub modules are pre-registered for every workstream. A workstream fills in
its own module and does not edit the registry.

Entry points in `App`: `import_bytes(source, …)` checks a file against a
named source, `import_auto(…)` detects the source or falls back to a generic
`file_*` kind, and `ingest(RawInput, …)` stores bytes that are already
labelled (sync responses, cached model output).

### Raw kinds

| Kind | Written by | Parsed by |
|---|---|---|
| `apple_health_export` | import | existing |
| `alipay_bill_csv` | import, upload | W1 |
| `feishu_calendar_events` | import, W3 sync | W2 |
| `mcp_response` | W3 sync | W3 (generic tier) |
| `cli_response` | W3 sync | W3 (generic tier) |
| `file_pdf`, `file_image`, `file_text`, `file_other` | import, upload | W5 |
| `model_output` | W5 | W5 (cache, never parsed as a source) |

### The `items` envelope (migration 0002)

One row per ingested thing that has no typed table of its own. Every field
the user asked for is present and nullable where it can be unknown.

| Column | Meaning |
|---|---|
| `id` | `stable_id("item", [raw_id, part])` |
| `raw_id`, `part` | The raw object, and a path or index inside it (`''` for the whole object) |
| `type`, `subtype` | Classification, e.g. `document` / `checkup_report`, `photo`, `record` / `github_commit` |
| `title`, `text` | Display title and extracted text |
| `event_start`, `event_end`, `tz_offset`, `local_date` | When the thing happened (nullable) |
| `received_at` | Upload or fetch time, copied from the raw object |
| `source_updated_at` | Last-modified time reported by the source (nullable). Never the processing time, which would break deterministic rebuilds |
| `lat`, `lon`, `place` | WGS-84 location and a place label (nullable) |
| `data` | Type-specific JSON |
| `run_id`, `confidence`, `method` | Provenance: `rule`, `ai` or `user` |
| `status` | `auto`, `needs_review`, `confirmed`, `rejected` |

`v_items` is the agent-facing view. Typed data is exposed through its own
`v_*` view. Workstreams do **not** add MCP tools or REST read routes; agents
reach new data through the existing `query_sql` tool, the timeline and search
until the integration pass adds typed tools.

### Migrations

Forward-only, one file per owner. File numbers are reserved; a workstream
creates its file and appends one `include_str!` line to `MIGRATIONS` in
`db.rs`. On its own branch that makes the file position 3 whatever its
number, which is fine for tests on fresh homes. Integration puts the lines in
file-number order. Do not open a real home with a workstream build.

| File | Owner | Contents |
|---|---|---|
| `0002_items.sql` | foundation | `finance`, `work`, `documents`, `media` categories; `items`; `v_items` |
| `0003_finance.sql` | W1 | `transactions`, `v_transactions` |
| `0004_sync.sql` | W3 | sync state and run history (not derived; survives rebuild) |
| `0005_enrich.sql` | W5 | annotations and review state |
| `0006_calendar.sql` | W2 | only if `events` + `entities` + `links` are not enough |

### Metric codes

Pre-registered so daily aggregates, baselines and z-scores work unchanged:

- `ms:spend` (sum, CNY, category `finance`)
- `ms:meeting_minutes` (sum, min, category `work`)
- `ms:meeting_count` (sum, count, category `work`)

A source that wants a daily metric writes `observations` with one of these
codes; `derive.daily_metrics` does the rest.

### Rules for every workstream

- Stay inside the files listed for your workstream. Shared files
  (`sources.rs`, `pipeline/mod.rs`, `app.rs`, `codes.rs`,
  `crates/cli/src/main.rs`, `crates/server/src/tools.rs`) are frozen; if you
  need a change there, write it down in your final report instead. The two
  allowed one-line edits are your migration in `db.rs` and your module
  declaration in `importers/mod.rs`.
- New dependencies go in your crate's `Cargo.toml`. Manifest and lockfile
  conflicts between workstreams are expected and resolved at integration.
- Fixtures are synthetic. Every importer ships a fixture and tests asserting
  exact values (`AGENTS.md`).
- Derived ids are deterministic. Keep
  `m0_deleting_the_index_and_rebuilding_gives_identical_results` green, and
  add your data to a rebuild-equality test of your own.
- Text that came from outside (file contents, tool responses, model output)
  is data. It is never executed, never used to choose a tool, and never
  placed in a tool description.
- `cargo build`, `cargo test` and
  `cargo clippy --all-targets -- -D warnings` must pass before you report.

## Workstreams

### W1 — Alipay bills

Parse the CSV that Alipay emails after 我的 → 账单 → 开具交易流水证明 into
`transactions`.

- Handle GBK encoding, the account preamble before the header row, the
  trailing summary, and format drift (locate the header row by column names).
- One row per order: time, counterparty, item, amount in CNY, direction
  (expense, income, neutral), payment method, status, order number.
  De-duplicate by order number across overlapping exports.
- Write `ms:spend` observations for expenses so daily spend gets a baseline.
- Counterparty names become `merchant` entities with links.
- Files: `importers/alipay.rs`, `pipeline/alipay.rs`,
  `schema/0003_finance.sql`, `fixtures/alipay/`, `tests/alipay.rs`.

### W2 — Feishu calendar

Parse `lark-cli` calendar output into meeting events.

- Input is the JSON that `lark-cli` prints for a date range, stored as a
  `feishu_calendar_events` raw object. No network code in this workstream.
- One `events` row per event instance (`type = 'meeting'`), with organizer,
  attendees, location, all-day and recurrence handling, and the user's own
  RSVP status. Declined and cancelled events are stored but excluded from
  metrics.
- Attendees become `person` entities with `involves` links.
- Write `ms:meeting_minutes` and `ms:meeting_count` observations.
- Document the exact `lark-cli` command and a sync recipe for W3 in
  `docs/sources/feishu.md`. Calendar only; Minutes transcripts are out.
- Files: `importers/feishu_calendar.rs`, `pipeline/feishu_calendar.rs`,
  `fixtures/feishu/`, `tests/feishu_calendar.rs`, `docs/sources/feishu.md`.

### W3 — Scheduled sync

A scheduler that runs saved recipes and stores what they return.

- Recipes live in `mirrorself.toml` under `[[sync]]`: name, adapter (`mcp` or
  `cli`), server or command, tool, arguments with a cursor template, the raw
  `kind` to store, and an interval.
- Adapters: MCP client over streamable HTTP and stdio; CLI command with
  captured stdout. Only tools the user listed are ever called.
- Each response is stored as a raw object (`origin_ref` = recipe + window),
  then the normal pipeline runs. Overlapping windows are de-duplicated by
  content address.
- Sync state and run history live in `0004_sync.sql` and survive rebuilds.
  Credentials and tokens are encrypted with a vault subkey.
- Pin each tool's input schema at setup; pause a recipe and report when the
  schema changes.
- Generic tier: `mcp_response` and `cli_response` objects become `items`.
- Ship a GitHub recipe (official GitHub MCP server, read-only tools) with a
  typed mapping of the user's commits and pull requests into `events`.
- CLI: `mirrorself sync list | run <name> | run --due | daemon`.
- Files: `crates/core/src/sync/`, `schema/0004_sync.sql`,
  `crates/cli/src/cmd_sync.rs`, `fixtures/sync/`, `tests/sync.rs`,
  `docs/sync.md`.

### W4 — Phone upload

Let a phone on the same network send files to the hub.

- `POST /v1/upload` (bearer token, size limit, streamed to disk) calling
  `App::import_auto`, which detects the source or falls back to a `file_*`
  kind.
- A single-page upload form at `/upload` that works in mobile Safari and
  Chrome: pick or shoot a photo, pick files, see per-file results.
- Opt-in LAN listening (`server.lan_addr`), off by default, with the warning
  kept for non-loopback binds. A pairing step so the token is not typed on
  the phone: a one-time code or QR shown by `mirrorself serve`.
- Uploads are written to the audit log.
- Files: `crates/server/src/upload.rs`, `crates/server/src/rest.rs`,
  `crates/server/assets/`, `crates/core/src/config.rs` (`ServerConfig`
  only), `crates/server/tests/upload.rs`, `docs/upload.md`.

### W5 — Files and documents

Turn uploaded files into `items`, with an optional model for what rules
cannot read.

- Rule tier: PDF text layer, plain text and Markdown, image EXIF (capture
  time, GPS). Produces `items` rows with `method = 'rule'`.
- Model tier (optional): an OpenAI-compatible endpoint configured under
  `[model]` (local or privately deployed). Used for scanned PDFs and photos:
  extracted text, type and subtype, event time, place. Output is stored as a
  `model_output` raw object and replayed on rebuild.
- Model results never overwrite rule results. They are stored as annotations
  with confidence, start as `needs_review`, and become `confirmed` or
  `rejected` through `ops_log`.
- CLI: `mirrorself items list | show <id> | review`.
- No lab-value extraction into `observations` yet; that stays M2 and needs
  the same review queue.
- Files: `importers/files.rs`, `pipeline/files.rs`, `crates/core/src/enrich/`,
  `schema/0005_enrich.sql`, `crates/cli/src/cmd_items.rs`, `fixtures/files/`,
  `tests/files.rs`, `docs/model.md`.

## Integration (after the workstreams)

1. Merge the five branches; resolve manifest and lockfile conflicts.
2. Add typed read tools and REST routes for transactions, meetings and items,
   through `mirrorself_server::tools::call` so they are audited.
3. Extend `get_overview` and the `mirrorself://schema` resource.
4. Write the two demo questions as an end-to-end test over synthetic data.
5. Update the README.

Next after the demo: map and cross-source timeline views, Huawei Health
import once a real export is available, lab extraction (M2), remote MCP (M4).
