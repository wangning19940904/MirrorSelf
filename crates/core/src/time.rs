//! Timestamp handling: store UTC plus the original offset, bucket by local date.

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, FixedOffset, Months, NaiveDate, SecondsFormat, Utc};

use crate::error::invalid;

/// HealthKit export format: `2026-10-01 08:00:00 +0800`.
pub fn parse_apple(s: &str) -> Result<DateTime<FixedOffset>> {
    DateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S %z")
        .map_err(|e| invalid(format!("bad HealthKit timestamp '{s}': {e}")))
}

pub fn utc_string(dt: &DateTime<FixedOffset>) -> String {
    dt.with_timezone(&Utc).to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn now_utc_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Microsecond precision, so import order survives a rebuild even when two
/// files arrive within the same second.
pub fn now_utc_precise() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Parses a stored UTC timestamp back to seconds since the epoch.
pub fn epoch_seconds(utc: &str) -> Result<i64> {
    Ok(DateTime::parse_from_rfc3339(utc)
        .map_err(|e| invalid(format!("bad stored timestamp '{utc}': {e}")))?
        .timestamp())
}

pub fn today_local() -> NaiveDate {
    chrono::Local::now().date_naive()
}

/// `YYYY-MM-DD`, `today`, `yesterday`, or a look-back such as `90d`, `12w`,
/// `6m`, `1y` counted from `today`.
pub fn parse_date_arg(s: &str, today: NaiveDate) -> Result<NaiveDate> {
    let s = s.trim();
    match s {
        "today" => return Ok(today),
        "yesterday" => return Ok(today - Duration::days(1)),
        _ => {}
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d);
    }
    let bad = || invalid(format!("bad date '{s}' (use YYYY-MM-DD, today, or 90d / 12w / 6m / 1y)"));
    let unit = s.chars().last().ok_or_else(bad)?;
    let n: u32 = s[..s.len() - unit.len_utf8()].parse().map_err(|_| bad())?;
    let date = match unit {
        'd' => today.checked_sub_signed(Duration::days(n.into())),
        'w' => today.checked_sub_signed(Duration::weeks(n.into())),
        'm' => today.checked_sub_months(Months::new(n)),
        'y' => today.checked_sub_months(Months::new(n.saturating_mul(12))),
        _ => None,
    };
    date.ok_or_else(bad)
}

/// Inclusive local-date range; open ends span all data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateRange {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

impl DateRange {
    pub fn parse(from: Option<&str>, to: Option<&str>, today: NaiveDate) -> Result<Self> {
        let from = match from {
            Some(s) if !s.trim().is_empty() => parse_date_arg(s, today)?,
            _ => NaiveDate::from_ymd_opt(1900, 1, 1).expect("valid date"),
        };
        let to = match to {
            Some(s) if !s.trim().is_empty() => parse_date_arg(s, today)?,
            _ => NaiveDate::from_ymd_opt(9999, 12, 31).expect("valid date"),
        };
        if from > to {
            return Err(invalid(format!("'from' {from} is after 'to' {to}")));
        }
        Ok(Self { from, to })
    }
}

/// Monday of the ISO week containing `d`.
pub fn week_start(d: NaiveDate) -> NaiveDate {
    d - Duration::days(d.weekday().num_days_from_monday().into())
}

pub fn month_start(d: NaiveDate) -> NaiveDate {
    d.with_day(1).expect("day 1 exists")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_args() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        assert_eq!(parse_date_arg("90d", today).unwrap(), NaiveDate::from_ymd_opt(2026, 8, 3).unwrap());
        assert_eq!(parse_date_arg("2w", today).unwrap(), NaiveDate::from_ymd_opt(2026, 10, 18).unwrap());
        assert_eq!(parse_date_arg("1y", today).unwrap(), NaiveDate::from_ymd_opt(2025, 11, 1).unwrap());
        assert_eq!(parse_date_arg("2026-01-05", today).unwrap(), NaiveDate::from_ymd_opt(2026, 1, 5).unwrap());
        assert!(parse_date_arg("soon", today).is_err());
    }

    #[test]
    fn apple_timestamps_keep_offset() {
        let dt = parse_apple("2026-10-01 23:30:00 +0800").unwrap();
        assert_eq!(utc_string(&dt), "2026-10-01T15:30:00Z");
        assert_eq!(dt.offset().local_minus_utc(), 8 * 3600);
        assert_eq!(dt.date_naive(), NaiveDate::from_ymd_opt(2026, 10, 1).unwrap());
    }
}
