# MirrorSelf

A user-owned personal data layer. MirrorSelf keeps every original file in an
encrypted vault, derives a linked, queryable index from it, and lets you and
the agents you choose (Claude, ChatGPT, local coding agents) ask questions
through a CLI, a REST API and MCP.

This build covers milestones **M0 (foundations)**, **M1 (server, Apple
Health, CLI, local MCP)** and **M2 (checkup reports)** from *MirrorSelf —
System Architecture v1*, plus the multi-source demo described in
[docs/demo-plan.md](docs/demo-plan.md). It runs as a personal server
(deployment mode B) on macOS or Linux.

## What works

- **Encrypted raw vault.** Each file gets its own AES-256-GCM key, wrapped by
  your vault key. Files are addressed by HMAC-SHA256, so importing the same
  file twice stores it once. Storage is local disk or NAS (`local:/path`).
- **Encrypted index.** SQLCipher, keyed from the vault key. Delete it at any
  time and run `mirrorself process --rebuild` to get identical results back.
- **Apple Health import.** `export.zip` or `export.xml`, streamed. Units are
  normalized to UCUM, timestamps are stored as UTC plus the original offset,
  and a watch's and a phone's overlapping samples count once.
- **Alipay bills.** The CSV that Alipay emails becomes exact transactions
  and a daily spend metric ([docs/sources/alipay.md](docs/sources/alipay.md)).
- **Feishu calendar.** `lark-cli` output becomes meetings, the people and
  rooms in them, and daily meeting time
  ([docs/sources/feishu.md](docs/sources/feishu.md)).
- **Files.** PDFs, text, Markdown and photos become items with a title,
  extracted text, time and location where the file carries them. A
  vision-capable model is optional; its answers wait for your review
  ([docs/model.md](docs/model.md)).
- **Scheduled sync.** Saved recipes pull from MCP servers and command-line
  tools on a schedule, read-only tools only, and store every response in the
  vault; a GitHub recipe is included ([docs/sync.md](docs/sync.md)).
- **Phone upload.** A page for sending files from a phone on the same
  network, paired with a one-time code ([docs/upload.md](docs/upload.md)).
- **Derived data.** Daily aggregates, 28-day personal baselines and z-scores,
  sleep sessions, workouts, notable deviations on a timeline, device and
  activity entities with links, and full-text search.
- **Provenance.** Every value carries `raw_id`, `run_id` and confidence.
  `processor_runs` records every processor version that ran.
- **Checkup reports.** `mirrorself import report` takes PDFs and photos
  (zh and en). Values become lab rows with LOINC codes, UCUM units, the
  report's own printed range and flag, confidence, and the page and box they
  were read from. Anything uncertain waits in a review queue; review
  decisions are user ops kept in the vault, so they survive a deleted index.
- **One query layer, three surfaces.** The CLI (`--json` on every read), REST
  (`/v1`, bearer token, loopback only) and MCP over stdio (12 read-only tools,
  the `mirrorself://schema` resource and 2 prompts). Every agent and API call
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

Checkup reports (PDF, JPEG, PNG or HEIC; `--lang` is a hint for OCR):

```bash
./target/release/mirrorself import report ~/Documents/checkup-2025.pdf --lang zh
```

```bash
./target/release/mirrorself review list
```

```bash
./target/release/mirrorself labs --analyte LDL-C --compare
```

Other files go in the same way. Name the source, or let it be detected:

```bash
./target/release/mirrorself import alipay ~/Downloads/alipay_record.csv
```

```bash
./target/release/mirrorself import auto ~/Downloads/checkup.pdf
```

Meeting time next to sleep, one row per day, through the same SQL surface
agents use:

```bash
./target/release/mirrorself sql "SELECT m.date, m.value AS meeting_min, s.value AS sleep_min FROM v_metrics_daily m JOIN v_metrics_daily s ON s.date = m.date AND s.code = 'ms:sleep_asleep_total' AND s.stat = 'sum' WHERE m.code = 'ms:meeting_minutes' AND m.stat = 'sum' ORDER BY m.date"
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
| `GET /v1/labs?code&from&to` | Lab results with printed ranges, flags and source pages |
| `GET /v1/labs/compare?code` | Checkup comparison: an analyte across reports |
| `GET /v1/documents` · `GET /v1/documents/{id}?include_text` | Documents; text is returned as untrusted data |
| `GET /v1/review?all&document` | Extracted values waiting for review |
| `POST /v1/review/{id}` | `{"action": "confirm"}`, `{"action": "edit", "value": 3.5, "unit": "mmol/L", …}` or `{"action": "reject"}` |
| `GET /v1/entities?q` · `GET /v1/entities/{id}/links` | Entities and linked records |
| `GET /v1/search?q&from&to&kinds` | Search (substring match for Chinese, Japanese and Korean) |
| `GET /v1/transactions?from&to&direction&counterparty&counted_only` | Transactions with exact totals |
| `GET /v1/items?from&to&types&status&located_only&include_text` | Files and generic records with metadata |
| `POST /v1/upload?name=…` | Store one file (raw request body); see docs/upload.md |
| `GET /upload` | The phone upload page |
| `POST /v1/query/sql` | Read-only SQL over `v_*` views |
| `GET /v1/audit` | Access log |
| `GET /v1/raw/{id}` | The original file |

Dates accept `YYYY-MM-DD`, `today`, or look-backs such as `90d`, `12w`, `6m`, `1y`.

## Checkup reports (M2)

1. **Ingest.** The original file goes into the encrypted vault like any other
   raw object; importing it again stores nothing new.
2. **OCR** (`report.ocr`), behind the `ModelProvider` trait. On macOS the
   `apple-vision` provider reads PDF text layers with PDFKit and scans and
   photos with Apple Vision (Chinese and English), through a small Swift
   helper (`crates/core/swift/`) that MirrorSelf compiles on first use. Each
   image is read twice, the second time English-only, and a number counts as
   confirmed when both readings agree. Elsewhere, set `[ocr] provider` in
   `mirrorself.toml` (`recorded:<dir>` replays saved OCR; `none` stores the
   file and reports that it cannot be read yet).
3. **Extraction** (`report.extract`) fills the lab schema from §5 of the
   architecture (analyte, LOINC, value, unit, printed range and flag, page,
   box, confidence) with a deterministic layout parser. A provider with an
   LLM can override this step and return the same schema.
4. **Review.** A value goes to the queue when its confidence is below 0.9
   (`[pipeline] review_confidence`), its unit does not fit the analyte, it is
   physiologically implausible, its printed flag contradicts its printed
   range, or its analyte has no LOINC mapping. Queued values stay out of
   `v_labs` and agent results until you confirm, edit or reject them.

Why this provider: on this Mac only Apple's command-line tools are installed
(no Ollama, PaddleOCR or Tesseract), and the on-device Foundation Models LLM
reports `deviceNotEligible`. Vision reads Chinese and English well, needs no
download and keeps every byte on the machine; an opt-in cloud API would
send health data off the host (and, for users in mainland China, abroad).

Flags are always the report's own; `vs_printed_range` only compares a value
with the range printed next to it. Document text is untrusted: tools return
it inside `<untrusted-document-text>` delimiters, and remarks on a report
never become lab rows.

Accuracy on the synthetic eval set (12 reports, zh and en, each as a PDF, a
scanned PDF and phone photos: 48 documents, 729 values): 97.5% of analyte,
value and unit triples correct straight from extraction, 100% after review,
with no wrong value skipping the queue. Re-run it with
`mirrorself dev eval-reports fixtures/reports/eval`.

## Not yet

The iOS app and sync (M3), remote MCP with OAuth 2.1 grants, scopes and
redaction (M4), device mode (M5), Android and Huawei import (M6), a web
console for the review queue, LLM extraction providers for lab reports,
cloud storage adapters, FHIR and Parquet export, and vector search.
Uploaded files (`import auto`, the phone upload page) become items; a
checkup report needs `import report` to become lab values.

The multi-source demo has been tested on synthetic data only. Nothing here
has been run against a real Alipay export, a real GitHub account or a real
model, and the PDF and EXIF readers are small hand-written parsers that
handle ordinary files, not every variant.

## Layout

```text
crates/core     schema, vault, storage, source registry, importers, pipeline,
                scheduled sync, model enrichment, OCR model providers, QueryService
crates/server   REST API (axum), upload page and MCP server (stdio)
crates/cli      the mirrorself binary
fixtures/       synthetic fixtures used by the tests, one folder per source
                (checkup reports: fixtures/reports)
docs/           the demo plan and one guide per source and feature
tools/          fixture generators (synthetic reports: render.swift, regenerate.sh)
```

See `AGENTS.md` for build and test commands and project conventions.
