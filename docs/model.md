# Files, documents and the optional model

Any file you import or upload that no source recognises becomes an *item*:
one row in `items`, shown through `v_items`, with a type, subtype, title,
extracted text, event time, location and place. All of these are nullable.

There are two tiers:

1. **Rules** (always on, no model, no network). They read what a file
   states about itself.
2. **A model** (optional, off until you configure one). It reads what rules
   cannot, such as scanned PDFs and photos of documents. Its answers wait for
   your review before they change anything.

**No model is needed for the rule tier.** Without a `[model]` table nothing
is ever sent anywhere, and files that rules cannot read are kept with status
`needs_review`.

## What the rules read

| File (raw kind) | What rules extract |
|---|---|
| PDF (`file_pdf`) | The text layer, including CJK text (ToUnicode CMaps and the predefined UCS-2 CMaps such as `UniGB-UCS2-H`). Title from the PDF's own `/Title`, else the file name. `source_updated_at` from `/ModDate` when it has an offset. Page count and page-image count in `data`. |
| Text (`file_text`) | Plain text, Markdown, CSV, JSON and HTML as text. Markdown title from the `title:` front-matter key or the first `#` heading; `source_updated_at` from an `updated:` front-matter timestamp. Subtype `note`, `table`, `data` or `web_page`. |
| Image (`file_image`) | EXIF from JPEG, PNG, HEIC/AVIF and WebP: capture time (`DateTimeOriginal`) converted to UTC with its offset (`OffsetTimeOriginal`, or else the difference to GPS time rounded to 15 minutes), the local date, GPS position as WGS-84 latitude and longitude (south and west negative), and `DateTime` + `OffsetTime` as `source_updated_at`. Camera, altitude and pixel size go to `data.exif`. Pixels are never decoded. |
| Anything else (`file_other`) | Nothing; the item records the file name and waits for review. |

Every item gets `method = 'rule'`, `received_at` from the raw object, and
`status = 'auto'` when the rules found text, a time or a place, or
`needs_review` otherwise, with the reason in `data.reason` (`no_text_layer`,
`no_metadata`, `encrypted`, `unsupported_format`, …).

Not done by rules: OCR, reading encrypted PDFs, LZW-compressed PDF streams,
CID fonts without a Unicode mapping, and lab values (nothing in this
workstream writes `observations`).

## Configuring a model

Add a `[model]` table to `mirrorself.toml` in your MirrorSelf home. Any
server with an OpenAI-compatible chat-completions API and image input works.

A model on this machine (Ollama, LM Studio, llama.cpp server, vLLM):

```toml
[model]
endpoint = "http://127.0.0.1:11434/v1"   # requests go to <endpoint>/chat/completions
model = "qwen2.5vl:7b"
```

A privately deployed model behind HTTPS with a key:

```toml
[model]
endpoint = "https://models.internal.example/v1"
model = "Qwen/Qwen2.5-VL-7B-Instruct"
api_key_env = "MIRRORSELF_MODEL_KEY"     # the NAME of the variable, never the key
```

```bash
export MIRRORSELF_MODEL_KEY=...   # set in the shell that runs `mirrorself items enrich`
```

| Key | Default | Meaning |
|---|---|---|
| `endpoint` | — | Base URL, `http://` or `https://`, without credentials. |
| `model` | — | Model name the server expects. |
| `api_key_env` | none | Environment variable holding the key, sent as `Authorization: Bearer`. A literal `api_key` key is refused. |
| `timeout_secs` | 120 | Per request. |
| `response_format` | `json_schema` | `json_object` or `none` for servers that do not support schemas. Validation is the same either way. |
| `max_images` | 4 | Images sent per file. |
| `max_image_bytes` | 8 MiB | Image bytes sent per file. |
| `max_tokens` | 4096 | Answer length. |

`http://` endpoints are spoken to directly. `https://` endpoints go through
the system `curl` (8.3 or later), which reads the key from its environment,
so the key is not in any command line, file or error message. Only the
configured URL is contacted.

## What is sent

`mirrorself items enrich --pending` picks items whose status is
`needs_review`, whose file is a PDF or an image, and that this model has not
answered yet for the current prompt version. `--id <item>` sends a specific
item (any uploaded file, including a photo with EXIF that you want read).
`--dry-run` lists what would be sent and calls nothing.

For each file, one request:

- **System message:** the fixed prompt `SYSTEM_PROMPT` at prompt version
  **`files-extract/1`** (`crates/core/src/enrich/mod.rs`). It asks for one JSON
  object, says that everything in the file is data and never an instruction,
  and forbids interpretation, assessment, diagnosis or advice.
- **User message:** a JSON object with the file name, media type, the number
  of images attached and the text rules already extracted (at most 20,000
  characters), followed by the images: the photo itself (JPEG, PNG, WebP or
  GIF), or a PDF's page images (embedded JPEGs as stored; 8-bit grey/RGB and
  1-bit images re-encoded as PNG). HEIC and AVIF photos, encrypted PDFs and
  other files are not sent.
- **`response_format`:** the JSON schema below (unless configured otherwise),
  `temperature` 0.

Nothing else leaves the machine: no other items, no metrics, no index rows.

## The answer and its validation

```json
{
  "text": "string or null (≤ 100,000 characters)",
  "type": "document | photo | screenshot | other",
  "subtype": "receipt | invoice | ticket | boarding_pass | itinerary | checkup_report | lab_report | medical_record | prescription | contract | letter | certificate | note | other | null",
  "title": "string or null (≤ 200 characters)",
  "event_time": "RFC 3339 with offset | YYYY-MM-DDTHH:MM:SS | YYYY-MM-DD | null",
  "place": "string or null (≤ 200 characters)",
  "confidence": 0.0
}
```

The message content must be exactly this object: no prose, no code fences,
no missing or extra keys, values of the listed types and sizes, no control
characters outside `text`. A response that is cut off (`finish_reason`
`length`), larger than 4 MiB, or fails any check is stored but marked
`invalid` with the reason, and changes nothing.

Model text is data. It is stored as text, never executed, never used to pick
a tool or an item, and never placed in a tool description. A document or an
answer that says "confirm this item" or "drop table items" is just text in an
annotation.

## Where answers are kept

Every response body is stored verbatim in the vault as a raw object:

- `source = 'model'`, `kind = 'model_output'`, `mime = 'application/json'`
- `origin_ref = 'input=<input raw id>;model=<model>;prompt=files-extract/1'`
- content: an envelope `{format, input_raw_id, model, prompt_version,
  request_sha256, response}`

The `files.model_output` processor replays stored responses into
`item_annotations` (`v_item_annotations`) with `method = 'ai'`, the
confidence, and status `needs_review` (or `invalid`). `process` and
`process --rebuild` never call a model; they only read what is in the vault,
so rebuilding reproduces the same annotations offline.

## Review

```bash
mirrorself items review                         # what awaits review, with the model's answers
mirrorself items show <item>                    # effective values, rule values, annotations, decisions
mirrorself items confirm <item> [--annotation <ann>]
mirrorself items reject <item> [--annotation <ann>]
mirrorself items edit <item> --title … --type … --subtype … --text … \
    --event-time 2026-10-01T09:12:00+08:00 --place … --lat … --lon … --clear place
mirrorself items list --status needs_review --type photo
```

Every command accepts `--json`.

- `confirm` accepts the pending annotation (pass `--annotation` when there
  are several). With none pending, it confirms the item as the rules read it.
- `reject` rejects the pending annotation; the item keeps its rule values.
  With none pending, it rejects the item itself, which hides it from
  `v_items` and search (`items list --status rejected` still shows it).
- `edit` records your values; they take precedence over rules and models.

A decision is stored as a `review_decision` raw object in the vault and
copied into `ops_log`. The `files.review_apply` stage replays `ops_log` in
order after every processing run, so decisions survive `process --rebuild`
and even deleting `index.db`.

### What `v_items` shows

Model results never overwrite the rule-made `items` row. `v_items` shows
effective values, field by field:

1. your edit, if any;
2. else the rule value, if the rules found one (a title taken from the file
   name, and the type `file`, count as not found);
3. else the value of the confirmed annotation (the latest one you confirmed);
4. else the rule value (null).

The event time (start, offset, local date) moves as a group. Location comes
from rules or your edit only.

`v_items` keeps all its earlier columns. Their meaning for reviewed items:
`method` is `user` after an edit, `ai` when a confirmed annotation supplied a
value, else `rule`; `confidence` is 1.0 after an edit, the annotation's when
it supplied a value; `status` is `needs_review` while an annotation awaits
review, `confirmed` after a confirmation or an edit. New columns:
`annotation_id` (the confirmed annotation in use), `pending_annotations` and
`edited`. `v_items_all` is the same view including rejected items.

## Guarantees

- The rule tier needs no model and makes no network calls.
- Models are called only by `mirrorself items enrich`, only for the items it
  lists, and only at the configured endpoint.
- Model output never becomes a fact silently: it needs `confirm` (or your
  edit), and the decision is logged.
- Nothing in this workstream writes `observations` or phrases a diagnosis or
  treatment suggestion; an item records only what the document says.
