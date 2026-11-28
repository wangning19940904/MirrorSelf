//! Feishu calendar → meeting events, people and daily meeting time
//! (workstream W2 in `docs/demo-plan.md`; user documentation in
//! `docs/sources/feishu.md`).
//!
//! - `feishu_calendar.parse` (per raw object) writes one `events` row per
//!   event instance, keyed by Feishu's instance id. Overlapping fetches
//!   update the row instead of adding one; the most recently received fetch
//!   wins, whatever order the objects are processed in.
//! - `feishu_calendar.meeting_metrics` (`pre_derive`) turns the counted
//!   meetings into `ms:meeting_minutes` and `ms:meeting_count` observations.
//! - `feishu_calendar.people` (`post_link`) adds `person` and `place`
//!   entities and links meetings to them.
//!
//! Metric rules:
//! - A meeting counts unless it is cancelled, the user declined it (or was
//!   removed from it), or it is an all-day event.
//! - `ms:meeting_count`: one per counted meeting, on the local date it starts.
//! - `ms:meeting_minutes`: time spent in at least one counted meeting.
//!   Overlapping meetings are merged per local date, so double-booked time
//!   counts once. A meeting crossing local midnight is split at midnight and
//!   each part counts toward its own date.
//!
//! Local dates use the event's own time zone when it has a fixed offset
//! (`Asia/Shanghai`, `Asia/Tokyo`, …, or an offset printed by `+agenda`);
//! zones with daylight saving time cannot be resolved without a time-zone
//! database and use the fetch's home offset instead (the most common
//! resolved offset in the same fetch, else UTC+8, Feishu's default).
//! All-day events are stored as UTC dates, as Feishu stores them.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, SecondsFormat, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::ids::{fingerprint, from_fp};
use crate::importers::feishu_calendar::{self as reader, Attendee, Event, Time};
use crate::pipeline::Processor;
use crate::sources::{
    add_rule_link, clear_standard, upsert_entity, GlobalCtx, GlobalStage, ImportSpec, RawCtx, RawProcessor,
    Source,
};

pub const PARSE: Processor = Processor { name: "feishu_calendar.parse", version: "1.0.0" };
pub const METRICS: Processor = Processor { name: "feishu_calendar.meeting_metrics", version: "1.0.0" };
pub const PEOPLE: Processor = Processor { name: "feishu_calendar.people", version: "1.0.0" };

/// `observations.source_name` for meeting metrics.
pub const METRIC_SOURCE_NAME: &str = "Feishu Calendar";
const MINUTES: &str = "ms:meeting_minutes";
const COUNT: &str = "ms:meeting_count";
/// Feishu's default zone for timed events.
const DEFAULT_OFFSET_SECS: i32 = 8 * 3600;
const UNTITLED: &str = "(no title)";

pub static SOURCE: Source = Source {
    imports: &[ImportSpec {
        name: reader::IMPORT_NAME,
        source: reader::SOURCE,
        kind: reader::KIND,
        expects: "Feishu calendar events as printed by `lark-cli calendar events instance_view` (JSON)",
        detect: detect_events,
    }],
    raw: &[RawProcessor {
        processor: PARSE,
        kinds: &[reader::KIND],
        load,
        clear: clear_standard,
    }],
    pre_derive: &[GlobalStage { processor: METRICS, run: meeting_metrics }],
    post_link: &[GlobalStage { processor: PEOPLE, run: people_links }],
    ..Source::empty("feishu_calendar")
};

fn detect_events(_name: Option<&str>, content: &[u8]) -> Option<&'static str> {
    reader::detect(content)
}

const INSERT_EVENT: &str = "INSERT OR REPLACE INTO events
  (id, fp, type, category_id, start_ts, end_ts, tz_offset, local_date, title, data, raw_id, run_id, derived)
  VALUES (?1, ?2, 'meeting', 'work', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0)";

struct Row {
    id: String,
    fp: String,
    start_ts: String,
    end_ts: String,
    tz_offset: i32,
    local_date: String,
    title: String,
    data: Value,
}

fn load(conn: &Connection, ctx: &RawCtx<'_>, content: &[u8]) -> Result<Value> {
    let fetch = reader::parse(content)?;
    let home = home_offset(&fetch.events);
    let fetched_at = ctx.meta.received_at.as_str();
    let (mut added, mut updated, mut older, mut people_filled) = (0u64, 0u64, 0u64, 0u64);
    let mut existing = conn.prepare_cached("SELECT data, raw_id FROM events WHERE id = ?1")?;
    let mut insert = conn.prepare_cached(INSERT_EVENT)?;
    let mut set_data = conn.prepare_cached("UPDATE events SET data = ?2 WHERE id = ?1")?;

    for ev in &fetch.events {
        let mut row = build_row(ev, home, fetched_at);
        let known = existing
            .query_row([&row.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
            .optional()?;
        if let Some((data, raw_id)) = known {
            let mut old: Value = serde_json::from_str(&data)?;
            let old_version = (old["fetched_at"].as_str().unwrap_or("").to_string(), raw_id.unwrap_or_default());
            if (fetched_at, ctx.raw_id) < (old_version.0.as_str(), old_version.1.as_str()) {
                // A newer fetch is already stored. It may lack the people list
                // this one has; nothing else of this older version is kept.
                if old["attendees"].is_null() && !row.data["attendees"].is_null() {
                    old["attendees"] = row.data["attendees"].take();
                    old["attendees_truncated"] = row.data["attendees_truncated"].take();
                    set_data.execute(params![row.id, old.to_string()])?;
                    people_filled += 1;
                }
                older += 1;
                continue;
            }
            if row.data["attendees"].is_null() && !old["attendees"].is_null() {
                row.data["attendees"] = old["attendees"].take();
                row.data["attendees_truncated"] = old["attendees_truncated"].take();
                people_filled += 1;
            }
            updated += 1;
        } else {
            added += 1;
        }
        insert.execute(params![
            row.id,
            row.fp,
            row.start_ts,
            row.end_ts,
            row.tz_offset,
            row.local_date,
            row.title,
            row.data.to_string(),
            ctx.raw_id,
            ctx.run_id
        ])?;
    }
    Ok(json!({
        "pages": fetch.pages,
        "records_read": fetch.records,
        "malformed": fetch.malformed,
        "instances": fetch.events.len(),
        "events_added": added,
        "events_updated": updated,
        "older_than_stored": older,
        "attendee_lists_carried": people_filled,
    }))
}

fn build_row(ev: &Event, home: FixedOffset, fetched_at: &str) -> Row {
    let fp = fingerprint(&["feishu_calendar", &ev.event_id]);
    let (start, end, offset, timezone) = match (&ev.start, &ev.end) {
        (Time::Date(s), Time::Date(e)) => {
            // Inclusive end date; stored as a half-open UTC span.
            let end = (*e).max(*s) + Duration::days(1);
            (midnight_utc(*s), midnight_utc(end), FixedOffset::east_opt(0).expect("valid"), Some("UTC".to_string()))
        }
        (Time::At { utc: s, timezone, offset }, Time::At { utc: e, .. }) => {
            let off = offset.or_else(|| timezone.as_deref().and_then(fixed_offset)).unwrap_or(home);
            (*s, (*e).max(*s), off, timezone.clone())
        }
        _ => unreachable!("the reader rejects mixed start and end forms"),
    };
    let local_date = start.with_timezone(&offset).date_naive();
    let minutes = (end - start).num_seconds() as f64 / 60.0;
    let excluded = exclusion(ev);

    let people = ev.people.as_ref().map(|list| list.iter().map(attendee_json).collect::<Vec<_>>());
    let rooms = ev.rooms.as_ref().map(|list| {
        list.iter()
            .map(|r| json!({ "name": r.name, "id": r.id, "status": r.rsvp }))
            .collect::<Vec<_>>()
    });
    let data = json!({
        "source": "feishu",
        "event_id": ev.event_id,
        "recurring_event_id": ev.recurring_event_id,
        "is_exception": ev.is_exception,
        "status": ev.status.as_deref().unwrap_or("confirmed"),
        "self_rsvp": ev.self_rsvp,
        "all_day": ev.is_all_day(),
        "timezone": timezone,
        "minutes": minutes,
        "organizer": ev.organizer.as_ref().map(|p| json!({ "name": p.name, "id": p.id })),
        "attendees": people,
        "attendees_truncated": ev.people_truncated,
        "rooms": rooms,
        "location": ev.location.as_ref().map(|l| json!({
            "name": l.name, "address": l.address, "lat": l.lat, "lon": l.lon,
        })),
        "video_meeting": ev.vc_type.as_deref().is_some_and(|t| t != "no_meeting") || ev.has_meeting_url,
        "vc_type": ev.vc_type,
        "free_busy": ev.free_busy,
        "counted_in_metrics": excluded.is_none(),
        "excluded_because": excluded,
        "fetched_at": fetched_at,
    });
    Row {
        id: from_fp("evt", &fp),
        fp,
        start_ts: utc(&start),
        end_ts: utc(&end),
        tz_offset: offset.local_minus_utc(),
        local_date: local_date.to_string(),
        title: ev.summary.clone().unwrap_or_else(|| UNTITLED.to_string()),
        data,
    }
}

fn attendee_json(a: &Attendee) -> Value {
    json!({
        "type": a.kind,
        "name": a.name,
        "id": a.id,
        "rsvp": a.rsvp,
        "organizer": a.organizer,
        "optional": a.optional,
        "external": a.external,
    })
}

/// Why a meeting is left out of the metrics, if it is.
fn exclusion(ev: &Event) -> Option<&'static str> {
    if ev.status.as_deref() == Some("cancelled") {
        Some("cancelled")
    } else if ev.self_rsvp.as_deref() == Some("decline") {
        Some("declined")
    } else if ev.self_rsvp.as_deref() == Some("removed") {
        Some("removed")
    } else if ev.is_all_day() {
        Some("all_day")
    } else {
        None
    }
}

/// The most common resolvable offset among the fetch's timed events (ties
/// go to the larger offset), else UTC+8.
fn home_offset(events: &[Event]) -> FixedOffset {
    let mut counts: BTreeMap<i32, usize> = BTreeMap::new();
    for ev in events {
        if let Time::At { timezone, offset, .. } = &ev.start {
            if let Some(off) = offset.or_else(|| timezone.as_deref().and_then(fixed_offset)) {
                *counts.entry(off.local_minus_utc()).or_default() += 1;
            }
        }
    }
    let secs = counts
        .into_iter()
        .max_by_key(|(secs, n)| (*n, *secs))
        .map_or(DEFAULT_OFFSET_SECS, |(secs, _)| secs);
    FixedOffset::east_opt(secs).expect("offsets come from valid FixedOffsets")
}

/// Offsets of IANA zones without daylight saving time in this century.
fn fixed_offset(zone: &str) -> Option<FixedOffset> {
    let minutes = match zone {
        "UTC" | "Etc/UTC" | "Etc/GMT" | "GMT" | "Etc/Universal" | "Universal" | "Zulu" => 0,
        "Asia/Dubai" | "Asia/Muscat" => 4 * 60,
        "Asia/Karachi" | "Asia/Tashkent" => 5 * 60,
        "Asia/Kolkata" | "Asia/Calcutta" => 5 * 60 + 30,
        "Asia/Dhaka" | "Asia/Urumqi" | "Asia/Almaty" => 6 * 60,
        "Asia/Bangkok" | "Asia/Jakarta" | "Asia/Ho_Chi_Minh" | "Asia/Saigon" => 7 * 60,
        "Asia/Shanghai" | "Asia/Chongqing" | "Asia/Chungking" | "Asia/Harbin" | "PRC" | "Asia/Hong_Kong"
        | "Hongkong" | "Asia/Macau" | "Asia/Taipei" | "Asia/Singapore" | "Singapore" | "Asia/Kuala_Lumpur"
        | "Asia/Manila" | "Australia/Perth" => 8 * 60,
        "Asia/Tokyo" | "Japan" | "Asia/Seoul" => 9 * 60,
        "Australia/Brisbane" => 10 * 60,
        _ => return None,
    };
    FixedOffset::east_opt(minutes * 60)
}

fn midnight_utc(d: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight exists"))
}

fn utc(dt: &DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Secs, true)
}

const INSERT_OBSERVATION: &str = "INSERT INTO observations
  (id, fp, code_system, code, value_num, unit, start_ts, end_ts, tz_offset, local_date,
   device, source_name, source_tier, raw_id, run_id)
  VALUES (?1, ?2, 'ms', ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, 2, ?11, ?12)";

/// Part of a meeting on one local date: start, end, offset seconds, raw id.
type Piece<'a> = (DateTime<Utc>, DateTime<Utc>, i32, &'a str);

/// One counted meeting as the metrics stage reads it back.
struct Counted {
    event_id: String,
    raw_id: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    offset: FixedOffset,
}

/// Rewrites the meeting observations from the stored events. Runs over the
/// whole index because one day's merged time can span several fetches.
fn meeting_metrics(conn: &Connection, ctx: &GlobalCtx<'_>) -> Result<Value> {
    conn.execute(
        "DELETE FROM observations WHERE code IN (?1, ?2) AND source_name = ?3",
        params![MINUTES, COUNT, METRIC_SOURCE_NAME],
    )?;
    let meetings = conn
        .prepare(
            "SELECT json_extract(e.data, '$.event_id'), e.raw_id, e.start_ts, e.end_ts, e.tz_offset
             FROM events e JOIN raw_objects r ON r.id = e.raw_id
             WHERE r.kind = ?1 AND e.type = 'meeting'
               AND json_extract(e.data, '$.counted_in_metrics') = 1 AND e.end_ts > e.start_ts
             ORDER BY e.start_ts, e.id",
        )?
        .query_map([reader::KIND], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i32>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(event_id, raw_id, start, end, offset)| {
            Ok(Counted {
                event_id,
                raw_id,
                start: DateTime::parse_from_rfc3339(&start)?.with_timezone(&Utc),
                end: DateTime::parse_from_rfc3339(&end)?.with_timezone(&Utc),
                offset: FixedOffset::east_opt(offset).ok_or_else(|| anyhow::anyhow!("bad stored offset"))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut insert = conn.prepare_cached(INSERT_OBSERVATION)?;
    let mut write = |code: &str, unit: &str, value: f64, fp: &str, start: &DateTime<Utc>, end: &DateTime<Utc>,
                     offset: i32, date: &NaiveDate, raw_id: &str|
     -> Result<()> {
        insert.execute(params![
            from_fp("obs", fp),
            fp,
            code,
            value,
            unit,
            utc(start),
            utc(end),
            offset,
            date.to_string(),
            METRIC_SOURCE_NAME,
            raw_id,
            ctx.run_id
        ])?;
        Ok(())
    };

    // Per local date: (start, end, offset, raw_id) pieces, split at midnight.
    let mut pieces: BTreeMap<NaiveDate, Vec<Piece<'_>>> = BTreeMap::new();
    for m in &meetings {
        let date = m.start.with_timezone(&m.offset).date_naive();
        let fp = fingerprint(&["feishu_meeting_count", &m.event_id]);
        write(COUNT, "{meetings}", 1.0, &fp, &m.start, &m.end, m.offset.local_minus_utc(), &date, &m.raw_id)?;
        for (date, s, e) in split_at_midnight(m.start, m.end, m.offset) {
            pieces.entry(date).or_default().push((s, e, m.offset.local_minus_utc(), &m.raw_id));
        }
    }
    let mut spans = 0u64;
    for (date, mut list) in pieces {
        list.sort();
        let mut merged: Vec<Piece<'_>> = Vec::new();
        for piece in list {
            match merged.last_mut() {
                Some(last) if piece.0 <= last.1 => last.1 = last.1.max(piece.1),
                _ => merged.push(piece),
            }
        }
        for (s, e, offset, raw_id) in merged {
            let minutes = (e - s).num_seconds() as f64 / 60.0;
            let fp = fingerprint(&["feishu_meeting_minutes", &date.to_string(), &utc(&s), &utc(&e)]);
            write(MINUTES, "min", minutes, &fp, &s, &e, offset, &date, raw_id)?;
            spans += 1;
        }
    }
    Ok(json!({ "meetings_counted": meetings.len(), "busy_spans": spans }))
}

/// Splits `[start, end)` at local midnights of `offset`.
fn split_at_midnight(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    offset: FixedOffset,
) -> Vec<(NaiveDate, DateTime<Utc>, DateTime<Utc>)> {
    let mut out = Vec::new();
    let mut s = start;
    while s < end {
        let date = s.with_timezone(&offset).date_naive();
        let next_midnight = offset
            .from_local_datetime(&(date + Duration::days(1)).and_hms_opt(0, 0, 0).expect("midnight exists"))
            .single()
            .expect("fixed offsets are unambiguous")
            .with_timezone(&Utc);
        let e = end.min(next_midnight);
        out.push((date, s, e));
        s = e;
    }
    out
}

/// Organizers and invited people become `person` entities linked with
/// `involves`; meeting rooms that accepted become `place` entities linked
/// with `occurred_at`. Group chats stay in the event data only.
fn people_links(conn: &Connection, _ctx: &GlobalCtx<'_>) -> Result<Value> {
    let events = conn
        .prepare(
            "SELECT e.id, e.data FROM events e JOIN raw_objects r ON r.id = e.raw_id
             WHERE r.kind = ?1 AND e.type = 'meeting' ORDER BY e.start_ts, e.id",
        )?
        .query_map([reader::KIND], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let (mut people, mut rooms) = (0u64, 0u64);
    for (event_id, data) in events {
        let data: Value = serde_json::from_str(&data)?;
        let mut names: Vec<&str> = Vec::new();
        if let Some(name) = data["organizer"]["name"].as_str() {
            names.push(name);
        }
        for a in data["attendees"].as_array().into_iter().flatten() {
            let person = matches!(a["type"].as_str(), Some("user" | "third_party"));
            if person && a["rsvp"].as_str() != Some("removed") {
                if let Some(name) = a["name"].as_str() {
                    names.push(name);
                }
            }
        }
        for name in names {
            let entity = upsert_entity(conn, "person", name)?;
            people += u64::from(add_rule_link(conn, "event", &event_id, &entity, "involves")?);
        }
        for room in data["rooms"].as_array().into_iter().flatten() {
            if let (Some(name), Some("accept")) = (room["name"].as_str(), room["status"].as_str()) {
                let entity = upsert_entity(conn, "place", name)?;
                rooms += u64::from(add_rule_link(conn, "event", &event_id, &entity, "occurred_at")?);
            }
        }
    }
    Ok(json!({ "person_links": people, "room_links": rooms }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn meetings_are_split_at_local_midnight() {
        let cst = FixedOffset::east_opt(8 * 3600).unwrap();
        let parts = split_at_midnight(at("2026-10-13T15:00:00Z"), at("2026-10-13T17:00:00Z"), cst);
        assert_eq!(
            parts,
            vec![
                (NaiveDate::from_ymd_opt(2026, 10, 13).unwrap(), at("2026-10-13T15:00:00Z"), at("2026-10-13T16:00:00Z")),
                (NaiveDate::from_ymd_opt(2026, 10, 14).unwrap(), at("2026-10-13T16:00:00Z"), at("2026-10-13T17:00:00Z")),
            ]
        );
        assert_eq!(split_at_midnight(at("2026-10-13T01:00:00Z"), at("2026-10-13T01:00:00Z"), cst), vec![]);
    }

    #[test]
    fn zones_with_daylight_saving_are_not_guessed() {
        assert_eq!(fixed_offset("Asia/Shanghai").unwrap().local_minus_utc(), 8 * 3600);
        assert_eq!(fixed_offset("Asia/Kolkata").unwrap().local_minus_utc(), 19800);
        assert_eq!(fixed_offset("America/Los_Angeles"), None);
        assert_eq!(fixed_offset("Europe/London"), None);
        assert_eq!(home_offset(&[]).local_minus_utc(), DEFAULT_OFFSET_SECS);
    }
}
