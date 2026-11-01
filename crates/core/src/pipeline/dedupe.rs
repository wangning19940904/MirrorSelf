//! Cross-device de-duplication for cumulative metrics.
//!
//! Apple Watch and iPhone both count steps (and both record sleep); summing
//! everything double-counts. Sources are ranked watch → phone → other, then by
//! name. A sample is marked `duplicate` when it overlaps time already covered
//! by a higher-ranked source. Samples from one source never cancel each other.

use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::codes::{self, Agg, METRICS, SLEEP_TOTAL};
use crate::time::epoch_seconds;

struct Span {
    id: String,
    start: i64,
    end: i64,
    rank: (i64, String),
}

pub(super) fn run(conn: &Connection) -> Result<Value> {
    conn.execute(
        "UPDATE observations SET status = 'auto' WHERE status = 'duplicate'",
        [],
    )?;

    let mut groups: Vec<Vec<&str>> = METRICS
        .iter()
        .filter(|d| d.agg == Agg::Sum && !codes::is_sleep(d.code))
        .map(|d| vec![d.code])
        .collect();
    groups.push(
        METRICS
            .iter()
            .filter(|d| codes::is_sleep(d.code) && d.code != SLEEP_TOTAL)
            .map(|d| d.code)
            .collect(),
    );

    let mut select = conn.prepare(
        "SELECT id, start_ts, end_ts, source_tier, coalesce(source_name, '')
         FROM observations
         WHERE code IN (SELECT value FROM json_each(?1)) AND status = 'auto'
         ORDER BY source_tier, source_name, start_ts, id",
    )?;
    let mut mark = conn.prepare("UPDATE observations SET status = 'duplicate' WHERE id = ?1")?;
    let mut marked = 0u64;
    for group in groups {
        let rows = select
            .query_map(params![serde_json::to_string(&group)?], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut spans = Vec::with_capacity(rows.len());
        for (id, start, end, tier, source) in rows {
            spans.push(Span {
                id,
                start: epoch_seconds(&start)?,
                end: epoch_seconds(&end)?,
                rank: (tier, source),
            });
        }
        for id in find_duplicates(&spans) {
            mark.execute([id])?;
            marked += 1;
        }
    }
    Ok(json!({ "duplicates_marked": marked }))
}

/// `spans` must be sorted by rank. Returns ids covered by a higher rank.
fn find_duplicates(spans: &[Span]) -> Vec<String> {
    let mut covered: Vec<(i64, i64)> = Vec::new();
    let mut dups = Vec::new();
    let mut i = 0;
    while i < spans.len() {
        let rank = &spans[i].rank;
        let mut fresh = Vec::new();
        let mut j = i;
        while j < spans.len() && &spans[j].rank == rank {
            let s = &spans[j];
            let end = s.end.max(s.start + 1);
            if overlaps(&covered, s.start, end) {
                dups.push(s.id.clone());
            } else {
                fresh.push((s.start, end));
            }
            j += 1;
        }
        covered = merge(covered, fresh);
        i = j;
    }
    dups
}

/// `merged` is sorted and non-overlapping; intervals are half-open.
fn overlaps(merged: &[(i64, i64)], start: i64, end: i64) -> bool {
    let idx = merged.partition_point(|iv| iv.1 <= start);
    idx < merged.len() && merged[idx].0 < end
}

fn merge(mut a: Vec<(i64, i64)>, b: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    a.extend(b);
    a.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::with_capacity(a.len());
    for (s, e) in a {
        if let Some(last) = out.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        out.push((s, e));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(id: &str, start: i64, end: i64, tier: i64, source: &str) -> Span {
        Span {
            id: id.into(),
            start,
            end,
            rank: (tier, source.into()),
        }
    }

    #[test]
    fn phone_samples_under_watch_coverage_are_duplicates() {
        let spans = vec![
            span("w1", 0, 30, 0, "Watch"),
            span("w2", 30, 60, 0, "Watch"),
            span("p1", 0, 60, 1, "iPhone"),
            span("p2", 120, 150, 1, "iPhone"),
            span("o1", 125, 130, 2, "App"),
        ];
        assert_eq!(find_duplicates(&spans), vec!["p1".to_string(), "o1".to_string()]);
    }

    #[test]
    fn same_source_never_cancels_itself() {
        let spans = vec![span("a", 0, 60, 0, "Watch"), span("b", 30, 90, 0, "Watch")];
        assert!(find_duplicates(&spans).is_empty());
    }
}
