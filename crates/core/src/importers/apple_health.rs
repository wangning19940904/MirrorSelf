//! Apple Health `export.zip` / `export.xml` reader.
//!
//! Streams the XML so multi-gigabyte exports never sit in memory as a DOM.
//! Records nested in `<Correlation>` are reported like top-level ones; the
//! pipeline's fingerprints collapse the copies.

use std::io::{BufRead, BufReader, Cursor};

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde::Serialize;

use crate::codes;
use crate::time::parse_apple;

pub const SOURCE: &str = "apple_health";
pub const KIND: &str = "apple_health_export";

#[derive(Debug, Clone)]
pub struct Sample {
    pub hk_type: String,
    pub source_name: String,
    /// Stable device label, e.g. `Apple Watch (Watch7,4)`.
    pub device: Option<String>,
    pub unit: Option<String>,
    pub value: Option<String>,
    pub start: DateTime<FixedOffset>,
    pub end: DateTime<FixedOffset>,
}

#[derive(Debug, Clone)]
pub struct Workout {
    /// `Running`, `Traditional Strength Training`, …
    pub activity: String,
    pub source_name: String,
    pub device: Option<String>,
    pub start: DateTime<FixedOffset>,
    pub end: DateTime<FixedOffset>,
    pub duration_min: f64,
    pub distance_km: Option<f64>,
    pub energy_kcal: Option<f64>,
}

#[derive(Debug, Clone)]
pub enum Item {
    Sample(Sample),
    Workout(Workout),
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ParseStats {
    pub records: u64,
    pub workouts: u64,
    /// Elements missing required attributes or with unreadable dates.
    pub malformed: u64,
}

pub fn looks_like_export(content: &[u8]) -> bool {
    if content.starts_with(b"PK\x03\x04") {
        return true;
    }
    // The Health app's export.xml opens with a DTD of roughly 10 KB.
    let head = &content[..content.len().min(64 * 1024)];
    head.windows(b"<HealthData".len()).any(|w| w == b"<HealthData")
}

/// Calls `sink` for every record and workout in an export.
pub fn parse(content: &[u8], mut sink: impl FnMut(Item) -> Result<()>) -> Result<ParseStats> {
    if content.starts_with(b"PK\x03\x04") {
        let mut archive =
            zip::ZipArchive::new(Cursor::new(content)).context("export.zip is not a readable zip")?;
        let name = archive
            .file_names()
            .find(|n| !n.starts_with("__MACOSX") && n.rsplit('/').next() == Some("export.xml"))
            .map(str::to_owned)
            .context("export.zip has no export.xml")?;
        let file = archive.by_name(&name)?;
        parse_xml(BufReader::with_capacity(1 << 16, file), &mut sink)
    } else {
        parse_xml(content, &mut sink)
    }
}

struct Attrs(Vec<(String, String)>);

impl Attrs {
    fn read(e: &BytesStart<'_>) -> Result<Self> {
        let mut out = Vec::with_capacity(12);
        for attr in e.attributes() {
            let attr = attr.context("malformed XML attribute")?;
            let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
            let value = attr
                .unescape_value()
                .context("malformed XML attribute value")?
                .into_owned();
            out.push((key, value));
        }
        Ok(Self(out))
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn date(&self, key: &str) -> Option<DateTime<FixedOffset>> {
        self.get(key).and_then(|s| parse_apple(s).ok())
    }

    fn num(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(|s| s.trim().parse().ok())
    }
}

fn parse_xml<R: BufRead>(input: R, sink: &mut impl FnMut(Item) -> Result<()>) -> Result<ParseStats> {
    let mut xml = Reader::from_reader(input);
    xml.config_mut().trim_text(true);
    let mut buf = Vec::with_capacity(4096);
    let mut stats = ParseStats::default();
    let mut open_workout: Option<Workout> = None;

    loop {
        let event = xml
            .read_event_into(&mut buf)
            .with_context(|| format!("export.xml is malformed near byte {}", xml.buffer_position()))?;
        match &event {
            Event::Start(e) | Event::Empty(e) => {
                let self_closing = matches!(&event, Event::Empty(_));
                match e.name().as_ref() {
                    b"Record" => {
                        stats.records += 1;
                        match sample(&Attrs::read(e)?) {
                            Some(s) => sink(Item::Sample(s))?,
                            None => stats.malformed += 1,
                        }
                    }
                    b"Workout" => match workout(&Attrs::read(e)?) {
                        Some(w) if self_closing => {
                            stats.workouts += 1;
                            sink(Item::Workout(w))?;
                        }
                        Some(w) => open_workout = Some(w),
                        None => stats.malformed += 1,
                    },
                    b"WorkoutStatistics" => {
                        if let Some(w) = open_workout.as_mut() {
                            add_statistics(w, &Attrs::read(e)?);
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) if e.name().as_ref() == b"Workout" => {
                if let Some(w) = open_workout.take() {
                    stats.workouts += 1;
                    sink(Item::Workout(w))?;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(stats)
}

fn sample(a: &Attrs) -> Option<Sample> {
    Some(Sample {
        hk_type: a.get("type")?.to_string(),
        source_name: a.get("sourceName").unwrap_or("Unknown source").to_string(),
        device: a.get("device").and_then(device_label),
        unit: a.get("unit").map(str::to_string),
        value: a.get("value").map(str::to_string),
        start: a.date("startDate")?,
        end: a.date("endDate")?,
    })
}

fn workout(a: &Attrs) -> Option<Workout> {
    let start = a.date("startDate")?;
    let end = a.date("endDate")?;
    let activity = activity_name(a.get("workoutActivityType")?);
    let duration_min = match (a.num("duration"), a.get("durationUnit")) {
        (Some(d), Some("s")) => d / 60.0,
        (Some(d), Some("hr")) => d * 60.0,
        (Some(d), _) => d,
        (None, _) => (end - start).num_seconds() as f64 / 60.0,
    };
    let distance_km = a.num("totalDistance").and_then(|v| {
        convert("ms:distance_walk_run", v, a.get("totalDistanceUnit").unwrap_or("km"))
    });
    let energy_kcal = a.num("totalEnergyBurned").and_then(|v| {
        convert("ms:active_energy", v, a.get("totalEnergyBurnedUnit").unwrap_or("kcal"))
    });
    Some(Workout {
        activity,
        source_name: a.get("sourceName").unwrap_or("Unknown source").to_string(),
        device: a.get("device").and_then(device_label),
        start,
        end,
        duration_min,
        distance_km,
        energy_kcal,
    })
}

/// Newer exports carry totals as `<WorkoutStatistics>` children; they win
/// over the legacy `totalDistance` / `totalEnergyBurned` attributes.
fn add_statistics(w: &mut Workout, a: &Attrs) {
    let (Some(kind), Some(sum)) = (a.get("type"), a.num("sum")) else {
        return;
    };
    let unit = a.get("unit").unwrap_or("");
    if kind == "HKQuantityTypeIdentifierActiveEnergyBurned" {
        if let Some(v) = convert("ms:active_energy", sum, unit) {
            w.energy_kcal = Some(v);
        }
    } else if kind.starts_with("HKQuantityTypeIdentifierDistance") {
        if let Some(v) = convert("ms:distance_walk_run", sum, unit) {
            w.distance_km = Some(v);
        }
    }
}

fn convert(code: &str, value: f64, unit: &str) -> Option<f64> {
    codes::to_canonical(codes::metric(code)?, value, unit)
}

/// `HKWorkoutActivityTypeTraditionalStrengthTraining` → `Traditional Strength Training`.
pub fn activity_name(raw: &str) -> String {
    let name = raw.strip_prefix("HKWorkoutActivityType").unwrap_or(raw);
    let mut out = String::with_capacity(name.len() + 4);
    let mut prev_lower = false;
    for ch in name.chars() {
        if ch.is_uppercase() && prev_lower {
            out.push(' ');
        }
        prev_lower = ch.is_lowercase();
        out.push(ch);
    }
    out
}

/// Pulls `name` and `hardware` out of HealthKit's device description
/// (`<<HKDevice: 0x…>, name:Apple Watch, …, hardware:Watch6,2, …>`). The
/// object address changes between exports, so it is dropped.
pub fn device_label(raw: &str) -> Option<String> {
    let mut name = None;
    let mut hardware = None;
    for field in raw.split(", ") {
        let field = field.trim_end_matches('>');
        if let Some(v) = field.strip_prefix("name:") {
            name = Some(v.trim());
        } else if let Some(v) = field.strip_prefix("hardware:") {
            hardware = Some(v.trim());
        }
    }
    match (name, hardware) {
        (Some(n), Some(h)) if !h.is_empty() => Some(format!("{n} ({h})")),
        (Some(n), _) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_labels() {
        let raw = "<<HKDevice: 0x283f5c1e0>, name:Apple Watch, manufacturer:Apple Inc., model:Watch, hardware:Watch6,2, software:9.1>";
        assert_eq!(device_label(raw).as_deref(), Some("Apple Watch (Watch6,2)"));
        assert_eq!(device_label("nonsense"), None);
    }

    #[test]
    fn activity_names() {
        assert_eq!(activity_name("HKWorkoutActivityTypeRunning"), "Running");
        assert_eq!(
            activity_name("HKWorkoutActivityTypeTraditionalStrengthTraining"),
            "Traditional Strength Training"
        );
    }
}
