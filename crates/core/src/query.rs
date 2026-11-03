//! QueryService: the one read path behind the app, REST, MCP and CLI
//! (architecture §8). Results are JSON values with citations, so every
//! surface returns the same thing.

use std::collections::BTreeMap;
use std::time::{Duration as StdDuration, Instant};

use anyhow::Result;
use chrono::NaiveDate;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection, ErrorCode, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::codes::{self, MetricDef, ASLEEP_CODES, METRICS, SLEEP_TOTAL};
use crate::error::{denied, invalid, not_found};
use crate::time::{month_start, today_local, week_start, DateRange};

pub const SQL_ROW_CAP: usize = 1000;
pub const SQL_TIMEOUT: StdDuration = StdDuration::from_secs(10);
const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1000;
const MAX_CITATIONS: usize = 200;

const KEY_METRICS: &[&str] = &[
    "ms:resting_hr",
    "ms:hrv_sdnn",
    SLEEP_TOTAL,
    "ms:steps",
    "ms:body_mass",
    "ms:vo2max",
];

#[derive(Debug, Default, Clone, Deserialize)]
pub struct TimelineQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    /// Event types (`workout`, `sleep`, `metric_deviation`) or categories.
    #[serde(default)]
    pub types: Vec<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct SeriesQuery {
    pub metric: String,
    pub from: Option<String>,
    pub to: Option<String>,
    /// `day` (default), `week` or `month`.
    pub bucket: Option<String>,
    pub stat: Option<String>,
}

/// `{"from": "...", "to": "..."}` or `"FROM..TO"`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Period {
    Range { from: String, to: String },
    Text(String),
}

impl Period {
    fn range(&self, today: NaiveDate) -> Result<DateRange> {
        match self {
            Period::Range { from, to } => DateRange::parse(Some(from), Some(to), today),
            Period::Text(s) => {
                let (a, b) = s
                    .split_once("..")
                    .ok_or_else(|| invalid(format!("period '{s}' must look like FROM..TO")))?;
                DateRange::parse(Some(a), Some(b), today)
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompareQuery {
    pub metric: String,
    pub period_a: Period,
    pub period_b: Period,
    pub stat: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct LabQuery {
    pub analyte: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct SearchQuery {
    pub query: String,
    pub from: Option<String>,
    pub to: Option<String>,
    /// `event`, `entity`, `metric`.
    #[serde(default)]
    pub kinds: Vec<String>,
    pub limit: Option<u32>,
}

/// Local date → (record id, raw id) for the records behind that day.
type DayRecords = BTreeMap<String, Vec<(String, Option<String>)>>;

struct Day {
    date: NaiveDate,
    value: f64,
    n: i64,
    baseline: Option<f64>,
    z: Option<f64>,
}

fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn r2o(v: Option<f64>) -> Option<f64> {
    v.map(r2)
}

fn limit(requested: Option<u32>) -> u32 {
    requested.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn json_list(items: &[String]) -> Option<String> {
    let items: Vec<&str> = items
        .iter()
        .flat_map(|t| t.split(','))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    (!items.is_empty()).then(|| serde_json::to_string(&items).expect("strings serialize"))
}

fn parse_json(text: Option<String>) -> Value {
    text.map(|t| serde_json::from_str(&t).unwrap_or(Value::String(t)))
        .unwrap_or(Value::Null)
}

pub fn resolve_metric(input: &str) -> Result<&'static MetricDef> {
    codes::resolve(input).ok_or_else(|| {
        let known: Vec<&str> = METRICS.iter().map(|d| d.code).collect();
        not_found(format!("unknown metric '{input}'. Known metrics: {}", known.join(", ")))
    })
}

fn underlying_codes(def: &MetricDef) -> Vec<&'static str> {
    if def.code == SLEEP_TOTAL {
        ASLEEP_CODES.to_vec()
    } else {
        vec![def.code]
    }
}

pub struct QueryService<'c> {
    conn: &'c Connection,
    today: NaiveDate,
}

impl<'c> QueryService<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self::with_today(conn, today_local())
    }

    /// Relative dates (`90d`) count back from `today`.
    pub fn with_today(conn: &'c Connection, today: NaiveDate) -> Self {
        Self { conn, today }
    }

    /// What data exists, from which sources, and the latest key values.
    pub fn overview(&self) -> Result<Value> {
        let (raw_count, first_received, last_received): (i64, Option<String>, Option<String>) =
            self.conn.query_row(
                "SELECT count(*), min(received_at), max(received_at) FROM raw_objects",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;

        let coverage = self
            .conn
            .prepare(
                "SELECT o.code, c.display, c.category_id, c.unit, min(o.local_date), max(o.local_date),
                        count(DISTINCT o.local_date), count(*)
                 FROM observations o JOIN codes c ON c.code = o.code
                 WHERE o.status IN ('auto', 'confirmed')
                 GROUP BY o.code ORDER BY c.category_id, o.code",
            )?
            .query_map([], |r| {
                Ok(json!({
                    "code": r.get::<_, String>(0)?,
                    "display": r.get::<_, String>(1)?,
                    "category": r.get::<_, String>(2)?,
                    "unit": r.get::<_, String>(3)?,
                    "first_date": r.get::<_, String>(4)?,
                    "last_date": r.get::<_, String>(5)?,
                    "days_with_data": r.get::<_, i64>(6)?,
                    "samples": r.get::<_, i64>(7)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let sources = self
            .conn
            .prepare(
                "SELECT coalesce(source_name, 'unknown'), min(source_tier), count(*),
                        sum(status = 'duplicate'), min(local_date), max(local_date)
                 FROM observations GROUP BY source_name ORDER BY count(*) DESC, source_name",
            )?
            .query_map([], |r| {
                let tier: i64 = r.get(1)?;
                Ok(json!({
                    "source": r.get::<_, String>(0)?,
                    "kind": match tier { 0 => "watch", 1 => "phone", _ => "other" },
                    "samples": r.get::<_, i64>(2)?,
                    "duplicates_skipped": r.get::<_, i64>(3)?,
                    "first_date": r.get::<_, String>(4)?,
                    "last_date": r.get::<_, String>(5)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let events = self
            .conn
            .prepare("SELECT type, count(*), min(local_date), max(local_date) FROM events GROUP BY type ORDER BY type")?
            .query_map([], |r| {
                Ok(json!({
                    "type": r.get::<_, String>(0)?,
                    "count": r.get::<_, i64>(1)?,
                    "first_date": r.get::<_, String>(2)?,
                    "last_date": r.get::<_, String>(3)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut latest = Vec::new();
        for code in KEY_METRICS {
            let def = codes::metric(code).expect("key metrics are defined");
            let row = self
                .conn
                .query_row(
                    "SELECT date, value, n, baseline_28d, z FROM daily_metrics
                     WHERE code = ?1 AND stat = ?2 ORDER BY date DESC LIMIT 1",
                    params![def.code, def.agg.stat()],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, f64>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, Option<f64>>(3)?,
                            r.get::<_, Option<f64>>(4)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((date, value, n, baseline, z)) = row {
                latest.push(json!({
                    "code": def.code, "display": def.display, "unit": def.unit, "stat": def.agg.stat(),
                    "date": date, "value": r2(value), "n_samples": n,
                    "baseline_28d": r2o(baseline), "z": r2o(z),
                    "record_id": format!("dm:{}:{}:{}", def.code, def.agg.stat(), date),
                }));
            }
        }

        Ok(json!({
            "raw_objects": { "count": raw_count, "first_received": first_received, "last_received": last_received },
            "coverage": coverage,
            "sources": sources,
            "events": events,
            "latest": latest,
            "notes": [
                "Values are informational, not medical advice. Baselines are the mean of the previous 28 days with data (at least 7).",
                "Lab results and documents arrive with checkup-report import (milestone M2)."
            ],
        }))
    }

    /// Events and notable deviations, newest first.
    pub fn timeline(&self, q: &TimelineQuery) -> Result<Value> {
        let range = DateRange::parse(q.from.as_deref(), q.to.as_deref(), self.today)?;
        let limit = limit(q.limit);
        let offset = q.offset.unwrap_or(0);
        let types = json_list(&q.types);
        let rows = self
            .conn
            .prepare(
                "SELECT kind, id, type, category, start_ts, end_ts, local_date, title, data, raw_id
                 FROM v_timeline
                 WHERE local_date BETWEEN ?1 AND ?2
                   AND (?3 IS NULL OR type IN (SELECT value FROM json_each(?3))
                                   OR category IN (SELECT value FROM json_each(?3)))
                 ORDER BY start_ts DESC, id
                 LIMIT ?4 OFFSET ?5",
            )?
            .query_map(
                params![range.from.to_string(), range.to.to_string(), types, limit, offset],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, Option<String>>(8)?,
                        r.get::<_, Option<String>>(9)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let items: Vec<Value> = rows
            .into_iter()
            .map(|(kind, id, typ, category, start, end, local_date, title, data, raw_id)| {
                let source = if kind == "event" { "events" } else { "daily_metrics" };
                json!({
                    "kind": kind, "id": id, "type": typ, "category": category,
                    "start": start, "end": end, "local_date": local_date,
                    "title": title, "data": parse_json(data),
                    "citations": [{ "record_id": id, "source": source, "raw_id": raw_id }],
                })
            })
            .collect();
        let count = items.len() as u32;
        Ok(json!({
            "from": range.from.to_string(),
            "to": range.to.to_string(),
            "count": count,
            "limit": limit,
            "offset": offset,
            "next_offset": (count == limit).then_some(offset + limit),
            "items": items,
        }))
    }

    fn stat_for(&self, def: &MetricDef, requested: Option<&str>) -> Result<String> {
        let mut allowed = vec![def.agg.stat()];
        if def.minmax {
            allowed.extend(["min", "max"]);
        }
        match requested {
            None => Ok(def.agg.stat().to_string()),
            Some(s) if allowed.contains(&s) => Ok(s.to_string()),
            Some(s) => Err(invalid(format!(
                "stat '{s}' is not available for {}; use {}",
                def.code,
                allowed.join(" or ")
            ))),
        }
    }

    fn daily(&self, code: &str, stat: &str, range: &DateRange) -> Result<Vec<Day>> {
        let rows = self
            .conn
            .prepare_cached(
                "SELECT date, value, n, baseline_28d, z FROM daily_metrics
                 WHERE code = ?1 AND stat = ?2 AND date BETWEEN ?3 AND ?4 ORDER BY date",
            )?
            .query_map(
                params![code, stat, range.from.to_string(), range.to.to_string()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, f64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<f64>>(3)?,
                        r.get::<_, Option<f64>>(4)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(date, value, n, baseline, z)| {
                Ok(Day {
                    date: NaiveDate::parse_from_str(&date, "%Y-%m-%d")?,
                    value,
                    n,
                    baseline,
                    z,
                })
            })
            .collect()
    }

    /// Record ids behind each day, for days with few records (≤ 5): the
    /// readings themselves for discrete metrics, sleep sessions for sleep.
    fn day_records(&self, def: &MetricDef, range: &DateRange) -> Result<DayRecords> {
        let (from, to) = (range.from.to_string(), range.to.to_string());
        let rows: Vec<(String, String, Option<String>)> = if def.code == SLEEP_TOTAL {
            self.conn
                .prepare_cached(
                    "SELECT local_date, id, raw_id FROM events
                     WHERE type = 'sleep' AND local_date BETWEEN ?1 AND ?2
                     ORDER BY local_date, start_ts, id",
                )?
                .query_map(params![from, to], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        } else {
            self.conn
                .prepare_cached(
                    "SELECT o.local_date, o.id, o.raw_id FROM observations o
                     WHERE o.code = ?1 AND o.status IN ('auto', 'confirmed')
                       AND o.local_date BETWEEN ?2 AND ?3
                       AND o.local_date IN (
                         SELECT local_date FROM observations
                         WHERE code = ?1 AND status IN ('auto', 'confirmed') AND local_date BETWEEN ?2 AND ?3
                         GROUP BY local_date HAVING count(*) <= 5)
                     ORDER BY o.local_date, o.start_ts, o.id",
                )?
                .query_map(params![def.code, from, to], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut out = DayRecords::new();
        for (date, id, raw_id) in rows {
            out.entry(date).or_default().push((id, raw_id));
        }
        Ok(out)
    }

    fn sources_for(&self, def: &MetricDef, range: &DateRange) -> Result<Vec<Value>> {
        let codes = serde_json::to_string(&underlying_codes(def))?;
        let rows = self
            .conn
            .prepare_cached(
                "SELECT raw_id, count(*) FROM observations
                 WHERE code IN (SELECT value FROM json_each(?1)) AND status IN ('auto', 'confirmed')
                   AND local_date BETWEEN ?2 AND ?3
                 GROUP BY raw_id ORDER BY raw_id",
            )?
            .query_map(
                params![codes, range.from.to_string(), range.to.to_string()],
                |r| Ok(json!({ "raw_id": r.get::<_, String>(0)?, "samples": r.get::<_, i64>(1)? })),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Daily (or weekly/monthly) values with baselines, a summary and citations.
    pub fn metric_series(&self, q: &SeriesQuery) -> Result<Value> {
        let def = resolve_metric(&q.metric)?;
        let stat = self.stat_for(def, q.stat.as_deref())?;
        let bucket = q.bucket.as_deref().unwrap_or("day");
        if !matches!(bucket, "day" | "week" | "month") {
            return Err(invalid(format!("bucket '{bucket}' must be day, week or month")));
        }
        let range = DateRange::parse(q.from.as_deref(), q.to.as_deref(), self.today)?;
        let days = self.daily(def.code, &stat, &range)?;
        let records = self.day_records(def, &range)?;

        let points: Vec<Value> = if bucket == "day" {
            days.iter()
                .map(|d| {
                    let mut p = json!({
                        "period": d.date.to_string(),
                        "value": r2(d.value),
                        "n_samples": d.n,
                        "baseline_28d": r2o(d.baseline),
                        "z": r2o(d.z),
                    });
                    if let Some(recs) = records.get(&d.date.to_string()) {
                        p["record_ids"] = json!(recs.iter().map(|(id, _)| id).collect::<Vec<_>>());
                    }
                    p
                })
                .collect()
        } else {
            bucketize(&days, bucket == "week")
        };

        let record_source = if def.code == SLEEP_TOTAL { "events" } else { "observations" };
        let total_records: usize = records.values().map(Vec::len).sum();
        let citations: Vec<Value> = records
            .values()
            .rev()
            .flatten()
            .take(MAX_CITATIONS)
            .map(|(id, raw_id)| json!({ "record_id": id, "source": record_source, "raw_id": raw_id }))
            .collect();

        let mut notes = vec![
            "Daily values are computed from HealthKit samples after cross-device de-duplication.".to_string(),
        ];
        if days.len() < 7 {
            notes.push(format!("Only {} day(s) of data in this range; treat any trend as tentative.", days.len()));
        }
        Ok(json!({
            "metric": def.code,
            "display": def.display,
            "unit": def.unit,
            "stat": stat,
            "bucket": bucket,
            "from": range.from.to_string(),
            "to": range.to.to_string(),
            "points": points,
            "summary": summarize(&days),
            "citations": citations,
            "citations_truncated": total_records > MAX_CITATIONS,
            "sources": self.sources_for(def, &range)?,
            "notes": notes,
        }))
    }

    /// Two periods of daily values side by side, with the difference and effect size.
    pub fn compare_periods(&self, q: &CompareQuery) -> Result<Value> {
        let def = resolve_metric(&q.metric)?;
        let stat = self.stat_for(def, q.stat.as_deref())?;
        let a_range = q.period_a.range(self.today)?;
        let b_range = q.period_b.range(self.today)?;
        let a: Vec<f64> = self.daily(def.code, &stat, &a_range)?.iter().map(|d| d.value).collect();
        let b: Vec<f64> = self.daily(def.code, &stat, &b_range)?.iter().map(|d| d.value).collect();
        let (sa, sb) = (describe(&a), describe(&b));

        let mut out = json!({
            "metric": def.code, "display": def.display, "unit": def.unit, "stat": stat,
            "period_a": period_json(&a_range, &sa),
            "period_b": period_json(&b_range, &sb),
            "note": "Descriptive comparison of daily values: a pattern to discuss or test, not a diagnosis.",
        });
        if let (Some(sa), Some(sb)) = (sa, sb) {
            let delta = sb.mean - sa.mean;
            out["delta"] = json!(r2(delta));
            out["delta_pct"] = json!((sa.mean != 0.0).then(|| r2(delta / sa.mean * 100.0)));
            if sa.n >= 2 && sb.n >= 2 {
                let pooled = (((sa.n - 1) as f64 * sa.sd.powi(2) + (sb.n - 1) as f64 * sb.sd.powi(2))
                    / (sa.n + sb.n - 2) as f64)
                    .sqrt();
                if pooled > 1e-9 {
                    let d = delta / pooled;
                    out["effect_size"] = json!(r2(d));
                    out["effect_size_label"] = json!(match d.abs() {
                        x if x < 0.2 => "negligible",
                        x if x < 0.5 => "small",
                        x if x < 0.8 => "medium",
                        _ => "large",
                    });
                }
            }
        }
        Ok(out)
    }

    pub fn lab_results(&self, q: &LabQuery) -> Result<Value> {
        let range = DateRange::parse(q.from.as_deref(), q.to.as_deref(), self.today)?;
        let results = self
            .conn
            .prepare(
                "SELECT id, code, display, value_num, value_text, unit, ref_low, ref_high, flag,
                        observed_at, local_date, document_id, raw_id, confidence, status
                 FROM v_labs
                 WHERE (?1 IS NULL OR code = ?1 OR display LIKE '%' || ?1 || '%')
                   AND local_date BETWEEN ?2 AND ?3
                 ORDER BY observed_at DESC, id",
            )?
            .query_map(
                params![q.analyte, range.from.to_string(), range.to.to_string()],
                |r| {
                    let id: String = r.get(0)?;
                    let raw_id: String = r.get(12)?;
                    Ok(json!({
                        "id": id, "code": r.get::<_, String>(1)?, "display": r.get::<_, Option<String>>(2)?,
                        "value": r.get::<_, Option<f64>>(3)?, "value_text": r.get::<_, Option<String>>(4)?,
                        "unit": r.get::<_, Option<String>>(5)?, "ref_low": r.get::<_, Option<f64>>(6)?,
                        "ref_high": r.get::<_, Option<f64>>(7)?, "flag_as_printed": r.get::<_, Option<String>>(8)?,
                        "observed_at": r.get::<_, String>(9)?, "local_date": r.get::<_, String>(10)?,
                        "document_id": r.get::<_, Option<String>>(11)?, "confidence": r.get::<_, f64>(13)?,
                        "status": r.get::<_, String>(14)?,
                        "citations": [{ "record_id": id, "source": "observations", "raw_id": raw_id }],
                    }))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let empty = results.is_empty();
        let mut out = json!({ "count": results.len(), "results": results });
        if empty {
            out["note"] = json!(
                "No lab results yet. Checkup-report import (PDF or photo → reviewed lab rows) arrives in milestone M2."
            );
        }
        Ok(out)
    }

    /// Full-text search over events, entities and metric names.
    pub fn search(&self, q: &SearchQuery) -> Result<Value> {
        let terms: Vec<String> = q
            .query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(|t| format!("\"{t}\"*"))
            .collect();
        if terms.is_empty() {
            return Err(invalid("search query needs at least one word"));
        }
        let fts = terms.join(" AND ");
        let range = DateRange::parse(q.from.as_deref(), q.to.as_deref(), self.today)?;
        let kinds = json_list(&q.kinds);
        let rows = self
            .conn
            .prepare(
                "SELECT search_fts.kind, search_fts.ref_id, search_fts.local_date, search_fts.title,
                        snippet(search_fts, -1, '[', ']', '…', 12), bm25(search_fts), e.raw_id
                 FROM search_fts
                 LEFT JOIN events e ON search_fts.kind = 'event' AND e.id = search_fts.ref_id
                 WHERE search_fts MATCH ?1
                   AND (?2 IS NULL OR search_fts.kind IN (SELECT value FROM json_each(?2)))
                   AND (search_fts.local_date IS NULL OR search_fts.local_date BETWEEN ?3 AND ?4)
                 ORDER BY bm25(search_fts), search_fts.ref_id
                 LIMIT ?5",
            )?
            .query_map(
                params![fts, kinds, range.from.to_string(), range.to.to_string(), limit(q.limit.or(Some(20)))],
                |r| {
                    let kind: String = r.get(0)?;
                    let id: String = r.get(1)?;
                    let source = match kind.as_str() {
                        "event" => "events",
                        "entity" => "entities",
                        _ => "codes",
                    };
                    Ok(json!({
                        "kind": kind, "id": id,
                        "local_date": r.get::<_, Option<String>>(2)?,
                        "title": r.get::<_, String>(3)?,
                        "snippet": r.get::<_, String>(4)?,
                        "score": r2(-r.get::<_, f64>(5)?),
                        "citations": [{ "record_id": id, "source": source, "raw_id": r.get::<_, Option<String>>(6)? }],
                    }))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({ "query": q.query, "count": rows.len(), "results": rows }))
    }

    pub fn entities(&self, q: Option<&str>) -> Result<Value> {
        let rows = self
            .conn
            .prepare(
                "SELECT id, kind, name FROM entities
                 WHERE ?1 IS NULL OR name LIKE '%' || ?1 || '%'
                 ORDER BY kind, name LIMIT 200",
            )?
            .query_map([q], |r| {
                Ok(json!({ "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "name": r.get::<_, String>(2)? }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({ "count": rows.len(), "entities": rows }))
    }

    /// An entity by id or name, with the records linked to it.
    pub fn entity(&self, name_or_id: &str) -> Result<Value> {
        let found = self
            .conn
            .query_row(
                "SELECT id, kind, name, aliases FROM entities
                 WHERE id = ?1 OR name = ?1 COLLATE NOCASE
                 ORDER BY id = ?1 DESC, kind LIMIT 1",
                [name_or_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)),
            )
            .optional()?;
        let Some((id, kind, name, aliases)) = found else {
            let similar = self.entities(Some(name_or_id))?;
            let names: Vec<String> = similar["entities"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e["name"].as_str().map(String::from))
                .take(10)
                .collect();
            return Err(not_found(if names.is_empty() {
                format!("no entity named '{name_or_id}'")
            } else {
                format!("no entity named '{name_or_id}'; similar: {}", names.join(", "))
            }));
        };

        let linked = self
            .conn
            .prepare(
                "SELECT l.relation, l.from_type, l.from_id, l.method, e.type, e.title, e.local_date, e.raw_id
                 FROM links l LEFT JOIN events e ON l.from_type = 'event' AND e.id = l.from_id
                 WHERE l.to_type = 'entity' AND l.to_id = ?1
                 ORDER BY e.start_ts DESC, l.from_id LIMIT 50",
            )?
            .query_map([&id], |r| {
                let from_id: String = r.get(2)?;
                Ok(json!({
                    "relation": r.get::<_, String>(0)?,
                    "record_type": r.get::<_, String>(1)?,
                    "record_id": from_id,
                    "method": r.get::<_, String>(3)?,
                    "event_type": r.get::<_, Option<String>>(4)?,
                    "title": r.get::<_, Option<String>>(5)?,
                    "local_date": r.get::<_, Option<String>>(6)?,
                    "citations": [{ "record_id": from_id, "source": "events", "raw_id": r.get::<_, Option<String>>(7)? }],
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let link_counts: BTreeMap<String, i64> = self
            .conn
            .prepare("SELECT relation, count(*) FROM links WHERE to_type = 'entity' AND to_id = ?1 GROUP BY relation")?
            .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let samples: i64 = if kind == "device" {
            self.conn.query_row(
                "SELECT count(*) FROM observations WHERE source_name = ?1",
                [&name],
                |r| r.get(0),
            )?
        } else {
            0
        };
        Ok(json!({
            "entity": { "id": id, "kind": kind, "name": name, "aliases": parse_json(Some(aliases)) },
            "link_counts": link_counts,
            "observations_from_source": samples,
            "linked": linked,
        }))
    }

    pub fn document(&self, id: &str, include_text: bool) -> Result<Value> {
        let doc = self
            .conn
            .query_row(
                "SELECT id, raw_id, doc_type, issued_at, provider_id, language, pages, text FROM documents WHERE id = ?1",
                [id],
                |r| {
                    Ok(json!({
                        "id": r.get::<_, String>(0)?, "raw_id": r.get::<_, String>(1)?,
                        "doc_type": r.get::<_, String>(2)?, "issued_at": r.get::<_, Option<String>>(3)?,
                        "provider_id": r.get::<_, Option<String>>(4)?, "language": r.get::<_, Option<String>>(5)?,
                        "pages": r.get::<_, Option<i64>>(6)?,
                        "text": if include_text { r.get::<_, Option<String>>(7)? } else { None },
                    }))
                },
            )
            .optional()?;
        let mut doc = doc.ok_or_else(|| {
            not_found(format!("no document '{id}' (document import arrives in milestone M2)"))
        })?;
        if include_text {
            // Extracted text is untrusted input: keep it in a delimited field, never as instructions.
            doc["text_is_untrusted"] = json!(true);
        }
        Ok(doc)
    }

    /// Read-only SQL over the `v_*` views, with a row cap and a time limit.
    pub fn run_sql(&self, sql: &str) -> Result<Value> {
        let sql = sql.trim().trim_end_matches(';').trim();
        if sql.is_empty() {
            return Err(invalid("empty SQL"));
        }
        // rusqlite's prepare() compiles the first statement and ignores the rest.
        if has_statement_separator(sql) {
            return Err(invalid("only one statement is allowed"));
        }
        let conn = self.conn;
        conn.authorizer(Some(views_only));
        let deadline = Instant::now() + SQL_TIMEOUT;
        conn.progress_handler(10_000, Some(move || Instant::now() > deadline));

        let result = (|| -> Result<Value> {
            let mut stmt = conn.prepare(sql).map_err(sql_error)?;
            if !stmt.readonly() {
                return Err(denied("only read-only SELECT statements are allowed"));
            }
            let columns: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
            let mut rows = stmt.query([]).map_err(sql_error)?;
            let mut out = Vec::new();
            let mut truncated = false;
            while let Some(row) = rows.next().map_err(sql_error)? {
                if out.len() == SQL_ROW_CAP {
                    truncated = true;
                    break;
                }
                let mut values = Vec::with_capacity(columns.len());
                for i in 0..columns.len() {
                    values.push(match row.get_ref(i)? {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(n) => json!(n),
                        ValueRef::Real(f) => json!(f),
                        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
                        ValueRef::Blob(b) => Value::String(format!("<blob, {} bytes>", b.len())),
                    });
                }
                out.push(Value::Array(values));
            }
            Ok(json!({
                "columns": columns,
                "row_count": out.len(),
                "truncated": truncated,
                "row_cap": SQL_ROW_CAP,
                "rows": out,
            }))
        })();

        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        conn.progress_handler(0, None::<fn() -> bool>);
        result
    }

    pub fn audit(&self, limit_rows: Option<u32>) -> Result<Value> {
        let rows = self
            .conn
            .prepare(
                "SELECT ts, actor, client_id, action, scope, query_digest, rows, outcome
                 FROM audit_log ORDER BY id DESC LIMIT ?1",
            )?
            .query_map([limit(limit_rows)], |r| {
                Ok(json!({
                    "ts": r.get::<_, String>(0)?, "actor": r.get::<_, String>(1)?,
                    "client_id": r.get::<_, String>(2)?, "action": r.get::<_, String>(3)?,
                    "scope": r.get::<_, String>(4)?, "query_digest": r.get::<_, String>(5)?,
                    "rows": r.get::<_, i64>(6)?, "outcome": r.get::<_, String>(7)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({ "count": rows.len(), "items": rows }))
    }

    /// Known metric codes with how many days of data each has.
    pub fn codes(&self) -> Result<Value> {
        let rows = self
            .conn
            .prepare(
                "SELECT c.code, c.display, c.display_zh, c.category_id, c.unit, c.aggregation,
                        (SELECT count(*) FROM daily_metrics d WHERE d.code = c.code AND d.stat = c.aggregation)
                 FROM codes c ORDER BY c.category_id, c.code",
            )?
            .query_map([], |r| {
                Ok(json!({
                    "code": r.get::<_, String>(0)?, "display": r.get::<_, String>(1)?,
                    "display_zh": r.get::<_, Option<String>>(2)?, "category": r.get::<_, String>(3)?,
                    "unit": r.get::<_, String>(4)?, "daily_stat": r.get::<_, String>(5)?,
                    "days_with_data": r.get::<_, i64>(6)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({ "count": rows.len(), "items": rows }))
    }

    /// Markdown description of the agent-facing views and code list.
    pub fn schema_doc(&self) -> Result<String> {
        let mut doc = String::from(
            "# MirrorSelf schema\n\nRead-only views for `run_sql` (MCP) and `POST /v1/query/sql` (REST). \
             Base tables are not accessible; results are capped at 1000 rows and 10 seconds.\n",
        );
        let views: Vec<(String, String)> = self
            .conn
            .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'view' AND name LIKE 'v\\_%' ESCAPE '\\' ORDER BY name")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (name, sql) in views {
            doc.push_str(&format!("\n## {name}\n\n```sql\n{sql}\n```\n"));
        }
        doc.push_str("\n## Metric codes\n\n| code | display | unit | category | daily stat |\n|---|---|---|---|---|\n");
        for d in METRICS {
            doc.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                d.code,
                d.display,
                d.unit,
                d.category,
                d.agg.stat()
            ));
        }
        Ok(doc)
    }
}

struct Described {
    n: usize,
    mean: f64,
    sd: f64,
    min: f64,
    max: f64,
}

fn describe(values: &[f64]) -> Option<Described> {
    if values.is_empty() {
        return None;
    }
    let n = values.len();
    let mean = values.iter().sum::<f64>() / n as f64;
    let sd = if n > 1 {
        (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt()
    } else {
        0.0
    };
    Some(Described {
        n,
        mean,
        sd,
        min: values.iter().copied().fold(f64::INFINITY, f64::min),
        max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    })
}

fn period_json(range: &DateRange, d: &Option<Described>) -> Value {
    match d {
        Some(d) => json!({
            "from": range.from.to_string(), "to": range.to.to_string(), "n_days": d.n,
            "mean": r2(d.mean), "sd": r2(d.sd), "min": r2(d.min), "max": r2(d.max),
        }),
        None => json!({ "from": range.from.to_string(), "to": range.to.to_string(), "n_days": 0 }),
    }
}

fn summarize(days: &[Day]) -> Value {
    let values: Vec<f64> = days.iter().map(|d| d.value).collect();
    let Some(stats) = describe(&values) else {
        return Value::Null;
    };
    let point = |d: &Day| json!({ "date": d.date.to_string(), "value": r2(d.value) });
    let min_day = days.iter().min_by(|a, b| a.value.total_cmp(&b.value)).expect("non-empty");
    let max_day = days.iter().max_by(|a, b| a.value.total_cmp(&b.value)).expect("non-empty");
    let k = days.len().min(7);
    let head = values[..k].iter().sum::<f64>() / k as f64;
    let tail = values[values.len() - k..].iter().sum::<f64>() / k as f64;

    // Least-squares slope over calendar days, scaled to 30 days.
    let first = days[0].date;
    let xs: Vec<f64> = days.iter().map(|d| (d.date - first).num_days() as f64).collect();
    let mx = xs.iter().sum::<f64>() / xs.len() as f64;
    let sxx: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
    let sxy: f64 = xs.iter().zip(&values).map(|(x, y)| (x - mx) * (y - stats.mean)).sum();
    let slope = (days.len() >= 3 && sxx > 0.0).then(|| r2(sxy / sxx * 30.0));

    json!({
        "n_days": stats.n,
        "mean": r2(stats.mean),
        "sd": r2(stats.sd),
        "min": point(min_day),
        "max": point(max_day),
        "first": point(&days[0]),
        "last": point(&days[days.len() - 1]),
        "first_7_days_mean": r2(head),
        "last_7_days_mean": r2(tail),
        "change_first_to_last_7_days": r2(tail - head),
        "slope_per_30_days": slope,
    })
}

fn bucketize(days: &[Day], weekly: bool) -> Vec<Value> {
    let mut groups: BTreeMap<NaiveDate, Vec<&Day>> = BTreeMap::new();
    for d in days {
        let key = if weekly { week_start(d.date) } else { month_start(d.date) };
        groups.entry(key).or_default().push(d);
    }
    groups
        .into_iter()
        .map(|(start, ds)| {
            let mean = ds.iter().map(|d| d.value).sum::<f64>() / ds.len() as f64;
            json!({
                "period": start.to_string(),
                "value": r2(mean),
                "days": ds.len(),
                "n_samples": ds.iter().map(|d| d.n).sum::<i64>(),
            })
        })
        .collect()
}

/// Authorizer for `run_sql`: SELECT only, reading only `v_*` views (and the
/// tables those views read), no PRAGMA, ATTACH, writes or risky functions.
fn views_only(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Select | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Read { table_name, .. } => {
            let via_view = ctx.accessor.is_some_and(|a| a.starts_with("v_"));
            if via_view || table_name.starts_with("v_") || matches!(table_name, "json_each" | "json_tree") {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        AuthAction::Function { function_name } => {
            let blocked = ["load_extension", "fts3_tokenizer", "sqlcipher_export", "readfile", "writefile", "edit"];
            if blocked.iter().any(|f| f.eq_ignore_ascii_case(function_name)) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }
        _ => Authorization::Deny,
    }
}

/// True if `sql` has a `;` outside quoted strings and identifiers.
fn has_statement_separator(sql: &str) -> bool {
    let mut quote: Option<char> = None;
    for c in sql.chars() {
        match (quote, c) {
            (None, '\'' | '"' | '`') => quote = Some(c),
            (None, '[') => quote = Some(']'),
            (None, ';') => return true,
            (Some(q), c) if c == q => quote = None,
            _ => {}
        }
    }
    false
}

fn sql_error(e: rusqlite::Error) -> anyhow::Error {
    match &e {
        rusqlite::Error::MultipleStatement => invalid("only one statement is allowed"),
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::AuthorizationForStatementDenied => denied(
            "only SELECT over the v_* views is allowed (see the mirrorself://schema resource)",
        ),
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::OperationInterrupted => {
            invalid(format!("query exceeded the {} s time limit", SQL_TIMEOUT.as_secs()))
        }
        _ => invalid(format!("SQL error: {e}")),
    }
}
