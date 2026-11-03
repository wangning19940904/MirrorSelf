//! Human-readable output. `--json` bypasses all of this.

use std::fmt::Write;

use serde_json::Value;

fn s(v: &Value) -> String {
    match v {
        Value::Null => "–".into(),
        Value::String(t) => t.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

pub fn pretty(v: &Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(v).unwrap_or_default())
}

pub fn init(v: &Value) -> String {
    format!(
        "Initialized MirrorSelf at {}\n  storage:   {}\n  vault key: {} (id {})\n  API token: {}\n\n\
         Back up the vault key somewhere safe: without it the vault cannot be decrypted.\n\
         Next: mirrorself import apple-health ~/Downloads/export.zip\n",
        s(&v["home"]),
        s(&v["storage"]),
        s(&v["key_file"]),
        s(&v["key_id"]),
        s(&v["token_file"])
    )
}

pub fn synth(v: &Value) -> String {
    format!(
        "Wrote {} ({} days, {} records, {} bytes of synthetic data)\n",
        s(&v["path"]),
        s(&v["days"]),
        s(&v["records"]),
        s(&v["bytes"])
    )
}

pub fn import(v: &Value) -> String {
    let mut out = if v["created"].as_bool() == Some(true) {
        format!("Stored raw object {} ({} bytes)\n", s(&v["raw_id"]), s(&v["bytes"]))
    } else {
        format!("Already in the vault: {} (nothing new stored)\n", s(&v["raw_id"]))
    };
    if !v["process"].is_null() {
        out.push_str(&process(&v["process"]));
    }
    out
}

pub fn process(v: &Value) -> String {
    let mut out = String::new();
    if v["rebuilt"].as_bool() == Some(true) {
        out.push_str("Rebuilt the index from the vault.\n");
    }
    let registered = v["registered_from_vault"].as_u64().unwrap_or(0);
    if registered > 0 {
        let _ = writeln!(out, "Registered {registered} raw object(s) found in the vault.");
    }
    let runs = v["runs"].as_array().cloned().unwrap_or_default();
    if runs.is_empty() {
        out.push_str("Nothing to process; the index is up to date.\n");
    }
    for r in runs {
        let target = r["raw_id"].as_str().map(|id| format!(" {}", &id[..12.min(id.len())])).unwrap_or_default();
        let _ = write!(out, "  {:<24} {:<6}{} {:>6} ms", s(&r["processor"]), s(&r["status"]), target, s(&r["elapsed_ms"]));
        if let Some(e) = r["error"].as_str() {
            let _ = write!(out, "  {e}");
        } else if let Some(o) = r["outputs"].as_object() {
            let summary: Vec<String> = o
                .iter()
                .filter(|(_, v)| v.is_number())
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            let _ = write!(out, "  {}", summary.join(" "));
        }
        out.push('\n');
    }
    let _ = writeln!(out, "Done in {} ms.", s(&v["elapsed_ms"]));
    out
}

pub fn overview(v: &Value) -> String {
    let mut out = String::new();
    let raw = &v["raw_objects"];
    let _ = writeln!(out, "Raw objects: {} (last import {})", s(&raw["count"]), s(&raw["last_received"]));
    out.push_str("\nCoverage\n");
    for c in v["coverage"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "  {:<28} {:<10} {} → {}  {:>5} days {:>8} samples",
            s(&c["code"]),
            s(&c["category"]),
            s(&c["first_date"]),
            s(&c["last_date"]),
            s(&c["days_with_data"]),
            s(&c["samples"])
        );
    }
    out.push_str("\nSources\n");
    for src in v["sources"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "  {:<24} {:<6} {:>8} samples ({} duplicates skipped)",
            s(&src["source"]),
            s(&src["kind"]),
            s(&src["samples"]),
            s(&src["duplicates_skipped"])
        );
    }
    out.push_str("\nEvents\n");
    for e in v["events"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "  {:<10} {:>6}  {} → {}", s(&e["type"]), s(&e["count"]), s(&e["first_date"]), s(&e["last_date"]));
    }
    out.push_str("\nLatest\n");
    for l in v["latest"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "  {:<30} {:>9} {:<12} on {}  (28-day baseline {}, z {})",
            s(&l["display"]),
            s(&l["value"]),
            s(&l["unit"]),
            s(&l["date"]),
            s(&l["baseline_28d"]),
            s(&l["z"])
        );
    }
    out
}

pub fn timeline(v: &Value) -> String {
    let mut out = String::new();
    for i in v["items"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "{}  {:<16} {}  [{}]", s(&i["local_date"]), s(&i["type"]), s(&i["title"]), s(&i["id"]));
    }
    if out.is_empty() {
        out.push_str("No events in this range.\n");
    }
    out
}

pub fn series(v: &Value) -> String {
    let mut out = format!(
        "{} ({}, {} {}), {} → {}\n",
        s(&v["display"]),
        s(&v["metric"]),
        s(&v["stat"]),
        s(&v["unit"]),
        s(&v["from"]),
        s(&v["to"])
    );
    for p in v["points"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "  {}  {:>10}   baseline {:>8}   z {:>6}",
            s(&p["period"]),
            s(&p["value"]),
            s(&p["baseline_28d"]),
            s(&p["z"])
        );
    }
    let sm = &v["summary"];
    if sm.is_null() {
        out.push_str("No data in this range.\n");
    } else {
        let _ = writeln!(
            out,
            "\n{} days, mean {} (sd {}); first 7 days {} → last 7 days {} (change {}); slope {} per 30 days",
            s(&sm["n_days"]),
            s(&sm["mean"]),
            s(&sm["sd"]),
            s(&sm["first_7_days_mean"]),
            s(&sm["last_7_days_mean"]),
            s(&sm["change_first_to_last_7_days"]),
            s(&sm["slope_per_30_days"])
        );
    }
    out
}

pub fn compare(v: &Value) -> String {
    let (a, b) = (&v["period_a"], &v["period_b"]);
    format!(
        "{} ({}, {})\n  A {} → {}: mean {} over {} days\n  B {} → {}: mean {} over {} days\n  \
         change {} ({}%), effect size {} ({})\n{}\n",
        s(&v["display"]),
        s(&v["stat"]),
        s(&v["unit"]),
        s(&a["from"]),
        s(&a["to"]),
        s(&a["mean"]),
        s(&a["n_days"]),
        s(&b["from"]),
        s(&b["to"]),
        s(&b["mean"]),
        s(&b["n_days"]),
        s(&v["delta"]),
        s(&v["delta_pct"]),
        s(&v["effect_size"]),
        s(&v["effect_size_label"]),
        s(&v["note"])
    )
}

pub fn search(v: &Value) -> String {
    let mut out = String::new();
    for r in v["results"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "{:<7} {:<12} {}  [{}]", s(&r["kind"]), s(&r["local_date"]), s(&r["title"]), s(&r["id"]));
    }
    if out.is_empty() {
        out.push_str("No matches.\n");
    }
    out
}

pub fn sql(v: &Value) -> String {
    let mut out = String::new();
    let cols: Vec<String> = v["columns"].as_array().into_iter().flatten().map(s).collect();
    let _ = writeln!(out, "{}", cols.join("\t"));
    for row in v["rows"].as_array().into_iter().flatten() {
        let cells: Vec<String> = row.as_array().into_iter().flatten().map(s).collect();
        let _ = writeln!(out, "{}", cells.join("\t"));
    }
    if v["truncated"].as_bool() == Some(true) {
        let _ = writeln!(out, "(truncated at {} rows)", s(&v["row_cap"]));
    }
    out
}

pub fn codes(v: &Value) -> String {
    let mut out = String::new();
    for c in v["items"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "{:<30} {:<32} {:<12} {:<10} {:>5} days",
            s(&c["code"]),
            s(&c["display"]),
            s(&c["unit"]),
            s(&c["category"]),
            s(&c["days_with_data"])
        );
    }
    out
}

pub fn audit(v: &Value) -> String {
    let mut out = String::new();
    for a in v["items"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "{}  {:<10} {:<20} {:<18} {:<16} {:<6} {:>6} rows",
            s(&a["ts"]),
            s(&a["actor"]),
            s(&a["client_id"]),
            s(&a["action"]),
            s(&a["scope"]),
            s(&a["outcome"]),
            s(&a["rows"])
        );
    }
    if out.is_empty() {
        out.push_str("No access recorded yet.\n");
    }
    out
}

pub fn raw_list(v: &Value) -> String {
    let mut out = String::new();
    for r in v["items"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "{}  {:<20} {:>12} bytes  {}  {}",
            s(&r["id"]),
            s(&r["kind"]),
            s(&r["bytes"]),
            s(&r["received_at"]),
            s(&r["name"])
        );
    }
    if out.is_empty() {
        out.push_str("The vault is empty.\n");
    }
    out
}
