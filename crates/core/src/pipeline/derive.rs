//! Daily aggregates and 28-day personal baselines (architecture §5, stage 5a).
//! Only `ms:` metrics: lab results stay per report (see `v_labs`).

use std::collections::{HashSet, VecDeque};

use anyhow::Result;
use chrono::NaiveDate;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::codes::{ASLEEP_CODES, SLEEP_TOTAL};
use crate::config::PipelineConfig;

/// Fewest prior days needed before a baseline (and z-score) is reported.
pub const MIN_BASELINE_DAYS: usize = 7;
pub const BASELINE_WINDOW_DAYS: i64 = 28;

pub(super) fn run(conn: &Connection, cfg: &PipelineConfig) -> Result<Value> {
    let asleep = ASLEEP_CODES
        .iter()
        .map(|c| format!("'{c}'"))
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute_batch(&format!(
        "DELETE FROM daily_metrics;

         INSERT INTO daily_metrics (date, code, stat, value, n)
         SELECT o.local_date, o.code, 'sum', SUM(o.value_num), COUNT(*)
         FROM observations o JOIN codes c ON c.code = o.code
         WHERE c.code_system = 'ms' AND c.aggregation = 'sum' AND o.status IN ('auto', 'confirmed') AND o.value_num IS NOT NULL
         GROUP BY o.code, o.local_date;

         INSERT INTO daily_metrics (date, code, stat, value, n)
         SELECT o.local_date, o.code, 'mean', AVG(o.value_num), COUNT(*)
         FROM observations o JOIN codes c ON c.code = o.code
         WHERE c.code_system = 'ms' AND c.aggregation = 'mean' AND o.status IN ('auto', 'confirmed') AND o.value_num IS NOT NULL
         GROUP BY o.code, o.local_date;

         INSERT INTO daily_metrics (date, code, stat, value, n)
         SELECT o.local_date, o.code, 'min', MIN(o.value_num), COUNT(*)
         FROM observations o JOIN codes c ON c.code = o.code
         WHERE c.code_system = 'ms' AND c.minmax = 1 AND o.status IN ('auto', 'confirmed') AND o.value_num IS NOT NULL
         GROUP BY o.code, o.local_date;

         INSERT INTO daily_metrics (date, code, stat, value, n)
         SELECT o.local_date, o.code, 'max', MAX(o.value_num), COUNT(*)
         FROM observations o JOIN codes c ON c.code = o.code
         WHERE c.code_system = 'ms' AND c.minmax = 1 AND o.status IN ('auto', 'confirmed') AND o.value_num IS NOT NULL
         GROUP BY o.code, o.local_date;

         INSERT INTO daily_metrics (date, code, stat, value, n)
         SELECT local_date, '{SLEEP_TOTAL}', 'sum', SUM(value_num), COUNT(*)
         FROM observations
         WHERE code IN ({asleep}) AND status IN ('auto', 'confirmed')
         GROUP BY local_date;"
    ))?;
    let rows: i64 = conn.query_row("SELECT count(*) FROM daily_metrics", [], |r| r.get(0))?;
    let (with_baseline, notable) = baselines(conn, cfg)?;
    Ok(json!({ "daily_rows": rows, "with_baseline": with_baseline, "notable": notable }))
}

/// For each day, mean and SD of the previous 28 calendar days that have data.
fn baselines(conn: &Connection, cfg: &PipelineConfig) -> Result<(u64, u64)> {
    let notable_codes: HashSet<String> = conn
        .prepare("SELECT code FROM codes WHERE notable = 1")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let keys: Vec<(String, String)> = conn
        .prepare("SELECT DISTINCT code, stat FROM daily_metrics ORDER BY code, stat")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut select =
        conn.prepare("SELECT date, value FROM daily_metrics WHERE code = ?1 AND stat = ?2 ORDER BY date")?;
    let mut update = conn.prepare(
        "UPDATE daily_metrics SET baseline_28d = ?1, sd_28d = ?2, z = ?3, notable = ?4
         WHERE code = ?5 AND stat = ?6 AND date = ?7",
    )?;
    let (mut with_baseline, mut notable_count) = (0u64, 0u64);
    for (code, stat) in keys {
        let primary_stat = stat == "sum" || stat == "mean";
        let can_be_notable = primary_stat && notable_codes.contains(&code);
        let series = select
            .query_map(params![code, stat], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut window: VecDeque<(NaiveDate, f64)> = VecDeque::new();
        for (date_text, value) in series {
            let date = NaiveDate::parse_from_str(&date_text, "%Y-%m-%d")?;
            while let Some((d, _)) = window.front() {
                if (date - *d).num_days() > BASELINE_WINDOW_DAYS {
                    window.pop_front();
                } else {
                    break;
                }
            }
            if window.len() >= MIN_BASELINE_DAYS {
                let n = window.len() as f64;
                let mean = window.iter().map(|(_, v)| v).sum::<f64>() / n;
                let var = window.iter().map(|(_, v)| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
                let sd = var.sqrt();
                let z = (sd > 1e-9).then(|| (value - mean) / sd);
                let notable = can_be_notable && z.is_some_and(|z| z.abs() >= cfg.notable_z);
                update.execute(params![mean, sd, z, notable, code, stat, date_text])?;
                with_baseline += 1;
                if notable {
                    notable_count += 1;
                }
            }
            window.push_back((date, value));
        }
    }
    Ok((with_baseline, notable_count))
}
