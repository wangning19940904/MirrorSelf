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
use mirrorself_core::query::{CompareQuery, LabQuery, Period, SearchQuery, SeriesQuery, TimelineQuery};
use mirrorself_core::synth::{self, SynthOptions};
use mirrorself_core::{App, ImportKind};
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
    /// Lab results from checkup reports (available from M2)
    Labs {
        #[arg(long)]
        analyte: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
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
        Command::Labs { analyte, from, to } => out(q.lab_results(&LabQuery { analyte, from, to })?, render::pretty),
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
