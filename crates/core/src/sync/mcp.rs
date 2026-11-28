//! A minimal, blocking MCP client: discovery or `initialize`, `tools/list`
//! and `tools/call`, over stdio or Streamable HTTP.
//!
//! Protocol revisions. The current revision (2026-07-28) is stateless: no
//! `initialize` handshake and no `Mcp-Session-Id`; every request carries the
//! protocol version, client info and capabilities in `params._meta`, and
//! over HTTP also the `MCP-Protocol-Version`, `Mcp-Method` and (for
//! `tools/call`) `Mcp-Name` headers. Older revisions (2025-11-25 back to
//! 2024-11-05) need `initialize` + `notifications/initialized`, and over
//! HTTP may hand out a session id. The client probes with `server/discover`
//! in the current revision's form; any answer other than a result means an
//! older server, and it falls back to `initialize`.
//!
//! HTTP goes through the system `curl`, so the core crate needs neither an
//! async runtime nor a TLS stack. Its configuration (URL, headers including
//! credentials, body) is written to curl's stdin, never to its arguments,
//! so other processes cannot read credentials from the process table.
//!
//! Everything a server returns is data. Nothing in it chooses what is called
//! next: the client only ever calls the tool its recipe names.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};

use crate::crypto::sha256_hex;
use crate::sync::process::{clean_text, run_captured, read_tail, STDERR_TAIL};

/// The current revision, tried first.
pub const MODERN_VERSION: &str = "2026-07-28";
/// Older revisions this client can speak, newest first.
pub const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const CLIENT_NAME: &str = "mirrorself-sync";
const MAX_TOOL_PAGES: usize = 50;

/// Where the server is.
#[derive(Debug, Clone)]
pub enum ServerSpec {
    Stdio { program: String, args: Vec<String>, env: BTreeMap<String, String> },
    /// Header values are already resolved (they may hold credentials).
    Http { url: String, headers: Vec<(String, String)> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Era {
    /// 2026-07-28 and later: stateless.
    Modern,
    /// An older revision, negotiated through `initialize`.
    Legacy(String),
}

impl Era {
    pub fn version(&self) -> &str {
        match self {
            Era::Modern => MODERN_VERSION,
            Era::Legacy(v) => v,
        }
    }
}

/// A tool as `tools/list` described it.
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub input_schema: Value,
    pub annotations: Value,
}

impl ToolInfo {
    /// True only when the server says so explicitly. Annotations are hints
    /// from the server; a missing or false hint means the tool may write.
    pub fn read_only(&self) -> bool {
        self.annotations.get("readOnlyHint").and_then(Value::as_bool) == Some(true)
            && self.annotations.get("destructiveHint").and_then(Value::as_bool) != Some(true)
    }

    /// SHA-256 of the input schema in canonical form (sorted keys, no spaces).
    pub fn schema_hash(&self) -> String {
        sha256_hex(canonical_json(&self.input_schema).as_bytes())
    }
}

/// JSON with object keys sorted at every level, so equal schemas hash alike
/// whatever order the server wrote them in.
pub fn canonical_json(v: &Value) -> String {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for k in keys {
                    out.insert(k.clone(), sort(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    // serde_json keeps insertion order when `preserve_order` is enabled
    // anywhere in the build, so sort explicitly and write keys in order.
    fn write(v: &Value, out: &mut String) {
        match v {
            Value::Object(m) => {
                out.push('{');
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String((*k).clone()).to_string());
                    out.push(':');
                    write(&m[*k], out);
                }
                out.push('}');
            }
            Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(x, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(&sort(v), &mut out);
    out
}

/// What came back for one request.
enum Reply {
    /// A JSON-RPC response with our id.
    Message(Value),
    /// An HTTP status without a JSON-RPC response in the body.
    Status(u16, String),
}

pub struct McpClient {
    transport: Transport,
    era: Era,
    next_id: u64,
    timeout: Duration,
}

enum Transport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

impl McpClient {
    /// Starts or reaches the server and settles the protocol revision.
    pub fn connect(spec: &ServerSpec, timeout: Duration, max_bytes: u64) -> Result<Self> {
        let transport = match spec {
            ServerSpec::Stdio { program, args, env } => Transport::Stdio(StdioTransport::spawn(program, args, env, max_bytes)?),
            ServerSpec::Http { url, headers } => Transport::Http(HttpTransport {
                url: url.clone(),
                headers: headers.clone(),
                session_id: None,
                max_bytes,
            }),
        };
        let mut client = Self { transport, era: Era::Modern, next_id: 0, timeout };
        client.negotiate()?;
        Ok(client)
    }

    pub fn era(&self) -> &Era {
        &self.era
    }

    fn negotiate(&mut self) -> Result<()> {
        match self.exchange("server/discover", json!({}), None)? {
            Reply::Message(m) if m.get("result").is_some() => {
                self.era = Era::Modern;
                return Ok(());
            }
            Reply::Message(m) => {
                // -32022: the server names the versions it supports.
                if m["error"]["code"].as_i64() == Some(-32022) {
                    let supported: Vec<&str> =
                        m["error"]["data"]["supported"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                    if !supported.is_empty() && !supported.iter().any(|v| LEGACY_VERSIONS.contains(v)) {
                        bail!(
                            "the MCP server supports protocol versions {} and this client {} plus {}",
                            supported.join(", "),
                            MODERN_VERSION,
                            LEGACY_VERSIONS.join(", ")
                        );
                    }
                }
            }
            Reply::Status(401 | 403, body) => bail!("the MCP server refused the credentials (HTTP 401/403): {body}"),
            Reply::Status(400 | 404 | 405, _) => {}
            Reply::Status(code, body) => bail!("the MCP server answered HTTP {code}: {body}"),
        }
        self.initialize_legacy()
    }

    fn initialize_legacy(&mut self) -> Result<()> {
        let preferred = LEGACY_VERSIONS[0];
        self.era = Era::Legacy(preferred.to_string());
        let params = json!({
            "protocolVersion": preferred,
            "capabilities": {},
            "clientInfo": client_info(),
        });
        let result = self.request("initialize", params, None)?;
        let version = result["protocolVersion"].as_str().unwrap_or_default();
        if !LEGACY_VERSIONS.contains(&version) {
            bail!("the MCP server chose protocol version '{}', which this client does not speak", clean_text(version, 40));
        }
        self.era = Era::Legacy(version.to_string());
        self.notify("notifications/initialized")?;
        Ok(())
    }

    /// Every tool the server lists, following pagination.
    pub fn list_tools(&mut self) -> Result<Vec<ToolInfo>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self.request("tools/list", params, None)?;
            for t in result["tools"].as_array().into_iter().flatten() {
                let Some(name) = t["name"].as_str() else { continue };
                tools.push(ToolInfo {
                    name: name.to_string(),
                    input_schema: t.get("inputSchema").cloned().unwrap_or(Value::Null),
                    annotations: t.get("annotations").cloned().unwrap_or(Value::Null),
                });
            }
            match result["nextCursor"].as_str() {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => return Ok(tools),
            }
        }
        bail!("the MCP server listed more than {MAX_TOOL_PAGES} pages of tools")
    }

    /// Calls one tool and returns its result object (`content`,
    /// `structuredContent`, `isError`).
    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.request("tools/call", json!({ "name": name, "arguments": arguments }), Some(name))
    }

    fn request(&mut self, method: &str, params: Value, name: Option<&str>) -> Result<Value> {
        match self.exchange(method, params, name)? {
            Reply::Message(m) => {
                if let Some(err) = m.get("error") {
                    bail!(
                        "the MCP server returned an error for {method}: {} (code {})",
                        clean_text(err["message"].as_str().unwrap_or("no message"), 300),
                        err["code"]
                    );
                }
                let result = m.get("result").cloned().unwrap_or(Value::Null);
                if result["resultType"].as_str() == Some("input_required") {
                    bail!("the MCP server asked for user input during {method}; scheduled sync cannot answer it");
                }
                Ok(result)
            }
            Reply::Status(code, body) => bail!("{method}: the MCP server answered HTTP {code}: {body}"),
        }
    }

    fn exchange(&mut self, method: &str, params: Value, name: Option<&str>) -> Result<Reply> {
        self.next_id += 1;
        let id = self.next_id;
        let params = self.with_meta(params);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let version = self.protocol_header(method);
        let deadline = Instant::now() + self.timeout;
        match &mut self.transport {
            Transport::Stdio(t) => t.request(&msg, id, deadline).map(Reply::Message),
            Transport::Http(t) => {
                let r = t.post(&msg, method, name, version.as_deref(), self.timeout)?;
                if method == "initialize" {
                    t.session_id = r.session_id.clone();
                }
                Ok(match find_response(&r, id) {
                    Some(m) => Reply::Message(m),
                    None => Reply::Status(r.status, clean_text(&String::from_utf8_lossy(&r.body), 300)),
                })
            }
        }
    }

    fn notify(&mut self, method: &str) -> Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method });
        let version = self.protocol_header(method);
        match &mut self.transport {
            Transport::Stdio(t) => t.send(&msg),
            Transport::Http(t) => {
                let r = t.post(&msg, method, None, version.as_deref(), self.timeout)?;
                if !(200..300).contains(&r.status) {
                    bail!("{method}: the MCP server answered HTTP {}", r.status);
                }
                Ok(())
            }
        }
    }

    /// The `MCP-Protocol-Version` header: always for the current revision,
    /// and for older ones once `initialize` has settled the version.
    fn protocol_header(&self, method: &str) -> Option<String> {
        match &self.era {
            Era::Modern => Some(MODERN_VERSION.to_string()),
            Era::Legacy(_) if method == "initialize" => None,
            Era::Legacy(v) => Some(v.clone()),
        }
    }

    fn with_meta(&self, params: Value) -> Value {
        if self.era != Era::Modern {
            return params;
        }
        let mut params = match params {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        let meta = params.entry("_meta").or_insert_with(|| json!({}));
        if let Value::Object(m) = meta {
            m.insert("io.modelcontextprotocol/protocolVersion".into(), json!(MODERN_VERSION));
            m.insert("io.modelcontextprotocol/clientInfo".into(), client_info());
            m.insert("io.modelcontextprotocol/clientCapabilities".into(), json!({}));
        }
        Value::Object(params)
    }

    /// Ends the session: closes a stdio server's stdin and reaps it.
    pub fn close(self) {
        if let Transport::Stdio(t) = self.transport {
            t.close();
        }
    }
}

fn client_info() -> Value {
    json!({ "name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION") })
}

// ---------------------------------------------------------------- stdio

struct StdioTransport {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Result<Vec<u8>, String>>,
    stderr: Arc<Mutex<String>>,
    program: String,
}

impl StdioTransport {
    fn spawn(program: &str, args: &[String], env: &BTreeMap<String, String>, max_bytes: u64) -> Result<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args).envs(env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().with_context(|| format!("starting MCP server {program}"))?;
        let stdout = child.stdout.take().expect("piped");
        let stderr_pipe = child.stderr.take().expect("piped");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_line_capped(&mut reader, max_bytes) {
                    Ok(Some(line)) => {
                        if tx.send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = stderr.clone();
        thread::spawn(move || {
            let tail = read_tail(stderr_pipe, STDERR_TAIL);
            if let Ok(mut s) = sink.lock() {
                *s = tail;
            }
        });
        Ok(Self { stdin: child.stdin.take(), child, lines: rx, stderr, program: program.to_string() })
    }

    fn send(&mut self, msg: &Value) -> Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(|| anyhow!("the MCP server's stdin is closed"))?;
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        stdin
            .write_all(&line)
            .and_then(|_| stdin.flush())
            .map_err(|e| anyhow!("writing to MCP server {}: {e}{}", self.program, self.stderr_note()))
    }

    fn request(&mut self, msg: &Value, id: u64, deadline: Instant) -> Result<Value> {
        self.send(msg)?;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(left) {
                Ok(Ok(line)) => line,
                Ok(Err(e)) => bail!("MCP server {}: {e}", self.program),
                Err(RecvTimeoutError::Timeout) => bail!("MCP server {} did not answer in time", self.program),
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("MCP server {} closed its output{}", self.program, self.stderr_note())
                }
            };
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let m: Value = serde_json::from_slice(&line)
                .map_err(|_| anyhow!("MCP server {} wrote something that is not JSON-RPC to stdout", self.program))?;
            if m.get("method").is_some() {
                if let Some(req_id) = m.get("id") {
                    // A request from the server (sampling, roots, …): decline.
                    let reply = json!({ "jsonrpc": "2.0", "id": req_id, "error": { "code": -32601, "message": "not supported by mirrorself sync" } });
                    self.send(&reply)?;
                }
                continue;
            }
            if m.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(m);
            }
        }
    }

    fn stderr_note(&self) -> String {
        let s = self.stderr.lock().map(|s| s.clone()).unwrap_or_default();
        if s.trim().is_empty() {
            String::new()
        } else {
            format!("; stderr: {}", clean_text(&s, 300))
        }
    }

    fn close(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// One newline-terminated line, at most `max` bytes; `None` at EOF.
fn read_line_capped(r: &mut impl BufRead, max: u64) -> Result<Option<Vec<u8>>, String> {
    let mut line = Vec::new();
    loop {
        let buf = r.fill_buf().map_err(|e| e.to_string())?;
        if buf.is_empty() {
            return Ok(if line.is_empty() { None } else { Some(line) });
        }
        let (take, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        line.extend_from_slice(&buf[..take]);
        r.consume(take);
        if line.len() as u64 > max {
            return Err(format!("a message was larger than {max} bytes"));
        }
        if done {
            line.pop();
            return Ok(Some(line));
        }
    }
}

// ----------------------------------------------------------------- HTTP

struct HttpTransport {
    url: String,
    headers: Vec<(String, String)>,
    session_id: Option<String>,
    max_bytes: u64,
}

struct HttpReply {
    status: u16,
    content_type: String,
    session_id: Option<String>,
    body: Vec<u8>,
}

impl HttpTransport {
    fn post(&self, msg: &Value, method: &str, name: Option<&str>, version: Option<&str>, timeout: Duration) -> Result<HttpReply> {
        let body = serde_json::to_string(msg)?;
        let mut headers: Vec<(String, String)> = vec![
            ("Content-Type".into(), "application/json".into()),
            ("Accept".into(), "application/json, text/event-stream".into()),
            ("Mcp-Method".into(), method.into()),
        ];
        if let Some(n) = name {
            headers.push(("Mcp-Name".into(), n.into()));
        }
        if let Some(v) = version {
            headers.push(("MCP-Protocol-Version".into(), v.into()));
        }
        if let Some(s) = &self.session_id {
            headers.push(("Mcp-Session-Id".into(), s.clone()));
        }
        headers.extend(self.headers.iter().cloned());

        let mut config = String::new();
        config.push_str(&format!("url = {}\n", curl_quote(&self.url)?));
        config.push_str("request = \"POST\"\n");
        for (k, v) in &headers {
            config.push_str(&format!("header = {}\n", curl_quote(&format!("{k}: {v}"))?));
        }
        config.push_str(&format!("data-binary = {}\n", curl_quote(&body)?));

        let mut cmd = Command::new("curl");
        // -q must come first: it skips ~/.curlrc.
        cmd.args(["-q", "--silent", "--show-error", "--include", "--proto", "=http,https", "--max-redirs", "0"])
            .arg("--max-time")
            .arg(timeout.as_secs().max(1).to_string())
            .args(["--config", "-"]);
        let out = run_captured(cmd, Some(config.into_bytes()), timeout + Duration::from_secs(5), self.max_bytes + 64 * 1024)
            .context("running curl for the MCP server (is curl installed?)")?;
        if !out.status.success() {
            bail!("could not reach the MCP server: {}", clean_text(&out.stderr_tail, 300));
        }
        parse_http_response(&out.stdout)
    }
}

/// A string in curl's config syntax: double-quoted with backslash escapes.
fn curl_quote(s: &str) -> Result<String> {
    if s.contains(['\r', '\n']) {
        bail!("header values and URLs cannot contain line breaks");
    }
    Ok(format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\t', "\\t")))
}

fn parse_http_response(raw: &[u8]) -> Result<HttpReply> {
    let mut rest = raw;
    loop {
        let end = find(rest, b"\r\n\r\n").ok_or_else(|| anyhow!("malformed HTTP response from the MCP server"))?;
        let head = String::from_utf8_lossy(&rest[..end]).into_owned();
        let body = &rest[end + 4..];
        let mut lines = head.split("\r\n");
        let status_line = lines.next().unwrap_or_default();
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("malformed HTTP status line from the MCP server"))?;
        // Informational responses and a proxy's CONNECT reply precede the real one.
        if (100..200).contains(&status) || (body.starts_with(b"HTTP/") && status == 200) {
            rest = body;
            continue;
        }
        let mut content_type = String::new();
        let mut session_id = None;
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
                if k == "content-type" {
                    content_type = v.to_ascii_lowercase();
                } else if k == "mcp-session-id" && !v.is_empty() && v.chars().all(|c| c.is_ascii_graphic()) {
                    session_id = Some(v.to_string());
                }
            }
        }
        return Ok(HttpReply { status, content_type, session_id, body: body.to_vec() });
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The JSON-RPC response with `id` in a JSON or SSE body.
fn find_response(r: &HttpReply, id: u64) -> Option<Value> {
    let matches = |m: &Value| {
        m.get("id").and_then(Value::as_u64) == Some(id) && (m.get("result").is_some() || m.get("error").is_some())
    };
    let pick = |v: Value| -> Option<Value> {
        match v {
            Value::Array(a) => a.into_iter().find(|m| matches(m)),
            m if matches(&m) => Some(m),
            _ => None,
        }
    };
    if r.content_type.starts_with("text/event-stream") {
        let text = String::from_utf8_lossy(&r.body);
        let mut data = String::new();
        for line in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).chain(std::iter::once("")) {
            if line.is_empty() {
                if !data.is_empty() {
                    if let Some(m) = serde_json::from_str(&data).ok().and_then(pick) {
                        return Some(m);
                    }
                    data.clear();
                }
            } else if let Some(d) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(d.strip_prefix(' ').unwrap_or(d));
            }
        }
        None
    } else {
        serde_json::from_slice(&r.body).ok().and_then(pick)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_keys_at_every_level() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":{"y":[{"q":1,"p":2}],"x":null}}"#).unwrap();
        assert_eq!(canonical_json(&a), r#"{"a":{"x":null,"y":[{"p":2,"q":1}]},"b":1}"#);
    }

    #[test]
    fn http_responses_parse_json_and_sse() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nMcp-Session-Id: abc\r\n\r\nevent: message\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\r\ndata: \"result\":{\"ok\":true}}\r\n\r\n";
        let r = parse_http_response(raw).unwrap();
        assert_eq!((r.status, r.session_id.as_deref()), (200, Some("abc")));
        assert_eq!(find_response(&r, 3).unwrap()["result"]["ok"], true);
        assert!(find_response(&r, 4).is_none());

        let raw = b"HTTP/2 400 \r\ncontent-type: application/json\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32022,\"message\":\"x\"}}";
        let r = parse_http_response(raw).unwrap();
        assert_eq!(find_response(&r, 1).unwrap()["error"]["code"], -32022);
    }

    #[test]
    fn curl_config_quoting() {
        assert_eq!(curl_quote(r#"a "b" \c"#).unwrap(), r#""a \"b\" \\c""#);
        assert!(curl_quote("a\nb").is_err());
    }

    #[test]
    fn read_only_needs_an_explicit_hint() {
        let t = |a: Value| ToolInfo { name: "t".into(), input_schema: json!({}), annotations: a }.read_only();
        assert!(t(json!({ "readOnlyHint": true })));
        assert!(!t(json!({})));
        assert!(!t(Value::Null));
        assert!(!t(json!({ "readOnlyHint": "true" })));
        assert!(!t(json!({ "readOnlyHint": true, "destructiveHint": true })));
    }
}
