//! The read tools shared by MCP and REST, with their scopes and input schemas.

use anyhow::Result;
use mirrorself_core::audit;
use mirrorself_core::error::{invalid, not_found};
use mirrorself_core::query::{CompareQuery, LabQuery, SearchQuery, SeriesQuery, TimelineQuery};
use mirrorself_core::App;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

pub struct Tool {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub scope: &'static str,
    pub input_schema: fn() -> Value,
}

const DATE_HINT: &str = "YYYY-MM-DD, 'today', or a look-back like '90d', '12w', '6m', '1y'";

fn date_prop(what: &str) -> Value {
    json!({ "type": "string", "description": format!("{what} local date, inclusive: {DATE_HINT}") })
}

pub const TOOLS: &[Tool] = &[
    Tool {
        name: "get_overview",
        title: "Data overview",
        description: "What health data exists: coverage by metric and date, sources (watch, phone, apps), event counts, and the latest value with its 28-day baseline for key metrics. Call this first.",
        scope: "health:summary",
        input_schema: || json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    },
    Tool {
        name: "query_timeline",
        title: "Timeline",
        description: "Events (workouts, sleep sessions) and notable daily deviations (|z| >= 2 vs the 28-day baseline), newest first, with record ids and citations.",
        scope: "health:read",
        input_schema: || json!({
            "type": "object",
            "properties": {
                "from": date_prop("Start"),
                "to": date_prop("End"),
                "types": { "type": "array", "items": { "type": "string" },
                           "description": "Event types (workout, sleep, metric_deviation) or categories (sleep, activity, cardio, metabolic)" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100 },
                "offset": { "type": "integer", "minimum": 0, "default": 0 }
            },
            "additionalProperties": false
        }),
    },
    Tool {
        name: "get_metric_series",
        title: "Metric series",
        description: "Daily, weekly or monthly values of one metric with the 28-day personal baseline, z-scores, a summary (mean, first/last 7-day means, slope per 30 days) and the record ids behind each day. Metrics: ms:resting_hr, ms:hrv_sdnn, ms:sleep_asleep_total, ms:steps, ms:body_mass, ms:heart_rate, ms:vo2max, ms:spo2 and more (see get_overview or mirrorself://schema).",
        scope: "health:read",
        input_schema: || json!({
            "type": "object",
            "properties": {
                "metric": { "type": "string", "description": "Metric code, e.g. ms:resting_hr (the ms: prefix is optional)" },
                "from": date_prop("Start"),
                "to": date_prop("End"),
                "bucket": { "type": "string", "enum": ["day", "week", "month"], "default": "day" },
                "stat": { "type": "string", "enum": ["sum", "mean", "min", "max"], "description": "Defaults to the metric's daily statistic" }
            },
            "required": ["metric"],
            "additionalProperties": false
        }),
    },
    Tool {
        name: "compare_periods",
        title: "Compare periods",
        description: "Compares one metric's daily values between two periods: means, SD, difference, percent change and effect size (Cohen's d). Descriptive, not diagnostic.",
        scope: "health:read",
        input_schema: || {
            let period = json!({
                "type": "object",
                "properties": { "from": date_prop("Start"), "to": date_prop("End") },
                "required": ["from", "to"]
            });
            json!({
                "type": "object",
                "properties": {
                    "metric": { "type": "string" },
                    "period_a": period.clone(),
                    "period_b": period,
                    "stat": { "type": "string", "enum": ["sum", "mean", "min", "max"] }
                },
                "required": ["metric", "period_a", "period_b"],
                "additionalProperties": false
            })
        },
    },
    Tool {
        name: "get_lab_results",
        title: "Lab results",
        description: "Lab analytes from checkup reports with units, the report's printed reference range and flag, and source document ids. Empty until checkup-report import (M2).",
        scope: "health:labs",
        input_schema: || json!({
            "type": "object",
            "properties": {
                "analyte": { "type": "string", "description": "LOINC code or analyte name" },
                "from": date_prop("Start"),
                "to": date_prop("End")
            },
            "additionalProperties": false
        }),
    },
    Tool {
        name: "search_records",
        title: "Search records",
        description: "Full-text search over events, entities (devices, activity types) and metric names, ranked, with snippets.",
        scope: "health:read",
        input_schema: || json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "from": date_prop("Start"),
                "to": date_prop("End"),
                "kinds": { "type": "array", "items": { "type": "string", "enum": ["event", "entity", "metric"] } },
                "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 20 }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    },
    Tool {
        name: "get_entity",
        title: "Entity",
        description: "An entity (device, app, activity type) by name or id, with the records linked to it.",
        scope: "health:read",
        input_schema: || json!({
            "type": "object",
            "properties": { "name": { "type": "string", "description": "Entity name or id" } },
            "required": ["name"],
            "additionalProperties": false
        }),
    },
    Tool {
        name: "get_document",
        title: "Document",
        description: "Document metadata and, optionally, extracted text (returned as untrusted data). Available from M2.",
        scope: "health:documents",
        input_schema: || json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "include_text": { "type": "boolean", "default": false }
            },
            "required": ["id"],
            "additionalProperties": false
        }),
    },
    Tool {
        name: "run_sql",
        title: "Read-only SQL",
        description: "One read-only SELECT over the v_* views (v_timeline, v_metrics_daily, v_labs, v_documents, v_entities, v_links, v_codes); 1000-row cap, 10 s limit. See the mirrorself://schema resource.",
        scope: "health:sql",
        input_schema: || json!({
            "type": "object",
            "properties": { "sql": { "type": "string" } },
            "required": ["sql"],
            "additionalProperties": false
        }),
    },
];

pub fn find(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|t| t.name == name)
}

fn args<T: DeserializeOwned>(tool: &str, v: &Value) -> Result<T> {
    let v = if v.is_null() { json!({}) } else { v.clone() };
    serde_json::from_value(v).map_err(|e| invalid(format!("bad arguments for {tool}: {e}")))
}

/// Runs a tool and records the attempt, whatever its outcome, in the audit log.
pub fn call(app: &App, actor: &str, client_id: &str, name: &str, arguments: &Value) -> Result<Value> {
    let tool = find(name).ok_or_else(|| not_found(format!("no tool named '{name}'")))?;
    let result = dispatch(app, tool, arguments);
    let request = json!({ "tool": tool.name, "arguments": arguments });
    let entry = audit::Entry { actor, client_id, action: tool.name, scope: tool.scope, request: &request };
    audit::record(app.conn(), &entry, &result)?;
    result
}

fn dispatch(app: &App, tool: &Tool, arguments: &Value) -> Result<Value> {
    let q = app.query();
    let name = tool.name;
    match name {
        "get_overview" => q.overview(),
        "query_timeline" => q.timeline(&args::<TimelineQuery>(name, arguments)?),
        "get_metric_series" => q.metric_series(&args::<SeriesQuery>(name, arguments)?),
        "compare_periods" => q.compare_periods(&args::<CompareQuery>(name, arguments)?),
        "get_lab_results" => q.lab_results(&args::<LabQuery>(name, arguments)?),
        "search_records" => q.search(&args::<SearchQuery>(name, arguments)?),
        "get_entity" => {
            let name_arg = arguments["name"]
                .as_str()
                .ok_or_else(|| invalid("get_entity needs a 'name'"))?;
            q.entity(name_arg)
        }
        "get_document" => {
            let id = arguments["id"].as_str().ok_or_else(|| invalid("get_document needs an 'id'"))?;
            q.document(id, arguments["include_text"].as_bool().unwrap_or(false))
        }
        "run_sql" => {
            let sql = arguments["sql"].as_str().ok_or_else(|| invalid("run_sql needs 'sql'"))?;
            q.run_sql(sql)
        }
        other => unreachable!("tool {other} is listed but not dispatched"),
    }
}
