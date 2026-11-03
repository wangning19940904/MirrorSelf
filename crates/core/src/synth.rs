//! Synthetic Apple Health exports for tests, demos and the M1 performance
//! check. Deterministic for a given seed. Never real personal data.
//!
//! Each day has watch heart rate every 10 minutes, watch and phone step
//! counts that overlap (to exercise de-duplication), resting HR, HRV, SpO2,
//! staged sleep plus an overlapping phone "in bed" sample, three runs a week,
//! weekly weight and VO2 max, and a one-week illness spell (higher resting
//! HR, lower HRV, longer sleep) 40 days before the end.

use std::io::{Cursor, Write};

use anyhow::Result;
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime};

const WATCH: &str = "Demo Apple Watch";
const WATCH_DEVICE: &str = "&lt;&lt;HKDevice: 0x600002f1c0a0&gt;, name:Apple Watch, manufacturer:Apple Inc., model:Watch, hardware:Watch7,4, software:11.0&gt;";
const PHONE: &str = "Demo iPhone";
const PHONE_DEVICE: &str = "&lt;&lt;HKDevice: 0x600002f1c1b0&gt;, name:iPhone, manufacturer:Apple Inc., model:iPhone, hardware:iPhone16,1, software:19.0&gt;";
const SCALE: &str = "Demo Scale";

#[derive(Debug, Clone)]
pub struct SynthOptions {
    pub days: u32,
    /// Last day of data (local date).
    pub end: NaiveDate,
    pub seed: u64,
    /// e.g. `+0800`.
    pub utc_offset: String,
}

impl SynthOptions {
    pub fn days(days: u32, end: NaiveDate) -> Self {
        Self {
            days,
            end,
            seed: 7,
            utc_offset: "+0800".into(),
        }
    }

    pub fn years(years: u32, end: NaiveDate) -> Self {
        Self::days(years * 365 + years / 4, end)
    }
}

/// SplitMix64: tiny, deterministic, good enough for fake data.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn normal(&mut self, mean: f64, sd: f64) -> f64 {
        let u1 = self.unit().max(1e-12);
        let u2 = self.unit();
        mean + sd * (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

struct Out<'a, W: Write> {
    w: &'a mut W,
    tz: &'a str,
    records: u64,
}

impl<W: Write> Out<'_, W> {
    fn ts(&self, t: NaiveDateTime) -> String {
        format!("{} {}", t.format("%Y-%m-%d %H:%M:%S"), self.tz)
    }

    #[allow(clippy::too_many_arguments)]
    fn quantity(
        &mut self,
        hk: &str,
        source: &str,
        device: &str,
        unit: &str,
        start: NaiveDateTime,
        end: NaiveDateTime,
        value: &str,
    ) -> std::io::Result<()> {
        self.records += 1;
        let (s, e) = (self.ts(start), self.ts(end));
        writeln!(
            self.w,
            r#" <Record type="HKQuantityTypeIdentifier{hk}" sourceName="{source}" sourceVersion="11.0" device="{device}" unit="{unit}" creationDate="{e}" startDate="{s}" endDate="{e}" value="{value}"/>"#
        )
    }

    fn sleep(&mut self, source: &str, device: &str, stage: &str, start: NaiveDateTime, end: NaiveDateTime) -> std::io::Result<()> {
        self.records += 1;
        let (s, e) = (self.ts(start), self.ts(end));
        writeln!(
            self.w,
            r#" <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="{source}" sourceVersion="11.0" device="{device}" creationDate="{e}" startDate="{s}" endDate="{e}" value="HKCategoryValueSleepAnalysis{stage}"/>"#
        )
    }
}

fn at(date: NaiveDate, minutes: i64) -> NaiveDateTime {
    date.and_time(NaiveTime::MIN) + Duration::minutes(minutes)
}

/// Writes an `export.xml` and returns the number of records written.
pub fn write_export_xml<W: Write>(w: &mut W, o: &SynthOptions) -> Result<u64> {
    let mut rng = Rng(o.seed);
    let tz = o.utc_offset.as_str();
    writeln!(w, r#"<?xml version="1.0" encoding="UTF-8"?>"#)?;
    writeln!(w, "<!DOCTYPE HealthData [\n<!ELEMENT HealthData (ExportDate,Me,(Record|Correlation|Workout|ActivitySummary|ClinicalRecord)*)>\n<!ATTLIST HealthData locale CDATA #REQUIRED>\n]>")?;
    writeln!(w, r#"<HealthData locale="en_US">"#)?;
    writeln!(w, r#" <ExportDate value="{} 23:59:00 {tz}"/>"#, o.end.format("%Y-%m-%d"))?;
    writeln!(w, r#" <Me HKCharacteristicTypeIdentifierDateOfBirth="" HKCharacteristicTypeIdentifierBiologicalSex="HKBiologicalSexNotSet" HKCharacteristicTypeIdentifierBloodType="HKBloodTypeNotSet" HKCharacteristicTypeIdentifierFitzpatrickSkinType="HKFitzpatrickSkinTypeNotSet" HKCharacteristicTypeIdentifierCardioFitnessMedicationsUse="None"/>"#)?;

    let mut out = Out { w, tz, records: 0 };
    let total = o.days.max(1) as i64;
    for i in 0..total {
        let date = o.end - Duration::days(total - 1 - i);
        let t = i as f64 / total as f64;
        let ill = total >= 60 && (total - 40..total - 33).contains(&i);
        let workout_day = !ill && matches!(date.weekday().num_days_from_monday(), 1 | 3 | 5);
        let workout_start = 18 * 60 + 30;
        let workout_min = if workout_day { rng.normal(38.0, 6.0).clamp(20.0, 70.0).round() as i64 } else { 0 };

        // Resting HR, HRV.
        let season = (std::f64::consts::TAU * i as f64 / 365.0).sin();
        let rhr = (60.0 - 4.0 * t + 2.0 * season + rng.normal(0.0, 1.2) + if ill { 7.0 } else { 0.0 }).round();
        out.quantity("RestingHeartRate", WATCH, WATCH_DEVICE, "count/min", at(date, 7 * 60), at(date, 7 * 60), &format!("{rhr:.0}"))?;
        for m in [3 * 60 + 10, 13 * 60 + 10, 21 * 60 + 10] {
            let hrv = (48.0 - 1.2 * (rhr - 58.0) - if ill { 12.0 } else { 0.0 } + rng.normal(0.0, 8.0)).max(8.0);
            out.quantity("HeartRateVariabilitySDNN", WATCH, WATCH_DEVICE, "ms", at(date, m), at(date, m + 1), &format!("{hrv:.2}"))?;
        }

        // Heart rate every 10 minutes.
        for k in 0..144i64 {
            let m = k * 10;
            let in_workout = workout_day && (workout_start..workout_start + workout_min).contains(&m);
            let hr = if in_workout {
                rng.normal(145.0, 10.0)
            } else if m < 7 * 60 {
                rhr + rng.normal(2.0, 2.0)
            } else {
                rhr + 18.0 + rng.normal(0.0, 8.0)
            };
            out.quantity("HeartRate", WATCH, WATCH_DEVICE, "count/min", at(date, m), at(date, m), &format!("{:.0}", hr.max(35.0)))?;
        }

        // Steps, distance and energy: watch every 30 minutes 07:00–22:00,
        // phone hourly over the same span (duplicates), phone alone 22:00–23:00.
        let mut hour_steps = [0f64; 24];
        for k in 0..30i64 {
            let m = 7 * 60 + k * 30;
            let in_workout = workout_day && (workout_start..workout_start + workout_min).contains(&m);
            let steps = (rng.normal(250.0, 150.0).max(0.0) + if in_workout { 2600.0 } else { 0.0 }).round();
            hour_steps[(m / 60) as usize] += steps;
            let (s, e) = (at(date, m), at(date, m + 30));
            out.quantity("StepCount", WATCH, WATCH_DEVICE, "count", s, e, &format!("{steps:.0}"))?;
            out.quantity("DistanceWalkingRunning", WATCH, WATCH_DEVICE, "km", s, e, &format!("{:.3}", steps * 0.00072))?;
            let kcal = steps * 0.04 + rng.normal(8.0, 3.0).max(1.0);
            out.quantity("ActiveEnergyBurned", WATCH, WATCH_DEVICE, "kcal", s, e, &format!("{kcal:.2}"))?;
        }
        for h in 7..22i64 {
            let steps = (hour_steps[h as usize] * 0.95).round();
            out.quantity("StepCount", PHONE, PHONE_DEVICE, "count", at(date, h * 60), at(date, h * 60 + 60), &format!("{steps:.0}"))?;
        }
        let evening = rng.normal(400.0, 100.0).max(0.0).round();
        out.quantity("StepCount", PHONE, PHONE_DEVICE, "count", at(date, 22 * 60), at(date, 23 * 60), &format!("{evening:.0}"))?;

        // Sleep for the night ending on `date`.
        let bed = at(date, -60 + rng.normal(20.0, 25.0).clamp(-40.0, 90.0).round() as i64);
        let target = rng.normal(430.0, 35.0) + if ill { 60.0 } else { 0.0 };
        let mut cursor = bed;
        let mut slept = 0.0;
        let mut cycle = 0;
        while slept < target {
            let early = cycle < 2;
            let stages = [
                ("AsleepCore", if early { 40.0 } else { 50.0 }),
                ("AsleepDeep", if early { 25.0 } else { 8.0 }),
                ("AsleepCore", 10.0),
                ("AsleepREM", if early { 12.0 } else { 28.0 }),
            ];
            for (stage, base) in stages {
                let len = (base + rng.normal(0.0, 4.0)).max(3.0).round() as i64;
                let end = cursor + Duration::minutes(len);
                out.sleep(WATCH, WATCH_DEVICE, stage, cursor, end)?;
                cursor = end;
                slept += len as f64;
            }
            if rng.unit() < 0.5 {
                let end = cursor + Duration::minutes(3);
                out.sleep(WATCH, WATCH_DEVICE, "Awake", cursor, end)?;
                cursor = end;
            }
            cycle += 1;
        }
        out.sleep(PHONE, PHONE_DEVICE, "InBed", bed - Duration::minutes(10), cursor + Duration::minutes(8))?;
        let rr = rng.normal(14.5, 0.6) + if ill { 1.5 } else { 0.0 };
        out.quantity("RespiratoryRate", WATCH, WATCH_DEVICE, "count/min", cursor - Duration::minutes(30), cursor - Duration::minutes(30), &format!("{rr:.1}"))?;

        for m in [4 * 60, 15 * 60] {
            let spo2 = rng.normal(0.97, 0.01).clamp(0.9, 1.0);
            out.quantity("OxygenSaturation", WATCH, WATCH_DEVICE, "%", at(date, m), at(date, m), &format!("{spo2:.2}"))?;
        }

        if workout_day {
            let start = at(date, workout_start);
            let end = start + Duration::minutes(workout_min);
            let km = workout_min as f64 / 6.0;
            let kcal = workout_min as f64 * 10.0;
            let (s, e) = (out.ts(start), out.ts(end));
            out.records += 1;
            // Even years use the legacy total* attributes, odd years the newer
            // WorkoutStatistics children, so both parser paths stay covered.
            if date.year() % 2 == 0 {
                writeln!(out.w, r#" <Workout workoutActivityType="HKWorkoutActivityTypeRunning" duration="{workout_min}" durationUnit="min" totalDistance="{km:.2}" totalDistanceUnit="km" totalEnergyBurned="{kcal:.0}" totalEnergyBurnedUnit="kcal" sourceName="{WATCH}" sourceVersion="11.0" device="{WATCH_DEVICE}" creationDate="{e}" startDate="{s}" endDate="{e}"/>"#)?;
            } else {
                writeln!(out.w, r#" <Workout workoutActivityType="HKWorkoutActivityTypeRunning" duration="{workout_min}" durationUnit="min" sourceName="{WATCH}" sourceVersion="11.0" device="{WATCH_DEVICE}" creationDate="{e}" startDate="{s}" endDate="{e}">"#)?;
                writeln!(out.w, r#"  <WorkoutStatistics type="HKQuantityTypeIdentifierActiveEnergyBurned" startDate="{s}" endDate="{e}" sum="{kcal:.0}" unit="kcal"/>"#)?;
                writeln!(out.w, r#"  <WorkoutStatistics type="HKQuantityTypeIdentifierDistanceWalkingRunning" startDate="{s}" endDate="{e}" sum="{km:.2}" unit="km"/>"#)?;
                writeln!(out.w, " </Workout>")?;
            }
        }

        if date.weekday().num_days_from_monday() == 0 {
            let kg = rng.normal(70.0 - 2.0 * t, 0.4);
            out.quantity("BodyMass", SCALE, "", "kg", at(date, 7 * 60 + 30), at(date, 7 * 60 + 30), &format!("{kg:.1}"))?;
        }
        if date.weekday().num_days_from_monday() == 6 {
            let vo2 = 41.0 + 4.0 * t + rng.normal(0.0, 0.5);
            out.quantity("VO2Max", WATCH, WATCH_DEVICE, "mL/min·kg", at(date, 19 * 60 + 30), at(date, 19 * 60 + 30), &format!("{vo2:.2}"))?;
        }
    }
    writeln!(out.w, "</HealthData>")?;
    Ok(out.records)
}

pub fn export_xml(o: &SynthOptions) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    write_export_xml(&mut buf, o)?;
    Ok(buf)
}

/// An `export.zip` laid out like the Health app's (`apple_health_export/export.xml`).
pub fn export_zip(o: &SynthOptions) -> Result<(Vec<u8>, u64)> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("apple_health_export/export.xml", options)?;
    let records = write_export_xml(&mut zip, o)?;
    let cursor = zip.finish()?;
    Ok((cursor.into_inner(), records))
}
