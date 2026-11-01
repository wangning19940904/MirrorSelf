//! Sleep sessions: merges a night's de-duplicated stage samples into one
//! timeline event (naps become their own sessions).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use super::apple_health::round;
use crate::codes::ASLEEP_CODES;
use crate::ids::{fingerprint, from_fp};
use crate::time::epoch_seconds;

/// Gaps up to this long stay within one session.
const MAX_GAP_SECONDS: i64 = 30 * 60;
const MAX_CITED_SAMPLES: usize = 100;

struct Segment {
    id: String,
    code: String,
    start: i64,
    end: i64,
    start_ts: String,
    end_ts: String,
    tz_offset: i64,
    local_date: String,
    raw_id: String,
    source: String,
    minutes: f64,
}

pub(super) fn run(conn: &Connection, run_id: &str) -> Result<Value> {
    conn.execute("DELETE FROM events WHERE type = 'sleep' AND derived = 1", [])?;
    let mut stmt = conn.prepare(
        "SELECT id, code, start_ts, end_ts, tz_offset, local_date, raw_id, coalesce(source_name, ''), value_num
         FROM observations
         WHERE code LIKE 'ms:sleep\\_%' ESCAPE '\\' AND status IN ('auto', 'confirmed')
         ORDER BY local_date, start_ts, id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, f64>(8)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut segments = Vec::with_capacity(rows.len());
    for (id, code, start_ts, end_ts, tz_offset, local_date, raw_id, source, minutes) in rows {
        segments.push(Segment {
            id,
            code,
            start: epoch_seconds(&start_ts)?,
            end: epoch_seconds(&end_ts)?,
            start_ts,
            end_ts,
            tz_offset,
            local_date,
            raw_id,
            source,
            minutes,
        });
    }

    let mut insert = conn.prepare(
        "INSERT OR IGNORE INTO events
           (id, fp, type, category_id, start_ts, end_ts, tz_offset, local_date, title, data, raw_id, run_id, derived)
         VALUES (?1, ?2, 'sleep', 'sleep', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1)",
    )?;
    let mut sessions = 0u64;
    let mut i = 0;
    while i < segments.len() {
        // One sleep date at a time; rows are sorted by date, then start.
        let mut j = i;
        while j < segments.len() && segments[j].local_date == segments[i].local_date {
            j += 1;
        }
        let mut k = i;
        while k < j {
            let mut end = segments[k].end;
            let mut last = k;
            let mut m = k + 1;
            while m < j && segments[m].start <= end + MAX_GAP_SECONDS {
                if segments[m].end > end {
                    end = segments[m].end;
                    last = m;
                }
                m += 1;
            }
            let session = &segments[k..m];
            let (title, data) = describe(session);
            let first = &segments[k];
            let end_ts = &segments[last].end_ts;
            let fp = fingerprint(&["sleep", &first.start_ts, end_ts]);
            insert.execute(params![
                from_fp("evt", &fp),
                fp,
                first.start_ts,
                end_ts,
                first.tz_offset,
                first.local_date,
                title,
                data.to_string(),
                first.raw_id,
                run_id
            ])?;
            sessions += 1;
            k = m;
        }
        i = j;
    }
    Ok(json!({ "sessions": sessions }))
}

fn describe(session: &[Segment]) -> (String, Value) {
    let mut by_code: BTreeMap<&str, f64> = BTreeMap::new();
    for s in session {
        *by_code.entry(s.code.as_str()).or_default() += s.minutes;
    }
    let get = |code: &str| by_code.get(code).copied().unwrap_or(0.0);
    let asleep: f64 = ASLEEP_CODES.iter().map(|c| get(c)).sum();
    let in_bed = get("ms:sleep_in_bed");
    let awake = get("ms:sleep_awake");
    let (core, deep, rem) = (
        get("ms:sleep_asleep_core"),
        get("ms:sleep_asleep_deep"),
        get("ms:sleep_asleep_rem"),
    );

    let title = if asleep > 0.0 {
        let mut t = format!("Sleep {} asleep", hm(asleep));
        if deep + rem + core > 0.0 {
            t.push_str(&format!(" (deep {}, REM {}, core {})", hm(deep), hm(rem), hm(core)));
        }
        t
    } else if in_bed > 0.0 {
        format!("In bed {}", hm(in_bed))
    } else {
        format!("Awake {}", hm(awake))
    };

    let sources: BTreeSet<&str> = session.iter().map(|s| s.source.as_str()).collect();
    let data = json!({
        "asleep_min": round(asleep, 1),
        "in_bed_min": round(in_bed, 1),
        "awake_min": round(awake, 1),
        "stages": {
            "core": round(core, 1),
            "deep": round(deep, 1),
            "rem": round(rem, 1),
            "unspecified": round(get("ms:sleep_asleep_unspecified"), 1),
        },
        "samples": session.len(),
        "sources": sources,
        "observation_ids": session.iter().take(MAX_CITED_SAMPLES).map(|s| s.id.as_str()).collect::<Vec<_>>(),
    });
    (title, data)
}

/// 412.0 → `6h 52m`; 45.0 → `45m`.
pub(crate) fn hm(minutes: f64) -> String {
    let total = minutes.round() as i64;
    let (h, m) = (total / 60, total % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}
