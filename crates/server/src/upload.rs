//! Phone upload: the `/upload` page, pairing, upload-only sessions and
//! `POST /v1/upload`.
//!
//! A phone never sees the long-lived API token. `mirrorself serve` prints a
//! short one-time pairing code; the page trades it at `POST /v1/pair` for a
//! session credential that can only upload, expires, and lives in this
//! process's memory (a restart signs every phone out). Each upload is stored
//! through [`App::import_auto`] and processed, and every pairing and upload is
//! written to the audit log.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use mirrorself_core::config::ServerConfig;
use mirrorself_core::crypto::{random_bytes, sha256_hex};
use mirrorself_core::time::now_utc_string;
use mirrorself_core::App;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::rest::constant_time_eq;

/// The upload page: one self-contained HTML file, no external requests.
pub const PAGE: &str = include_str!("../assets/upload.html");

/// Header carrying the file name, percent-encoded UTF-8
/// (`encodeURIComponent`). `?name=` works too.
pub const FILE_NAME_HEADER: &str = "x-file-name";

/// Audit actor for requests made with an upload session.
pub const SESSION_ACTOR: &str = "upload-page";

const PAIR_DIGITS: usize = 8;
const MAX_NAME_CHARS: usize = 255;

/// Limits for pairing and uploads. [`UploadSettings::from_config`] reads
/// `[server]`; the rest are fixed defaults that tests may shorten.
#[derive(Debug, Clone)]
pub struct UploadSettings {
    /// Largest accepted file, in bytes.
    pub max_bytes: u64,
    /// Lifetime of a paired session.
    pub session_ttl: Duration,
    /// Lifetime of a pairing code that nobody used.
    pub code_ttl: Duration,
    /// Pairing attempts accepted per `pair_window`, from all clients together.
    pub pair_attempts_per_window: usize,
    pub pair_window: Duration,
    /// Wrong codes after which the code is replaced.
    pub failures_before_new_code: u32,
    /// Sessions held at once; the oldest is dropped beyond this.
    pub max_sessions: usize,
    /// Print each new pairing code to stderr (the hub's console).
    pub announce: bool,
}

impl Default for UploadSettings {
    fn default() -> Self {
        Self::from_config(&ServerConfig::default())
    }
}

impl UploadSettings {
    pub fn from_config(server: &ServerConfig) -> Self {
        Self {
            max_bytes: server.upload_max_bytes,
            session_ttl: Duration::from_secs(server.upload_session_minutes.saturating_mul(60)),
            code_ttl: Duration::from_secs(10 * 60),
            pair_attempts_per_window: 10,
            pair_window: Duration::from_secs(60),
            failures_before_new_code: 5,
            max_sessions: 16,
            announce: false,
        }
    }
}

/// Pairing code and sessions, held in memory only.
pub struct Uploads {
    settings: UploadSettings,
    inner: Mutex<Inner>,
}

struct Inner {
    code: Code,
    attempts: VecDeque<Instant>,
    /// Keyed by the SHA-256 of the session credential.
    sessions: HashMap<String, Session>,
}

struct Code {
    digits: String,
    issued: Instant,
    failures: u32,
}

struct Session {
    label: String,
    created: Instant,
    expires: Instant,
}

/// A newly paired session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    pub token: String,
    /// Short public id for the audit log (`session-<label>`).
    pub label: String,
    pub expires_in: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairError {
    /// Too many attempts; try again after this many seconds.
    RateLimited(u64),
    /// Wrong code (or the code was already used).
    Wrong,
    /// The code was not used in time; a new one has been issued.
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCheck {
    Valid(String),
    Expired,
    Unknown,
}

impl Uploads {
    pub fn new(settings: UploadSettings) -> Self {
        Self {
            inner: Mutex::new(Inner {
                code: Code::new(),
                attempts: VecDeque::new(),
                sessions: HashMap::new(),
            }),
            settings,
        }
    }

    pub fn settings(&self) -> &UploadSettings {
        &self.settings
    }

    /// The current pairing code, formatted `1234-5678`.
    pub fn pairing_code(&self) -> String {
        format_code(&self.lock().code.digits)
    }

    /// Trades a pairing code for an upload session. A code works once: it is
    /// replaced after a successful pairing, after too many wrong attempts,
    /// and when it expires.
    pub fn pair(&self, presented: &str) -> Result<Paired, PairError> {
        let now = Instant::now();
        let mut inner = self.lock();
        let window = self.settings.pair_window;
        while inner.attempts.front().is_some_and(|t| now.duration_since(*t) >= window) {
            inner.attempts.pop_front();
        }
        if inner.attempts.len() >= self.settings.pair_attempts_per_window {
            let oldest = *inner.attempts.front().expect("non-empty when over the limit");
            let wait = window.saturating_sub(now.duration_since(oldest));
            return Err(PairError::RateLimited(wait.as_secs().max(1)));
        }
        inner.attempts.push_back(now);

        if now.duration_since(inner.code.issued) >= self.settings.code_ttl {
            self.replace_code(&mut inner, "the previous code expired");
            return Err(PairError::Expired);
        }
        let digits: String = presented.chars().filter(char::is_ascii_digit).collect();
        if !constant_time_eq(digits.as_bytes(), inner.code.digits.as_bytes()) {
            inner.code.failures += 1;
            if inner.code.failures >= self.settings.failures_before_new_code {
                self.replace_code(&mut inner, "too many wrong attempts");
            }
            return Err(PairError::Wrong);
        }

        self.replace_code(&mut inner, "the previous code was used");
        let token = hex(&random_bytes::<32>());
        let key = sha256_hex(token.as_bytes());
        let label = key[..8].to_string();
        inner.sessions.retain(|_, s| s.expires > now);
        while inner.sessions.len() >= self.settings.max_sessions {
            let oldest = inner
                .sessions
                .iter()
                .min_by_key(|(_, s)| s.created)
                .map(|(k, _)| k.clone())
                .expect("non-empty");
            inner.sessions.remove(&oldest);
        }
        inner.sessions.insert(
            key,
            Session { label: label.clone(), created: now, expires: now + self.settings.session_ttl },
        );
        Ok(Paired { token, label, expires_in: self.settings.session_ttl })
    }

    /// Looks up a session credential. Expired sessions are forgotten.
    pub fn check_session(&self, token: &str) -> SessionCheck {
        let key = sha256_hex(token.as_bytes());
        let mut inner = self.lock();
        match inner.sessions.get(&key) {
            None => SessionCheck::Unknown,
            Some(s) if s.expires <= Instant::now() => {
                inner.sessions.remove(&key);
                SessionCheck::Expired
            }
            Some(s) => SessionCheck::Valid(s.label.clone()),
        }
    }

    /// Time left on a valid session.
    pub fn session_remaining(&self, token: &str) -> Option<Duration> {
        let key = sha256_hex(token.as_bytes());
        let now = Instant::now();
        let inner = self.lock();
        inner.sessions.get(&key).and_then(|s| s.expires.checked_duration_since(now)).filter(|d| !d.is_zero())
    }

    /// Ends a session; returns whether it existed.
    pub fn end_session(&self, token: &str) -> bool {
        self.lock().sessions.remove(&sha256_hex(token.as_bytes())).is_some()
    }

    fn replace_code(&self, inner: &mut Inner, reason: &str) {
        inner.code = Code::new();
        if self.settings.announce {
            eprintln!("new pairing code: {} ({reason})", format_code(&inner.code.digits));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Code {
    fn new() -> Self {
        let n = u64::from_le_bytes(random_bytes::<8>()) % 10u64.pow(PAIR_DIGITS as u32);
        Self { digits: format!("{n:0width$}", width = PAIR_DIGITS), issued: Instant::now(), failures: 0 }
    }
}

fn format_code(digits: &str) -> String {
    format!("{}-{}", &digits[..4], &digits[4..])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// State for the upload routes. `lan` is true on the LAN listener, where the
/// API token is refused so that it never travels over the local network.
#[derive(Clone)]
pub(crate) struct UploadState {
    pub app: Arc<Mutex<App>>,
    pub token: Arc<String>,
    pub uploads: Arc<Uploads>,
    pub lan: bool,
}

/// `/upload`, `/v1/pair`, `/v1/unpair` and `/v1/upload`.
pub(crate) fn routes(state: UploadState) -> Router {
    Router::new()
        .route("/upload", get(page))
        .route("/v1/pair", post(pair))
        .route("/v1/unpair", post(unpair))
        .route("/v1/session", get(session))
        .route("/v1/upload", post(upload))
        .with_state(state)
}

enum Credential {
    ApiToken,
    Session(String),
}

impl Credential {
    fn audit_identity(&self, peer: Option<IpAddr>) -> (&'static str, String) {
        match self {
            Credential::ApiToken => (crate::rest::ACTOR, crate::rest::CLIENT.to_string()),
            Credential::Session(label) => (SESSION_ACTOR, with_peer(format!("session-{label}"), peer)),
        }
    }
}

fn with_peer(client: String, peer: Option<IpAddr>) -> String {
    match peer {
        Some(ip) => format!("{client}@{ip}"),
        None => client,
    }
}

pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
}

fn peer(req: &Request) -> Option<IpAddr> {
    req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip())
}

/// A JSON error with a stable machine-readable `code` for the page.
fn fail(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let mut resp = (status, Json(json!({ "error": message.into(), "code": code }))).into_response();
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if status == StatusCode::UNAUTHORIZED {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    resp
}

impl UploadState {
    fn authenticate(&self, headers: &HeaderMap) -> Result<Credential, Box<Response>> {
        let Some(presented) = bearer(headers) else {
            return Err(Box::new(fail(StatusCode::UNAUTHORIZED, "pair_required", "pair this device first")));
        };
        if constant_time_eq(presented.as_bytes(), self.token.as_bytes()) {
            if self.lan {
                return Err(Box::new(fail(
                    StatusCode::FORBIDDEN,
                    "token_not_allowed",
                    "the API token is not accepted on the LAN address; pair this device instead",
                )));
            }
            return Ok(Credential::ApiToken);
        }
        match self.uploads.check_session(presented) {
            SessionCheck::Valid(label) => Ok(Credential::Session(label)),
            SessionCheck::Expired => Err(Box::new(fail(
                StatusCode::UNAUTHORIZED,
                "pair_required",
                "this upload session has expired; pair again with the code shown by mirrorself serve",
            ))),
            SessionCheck::Unknown => Err(Box::new(fail(
                StatusCode::UNAUTHORIZED,
                "pair_required",
                "unknown credential; pair this device with the code shown by mirrorself serve",
            ))),
        }
    }

    /// Writes one audit entry on a blocking thread.
    async fn audit(&self, actor: &'static str, client: String, action: &'static str, request: Value, rows: i64, outcome: &'static str) {
        let app = self.app.clone();
        let written = tokio::task::spawn_blocking(move || {
            let app = app.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            record(&app, actor, &client, action, &request, rows, outcome)
        })
        .await;
        if let Err(e) | Ok(Err(e)) = written.map_err(anyhow::Error::new) {
            eprintln!("warning: could not write the audit log: {e:#}");
        }
    }
}

/// Audit row for uploads and pairing. `rows` holds the byte count for an
/// upload. The request (which holds the file name) is stored only as a digest.
pub(crate) fn record(
    app: &App,
    actor: &str,
    client: &str,
    action: &str,
    request: &Value,
    rows: i64,
    outcome: &str,
) -> anyhow::Result<()> {
    let digest = &sha256_hex(request.to_string().as_bytes())[..16];
    app.conn().execute(
        "INSERT INTO audit_log (ts, actor, client_id, action, scope, query_digest, rows, outcome)
         VALUES (?1, ?2, ?3, ?4, 'upload:write', ?5, ?6, ?7)",
        (now_utc_string(), actor, client, action, digest, rows, outcome),
    )?;
    Ok(())
}

async fn page() -> Response {
    let mut resp = axum::response::Html(PAGE).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
             img-src 'self' data:; connect-src 'self'; form-action 'none'; base-uri 'none'; \
             frame-ancestors 'none'",
        ),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
}

async fn pair(State(s): State<UploadState>, req: Request) -> Response {
    let peer = peer(&req);
    let client = with_peer("unpaired".into(), peer);
    let body = match Limited::new(req.into_body(), 1024).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return fail(StatusCode::BAD_REQUEST, "bad_request", "send {\"code\": \"12345678\"}"),
    };
    let Ok(body) = serde_json::from_slice::<PairBody>(&body) else {
        return fail(StatusCode::BAD_REQUEST, "bad_request", "send {\"code\": \"12345678\"}");
    };
    match s.uploads.pair(&body.code) {
        Ok(paired) => {
            let client = with_peer(format!("session-{}", paired.label), peer);
            s.audit(SESSION_ACTOR, client, "pair", json!({}), 1, "ok").await;
            let mut resp = Json(json!({
                "session": paired.token,
                "expires_in": paired.expires_in.as_secs(),
                "max_bytes": s.uploads.settings().max_bytes,
            }))
            .into_response();
            resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Err(PairError::RateLimited(secs)) => {
            let mut resp = fail(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                format!("too many pairing attempts; try again in {secs} s"),
            );
            resp.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(secs));
            resp
        }
        Err(PairError::Wrong) => {
            s.audit(SESSION_ACTOR, client, "pair", json!({}), 0, "denied").await;
            fail(StatusCode::UNAUTHORIZED, "wrong_code", "wrong pairing code; check the code shown by mirrorself serve")
        }
        Err(PairError::Expired) => {
            s.audit(SESSION_ACTOR, client, "pair", json!({}), 0, "denied").await;
            fail(
                StatusCode::UNAUTHORIZED,
                "code_expired",
                "that pairing code expired; mirrorself serve has printed a new one",
            )
        }
    }
}

/// Lets the page check a stored session (after a reload or a hub restart)
/// and learn the current upload limit. Not audited: it reads nothing.
async fn session(State(s): State<UploadState>, req: Request) -> Response {
    let remaining = bearer(req.headers()).and_then(|t| s.uploads.session_remaining(t));
    match remaining {
        Some(d) => {
            let mut resp = Json(json!({
                "expires_in": d.as_secs(),
                "max_bytes": s.uploads.settings().max_bytes,
            }))
            .into_response();
            resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        None => fail(StatusCode::UNAUTHORIZED, "pair_required", "pair this device first"),
    }
}

async fn unpair(State(s): State<UploadState>, req: Request) -> Response {
    let Some(presented) = bearer(req.headers()) else {
        return fail(StatusCode::UNAUTHORIZED, "pair_required", "no session to end");
    };
    if let SessionCheck::Valid(label) = s.uploads.check_session(presented) {
        s.uploads.end_session(presented);
        let client = with_peer(format!("session-{label}"), peer(&req));
        s.audit(SESSION_ACTOR, client, "unpair", json!({}), 0, "ok").await;
    }
    Json(json!({ "ended": true })).into_response()
}

#[derive(Deserialize)]
struct UploadParams {
    name: Option<String>,
}

async fn upload(State(s): State<UploadState>, Query(q): Query<UploadParams>, req: Request) -> Response {
    let credential = match s.authenticate(req.headers()) {
        Ok(c) => c,
        Err(resp) => return *resp,
    };
    let (actor, client) = credential.audit_identity(peer(&req));
    let max = s.uploads.settings().max_bytes;

    let name = match file_name(req.headers(), q.name) {
        Ok(n) => n,
        Err(msg) => {
            s.audit(actor, client, "upload", json!({}), 0, "error").await;
            return fail(StatusCode::BAD_REQUEST, "bad_name", msg);
        }
    };
    let request = json!({ "name": name });
    let too_large = || {
        fail(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!("the file is larger than the upload limit of {} (server.upload_max_bytes)", human_bytes(max)),
        )
    };
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > max) {
        s.audit(actor, client, "upload", request, declared.unwrap_or(0) as i64, "error").await;
        return too_large();
    }

    // Reads at most `max` bytes, whatever Content-Length claimed.
    let limit = usize::try_from(max).unwrap_or(usize::MAX);
    let content = match Limited::new(req.into_body(), limit).collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => {
            s.audit(actor, client, "upload", request, 0, "error").await;
            return too_large();
        }
        Err(_) => {
            s.audit(actor, client, "upload", request, 0, "error").await;
            return fail(StatusCode::BAD_REQUEST, "interrupted", "the upload was interrupted; try again");
        }
    };
    if content.is_empty() {
        s.audit(actor, client, "upload", request, 0, "error").await;
        return fail(StatusCode::BAD_REQUEST, "empty", "the file is empty");
    }

    let app = s.app.clone();
    let result = tokio::task::spawn_blocking(move || {
        let app = app.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = import(&app, name.clone(), &content);
        let (rows, outcome) = match &result {
            Ok(v) => (v["bytes"].as_i64().unwrap_or(0), "ok"),
            Err(_) => (content.len() as i64, "error"),
        };
        if let Err(e) = record(&app, actor, &client, "upload", &request, rows, outcome) {
            eprintln!("warning: could not write the audit log: {e:#}");
        }
        result
    })
    .await;
    match result {
        Ok(Ok(report)) => {
            let status = if report["created"] == true { StatusCode::CREATED } else { StatusCode::OK };
            (status, Json(report)).into_response()
        }
        Ok(Err(e)) => fail(StatusCode::INTERNAL_SERVER_ERROR, "import_failed", format!("could not store the file: {e:#}")),
        Err(_) => fail(StatusCode::INTERNAL_SERVER_ERROR, "import_failed", "the import worker failed"),
    }
}

/// Stores the file, then processes. A processing failure is reported next to
/// the stored raw object instead of hiding that the file was kept.
fn import(app: &App, name: Option<String>, content: &[u8]) -> anyhow::Result<Value> {
    let report = app.import_auto(name.clone(), content, false)?;
    let errors: Vec<Value> = match app.process(false) {
        Ok(p) => p
            .errors()
            .filter(|r| r.raw_id.is_none() || r.raw_id.as_deref() == Some(report.raw_id.as_str()))
            .map(|r| json!({ "processor": r.processor, "status": r.status, "error": r.error }))
            .collect(),
        Err(e) => vec![json!({ "processor": "pipeline", "status": "error", "error": format!("{e:#}") })],
    };
    Ok(json!({
        "raw_id": report.raw_id,
        "created": report.created,
        "bytes": report.bytes,
        "source": report.source,
        "kind": report.kind,
        "name": name,
        "errors": errors,
    }))
}

/// The file name from `X-File-Name` (percent-encoded) or `?name=`, reduced to
/// its last path component without control characters.
fn file_name(headers: &HeaderMap, query: Option<String>) -> Result<Option<String>, String> {
    let raw = match headers.get(FILE_NAME_HEADER) {
        Some(v) => {
            let v = v.to_str().map_err(|_| "X-File-Name must be percent-encoded ASCII".to_string())?;
            percent_decode(v).ok_or_else(|| "X-File-Name is not valid percent-encoded UTF-8".to_string())?
        }
        None => match query {
            Some(q) => q,
            None => return Ok(None),
        },
    };
    let last = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = last.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
    let clean = clean.trim();
    Ok((!clean.is_empty() && clean != "." && clean != "..").then(|| clean.to_string()))
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn human_bytes(n: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if n >= MIB && n.is_multiple_of(MIB) {
        format!("{} MiB", n / MIB)
    } else {
        format!("{n} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_decoded_and_reduced_to_their_last_component() {
        let mut h = HeaderMap::new();
        h.insert(FILE_NAME_HEADER, HeaderValue::from_static("%E4%BD%93%E6%A3%80%20report.pdf"));
        assert_eq!(file_name(&h, None).unwrap().as_deref(), Some("体检 report.pdf"));
        h.insert(FILE_NAME_HEADER, HeaderValue::from_static("..%2F..%2Fetc%2Fpasswd"));
        assert_eq!(file_name(&h, None).unwrap().as_deref(), Some("passwd"));
        h.insert(FILE_NAME_HEADER, HeaderValue::from_static("a%0Ab.txt"));
        assert_eq!(file_name(&h, None).unwrap().as_deref(), Some("ab.txt"));
        h.insert(FILE_NAME_HEADER, HeaderValue::from_static("%ZZ"));
        assert!(file_name(&h, None).is_err());
        assert_eq!(file_name(&HeaderMap::new(), Some("C:\\x\\y.csv".into())).unwrap().as_deref(), Some("y.csv"));
        assert_eq!(file_name(&HeaderMap::new(), None).unwrap(), None);
    }

    #[test]
    fn codes_are_eight_digits_and_rotate_after_use() {
        let uploads = Uploads::new(UploadSettings::default());
        let code = uploads.pairing_code();
        assert_eq!(code.len(), 9);
        assert!(code.chars().enumerate().all(|(i, c)| if i == 4 { c == '-' } else { c.is_ascii_digit() }));
        let paired = uploads.pair(&code.replace('-', " ")).unwrap();
        assert_eq!(uploads.check_session(&paired.token), SessionCheck::Valid(paired.label));
        assert_ne!(uploads.pairing_code(), code);
        assert_eq!(uploads.pair(&code), Err(PairError::Wrong));
    }

    #[test]
    fn too_many_wrong_codes_replace_the_code() {
        let uploads = Uploads::new(UploadSettings::default());
        let code = uploads.pairing_code();
        for _ in 0..5 {
            assert_eq!(uploads.pair("not it"), Err(PairError::Wrong));
        }
        assert_ne!(uploads.pairing_code(), code);
    }
}
