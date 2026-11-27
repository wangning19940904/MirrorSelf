# Feishu calendar

MirrorSelf reads the signed-in user's Feishu (Lark) calendar from the JSON
that Feishu's official CLI, [`lark-cli`](https://github.com/larksuite/cli),
prints. Each event instance becomes a meeting in `events`, the people on it
become `person` entities, and the time spent in meetings becomes the daily
metrics `ms:meeting_minutes` and `ms:meeting_count`, with 28-day baselines,
so an agent can line meeting load up against sleep and other health data.

**This is employer data.** Your calendar lives in your employer's Feishu
tenant and holds colleagues' names, meeting titles and room bookings. Check
that keeping a personal copy is allowed where you work. MirrorSelf keeps it
in your encrypted vault and index only, and never writes names or titles to
logs or error messages.

Out of scope: Minutes (妙记) transcripts, meeting notes, documents and chat
messages. Only calendar events are read.

MirrorSelf itself makes no network calls for this source. You (or the
scheduled sync, see `docs/sync.md`) run `lark-cli`; MirrorSelf parses what it
printed.

## Prerequisites

1. Install `lark-cli` (tested with 1.0.100) and configure it for your tenant:
   `lark-cli config init`.
2. Log in **as yourself** (user identity, not the bot), with calendar read
   access. Any one of `calendar:calendar.event:read`,
   `calendar:calendar:readonly` or `calendar:calendar` is enough:

   ```bash
   lark-cli auth login --domain calendar
   ```

   `lark-cli auth status` should report `"identity": "user"`.

## The command

List every event instance on your primary calendar between two moments,
given as Unix seconds. Feishu limits one call to less than 40 days.

```bash
lark-cli calendar events instance_view --as user --calendar-id primary \
  --start-time 1760198400 --end-time 1760630400 --format json
```

This prints `{"ok": true, "identity": "user", "data": {"items": [...]}}`.
Each item is one instance: recurring events are expanded, and each
occurrence has its own `event_id` (`<series id>_<start seconds>`; a single
event ends in `_0`). An item carries the title (`summary`), start and end
(`timestamp` plus IANA `timezone`, or `date` for all-day events), `status`
(including `cancelled`), your own RSVP (`self_rsvp_status`), the organizer,
meeting rooms, location, video-meeting type and recurrence ids.

Use this command rather than the `+agenda` shortcut: `+agenda` drops
cancelled events and the event status. (Its output is accepted too.)

### Attendees (optional)

The instance list names meeting rooms but not people. Feishu lists people
only per event, so a full fetch adds one `events get` call per instance and
prints the responses after the instance list:

```bash
S=1760198400; E=1760630400
lark-cli calendar events instance_view --as user --calendar-id primary \
  --start-time "$S" --end-time "$E" --format json > /tmp/ms-feishu.json
cat /tmp/ms-feishu.json
jq -r '.data.items[]?.event_id' /tmp/ms-feishu.json | while read -r id; do
  lark-cli calendar events get --as user --calendar-id primary --event-id "$id" \
    --need-attendee true --max-attendee-num 100 --format json
done
```

The `events get` responses (`{"ok": true, "data": {"event": {...}}}`) are
merged into the instance with the same `event_id`. Without them, meetings
still get their organizer as a linked person, and their `attendees` field is
`null` (unknown), not an empty list.

## Import

```bash
lark-cli calendar events instance_view --as user --calendar-id primary \
  --start-time "$S" --end-time "$E" --format json > calendar.json
mirrorself import feishu-calendar calendar.json
```

The raw object is stored with source `feishu` and kind
`feishu_calendar_events`. Auto-detection (`mirrorself import auto`, phone
upload) recognises the same files.

### Accepted shapes

One raw object is one fetch: everything one or more `lark-cli` calls printed.

- One response envelope: lark-cli's `{"ok": true, "data": …}` or the Open
  API's `{"code": 0, "msg": "success", "data": …}`. An envelope with
  `"ok": false` or a non-zero `code` is rejected.
- Several envelopes, one per page or per call, concatenated (pretty printed
  or one per line) or in a JSON array. `has_more` and `page_token` are
  ignored.
- Bare event objects, one per line (`--format ndjson`).
- `data` may be `{"items": [...]}` (`instance_view`), `[...]` (`+agenda`), or
  `{"event": {...}}` (`events get`). An empty window (`data` without `items`)
  is valid and adds nothing.

Detection is strict: every top-level value must be a successful response or
an event, every record must have an `event_id` and readable start and end,
and there must be at least one event.

## What is stored

One `events` row per instance:

| Column | Value |
|---|---|
| `type`, `category_id` | `meeting`, `work` |
| `start_ts`, `end_ts` | UTC. All-day events: midnight UTC of the first day to midnight UTC after the last day (Feishu's end date is inclusive and its all-day zone is UTC) |
| `tz_offset`, `local_date` | See [Time zones](#time-zones) |
| `title` | The summary, or `(no title)` |
| `data` | JSON, below |

`data` fields: `event_id`, `recurring_event_id`, `is_exception`, `status`
(`confirmed`, `tentative` or `cancelled`), `self_rsvp` (`accept`,
`tentative`, `needs_action`, `decline`, `removed`), `all_day`, `timezone`,
`minutes`, `organizer` (`name`, `id`), `attendees` (list of `type`, `name`,
`id`, `rsvp`, `organizer`, `optional`, `external`, or `null` when the fetch
did not list people), `attendees_truncated`, `rooms` (`name`, `id`,
`status`), `location` (`name`, `address`, `lat`, `lon`; Feishu gives GCJ-02
in mainland China), `video_meeting`, `vc_type`, `free_busy`,
`counted_in_metrics`, `excluded_because` (`cancelled`, `declined`,
`removed`, `all_day` or `null`) and `fetched_at` (when the fetch was stored).
Descriptions, links and e-mail addresses are not kept.

### Overlapping fetches

Events are keyed by Feishu's instance `event_id`, so fetching overlapping
windows never duplicates a meeting. When an instance appears in several
fetches, the most recently stored fetch wins (by the raw object's
`received_at`), whatever order they are processed in: a meeting that moved,
was renamed, cancelled or declined since the last fetch shows its latest
state. If the newest fetch has no people list for an instance, the most
recent list from an older fetch is kept.

An event deleted outright (rather than cancelled) is not removed: a fetch
does not say which window it covered, so absence cannot be told from "not
in this window".

## Metrics

| Code | Per day |
|---|---|
| `ms:meeting_minutes` | Minutes spent in at least one counted meeting |
| `ms:meeting_count` | Counted meetings that start that day |

- **Counted** means: not cancelled, not declined by you (or removed), and
  not all-day. Meetings you have not answered (`needs_action`) or marked
  `tentative` count. Excluded events stay in `events` with
  `counted_in_metrics = false`.
- **Overlaps** count once: on each local date, the counted meetings' spans
  are merged and the merged length is the day's minutes. Two parallel
  one-hour meetings add 60 minutes and 2 meetings.
- **Crossing midnight**: a meeting is split at local midnight and each part
  counts toward its own date; its count goes to the date it starts.
- Days without counted meetings have no row (not zero), so the 28-day
  baseline is over days with meetings.

The observations carry `source_name = 'Feishu Calendar'` and are rebuilt
from `events` on every processing run.

## People and rooms

Organizers and invitees of type `user` or `third_party` become `person`
entities (by display name) linked to the meeting with `involves`, except
invitees whose status is `removed`. Meeting rooms that accepted the booking
become `place` entities linked with `occurred_at`. Group chats stay in the
event data only. People with the same display name share one entity.

## Time zones

UTC instants are exact. The local date (and `tz_offset`) uses the event's own
time zone when it has had a fixed offset this century (`Asia/Shanghai`,
`Asia/Tokyo`, `Asia/Singapore`, `Asia/Kolkata`, …) or when `+agenda` printed
an offset. Zones with daylight saving time (`America/Los_Angeles`,
`Europe/London`, …) are not guessed without a time-zone database: those
events use the fetch's home offset, the most common resolved offset among
its events (UTC+8 if none), which is normally your own.

## Scheduled sync recipe

For the scheduler in `docs/sync.md` (workstream W3). The template variables
are the sync window in Unix seconds; rename them to whatever the scheduler
uses.

```toml
[[sync]]
name = "feishu-calendar"
adapter = "cli"
command = "lark-cli"
args = [
  "calendar", "events", "instance_view",
  "--as", "user", "--calendar-id", "primary",
  "--start-time", "{window_start_unix}",
  "--end-time", "{window_end_unix}",
  "--format", "json",
]
kind = "feishu_calendar_events"
source = "feishu"
interval = "6h"
# Window: from 14 days back (re-reads recent days, catching late RSVP
# changes, cancellations and moves) to 7 days ahead. Must stay under 40 days.
lookback = "14d"
lookahead = "7d"
```

- Interval: 6 hours is plenty for daily metrics; meetings rarely change
  after they happen. Each run's stdout is one raw object; identical output
  is stored once.
- Overlap: a 14-day look-back keeps the last two weeks current. Overlapping
  windows are safe (see [Overlapping fetches](#overlapping-fetches)).
- A non-zero exit or an `"ok": false` response should be reported as a failed
  run and not stored; if it is stored anyway, processing reports an error
  for that object and stores nothing from it.
- For attendees, run the shell loop from [Attendees](#attendees-optional) as
  the command instead (`command = "sh"`, `args = ["-c", "…"]`) with the
  window substituted; it makes one extra call per instance.

## Example

`fixtures/feishu/` holds two synthetic, overlapping fetches with invented
people and rooms, generated by `fixtures/feishu/generate.py`;
`crates/core/tests/feishu_calendar.rs` asserts their exact results.
