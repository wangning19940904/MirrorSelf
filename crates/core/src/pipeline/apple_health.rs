//! Loads an Apple Health export into observations and workout events,
//! normalizing units to UCUM and keeping UTC plus the original offset.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{DateTime, Duration, FixedOffset, NaiveDate};
use rusqlite::{params, CachedStatement, Connection};
use serde_json::{json, Value};

use crate::codes::{self, SLEEP_HK_TYPE};
use crate::config::PipelineConfig;
use crate::ids::{fingerprint, from_fp};
use crate::importers::apple_health::{self as reader, Item, Sample, Workout};
use crate::pipeline::Processor;
use crate::sources::{clear_standard, ImportSpec, RawCtx, RawProcessor, Source};
use crate::time::utc_string;

const INSERT_OBSERVATION: &str = "INSERT OR IGNORE INTO observations
  (id, fp, code_system, code, value_num, unit, start_ts, end_ts, tz_offset, local_date,
   device, source_name, source_tier, raw_id, run_id)
  VALUES (?1, ?2, 'ms', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)";

const INSERT_EVENT: &str = "INSERT OR IGNORE INTO events
  (id, fp, type, category_id, start_ts, end_ts, tz_offset, local_date, title, data, raw_id, run_id, derived)
  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0)";

#[derive(Default)]
struct Counts {
    observations: u64,
    events: u64,
    already_known: u64,
    skipped_types: BTreeMap<String, u64>,
    unconvertible: BTreeMap<String, u64>,
    bad_values: u64,
}

pub const PARSE: Processor = Processor {
    name: "apple_health.parse",
    version: "1.0.0",
};

pub static SOURCE: Source = Source {
    imports: &[ImportSpec {
        name: "apple-health",
        source: reader::SOURCE,
        kind: reader::KIND,
        expects: "an Apple Health export: export.zip or export.xml from the Health app",
        detect: detect_export,
        auto_detect: true,
    }],
    raw: &[RawProcessor {
        processor: PARSE,
        kinds: &[reader::KIND],
        load: load_raw,
        clear: clear_standard,
    }],
    ..Source::empty("apple_health")
};

fn detect_export(_name: Option<&str>, content: &[u8]) -> Option<&'static str> {
    reader::detect(content)
}

fn load_raw(conn: &Connection, ctx: &RawCtx<'_>, content: &[u8]) -> Result<Value> {
    load(conn, &ctx.config.pipeline, ctx.raw_id, ctx.run_id, content)
}

fn load(
    conn: &Connection,
    cfg: &PipelineConfig,
    raw_id: &str,
    run_id: &str,
    content: &[u8],
) -> Result<Value> {
    let mut obs = conn.prepare_cached(INSERT_OBSERVATION)?;
    let mut evt = conn.prepare_cached(INSERT_EVENT)?;
    let mut c = Counts::default();
    let ctx = Ctx { cfg, raw_id, run_id };

    let stats = reader::parse(content, |item| match item {
        Item::Sample(s) => ctx.sample(&mut obs, s, &mut c),
        Item::Workout(w) => ctx.workout(&mut evt, w, &mut c),
    })?;

    Ok(json!({
        "records_read": stats.records,
        "workouts_read": stats.workouts,
        "malformed": stats.malformed,
        "observations_added": c.observations,
        "events_added": c.events,
        "already_known": c.already_known,
        "bad_values": c.bad_values,
        "skipped_types": c.skipped_types,
        "unconvertible_units": c.unconvertible,
    }))
}

struct Ctx<'a> {
    cfg: &'a PipelineConfig,
    raw_id: &'a str,
    run_id: &'a str,
}

impl Ctx<'_> {
    fn sample(&self, stmt: &mut CachedStatement<'_>, s: Sample, c: &mut Counts) -> Result<()> {
        let (code, value, unit, local_date) = if s.hk_type == SLEEP_HK_TYPE {
            let Some(code) = s.value.as_deref().and_then(codes::sleep_code) else {
                *c.skipped_types
                    .entry(format!("{SLEEP_HK_TYPE}:{}", s.value.as_deref().unwrap_or("")))
                    .or_default() += 1;
                return Ok(());
            };
            let minutes = (s.end - s.start).num_seconds() as f64 / 60.0;
            if minutes < 0.0 {
                c.bad_values += 1;
                return Ok(());
            }
            let date = sleep_date(&s.start, self.cfg.sleep_day_boundary_hour);
            (code, minutes, "min", date)
        } else if let Some(def) = codes::for_hk_quantity(&s.hk_type) {
            let Some(raw) = s.value.as_deref().and_then(|v| v.trim().parse::<f64>().ok()) else {
                c.bad_values += 1;
                return Ok(());
            };
            let unit = s.unit.as_deref().unwrap_or("");
            let Some(value) = codes::to_canonical(def, raw, unit) else {
                *c.unconvertible.entry(format!("{} [{unit}]", def.code)).or_default() += 1;
                return Ok(());
            };
            (def.code, value, def.unit, s.start.date_naive())
        } else {
            *c.skipped_types.entry(s.hk_type).or_default() += 1;
            return Ok(());
        };

        let start = utc_string(&s.start);
        let end = utc_string(&s.end);
        let value_repr = format!("{value:.6}");
        let fp = fingerprint(&["obs", code, &s.source_name, &start, &end, &value_repr]);
        let inserted = stmt.execute(params![
            from_fp("obs", &fp),
            fp,
            code,
            value,
            unit,
            start,
            end,
            s.start.offset().local_minus_utc(),
            local_date.to_string(),
            s.device,
            s.source_name,
            source_tier(&s.source_name, s.device.as_deref()),
            self.raw_id,
            self.run_id
        ])?;
        if inserted == 0 {
            c.already_known += 1;
        } else {
            c.observations += 1;
        }
        Ok(())
    }

    fn workout(&self, stmt: &mut CachedStatement<'_>, w: Workout, c: &mut Counts) -> Result<()> {
        let start = utc_string(&w.start);
        let end = utc_string(&w.end);
        let fp = fingerprint(&["workout", &w.activity, &w.source_name, &start, &end]);
        let data = json!({
            "activity": w.activity,
            "duration_min": round(w.duration_min, 1),
            "distance_km": w.distance_km.map(|v| round(v, 2)),
            "energy_kcal": w.energy_kcal.map(|v| round(v, 0)),
            "source_name": w.source_name,
            "device": w.device,
        });
        let inserted = stmt.execute(params![
            from_fp("evt", &fp),
            fp,
            "workout",
            "activity",
            start,
            end,
            w.start.offset().local_minus_utc(),
            w.start.date_naive().to_string(),
            workout_title(&w),
            data.to_string(),
            self.raw_id,
            self.run_id
        ])?;
        if inserted == 0 {
            c.already_known += 1;
        } else {
            c.events += 1;
        }
        Ok(())
    }
}

/// The night a sleep sample belongs to: samples starting at or after the
/// boundary hour count toward the next date.
pub(crate) fn sleep_date(start: &DateTime<FixedOffset>, boundary_hour: u32) -> NaiveDate {
    let shift = i64::from((24 - boundary_hour.min(24)) % 24);
    (start.naive_local() + Duration::hours(shift)).date()
}

/// 0 = watch, 1 = phone, 2 = anything else. Lower tiers win when samples
/// from different sources overlap.
fn source_tier(source_name: &str, device: Option<&str>) -> i64 {
    let label = format!("{source_name} {}", device.unwrap_or(""));
    if label.contains("Watch") {
        0
    } else if label.contains("iPhone") {
        1
    } else {
        2
    }
}

fn workout_title(w: &Workout) -> String {
    let mut parts = vec![w.activity.clone()];
    if let Some(km) = w.distance_km.filter(|v| *v > 0.0) {
        parts.push(format!("{km:.2} km"));
    }
    parts.push(format!("{:.0} min", w.duration_min));
    if let Some(kcal) = w.energy_kcal.filter(|v| *v > 0.0) {
        parts.push(format!("{kcal:.0} kcal"));
    }
    parts.join(" · ")
}

pub(crate) fn round(v: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (v * f).round() / f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::parse_apple;

    #[test]
    fn sleep_dates_follow_the_boundary() {
        let late = parse_apple("2026-10-01 23:30:00 +0800").unwrap();
        let early = parse_apple("2026-10-02 01:00:00 +0800").unwrap();
        let nap = parse_apple("2026-10-02 14:00:00 +0800").unwrap();
        let d2 = NaiveDate::from_ymd_opt(2026, 10, 2).unwrap();
        assert_eq!(sleep_date(&late, 18), d2);
        assert_eq!(sleep_date(&early, 18), d2);
        assert_eq!(sleep_date(&nap, 18), d2);
        assert_eq!(sleep_date(&late, 0), NaiveDate::from_ymd_opt(2026, 10, 1).unwrap());
    }
}
