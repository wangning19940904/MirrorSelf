//! `[[sync]]` recipes: how they are written in `mirrorself.toml`, how they
//! are checked, and how a window is rendered into a call.
//!
//! A recipe is checked when the config is parsed, so a bad recipe stops
//! `mirrorself` with an error that names the recipe and the field.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Raw kinds the generic tier parses.
pub const KIND_MCP_RESPONSE: &str = "mcp_response";
pub const KIND_CLI_RESPONSE: &str = "cli_response";

/// Placeholders a recipe may use in its arguments.
pub const PLACEHOLDERS: &[&str] = &["since", "until", "since_unix", "until_unix", "since_date", "until_date"];

pub const DEFAULT_CURSOR_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Upper bound on `max_bytes`: responses are kept in memory.
pub const MAX_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// One `[[sync]]` table in `mirrorself.toml`, as written.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeDef {
    pub name: String,
    /// `mcp` or `cli`.
    pub adapter: String,
    /// `raw_objects.source` for what this recipe stores, e.g. `github`.
    pub source: String,
    /// `raw_objects.kind`: `mcp_response`, `cli_response`, or a typed kind
    /// such as `feishu_calendar_events`.
    pub kind: String,
    /// How often the recipe is due, e.g. `1h`.
    pub interval: String,
    /// How far each window reaches back before the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap: Option<String>,
    /// The first window, when there is no cursor yet. Default `7d`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookback: Option<String>,
    /// strftime format for `{{since}}` and `{{until}}`. Default RFC 3339 UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_format: Option<String>,
    /// UTC offset the window is rendered in, and the local day of records
    /// whose timestamps carry no offset of their own. Default `+00:00`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utc_offset: Option<String>,
    /// Per-call timeout, e.g. `60s`. Default 120 s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// Largest response kept, in bytes. Default 16 MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<CliDef>,
    /// How the generic tier turns responses into `items`.
    #[serde(default, skip_serializing_if = "ItemRules::is_default")]
    pub items: ItemRules,
    /// Options for the typed GitHub mapping (`source = "github"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<GithubDef>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpDef {
    /// Streamable HTTP endpoint. Exactly one of `url` and `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Name of a stored secret sent as `Authorization: Bearer …`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_secret: Option<String>,
    /// Extra HTTP headers. Values may reference secrets: `${secret:name}`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// A stdio server: the program, run without a shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment for the stdio server. Values may reference secrets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The one tool this recipe calls.
    pub tool: String,
    /// Tool arguments. String values may contain window placeholders.
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub arguments: toml::Table,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliDef {
    /// The program, run directly (no shell). A bare name is looked up on PATH.
    pub program: String,
    /// Arguments; may contain window placeholders, never secrets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment. Values may reference secrets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// Simple extraction rules for the generic tier. Paths are dot-separated
/// object keys (`commit.author.date`); an empty path is the value itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemRules {
    /// `items.subtype`, e.g. `github_commit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    /// `items.category_id`, e.g. `work`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Where the array of records is (e.g. `items` for a search result).
    /// Without it, a top-level array is split into records and anything
    /// else is one record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub records: Option<String>,
    /// Field that identifies a record across responses. Overlapping windows
    /// then keep one item per record (the newest response wins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_field: Option<String>,
    /// When the record happened: RFC 3339, `YYYY-MM-DD` or Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_field: Option<String>,
    /// The source's own last-modified time → `items.source_updated_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_field: Option<String>,
}

impl ItemRules {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubDef {
    /// Keep only commits and pull requests authored by this login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    Mcp,
    Cli,
}

/// A checked recipe. Built only through [`Recipe::from_def`], so the parsed
/// fields are always consistent with the definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RecipeDef", into = "RecipeDef")]
pub struct Recipe {
    pub name: String,
    pub def: RecipeDef,
    pub adapter: AdapterKind,
    pub interval_secs: i64,
    pub overlap_secs: i64,
    pub lookback_secs: i64,
    pub timeout_secs: u64,
    pub max_bytes: u64,
    pub cursor_format: String,
    pub utc_offset: FixedOffset,
}

impl TryFrom<RecipeDef> for Recipe {
    type Error = String;
    fn try_from(def: RecipeDef) -> Result<Self, String> {
        Recipe::from_def(def).map_err(|e| format!("{e:#}"))
    }
}

impl From<Recipe> for RecipeDef {
    fn from(r: Recipe) -> Self {
        r.def
    }
}

impl Recipe {
    pub fn from_def(def: RecipeDef) -> Result<Self> {
        let label = if def.name.is_empty() { "[[sync]]".to_string() } else { format!("sync recipe '{}'", def.name) };
        check(&def).map_err(|e| anyhow::anyhow!("{label}: {e}"))
    }

    /// The window a run at `now` covers, given the last successful cursor.
    pub fn window(&self, cursor: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Window {
        let since = match cursor {
            Some(c) => c - chrono::Duration::seconds(self.overlap_secs),
            None => now - chrono::Duration::seconds(self.lookback_secs),
        };
        Window { since: since.min(now), until: now }
    }

    /// Placeholder values for a window, rendered as the recipe asks.
    pub fn placeholders(&self, w: &Window) -> BTreeMap<&'static str, String> {
        let since = w.since.with_timezone(&self.utc_offset);
        let until = w.until.with_timezone(&self.utc_offset);
        BTreeMap::from([
            ("since", since.format(&self.cursor_format).to_string()),
            ("until", until.format(&self.cursor_format).to_string()),
            ("since_unix", w.since.timestamp().to_string()),
            ("until_unix", w.until.timestamp().to_string()),
            ("since_date", since.format("%Y-%m-%d").to_string()),
            ("until_date", until.format("%Y-%m-%d").to_string()),
        ])
    }

    /// Names of the secrets this recipe needs.
    pub fn secret_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut add = |s: &str| {
            for n in secret_refs(s) {
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        };
        if let Some(m) = &self.def.mcp {
            if let Some(b) = &m.bearer_secret {
                add(&format!("${{secret:{b}}}"));
            }
            m.headers.values().for_each(|v| add(v));
            m.env.values().for_each(|v| add(v));
        }
        if let Some(c) = &self.def.cli {
            c.env.values().for_each(|v| add(v));
        }
        names
    }
}

/// The time span one run asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
}

impl Window {
    /// `since..until` in UTC, for `origin_ref` and the run history.
    pub fn label(&self) -> String {
        format!("{}..{}", rfc3339(&self.since), rfc3339(&self.until))
    }
}

pub fn rfc3339(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn check(def: &RecipeDef) -> Result<Recipe> {
    check_ident("name", &def.name, true)?;
    check_ident("source", &def.source, false)?;
    check_ident("kind", &def.kind, false)?;
    let adapter = match def.adapter.as_str() {
        "mcp" => AdapterKind::Mcp,
        "cli" => AdapterKind::Cli,
        other => bail!("adapter must be \"mcp\" or \"cli\", not \"{other}\""),
    };
    match (adapter, &def.mcp, &def.cli) {
        (AdapterKind::Mcp, Some(m), None) => check_mcp(m)?,
        (AdapterKind::Cli, None, Some(c)) => check_cli(c)?,
        (AdapterKind::Mcp, _, _) => bail!("adapter \"mcp\" needs a [sync.mcp] table and no [sync.cli] table"),
        (AdapterKind::Cli, _, _) => bail!("adapter \"cli\" needs a [sync.cli] table and no [sync.mcp] table"),
    }
    let interval_secs = parse_duration("interval", &def.interval)?;
    if interval_secs < 60 {
        bail!("interval must be at least 1m");
    }
    let overlap_secs = match &def.overlap {
        Some(s) => parse_duration_allow_zero("overlap", s)?,
        None => 0,
    };
    let lookback_secs = match &def.lookback {
        Some(s) => parse_duration("lookback", s)?,
        None => 7 * 86_400,
    };
    let timeout_secs = match &def.timeout {
        Some(s) => parse_duration("timeout", s)? as u64,
        None => DEFAULT_TIMEOUT_SECS,
    };
    if timeout_secs > 3600 {
        bail!("timeout must be at most 1h");
    }
    let max_bytes = def.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    if max_bytes == 0 || max_bytes > MAX_MAX_BYTES {
        bail!("max_bytes must be between 1 and {MAX_MAX_BYTES}");
    }
    let cursor_format = def.cursor_format.clone().unwrap_or_else(|| DEFAULT_CURSOR_FORMAT.into());
    if cursor_format.is_empty() || StrftimeItems::new(&cursor_format).any(|i| matches!(i, Item::Error)) {
        bail!("cursor_format \"{cursor_format}\" is not a valid strftime format");
    }
    let utc_offset = match &def.utc_offset {
        Some(s) => parse_offset(s)?,
        None => FixedOffset::east_opt(0).expect("zero offset"),
    };
    check_items(&def.items)?;
    if def.github.is_some() && def.source != "github" {
        bail!("[sync.github] only applies to recipes with source = \"github\"");
    }
    Ok(Recipe {
        name: def.name.clone(),
        def: def.clone(),
        adapter,
        interval_secs,
        overlap_secs,
        lookback_secs,
        timeout_secs,
        max_bytes,
        cursor_format,
        utc_offset,
    })
}

fn check_ident(field: &str, v: &str, allow_dash: bool) -> Result<()> {
    let ok_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || (allow_dash && c == '-');
    if v.is_empty() || v.len() > 64 || !v.chars().all(ok_char) || !v.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        let allowed = if allow_dash { "a-z, 0-9, '_' and '-'" } else { "a-z, 0-9 and '_'" };
        bail!("{field} \"{v}\" must be 1–64 characters of {allowed}, starting with a letter or digit");
    }
    Ok(())
}

fn check_mcp(m: &McpDef) -> Result<()> {
    match (&m.url, &m.command) {
        (Some(url), None) => {
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                bail!("mcp.url must start with https:// or http://");
            }
            if url.starts_with("http://") && !is_loopback_url(url) {
                bail!("mcp.url uses plain http:// for a non-loopback host; use https://");
            }
            if url.chars().any(|c| c.is_control() || c == ' ' || c == '"') {
                bail!("mcp.url contains spaces, quotes or control characters");
            }
            if !m.args.is_empty() || !m.env.is_empty() {
                bail!("mcp.args and mcp.env only apply to a stdio server (mcp.command)");
            }
        }
        (None, Some(cmd)) => {
            if cmd.trim().is_empty() {
                bail!("mcp.command is empty");
            }
            if m.bearer_secret.is_some() || !m.headers.is_empty() {
                bail!("mcp.bearer_secret and mcp.headers only apply to an HTTP server (mcp.url); pass secrets to a stdio server through mcp.env");
            }
        }
        _ => bail!("set exactly one of mcp.url (streamable HTTP) and mcp.command (stdio)"),
    }
    if m.tool.is_empty() || m.tool.len() > 128 || !m.tool.chars().all(|c| c.is_ascii_alphanumeric() || "_-./".contains(c)) {
        bail!("mcp.tool \"{}\" is not a valid tool name", m.tool);
    }
    if let Some(b) = &m.bearer_secret {
        check_secret_name(b).map_err(|e| anyhow::anyhow!("mcp.bearer_secret: {e}"))?;
    }
    for (k, v) in &m.headers {
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            bail!("mcp.headers: \"{k}\" is not a valid header name");
        }
        if v.chars().any(|c| c.is_control()) {
            bail!("mcp.headers.{k} contains control characters");
        }
        let lower = k.to_ascii_lowercase();
        if lower == "authorization" && m.bearer_secret.is_some() {
            bail!("set either mcp.bearer_secret or an Authorization header, not both");
        }
        if is_sensitive_name(&lower) && secret_refs(v).is_empty() {
            bail!("mcp.headers.{k} looks like a credential; store it with `mirrorself sync secret set <name>` and write \"${{secret:<name>}}\" here");
        }
        if is_reserved_header(&lower) {
            bail!("mcp.headers.{k} is set by the MCP client and cannot be overridden");
        }
        check_secret_refs(&format!("mcp.headers.{k}"), v)?;
        check_no_placeholders(&format!("mcp.headers.{k}"), v)?;
    }
    check_env("mcp.env", &m.env)?;
    for (i, a) in m.args.iter().enumerate() {
        check_no_secrets(&format!("mcp.args[{i}]"), a)?;
        check_no_placeholders(&format!("mcp.args[{i}]"), a)?;
    }
    check_arguments("mcp.arguments", &toml::Value::Table(m.arguments.clone()))?;
    Ok(())
}

fn check_cli(c: &CliDef) -> Result<()> {
    if c.program.trim().is_empty() {
        bail!("cli.program is empty");
    }
    check_no_secrets("cli.program", &c.program)?;
    check_no_placeholders("cli.program", &c.program)?;
    for (i, a) in c.args.iter().enumerate() {
        check_no_secrets(&format!("cli.args[{i}]"), a)?;
        check_placeholders(&format!("cli.args[{i}]"), a)?;
    }
    check_env("cli.env", &c.env)
}

fn check_env(field: &str, env: &BTreeMap<String, String>) -> Result<()> {
    for (k, v) in env {
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("{field}: \"{k}\" is not a valid environment variable name");
        }
        if is_sensitive_name(&k.to_ascii_lowercase()) && secret_refs(v).is_empty() {
            bail!("{field}.{k} looks like a credential; store it with `mirrorself sync secret set <name>` and write \"${{secret:<name>}}\" here");
        }
        check_secret_refs(&format!("{field}.{k}"), v)?;
        check_no_placeholders(&format!("{field}.{k}"), v)?;
    }
    Ok(())
}

fn check_arguments(field: &str, v: &toml::Value) -> Result<()> {
    match v {
        toml::Value::String(s) => {
            check_no_secrets(field, s)?;
            check_placeholders(field, s)
        }
        toml::Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                check_arguments(&format!("{field}[{i}]"), x)?;
            }
            Ok(())
        }
        toml::Value::Table(t) => {
            for (k, x) in t {
                check_arguments(&format!("{field}.{k}"), x)?;
            }
            Ok(())
        }
        toml::Value::Datetime(_) => bail!("{field}: write dates as strings, or use a placeholder such as {{{{since}}}}"),
        _ => Ok(()),
    }
}

fn check_items(r: &ItemRules) -> Result<()> {
    if let Some(s) = &r.subtype {
        check_ident("items.subtype", s, false)?;
    }
    if let Some(c) = &r.category {
        check_ident("items.category", c, false)?;
    }
    for (field, v) in [
        ("items.records", &r.records),
        ("items.key_field", &r.key_field),
        ("items.title_field", &r.title_field),
        ("items.text_field", &r.text_field),
        ("items.time_field", &r.time_field),
        ("items.end_field", &r.end_field),
        ("items.updated_field", &r.updated_field),
    ] {
        if let Some(p) = v {
            if p.split('.').any(str::is_empty) && !p.is_empty() {
                bail!("{field} \"{p}\" is not a dot-separated path");
            }
        }
    }
    Ok(())
}

fn is_sensitive_name(lower: &str) -> bool {
    ["authorization", "token", "secret", "password", "passwd", "api-key", "api_key", "apikey", "cookie", "credential"]
        .iter()
        .any(|w| lower.contains(w))
}

fn is_reserved_header(lower: &str) -> bool {
    ["content-type", "accept", "content-length", "host", "mcp-session-id", "mcp-protocol-version", "mcp-method", "mcp-name"]
        .contains(&lower)
}

fn is_loopback_url(url: &str) -> bool {
    let rest = url.trim_start_matches("http://");
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    host == "localhost"
        || host.starts_with("localhost:")
        || host.starts_with("127.")
        || host.starts_with("[::1]")
}

pub(crate) fn check_secret_name(n: &str) -> Result<()> {
    if n.is_empty() || n.len() > 64 || !n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
        bail!("secret name \"{n}\" must be 1–64 characters of a-z, 0-9, '_' and '-'");
    }
    Ok(())
}

/// Secret names referenced as `${secret:name}` in `s`.
pub fn secret_refs(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("${secret:") {
        let after = &rest[i + 9..];
        match after.find('}') {
            Some(j) => {
                out.push(after[..j].to_string());
                rest = &after[j + 1..];
            }
            None => break,
        }
    }
    out
}

fn check_secret_refs(field: &str, s: &str) -> Result<()> {
    if s.matches("${").count() != secret_refs(s).len() {
        bail!("{field}: only ${{secret:<name>}} references are allowed, and each needs a closing brace");
    }
    for n in secret_refs(s) {
        check_secret_name(&n).map_err(|e| anyhow::anyhow!("{field}: {e}"))?;
    }
    Ok(())
}

fn check_no_secrets(field: &str, s: &str) -> Result<()> {
    if s.contains("${secret:") {
        bail!("{field}: secrets can only be used in headers and environment variables, never in arguments (they would be visible to other processes or sent to the tool)");
    }
    Ok(())
}

/// `{{name}}` placeholders in `s`, in order.
pub fn placeholder_refs(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("{{") {
        let after = &rest[i + 2..];
        match after.find("}}") {
            Some(j) => {
                out.push(after[..j].trim().to_string());
                rest = &after[j + 2..];
            }
            None => break,
        }
    }
    out
}

fn check_placeholders(field: &str, s: &str) -> Result<()> {
    if s.matches("{{").count() != placeholder_refs(s).len() {
        bail!("{field}: a placeholder is missing its closing }}}}");
    }
    for p in placeholder_refs(s) {
        if !PLACEHOLDERS.contains(&p.as_str()) {
            bail!(
                "{field}: unknown placeholder {{{{{p}}}}}; use one of {}",
                PLACEHOLDERS.iter().map(|p| format!("{{{{{p}}}}}")).collect::<Vec<_>>().join(", ")
            );
        }
    }
    Ok(())
}

fn check_no_placeholders(field: &str, s: &str) -> Result<()> {
    if !placeholder_refs(s).is_empty() {
        bail!("{field}: window placeholders are only allowed in arguments");
    }
    Ok(())
}

/// Replaces window placeholders in `s`. Unknown names were rejected when
/// the recipe was checked.
pub fn render(s: &str, values: &BTreeMap<&'static str, String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        match after.find("}}") {
            Some(j) => {
                let name = after[..j].trim();
                match values.get(name) {
                    Some(v) => out.push_str(v),
                    None => out.push_str(&rest[i..i + 2 + j + 2]),
                }
                rest = &after[j + 2..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Renders tool arguments to JSON, filling placeholders in string values.
pub fn render_arguments(args: &toml::Table, values: &BTreeMap<&'static str, String>) -> Value {
    fn conv(v: &toml::Value, values: &BTreeMap<&'static str, String>) -> Value {
        match v {
            toml::Value::String(s) => Value::String(render(s, values)),
            toml::Value::Integer(i) => Value::from(*i),
            toml::Value::Float(f) => serde_json::Number::from_f64(*f).map(Value::Number).unwrap_or(Value::Null),
            toml::Value::Boolean(b) => Value::Bool(*b),
            toml::Value::Datetime(d) => Value::String(d.to_string()),
            toml::Value::Array(a) => Value::Array(a.iter().map(|x| conv(x, values)).collect()),
            toml::Value::Table(t) => Value::Object(t.iter().map(|(k, x)| (k.clone(), conv(x, values))).collect()),
        }
    }
    Value::Object(args.iter().map(|(k, v)| (k.clone(), conv(v, values))).collect())
}

/// `30s`, `15m`, `2h`, `1d`, `1w`.
pub fn parse_duration(field: &str, s: &str) -> Result<i64> {
    let secs = parse_duration_allow_zero(field, s)?;
    if secs == 0 {
        bail!("{field} must be longer than zero");
    }
    Ok(secs)
}

fn parse_duration_allow_zero(field: &str, s: &str) -> Result<i64> {
    let s = s.trim();
    let bad = || anyhow::anyhow!("{field} \"{s}\" is not a duration such as 30s, 15m, 1h, 1d or 1w");
    let unit = s.chars().last().ok_or_else(bad)?;
    let n: i64 = s[..s.len() - unit.len_utf8()].parse().map_err(|_| bad())?;
    let mult = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        'w' => 7 * 86_400,
        _ => return Err(bad()),
    };
    if !(0..=3650 * 86_400).contains(&n.saturating_mul(mult)) {
        return Err(bad());
    }
    Ok(n * mult)
}

/// `+08:00`, `-05:30`, `Z`.
pub fn parse_offset(s: &str) -> Result<FixedOffset> {
    let bad = || anyhow::anyhow!("utc_offset \"{s}\" must look like +08:00, -05:30 or Z");
    if s == "Z" || s == "+00:00" {
        return Ok(FixedOffset::east_opt(0).expect("zero offset"));
    }
    let (sign, rest) = match s.split_at_checked(1) {
        Some(("+", r)) => (1, r),
        Some(("-", r)) => (-1, r),
        _ => return Err(bad()),
    };
    let (h, m) = rest.split_once(':').ok_or_else(bad)?;
    if h.len() != 2 || m.len() != 2 {
        return Err(bad());
    }
    let h: i32 = h.parse().map_err(|_| bad())?;
    let m: i32 = m.parse().map_err(|_| bad())?;
    if h > 14 || m > 59 {
        return Err(bad());
    }
    FixedOffset::east_opt(sign * (h * 3600 + m * 60)).ok_or_else(bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_offsets() {
        assert_eq!(parse_duration("x", "90s").unwrap(), 90);
        assert_eq!(parse_duration("x", "2h").unwrap(), 7200);
        assert_eq!(parse_duration("x", "1w").unwrap(), 604_800);
        assert!(parse_duration("x", "0m").is_err());
        assert!(parse_duration("x", "1y").is_err());
        assert!(parse_duration("x", "h").is_err());
        assert_eq!(parse_offset("+08:00").unwrap().local_minus_utc(), 28_800);
        assert_eq!(parse_offset("-05:30").unwrap().local_minus_utc(), -19_800);
        assert!(parse_offset("8").is_err());
    }

    #[test]
    fn placeholders_render_and_unknown_ones_stay() {
        let v = BTreeMap::from([("since", "A".to_string()), ("until", "B".to_string())]);
        assert_eq!(render("from {{since}} to {{ until }}", &v), "from A to B");
        assert_eq!(render("{{nope}} {{since", &v), "{{nope}} {{since");
        assert_eq!(placeholder_refs("a {{since}} {{x}} {{y"), vec!["since", "x"]);
        assert_eq!(secret_refs("Bearer ${secret:gh} ${secret:b}"), vec!["gh", "b"]);
    }
}
