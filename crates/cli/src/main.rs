//! `mirrorself`: import, process and query a MirrorSelf home; serve REST and MCP.
//!
//! Every read command accepts `--json`. Exit codes: 0 ok, 1 error, 2 bad
//! usage or input, 3 not initialized, 4 not found, 5 conflict (e.g. port in
//! use), 6 denied.

mod render;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mirrorself_core::config::Home;
use mirrorself_core::error::{invalid, user_error, UserError};
use mirrorself_core::config::ModelsConfig;
use mirrorself_core::models::{self, recorded::Recording};
use mirrorself_core::ops::Edit;
use mirrorself_core::query::{CompareQuery, LabQuery, Period, ReviewQuery, SearchQuery, SeriesQuery, TimelineQuery};
use mirrorself_core::synth::{self, SynthOptions};
use mirrorself_core::{eval, synth_report, App, Decision, ImportKind, Lang};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(name = "mirrorself", version, about = "MirrorSelf: a user-owned personal data layer")]
struct Cli {
    /// MirrorSelf home directory (default: $MIRRORSELF_HOME or ~/.mirrorself)
    #[arg(long, global = true, env = "MIRRORSELF_HOME")]
    home: Option<PathBuf>,
    /// Print machine-readable JSON
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a home: vault key, API token, config, encrypted vault and index
    Init {
        #[arg(long, default_value = "server")]
        mode: String,
        /// Storage URI for raw objects, e.g. local:/data/mirrorself (default: <home>/vault)
        #[arg(long)]
        storage: Option<String>,
    },
    /// Store a file in the vault and process it
    #[command(subcommand)]
    Import(ImportCmd),
    /// Run pending processors; --rebuild re-derives everything from the vault
    Process {
        #[arg(long)]
        rebuild: bool,
    },
    /// Data coverage, sources and latest key values
    Overview,
    /// Events and notable deviations, newest first
    Timeline {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        /// Comma-separated event types or categories (workout, sleep, metric_deviation, cardio, …)
        #[arg(long, value_delimiter = ',')]
        types: Vec<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// One metric over time with its 28-day baseline (e.g. ms:resting_hr)
    Metrics {
        metric: String,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long, default_value = "day")]
        bucket: String,
        #[arg(long)]
        stat: Option<String>,
    },
    /// Compare a metric between two periods (FROM..TO each)
    Compare {
        metric: String,
        #[arg(long)]
        a: String,
        #[arg(long)]
        b: String,
        #[arg(long)]
        stat: Option<String>,
    },
    /// Lab results from checkup reports; --compare lines them up across reports
    Labs {
        /// LOINC code (13457-7, LOINC:13457-7) or name (LDL-C, 低密度脂蛋白胆固醇)
        #[arg(long)]
        analyte: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        /// Checkup comparison: each analyte across reports with each report's own range and flag
        #[arg(long)]
        compare: bool,
    },
    /// Extracted lab values waiting for review; confirm, edit or reject them
    #[command(subcommand)]
    Review(ReviewCmd),
    /// Imported documents (checkup reports)
    Documents,
    /// One document with its pages and lab values
    Document {
        id: String,
        /// Include the extracted text (untrusted document content)
        #[arg(long)]
        text: bool,
    },
    /// Full-text search over events, entities and metric names
    Search {
        query: String,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// An entity (device, activity type) and its linked records
    Entity { name: String },
    /// Read-only SQL over the v_* views
    Sql { sql: String },
    /// Metric codes and how much data each has
    Codes,
    /// Recent agent and API access
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Raw objects in the vault
    #[command(subcommand)]
    Raw(RawCmd),
    /// MCP server
    #[command(subcommand)]
    Mcp(McpCmd),
    /// REST API on loopback
    Serve {
        /// Listen address (default from mirrorself.toml, normally 127.0.0.1:12500)
        #[arg(long)]
        addr: Option<String>,
    },
    /// Developer tools
    #[command(subcommand, hide = true)]
    Dev(DevCmd),
}

#[derive(Subcommand)]
enum ImportCmd {
    /// Apple Health export.zip (or export.xml) from Health → Export All Health Data
    AppleHealth {
        path: PathBuf,
        /// Only store it; run `mirrorself process` later
        #[arg(long)]
        no_process: bool,
    },
    /// A checkup report or lab sheet: PDF or photo (JPEG, PNG, HEIC)
    Report {
        path: PathBuf,
        /// The report's language, a hint for OCR
        #[arg(long, value_parser = ["zh", "en"])]
        lang: Option<String>,
        /// Only store it; run `mirrorself process` later
        #[arg(long)]
        no_process: bool,
    },
}

#[derive(Subcommand)]
enum ReviewCmd {
    /// Values waiting for review, with the reason and the page they came from
    List {
        /// Also list unverified values that passed every check
        #[arg(long)]
        all: bool,
        /// Only this document
        #[arg(long)]
        document: Option<String>,
    },
    /// Confirm values as read
    Confirm {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Correct a value, then confirm it
    Edit {
        id: String,
        #[arg(long)]
        value: Option<f64>,
        /// A qualitative result (阴性, negative) or a censored one (<0.10)
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        unit: Option<String>,
        #[arg(long)]
        loinc: Option<String>,
        #[arg(long)]
        ref_low: Option<f64>,
        #[arg(long)]
        ref_high: Option<f64>,
        /// The flag as printed (↑, H, …)
        #[arg(long)]
        flag: Option<String>,
        /// The report date, YYYY-MM-DD
        #[arg(long)]
        date: Option<String>,
    },
    /// Reject values that are not real results
    Reject {
        #[arg(required = true)]
        ids: Vec<String>,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Subcommand)]
enum RawCmd {
    /// List raw objects
    List,
    /// Write a raw object's original bytes to a file
    Get {
        id: String,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum McpCmd {
    /// Serve MCP (read-only tools) to a local agent
    Serve {
        /// Speak MCP over stdin/stdout (the only transport in this build)
        #[arg(long)]
        stdio: bool,
    },
}

#[derive(Subcommand)]
enum DevCmd {
    /// Write a synthetic Apple Health export.zip (fake data, deterministic)
    SynthAppleHealth {
        #[arg(long, default_value_t = 1)]
        years: u32,
        /// Use this many days instead of --years
        #[arg(long)]
        days: Option<u32>,
        /// Last day of data (default: today)
        #[arg(long)]
        end: Option<String>,
        #[arg(long, default_value_t = 7)]
        seed: u64,
        #[arg(long)]
        out: PathBuf,
    },
    /// Write synthetic checkup-report specs (render them with tools/report-fixtures)
    SynthReports {
        #[arg(long, default_value_t = 12)]
        count: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long)]
        out: PathBuf,
    },
    /// OCR files with this host's provider; --record saves <file>.ocr.json next to each
    Ocr {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[arg(long, value_parser = ["zh", "en"])]
        lang: Option<String>,
        #[arg(long)]
        record: bool,
    },
    /// Score extraction against report specs in a directory
    EvalReports {
        dir: PathBuf,
        /// OCR the rendered files now instead of replaying recordings
        #[arg(long)]
        live: bool,
        #[arg(long, default_value_t = 0.9)]
        threshold: f64,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(match user_error(&err) {
                Some(UserError::Invalid(_)) => 2,
                Some(UserError::NotInitialized(_)) => 3,
                Some(UserError::NotFound(_)) => 4,
                Some(UserError::Conflict(_)) => 5,
                Some(UserError::Denied(_)) => 6,
                None => 1,
            })
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let home = Home::resolve(cli.home)?;
    let json_out = cli.json;
    let out = |v: Value, human: fn(&Value) -> String| {
        if json_out {
            println!("{}", serde_json::to_string_pretty(&v).expect("JSON values serialize"));
        } else {
            print!("{}", human(&v));
        }
    };

    match cli.command {
        Command::Init { mode, storage } => {
            let report = App::init(&home, &mode, storage.as_deref())?;
            out(serde_json::to_value(&report)?, render::init);
        }
        Command::Dev(DevCmd::SynthAppleHealth { years, days, end, seed, out: path }) => {
            let end = match end {
                Some(s) => mirrorself_core::time::parse_date_arg(&s, mirrorself_core::time::today_local())?,
                None => mirrorself_core::time::today_local(),
            };
            let mut opts = match days {
                Some(d) => SynthOptions::days(d, end),
                None => SynthOptions::years(years, end),
            };
            opts.seed = seed;
            let (bytes, records) = synth::export_zip(&opts)?;
            std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
            out(
                json!({ "path": path.display().to_string(), "days": opts.days, "records": records, "bytes": bytes.len() }),
                render::synth,
            );
        }
        Command::Dev(DevCmd::SynthReports { count, seed, out: dir }) => {
            std::fs::create_dir_all(&dir)?;
            let specs = synth_report::eval_specs(count, seed);
            for spec in &specs {
                let path = dir.join(format!("{}.json", spec.id));
                std::fs::write(&path, serde_json::to_string_pretty(spec)? + "\n")?;
            }
            out(json!({ "dir": dir.display().to_string(), "reports": specs.len() }), render::pretty);
        }
        Command::Dev(DevCmd::Ocr { files, lang, record }) => {
            let provider = models::from_config(&ModelsConfig::default())?;
            let lang = lang.as_deref().map(Lang::parse).transpose()?;
            let mut done = Vec::new();
            for path in files {
                let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                let mime = mirrorself_core::importers::report::sniff(&bytes)
                    .ok_or_else(|| invalid(format!("{} is not a supported report file", path.display())))?;
                let ocr = provider.ocr(&models::DocumentInput { mime, bytes: &bytes, lang: lang.map(Lang::as_str) })?;
                let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if record {
                    let rec = Recording { sha256: mirrorself_core::crypto::sha256_hex(&bytes), file: file.clone(), ocr: ocr.clone() };
                    let target = path.with_file_name(format!("{file}.ocr.json"));
                    std::fs::write(&target, serde_json::to_string(&rec)? + "\n")?;
                }
                let x = provider.extract(&ocr, lang.map(Lang::as_str))?;
                done.push(json!({ "file": file, "pages": ocr.pages.len(), "extraction": x }));
            }
            out(json!({ "files": done }), render::pretty);
        }
        Command::Dev(DevCmd::EvalReports { dir, live, threshold }) => {
            let provider = if live { Some(models::from_config(&ModelsConfig::default())?) } else { None };
            let (scores, summary) = eval::run_dir(&dir, provider.as_deref(), threshold)?;
            out(json!({ "summary": summary, "documents": scores }), render::eval);
        }
        Command::Serve { addr } => {
            let app = App::open(home)?;
            let addr = addr.unwrap_or_else(|| app.config().server.addr.clone());
            let addr: SocketAddr = addr
                .parse()
                .map_err(|e| invalid(format!("bad listen address '{addr}': {e}")))?;
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            runtime.block_on(mirrorself_server::rest::serve(app, addr))?;
        }
        command => {
            let app = App::open(home)?;
            run_with_app(&app, command, &out)?;
        }
    }
    Ok(())
}

fn run_with_app(app: &App, command: Command, out: &dyn Fn(Value, fn(&Value) -> String)) -> Result<()> {
    let q = app.query();
    match command {
        Command::Import(ImportCmd::AppleHealth { path, no_process }) => {
            let report = app.import_file(ImportKind::AppleHealth, &path, !no_process)?;
            out(serde_json::to_value(&report)?, render::import);
        }
        Command::Import(ImportCmd::Report { path, lang, no_process }) => {
            let lang = lang.as_deref().map(Lang::parse).transpose()?;
            let report = app.import_file(ImportKind::Report(lang), &path, !no_process)?;
            let failed: Vec<String> = report
                .process
                .iter()
                .flat_map(|p| p.errors())
                .filter(|r| r.raw_id.as_deref() == Some(report.raw_id.as_str()))
                .filter_map(|r| r.error.clone())
                .collect();
            let mut v = serde_json::to_value(&report)?;
            let doc = mirrorself_core::pipeline::report::document_id(&report.raw_id);
            if let Ok(d) = q.document(&doc, false) {
                v["document"] = d;
            }
            out(v, render::import_report);
            if let Some(e) = failed.first() {
                return Err(anyhow::anyhow!("the report was stored but could not be read: {e}"));
            }
        }
        Command::Process { rebuild } => {
            let report = app.process(rebuild)?;
            out(serde_json::to_value(&report)?, render::process);
        }
        Command::Overview => out(q.overview()?, render::overview),
        Command::Timeline { from, to, types, limit } => {
            let v = q.timeline(&TimelineQuery { from, to, types, limit: Some(limit), offset: None })?;
            out(v, render::timeline);
        }
        Command::Metrics { metric, from, to, bucket, stat } => {
            let v = q.metric_series(&SeriesQuery { metric, from, to, bucket: Some(bucket), stat })?;
            out(v, render::series);
        }
        Command::Compare { metric, a, b, stat } => {
            let v = q.compare_periods(&CompareQuery {
                metric,
                period_a: Period::Text(a),
                period_b: Period::Text(b),
                stat,
            })?;
            out(v, render::compare);
        }
        Command::Labs { analyte, from, to, compare: true } => {
            if from.is_some() || to.is_some() {
                return Err(invalid("--compare spans every report; drop --from and --to"));
            }
            out(q.lab_comparison(analyte.as_deref())?, render::lab_comparison)
        }
        Command::Labs { analyte, from, to, compare: false } => out(q.lab_results(&LabQuery { analyte, from, to })?, render::labs),
        Command::Review(ReviewCmd::List { all, document }) => {
            out(q.review_queue(&ReviewQuery { all, document, limit: None })?, render::review_list)
        }
        Command::Review(ReviewCmd::Confirm { ids }) => {
            let items = ids.iter().map(|id| app.review(id, Decision::Confirm, "cli")).collect::<Result<Vec<_>>>()?;
            out(json!({ "decision": "confirmed", "items": items }), render::review_done);
        }
        Command::Review(ReviewCmd::Edit { id, value, text, unit, loinc, ref_low, ref_high, flag, date }) => {
            let edit = Edit { value, value_text: text, unit, loinc, ref_low, ref_high, flag, date };
            let item = app.review(&id, Decision::Edit(edit), "cli")?;
            out(json!({ "decision": "edited", "items": [item] }), render::review_done);
        }
        Command::Review(ReviewCmd::Reject { ids, reason }) => {
            let items = ids
                .iter()
                .map(|id| app.review(id, Decision::Reject { reason: reason.clone() }, "cli"))
                .collect::<Result<Vec<_>>>()?;
            out(json!({ "decision": "rejected", "items": items }), render::review_done);
        }
        Command::Documents => out(q.documents()?, render::documents),
        Command::Document { id, text } => out(q.document(&id, text)?, render::pretty),
        Command::Search { query, from, to, kinds, limit } => {
            let v = q.search(&SearchQuery { query, from, to, kinds, limit: Some(limit) })?;
            out(v, render::search);
        }
        Command::Entity { name } => out(q.entity(&name)?, render::pretty),
        Command::Sql { sql } => out(q.run_sql(&sql)?, render::sql),
        Command::Codes => out(q.codes()?, render::codes),
        Command::Audit { limit } => out(q.audit(Some(limit))?, render::audit),
        Command::Raw(RawCmd::List) => out(app.raw_list()?, render::raw_list),
        Command::Raw(RawCmd::Get { id, out: path }) => {
            let (meta, bytes) = app.raw_original(&id)?;
            std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
            out(
                json!({ "id": id, "name": meta.name, "bytes": bytes.len(), "path": path.display().to_string() }),
                render::pretty,
            );
        }
        Command::Mcp(McpCmd::Serve { stdio }) => {
            if !stdio {
                return Err(invalid(
                    "pass --stdio (remote MCP over Streamable HTTP with OAuth arrives in milestone M4)",
                ));
            }
            mirrorself_server::mcp::serve_stdio(app)?;
        }
        Command::Init { .. } | Command::Dev(_) | Command::Serve { .. } => {
            unreachable!("handled in run()")
        }
    }
    Ok(())
}
