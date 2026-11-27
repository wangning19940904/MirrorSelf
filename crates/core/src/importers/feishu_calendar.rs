//! Reader for the calendar JSON that Feishu's `lark-cli` prints.
//!
//! The command and the stored format are documented in
//! `docs/sources/feishu.md`. One raw object is one fetch: the stdout of one
//! or more `lark-cli` calls, which this reader accepts as
//!
//! - one response envelope: `{"ok": true, "identity": "user", "data": …}`
//!   from lark-cli, or `{"code": 0, "msg": "success", "data": …}` from the
//!   Open API itself;
//! - several envelopes, one per page or per call, either concatenated (pretty
//!   printed one after another, or one per line) or inside a JSON array;
//! - bare event objects, one per line (`--format ndjson`).
//!
//! An envelope's `data` is one of
//!
//! - `{"items": [instance, …]}` from `calendar events instance_view` (the
//!   recommended command; it keeps cancelled instances), with optional
//!   `has_more` / `page_token` that are ignored;
//! - `[instance, …]` from `calendar +agenda` (times as `datetime`);
//! - `{"event": event}` from `calendar events get --need-attendee`, which is
//!   the only call that lists people; instance lists carry meeting rooms only.
//!
//! Records with the same `event_id` within one fetch are merged, later ones
//! winning, so an instance list followed by `events get` calls yields one
//! event with its full attendee list.
//!
//! Nothing here logs or reports titles or names: error messages carry only
//! positions and counts.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset, NaiveDate, TimeZone, Utc};
use serde_json::{Map, Value};

pub const SOURCE: &str = "feishu";
pub const KIND: &str = "feishu_calendar_events";
pub const IMPORT_NAME: &str = "feishu-calendar";

/// One end of an event.
#[derive(Debug, Clone, PartialEq)]
pub enum Time {
    /// A moment: from `timestamp` (Unix seconds) or `datetime` (RFC 3339).
    At {
        utc: DateTime<Utc>,
        /// IANA name the event was created in, e.g. `Asia/Shanghai`.
        timezone: Option<String>,
        /// The offset printed with a `datetime`, if that was the form.
        offset: Option<FixedOffset>,
    },
    /// An all-day boundary: a calendar date, inclusive at both ends.
    Date(NaiveDate),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Person {
    pub name: Option<String>,
    pub id: Option<String>,
}

/// An invitee as Feishu lists it: a user, a group chat, a meeting room
/// (`resource`) or an e-mail invitee (`third_party`).
#[derive(Debug, Clone, PartialEq)]
pub struct Attendee {
    pub kind: String,
    pub name: Option<String>,
    /// `user_id`, `chat_id` or `room_id`, by kind. E-mail addresses are not kept.
    pub id: Option<String>,
    /// `needs_action`, `accept`, `tentative`, `decline` or `removed`.
    pub rsvp: Option<String>,
    pub organizer: bool,
    pub optional: bool,
    pub external: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub name: Option<String>,
    pub address: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

/// One event instance (a recurring event's occurrences are separate
/// instances with their own `event_id`).
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub event_id: String,
    pub summary: Option<String>,
    /// `tentative`, `confirmed` or `cancelled`; `+agenda` omits it.
    pub status: Option<String>,
    /// The signed-in user's own RSVP.
    pub self_rsvp: Option<String>,
    pub start: Time,
    pub end: Time,
    pub organizer: Option<Person>,
    /// People and group chats. `None` when the call did not list them
    /// (instance lists carry meeting rooms only).
    pub people: Option<Vec<Attendee>>,
    /// Whether Feishu said more attendees exist than were returned.
    pub people_truncated: bool,
    /// Meeting rooms. `None` when the call had no attendee field at all.
    pub rooms: Option<Vec<Attendee>>,
    pub location: Option<Location>,
    /// `vc`, `third_party`, `no_meeting`, `lark_live`, `unknown`, …
    pub vc_type: Option<String>,
    pub has_meeting_url: bool,
    pub recurring_event_id: Option<String>,
    pub is_exception: bool,
    /// `busy` or `free`.
    pub free_busy: Option<String>,
}

impl Event {
    pub fn is_all_day(&self) -> bool {
        matches!(self.start, Time::Date(_))
    }

    /// Fields of `later` win; what `later` does not carry is kept from `self`.
    fn merge(self, later: Event) -> Event {
        Event {
            event_id: later.event_id,
            summary: later.summary.or(self.summary),
            status: later.status.or(self.status),
            self_rsvp: later.self_rsvp.or(self.self_rsvp),
            start: later.start,
            end: later.end,
            organizer: later.organizer.or(self.organizer),
            people_truncated: if later.people.is_some() { later.people_truncated } else { self.people_truncated },
            people: later.people.or(self.people),
            rooms: later.rooms.or(self.rooms),
            location: later.location.or(self.location),
            vc_type: later.vc_type.or(self.vc_type),
            has_meeting_url: later.has_meeting_url || self.has_meeting_url,
            recurring_event_id: later.recurring_event_id.or(self.recurring_event_id),
            is_exception: later.is_exception || self.is_exception,
            free_busy: later.free_busy.or(self.free_busy),
        }
    }
}

#[derive(Debug, Default)]
pub struct Fetch {
    /// Distinct instances in order of first appearance.
    pub events: Vec<Event>,
    /// Response envelopes (pages or calls) read.
    pub pages: u64,
    /// Instance records read before merging.
    pub records: u64,
    /// Records without an `event_id` or readable times; skipped.
    pub malformed: u64,
}

/// The MIME type when `content` is lark-cli calendar output: every top-level
/// value is a successful response or an event, at least one event is
/// present, and every record in it is a readable calendar event.
pub fn detect(content: &[u8]) -> Option<&'static str> {
    let first = content
        .strip_prefix(b"\xEF\xBB\xBF")
        .unwrap_or(content)
        .iter()
        .find(|b| !b.is_ascii_whitespace())?;
    if *first != b'{' && *first != b'[' {
        return None;
    }
    let fetch = parse(content).ok()?;
    (!fetch.events.is_empty() && fetch.malformed == 0).then_some("application/json")
}

/// Reads one fetch. Fails on anything that is not lark-cli calendar output,
/// including error responses; skips (and counts) individual records that
/// cannot be read.
pub fn parse(content: &[u8]) -> Result<Fetch> {
    let content = content.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(content);
    let mut fetch = Fetch::default();
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, Event> = HashMap::new();
    let mut sink = |fetch: &mut Fetch, record: &Value| {
        fetch.records += 1;
        match read_event(record) {
            Some(ev) => match by_id.remove(&ev.event_id) {
                Some(prev) => {
                    by_id.insert(ev.event_id.clone(), prev.merge(ev));
                }
                None => {
                    order.push(ev.event_id.clone());
                    by_id.insert(ev.event_id.clone(), ev);
                }
            },
            None => fetch.malformed += 1,
        }
    };

    let stream = serde_json::Deserializer::from_slice(content).into_iter::<Value>();
    let mut values = 0usize;
    for (i, value) in stream.enumerate() {
        let value = value.with_context(|| format!("value {} is not valid JSON", i + 1))?;
        values += 1;
        match value {
            Value::Array(parts) => {
                for (j, part) in parts.iter().enumerate() {
                    top_level(part, &mut fetch, &mut sink)
                        .with_context(|| format!("value {}, element {}", i + 1, j + 1))?;
                }
            }
            other => top_level(&other, &mut fetch, &mut sink).with_context(|| format!("value {}", i + 1))?,
        }
    }
    if values == 0 {
        bail!("empty input");
    }
    fetch.events = order
        .into_iter()
        .map(|id| by_id.remove(&id).expect("every id in order was inserted"))
        .collect();
    Ok(fetch)
}

fn top_level(value: &Value, fetch: &mut Fetch, sink: &mut impl FnMut(&mut Fetch, &Value)) -> Result<()> {
    let Some(obj) = value.as_object() else {
        bail!("expected a response object or an event");
    };
    if obj.contains_key("event_id") {
        sink(fetch, value);
        return Ok(());
    }
    let ok_flag = obj.get("ok").and_then(Value::as_bool);
    let code = obj.get("code").and_then(Value::as_i64);
    if ok_flag == Some(false) || code.is_some_and(|c| c != 0) {
        // The error object may quote request details; report only its type.
        let kind = obj
            .get("error")
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str)
            .filter(|t| t.len() <= 40 && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or("unknown");
        bail!("the response is an error (type {kind}, code {})", code.unwrap_or(-1));
    }
    if ok_flag != Some(true) && code != Some(0) {
        bail!("not a lark-cli or Open API response");
    }
    fetch.pages += 1;
    match obj.get("data") {
        // An empty window: Feishu omits `items`.
        None | Some(Value::Null) => {}
        Some(Value::Array(records)) => records.iter().for_each(|r| sink(fetch, r)),
        Some(Value::Object(data)) => {
            if let Some(items) = data.get("items") {
                let Some(items) = items.as_array() else {
                    bail!("data.items is not a list");
                };
                items.iter().for_each(|r| sink(fetch, r));
            } else if let Some(event) = data.get("event") {
                if !event.is_object() {
                    bail!("data.event is not an object");
                }
                sink(fetch, event);
            } else if data.keys().any(|k| !matches!(k.as_str(), "has_more" | "page_token" | "sync_token")) {
                bail!("data holds neither events nor an empty page");
            }
        }
        Some(_) => bail!("data is neither a list nor an object"),
    }
    Ok(())
}

fn read_event(v: &Value) -> Option<Event> {
    let o = v.as_object()?;
    let event_id = text(o, "event_id")?;
    let start = read_time(o.get("start_time")?)?;
    let end = read_time(o.get("end_time")?)?;
    // Both ends must be the same form.
    if matches!(start, Time::Date(_)) != matches!(end, Time::Date(_)) {
        return None;
    }

    let mut people = Vec::new();
    let mut rooms = Vec::new();
    let attendees = o.get("attendees").and_then(Value::as_array);
    for a in attendees.into_iter().flatten() {
        let Some(att) = read_attendee(a) else { continue };
        if att.kind == "resource" {
            rooms.push(att);
        } else {
            people.push(att);
        }
    }
    // `events get --need-attendee` says whether it truncated the list; only
    // that call lists people, so its presence means the list is complete or
    // marked as cut.
    let truncated = o.get("has_more_attendee").and_then(Value::as_bool);
    let people_listed = truncated.is_some() || !people.is_empty();

    let organizer = o.get("event_organizer").and_then(Value::as_object).and_then(|p| {
        let person = Person { name: text(p, "display_name"), id: text(p, "user_id") };
        (person.name.is_some() || person.id.is_some()).then_some(person)
    });
    let location = o.get("location").and_then(Value::as_object).and_then(|l| {
        let loc = Location {
            name: text(l, "name"),
            address: text(l, "address"),
            lat: l.get("latitude").and_then(Value::as_f64),
            lon: l.get("longitude").and_then(Value::as_f64),
        };
        (loc.name.is_some() || loc.address.is_some() || loc.lat.is_some()).then_some(loc)
    });
    let vchat = o.get("vchat").and_then(Value::as_object);

    Some(Event {
        event_id,
        summary: text(o, "summary"),
        status: text(o, "status"),
        self_rsvp: text(o, "self_rsvp_status"),
        start,
        end,
        organizer,
        people: people_listed.then_some(people),
        people_truncated: truncated.unwrap_or(false),
        rooms: attendees.is_some().then_some(rooms),
        location,
        vc_type: vchat.and_then(|v| text(v, "vc_type")),
        has_meeting_url: vchat.and_then(|v| text(v, "meeting_url")).is_some(),
        recurring_event_id: text(o, "recurring_event_id"),
        is_exception: o.get("is_exception").and_then(Value::as_bool).unwrap_or(false),
        free_busy: text(o, "free_busy_status"),
    })
}

fn read_attendee(v: &Value) -> Option<Attendee> {
    let o = v.as_object()?;
    let kind = text(o, "type")?;
    let id = match kind.as_str() {
        "user" => text(o, "user_id"),
        "chat" => text(o, "chat_id"),
        "resource" => text(o, "room_id"),
        _ => None,
    };
    Some(Attendee {
        kind,
        name: text(o, "display_name"),
        id,
        rsvp: text(o, "rsvp_status"),
        organizer: o.get("is_organizer").and_then(Value::as_bool).unwrap_or(false),
        optional: o.get("is_optional").and_then(Value::as_bool).unwrap_or(false),
        external: o.get("is_external").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn read_time(v: &Value) -> Option<Time> {
    let o = v.as_object()?;
    let timezone = text(o, "timezone");
    if let Some(ts) = o.get("timestamp") {
        let secs = match ts {
            Value::String(s) => s.trim().parse::<i64>().ok()?,
            Value::Number(n) => n.as_i64()?,
            _ => return None,
        };
        let utc = Utc.timestamp_opt(secs, 0).single()?;
        return Some(Time::At { utc, timezone, offset: None });
    }
    if let Some(dt) = text(o, "datetime") {
        let dt = DateTime::parse_from_rfc3339(&dt).ok()?;
        return Some(Time::At { utc: dt.with_timezone(&Utc), timezone, offset: Some(*dt.offset()) });
    }
    let date = text(o, "date")?;
    NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok().map(Time::Date)
}

/// A non-empty, trimmed string field.
fn text(o: &Map<String, Value>, key: &str) -> Option<String> {
    o.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTANCE: &str = r#"{"event_id":"e1_0","summary":"Sync","status":"confirmed",
        "self_rsvp_status":"accept","start_time":{"timestamp":"1760234400","timezone":"Asia/Shanghai"},
        "end_time":{"timestamp":"1760238000","timezone":"Asia/Shanghai"},
        "attendees":[{"type":"resource","room_id":"omm_1","display_name":"Room","rsvp_status":"accept"}]}"#;

    #[test]
    fn reads_every_documented_shape() {
        let envelope = format!(r#"{{"ok":true,"identity":"user","data":{{"items":[{INSTANCE}]}}}}"#);
        let api = format!(r#"{{"code":0,"msg":"success","data":{{"items":[{INSTANCE}],"has_more":false}}}}"#);
        let array = format!("[{envelope}, {envelope}]");
        let ndjson = INSTANCE.replace('\n', " ");
        for input in [envelope.clone(), api, array, ndjson, format!("{envelope}\n{envelope}")] {
            let f = parse(input.as_bytes()).unwrap();
            assert_eq!(f.events.len(), 1, "{input}");
            assert_eq!(f.events[0].rooms.as_ref().map(Vec::len), Some(1));
            assert_eq!(f.events[0].people, None);
            assert_eq!(detect(input.as_bytes()), Some("application/json"));
        }
    }

    #[test]
    fn agenda_datetimes_keep_their_offset() {
        let agenda = r#"{"ok":true,"data":[{"event_id":"e2_0",
            "start_time":{"datetime":"2026-10-12T23:00:00+08:00","timezone":"Asia/Shanghai"},
            "end_time":{"datetime":"2026-10-13T01:00:00+08:00","timezone":"Asia/Shanghai"}}],"meta":{"count":1}}"#;
        let f = parse(agenda.as_bytes()).unwrap();
        let Time::At { utc, offset, .. } = &f.events[0].start else { panic!() };
        assert_eq!(utc.to_rfc3339(), "2026-10-12T15:00:00+00:00");
        assert_eq!(offset.unwrap().local_minus_utc(), 8 * 3600);
        assert_eq!(f.events[0].rooms, None);
    }

    #[test]
    fn a_later_get_adds_people_to_an_instance() {
        let get = r#"{"ok":true,"data":{"event":{"event_id":"e1_0","summary":"Sync (renamed)",
            "start_time":{"timestamp":"1760234400"},"end_time":{"timestamp":"1760238000"},
            "has_more_attendee":false,"attendees":[
              {"type":"user","user_id":"ou_1","display_name":"A","rsvp_status":"accept","is_organizer":true},
              {"type":"resource","room_id":"omm_1","display_name":"Room","rsvp_status":"accept"}]}}}"#;
        let input = format!(r#"{{"ok":true,"data":{{"items":[{INSTANCE}]}}}}{get}"#);
        let f = parse(input.as_bytes()).unwrap();
        assert_eq!((f.pages, f.records, f.events.len()), (2, 2, 1));
        let e = &f.events[0];
        assert_eq!(e.summary.as_deref(), Some("Sync (renamed)"));
        assert_eq!(e.status.as_deref(), Some("confirmed"), "kept from the instance");
        assert_eq!(e.people.as_ref().unwrap()[0].id.as_deref(), Some("ou_1"));
    }

    #[test]
    fn rejects_other_json_and_errors() {
        for input in [
            r#"{"ok":true,"data":{"items":[{"task_id":"t1","summary":"not an event"}]}}"#,
            r#"{"ok":false,"error":{"type":"authorization","code":99991679}}"#,
            r#"{"code":99991679,"msg":"denied"}"#,
            r#"{"ok":true,"data":{"items":[]}}"#,
            r#"{"name":"package.json"}"#,
            r#"[1, 2, 3]"#,
            "event_id,start\n1,2\n",
            "",
        ] {
            assert_eq!(detect(input.as_bytes()), None, "{input}");
        }
        assert!(parse(br#"{"ok":false,"error":{"type":"authorization"}}"#).is_err());
        // An empty window is valid input for the pipeline, just not detectable.
        assert!(parse(br#"{"ok":true,"identity":"user","data":{}}"#).unwrap().events.is_empty());
    }
}
