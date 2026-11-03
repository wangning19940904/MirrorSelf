//! Internal `ms:` metric codes, their HealthKit sources and unit conversion
//! to UCUM (architecture §6, "Code systems").

use anyhow::Result;
use rusqlite::{params, Connection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    /// Cumulative over the day (steps, energy, sleep minutes).
    Sum,
    /// Averaged over the day (heart rate, weight).
    Mean,
}

impl Agg {
    pub fn stat(self) -> &'static str {
        match self {
            Agg::Sum => "sum",
            Agg::Mean => "mean",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MetricDef {
    pub code: &'static str,
    /// HealthKit type identifier this code is read from, if any.
    pub hk: Option<&'static str>,
    pub display: &'static str,
    pub display_zh: &'static str,
    pub category: &'static str,
    /// UCUM unit after normalization.
    pub unit: &'static str,
    pub agg: Agg,
    /// Also keep daily min and max.
    pub minmax: bool,
    /// Daily deviations show on the timeline as notable.
    pub notable: bool,
}

#[allow(clippy::too_many_arguments)]
const fn m(
    code: &'static str,
    hk: Option<&'static str>,
    display: &'static str,
    display_zh: &'static str,
    category: &'static str,
    unit: &'static str,
    agg: Agg,
    minmax: bool,
    notable: bool,
) -> MetricDef {
    MetricDef { code, hk, display, display_zh, category, unit, agg, minmax, notable }
}

use Agg::{Mean, Sum};

pub const SLEEP_TOTAL: &str = "ms:sleep_asleep_total";

pub const METRICS: &[MetricDef] = &[
    m("ms:steps", Some("HKQuantityTypeIdentifierStepCount"), "Steps", "步数", "activity", "{steps}", Sum, false, false),
    m("ms:distance_walk_run", Some("HKQuantityTypeIdentifierDistanceWalkingRunning"), "Walking + running distance", "步行+跑步距离", "activity", "km", Sum, false, false),
    m("ms:active_energy", Some("HKQuantityTypeIdentifierActiveEnergyBurned"), "Active energy", "活动能量", "activity", "kcal", Sum, false, false),
    m("ms:basal_energy", Some("HKQuantityTypeIdentifierBasalEnergyBurned"), "Resting energy", "静息能量", "activity", "kcal", Sum, false, false),
    m("ms:flights_climbed", Some("HKQuantityTypeIdentifierFlightsClimbed"), "Flights climbed", "爬楼层数", "activity", "{flights}", Sum, false, false),
    m("ms:exercise_time", Some("HKQuantityTypeIdentifierAppleExerciseTime"), "Exercise minutes", "锻炼分钟数", "activity", "min", Sum, false, false),
    m("ms:heart_rate", Some("HKQuantityTypeIdentifierHeartRate"), "Heart rate", "心率", "cardio", "/min", Mean, true, false),
    m("ms:resting_hr", Some("HKQuantityTypeIdentifierRestingHeartRate"), "Resting heart rate", "静息心率", "cardio", "/min", Mean, false, true),
    m("ms:walking_hr", Some("HKQuantityTypeIdentifierWalkingHeartRateAverage"), "Walking heart rate", "步行平均心率", "cardio", "/min", Mean, false, true),
    m("ms:hrv_sdnn", Some("HKQuantityTypeIdentifierHeartRateVariabilitySDNN"), "Heart rate variability (SDNN)", "心率变异性", "cardio", "ms", Mean, false, true),
    m("ms:vo2max", Some("HKQuantityTypeIdentifierVO2Max"), "VO2 max", "最大摄氧量", "cardio", "mL/(kg.min)", Mean, false, false),
    m("ms:spo2", Some("HKQuantityTypeIdentifierOxygenSaturation"), "Blood oxygen", "血氧", "cardio", "%", Mean, true, true),
    m("ms:respiratory_rate", Some("HKQuantityTypeIdentifierRespiratoryRate"), "Respiratory rate", "呼吸频率", "cardio", "/min", Mean, false, true),
    m("ms:bp_systolic", Some("HKQuantityTypeIdentifierBloodPressureSystolic"), "Systolic blood pressure", "收缩压", "cardio", "mm[Hg]", Mean, false, true),
    m("ms:bp_diastolic", Some("HKQuantityTypeIdentifierBloodPressureDiastolic"), "Diastolic blood pressure", "舒张压", "cardio", "mm[Hg]", Mean, false, true),
    m("ms:body_mass", Some("HKQuantityTypeIdentifierBodyMass"), "Weight", "体重", "metabolic", "kg", Mean, false, true),
    m("ms:body_fat", Some("HKQuantityTypeIdentifierBodyFatPercentage"), "Body fat", "体脂率", "metabolic", "%", Mean, false, false),
    m("ms:bmi", Some("HKQuantityTypeIdentifierBodyMassIndex"), "Body mass index", "体重指数", "metabolic", "kg/m2", Mean, false, false),
    m("ms:glucose", Some("HKQuantityTypeIdentifierBloodGlucose"), "Blood glucose", "血糖", "metabolic", "mmol/L", Mean, true, true),
    m("ms:body_temp", Some("HKQuantityTypeIdentifierBodyTemperature"), "Body temperature", "体温", "metabolic", "Cel", Mean, false, true),
    m("ms:sleep_in_bed", None, "Time in bed", "卧床时间", "sleep", "min", Sum, false, false),
    m("ms:sleep_asleep_unspecified", None, "Asleep (unstaged)", "睡眠（未分期）", "sleep", "min", Sum, false, false),
    m("ms:sleep_asleep_core", None, "Core sleep", "核心睡眠", "sleep", "min", Sum, false, false),
    m("ms:sleep_asleep_deep", None, "Deep sleep", "深度睡眠", "sleep", "min", Sum, false, false),
    m("ms:sleep_asleep_rem", None, "REM sleep", "快速眼动睡眠", "sleep", "min", Sum, false, false),
    m("ms:sleep_awake", None, "Awake during sleep", "睡眠中清醒", "sleep", "min", Sum, false, false),
    m(SLEEP_TOTAL, None, "Total sleep", "总睡眠时长", "sleep", "min", Sum, false, true),
];

/// Sleep stage codes that count as asleep (they add up to [`SLEEP_TOTAL`]).
pub const ASLEEP_CODES: &[&str] = &[
    "ms:sleep_asleep_unspecified",
    "ms:sleep_asleep_core",
    "ms:sleep_asleep_deep",
    "ms:sleep_asleep_rem",
];

pub const SLEEP_HK_TYPE: &str = "HKCategoryTypeIdentifierSleepAnalysis";

pub fn metric(code: &str) -> Option<&'static MetricDef> {
    METRICS.iter().find(|d| d.code == code)
}

/// Accepts `ms:resting_hr`, `resting_hr`, a HealthKit identifier or a display name.
pub fn resolve(input: &str) -> Option<&'static MetricDef> {
    let input = input.trim();
    metric(input)
        .or_else(|| METRICS.iter().find(|d| d.code.strip_prefix("ms:") == Some(input)))
        .or_else(|| METRICS.iter().find(|d| d.hk == Some(input)))
        .or_else(|| METRICS.iter().find(|d| d.display.eq_ignore_ascii_case(input)))
}

pub fn for_hk_quantity(hk_type: &str) -> Option<&'static MetricDef> {
    METRICS.iter().find(|d| d.hk == Some(hk_type))
}

/// `HKCategoryValueSleepAnalysis*` → sleep stage code.
pub fn sleep_code(value: &str) -> Option<&'static str> {
    let stage = value.strip_prefix("HKCategoryValueSleepAnalysis").unwrap_or(value);
    Some(match stage {
        "InBed" => "ms:sleep_in_bed",
        "Asleep" | "AsleepUnspecified" => "ms:sleep_asleep_unspecified",
        "AsleepCore" => "ms:sleep_asleep_core",
        "AsleepDeep" => "ms:sleep_asleep_deep",
        "AsleepREM" => "ms:sleep_asleep_rem",
        "Awake" => "ms:sleep_awake",
        _ => return None,
    })
}

pub fn is_sleep(code: &str) -> bool {
    code.starts_with("ms:sleep_")
}

/// Converts a HealthKit value in `unit` to the code's UCUM unit.
/// `None` when the unit is not one we know how to convert.
pub fn to_canonical(def: &MetricDef, value: f64, unit: &str) -> Option<f64> {
    let u = unit.trim();
    let v = match def.unit {
        "{steps}" | "{flights}" => match u {
            "count" | "" => value,
            _ => return None,
        },
        "/min" => match u {
            "count/min" | "/min" | "bpm" => value,
            "count/s" => value * 60.0,
            _ => return None,
        },
        "ms" => match u {
            "ms" => value,
            "s" => value * 1000.0,
            _ => return None,
        },
        "kg" => match u {
            "kg" => value,
            "g" => value / 1000.0,
            "lb" => value * 0.453_592_37,
            "st" => value * 6.350_293_18,
            _ => return None,
        },
        // HealthKit exports percentages as fractions with unit "%".
        "%" => match u {
            "%" if value <= 1.0 => value * 100.0,
            "%" => value,
            _ => return None,
        },
        "mm[Hg]" => match u {
            "mmHg" | "mm[Hg]" => value,
            _ => return None,
        },
        "mmol/L" => {
            if u.starts_with("mg/dL") || u.starts_with("mg/dl") {
                value / 18.0156
            } else if u.starts_with("mmol") {
                value
            } else {
                return None;
            }
        }
        "kcal" => match u {
            "kcal" | "Cal" => value,
            "cal" => value / 1000.0,
            "kJ" => value / 4.184,
            _ => return None,
        },
        "km" => match u {
            "km" => value,
            "m" => value / 1000.0,
            "mi" => value * 1.609_344,
            "yd" => value * 0.000_914_4,
            "ft" => value * 0.000_304_8,
            _ => return None,
        },
        "mL/(kg.min)" => {
            if u.starts_with("mL/min") || u.starts_with("ml/min") || u.starts_with("mL/(kg") {
                value
            } else {
                return None;
            }
        }
        "Cel" => match u {
            "degC" => value,
            "degF" => (value - 32.0) * 5.0 / 9.0,
            _ => return None,
        },
        "min" => match u {
            "min" => value,
            "s" => value / 60.0,
            "hr" => value * 60.0,
            _ => return None,
        },
        "kg/m2" => match u {
            "count" | "kg/m^2" | "kg/m2" | "" => value,
            _ => return None,
        },
        _ => return None,
    };
    Some(v)
}

/// Keeps the `codes` table in step with [`METRICS`].
pub fn seed(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO codes (code, code_system, display, display_zh, category_id, unit, aggregation, minmax, notable)
             VALUES (?1, 'ms', ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(code) DO UPDATE SET display = excluded.display, display_zh = excluded.display_zh,
               category_id = excluded.category_id, unit = excluded.unit, aggregation = excluded.aggregation,
               minmax = excluded.minmax, notable = excluded.notable",
        )?;
        for d in METRICS {
            stmt.execute(params![
                d.code,
                d.display,
                d.display_zh,
                d.category,
                d.unit,
                d.agg.stat(),
                d.minmax,
                d.notable
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        let mass = metric("ms:body_mass").unwrap();
        assert!((to_canonical(mass, 154.3236, "lb").unwrap() - 70.0).abs() < 1e-3);
        let spo2 = metric("ms:spo2").unwrap();
        assert_eq!(to_canonical(spo2, 0.97, "%").unwrap(), 97.0);
        let glucose = metric("ms:glucose").unwrap();
        assert!((to_canonical(glucose, 99.0, "mg/dL").unwrap() - 5.4952).abs() < 1e-3);
        assert_eq!(to_canonical(glucose, 5.5, "mmol<180.15588000005408>/L"), Some(5.5));
        let vo2 = metric("ms:vo2max").unwrap();
        assert_eq!(to_canonical(vo2, 45.1, "mL/min·kg"), Some(45.1));
        assert_eq!(to_canonical(mass, 1.0, "furlong"), None);
    }

    #[test]
    fn resolve_accepts_aliases() {
        assert_eq!(resolve("resting_hr").unwrap().code, "ms:resting_hr");
        assert_eq!(resolve("ms:resting_hr").unwrap().code, "ms:resting_hr");
        assert_eq!(resolve("HKQuantityTypeIdentifierStepCount").unwrap().code, "ms:steps");
        assert_eq!(resolve("weight").unwrap().code, "ms:body_mass");
        assert!(resolve("nope").is_none());
    }
}
