//! REST API (`/v1`). Loopback by default, bearer-token authenticated.
//!
//! The phone upload routes (`/upload`, `/v1/pair`, `/v1/upload`; see
//! [`crate::upload`]) are served here too and, when `server.lan_addr` is set,
//! alone on a second listener on the local network.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mirrorself_core::audit;
use mirrorself_core::error::{conflict, invalid, user_error, UserError};
use mirrorself_core::ops::Edit;
use mirrorself_core::query::ReviewQuery;
use mirrorself_core::{App, Decision};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::tools;
use crate::upload::{self, SessionCheck, UploadSettings, UploadState, Uploads};

pub(crate) const ACTOR: &str = "rest";
pub(crate) const CLIENT: &str = "bearer-token";
/// Body bytes read and discarded before refusing an unauthenticated request.
const REFUSED_BODY_DRAIN: u64 = 1024 * 1024;

#[derive(Clone)]
struct AppState {
    app: Arc<Mutex<App>>,
    token: Arc<String>,
    uploads: Arc<Uploads>,
}

/// One opened home behind the REST API: the loopback router, the LAN upload
/// router and the in-memory pairing state they share.
pub struct Hub {
    state: AppState,
}

impl Hub {
    /// Upload limits from `[server]` in `mirrorself.toml`.
    pub fn new(app: App) -> Result<Self> {
        let settings = UploadSettings::from_config(&app.config().server);
        Self::with_settings(app, settings)
    }

    pub fn with_settings(app: App, settings: UploadSettings) -> Result<Self> {
        let token = app.api_token()?;
        Ok(Self {
            state: AppState {
                app: Arc::new(Mutex::new(app)),
                token: Arc::new(token),
                uploads: Arc::new(Uploads::new(settings)),
            },
        })
    }

    /// The current one-time pairing code (`1234-5678`).
    pub fn pairing_code(&self) -> String {
        self.state.uploads.pairing_code()
    }

    pub fn uploads(&self) -> &Uploads {
        &self.state.uploads
    }

    /// Everything: the read API (API token only) and the upload routes.
    pub fn router(&self) -> Router {
        let state = self.state.clone();
        Router::new()
            .route("/v1/overview", get(overview))
            .route("/v1/timeline", get(timeline))
            .route("/v1/metrics", get(codes))
            .route("/v1/metrics/{code}/series", get(series))
            .route("/v1/metrics/{code}/compare", get(compare))
            .route("/v1/labs", get(labs))
            .route("/v1/labs/compare", get(labs_compare))
            .route("/v1/documents", get(documents))
            .route("/v1/documents/{id}", get(document))
            .route("/v1/review", get(review_list))
            .route("/v1/review/{id}", post(review_decide))
            .route("/v1/entities", get(entities))
            .route("/v1/entities/{id}/links", get(entity_links))
            .route("/v1/search", get(search))
            .route("/v1/transactions", get(transactions))
            .route("/v1/items", get(items))
            .route("/v1/query/sql", post(sql))
            .route("/v1/audit", get(audit))
            .route("/v1/raw/{id}", get(raw))
            .layer(middleware::from_fn_with_state(state.clone(), require_token))
            .route("/v1/health", get(|| async { Json(json!({ "ok": true })) }))
            .with_state(state)
            .merge(upload::routes(self.upload_state(false)))
    }

    /// The upload page, pairing and upload only, for `server.lan_addr`. The
    /// read API does not exist here, and the API token is refused.
    pub fn lan_router(&self) -> Router {
        Router::new()
            .route("/", get(|| async { Redirect::to("/upload") }))
            .merge(upload::routes(self.upload_state(true)))
    }

    fn upload_state(&self, lan: bool) -> UploadState {
        UploadState {
            app: self.state.app.clone(),
            token: self.state.token.clone(),
            uploads: self.state.uploads.clone(),
            lan,
        }
    }
}

/// The loopback router for `app` (read API plus upload routes).
pub fn router(app: App) -> Result<Router> {
    Ok(Hub::new(app)?.router())
}

/// Binds `addr` and, if `server.lan_addr` is set, the LAN upload address
/// (failing on a conflict rather than picking another port), prints the
/// pairing code, and serves both until Ctrl-C.
pub async fn serve(app: App, addr: SocketAddr) -> Result<()> {
    let lan_addr = match app.config().server.lan_addr.as_deref() {
        Some(a) => Some(
            a.parse::<SocketAddr>()
                .map_err(|e| invalid(format!("bad server.lan_addr '{a}': {e}")))?,
        ),
        None => None,
    };
    if !addr.ip().is_loopback() {
        eprintln!("warning: binding {addr}; MirrorSelf should listen on loopback and be exposed only through a tunnel");
    }
    if let Some(lan) = lan_addr {
        if lan.port() == addr.port() {
            return Err(invalid(format!(
                "server.lan_addr ({lan}) must use a different port from the API ({addr}); 12501 is reserved for it"
            )));
        }
        if !lan.ip().is_loopback() {
            eprintln!(
                "warning: the upload page is reachable from the local network at {lan} over plain HTTP; \
                 anyone on that network can see uploads in transit (see docs/upload.md)"
            );
        }
    }
    let mut settings = UploadSettings::from_config(&app.config().server);
    settings.announce = true;
    let hub = Hub::with_settings(app, settings)?;

    let listener = bind(addr).await?;
    let lan_listener = match lan_addr {
        Some(a) => Some(bind(a).await?),
        None => None,
    };
    eprintln!("mirrorself REST API listening on http://{addr}/v1");
    eprintln!("upload page on this computer: http://{addr}/upload");
    if let Some(lan) = lan_addr {
        eprintln!("upload page for your phone:   http://{}/upload", display_addr(lan));
    }
    eprintln!(
        "pairing code: {}  (enter it on the upload page; it works once, and a new one is printed after each use)",
        hub.pairing_code()
    );

    let api = axum::serve(listener, hub.router().into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown());
    match lan_listener {
        None => api.await.context("serving the REST API"),
        Some(l) => {
            let lan = axum::serve(l, hub.lan_router().into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(shutdown());
            tokio::try_join!(
                async { api.await.context("serving the REST API") },
                async { lan.await.context("serving the LAN upload page") },
            )?;
            Ok(())
        }
    }
}

async fn bind(addr: SocketAddr) -> Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            conflict(format!("{addr} is already in use; stop the other process or change the address"))
        } else {
            anyhow::Error::new(e).context(format!("binding {addr}"))
        }
    })
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

/// For a wildcard LAN address, the address other devices most likely reach
/// this machine at: the local side of a UDP socket "connected" towards a
/// documentation address. Connecting a UDP socket sends no packets.
fn display_addr(addr: SocketAddr) -> String {
    if !addr.ip().is_unspecified() {
        return addr.to_string();
    }
    let guess = std::net::UdpSocket::bind(("0.0.0.0", 0))
        .and_then(|s| s.connect(("198.51.100.1", 9)).map(|_| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip())
        .ok()
        .filter(|ip: &IpAddr| !ip.is_unspecified() && !ip.is_loopback());
    match guess {
        Some(ip) => SocketAddr::new(ip, addr.port()).to_string(),
        None => format!("<this computer's LAN address>:{}", addr.port()),
    }
}

async fn require_token(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let refusal = match upload::bearer(req.headers()) {
        Some(t) if constant_time_eq(t.as_bytes(), state.token.as_bytes()) => return next.run(req).await,
        Some(t) if matches!(state.uploads.check_session(t), SessionCheck::Valid(_)) => (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "this credential can only upload files" })),
        )
            .into_response(),
        _ => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            Json(json!({ "error": "missing or invalid bearer token" })),
        )
            .into_response(),
    };
    // Let the client read the refusal instead of a connection reset.
    upload::drain(req.into_body(), REFUSED_BODY_DRAIN).await;
    refusal
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn error_response(err: anyhow::Error) -> Response {
    let (status, message) = match user_error(&err) {
        Some(UserError::NotFound(m)) => (StatusCode::NOT_FOUND, m.clone()),
        Some(UserError::Invalid(m)) => (StatusCode::BAD_REQUEST, m.clone()),
        Some(UserError::Denied(m)) => (StatusCode::FORBIDDEN, m.clone()),
        Some(UserError::Conflict(m)) => (StatusCode::CONFLICT, m.clone()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")),
    };
    (status, Json(json!({ "error": message }))).into_response()
}

/// Runs `f` on a blocking thread with the app locked.
async fn blocking<T, F>(state: &AppState, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&App) -> Result<T> + Send + 'static,
{
    let app = state.app.clone();
    tokio::task::spawn_blocking(move || {
        let guard = app.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&guard)
    })
    .await
    .context("worker thread failed")?
}

async fn tool(state: &AppState, name: &'static str, args: Value) -> Response {
    match blocking(state, move |app| tools::call(app, ACTOR, CLIENT, name, &args)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(e),
    }
}

/// `types=a,b` and `kinds=a,b` arrive as comma-separated strings.
fn split(v: Option<String>) -> Value {
    json!(v
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>())
}

#[derive(Deserialize)]
struct TimelineParams {
    from: Option<String>,
    to: Option<String>,
    types: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

#[derive(Deserialize)]
struct SeriesParams {
    from: Option<String>,
    to: Option<String>,
    bucket: Option<String>,
    stat: Option<String>,
}

#[derive(Deserialize)]
struct CompareParams {
    a_from: String,
    a_to: String,
    b_from: String,
    b_to: String,
    stat: Option<String>,
}

#[derive(Deserialize)]
struct LabParams {
    code: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    from: Option<String>,
    to: Option<String>,
    kinds: Option<String>,
    limit: Option<u32>,
}

#[derive(Deserialize)]
struct TransactionParams {
    from: Option<String>,
    to: Option<String>,
    direction: Option<String>,
    counterparty: Option<String>,
    counted_only: Option<bool>,
    limit: Option<u32>,
    offset: Option<u32>,
}

#[derive(Deserialize)]
struct ItemParams {
    from: Option<String>,
    to: Option<String>,
    types: Option<String>,
    status: Option<String>,
    located_only: Option<bool>,
    include_text: Option<bool>,
    limit: Option<u32>,
    offset: Option<u32>,
}

#[derive(Deserialize)]
struct EntityParams {
    q: Option<String>,
}

#[derive(Deserialize)]
struct DocumentParams {
    include_text: Option<bool>,
}

#[derive(Deserialize)]
struct SqlBody {
    sql: String,
}

#[derive(Deserialize)]
struct CompareLabParams {
    code: Option<String>,
}

#[derive(Deserialize)]
struct ReviewParams {
    all: Option<bool>,
    document: Option<String>,
    limit: Option<u32>,
}

/// `POST /v1/review/{id}`: `{"action": "confirm" | "edit" | "reject", …}`.
#[derive(Deserialize)]
struct ReviewBody {
    action: String,
    #[serde(flatten)]
    edit: Edit,
    reason: Option<String>,
}

#[derive(Deserialize)]
struct AuditParams {
    limit: Option<u32>,
}

async fn overview(State(s): State<AppState>) -> Response {
    tool(&s, "get_overview", json!({})).await
}

async fn timeline(State(s): State<AppState>, Query(p): Query<TimelineParams>) -> Response {
    let args = json!({ "from": p.from, "to": p.to, "types": split(p.types), "limit": p.limit, "offset": p.offset });
    tool(&s, "query_timeline", args).await
}

async fn series(State(s): State<AppState>, Path(code): Path<String>, Query(p): Query<SeriesParams>) -> Response {
    let args = json!({ "metric": code, "from": p.from, "to": p.to, "bucket": p.bucket, "stat": p.stat });
    tool(&s, "get_metric_series", args).await
}

async fn compare(State(s): State<AppState>, Path(code): Path<String>, Query(p): Query<CompareParams>) -> Response {
    let args = json!({
        "metric": code,
        "period_a": { "from": p.a_from, "to": p.a_to },
        "period_b": { "from": p.b_from, "to": p.b_to },
        "stat": p.stat,
    });
    tool(&s, "compare_periods", args).await
}

async fn labs(State(s): State<AppState>, Query(p): Query<LabParams>) -> Response {
    tool(&s, "get_lab_results", json!({ "analyte": p.code, "from": p.from, "to": p.to })).await
}

async fn labs_compare(State(s): State<AppState>, Query(p): Query<CompareLabParams>) -> Response {
    tool(&s, "compare_lab_results", json!({ "analyte": p.code })).await
}

/// Runs `f` and records it in the audit log like a tool call.
async fn audited<F>(s: &AppState, action: &'static str, scope: &'static str, request: Value, f: F) -> Response
where
    F: FnOnce(&App) -> Result<Value> + Send + 'static,
{
    let result = blocking(s, move |app| {
        let result = f(app);
        let entry = audit::Entry { actor: ACTOR, client_id: CLIENT, action, scope, request: &request };
        audit::record(app.conn(), &entry, &result)?;
        result
    })
    .await;
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(e),
    }
}

async fn documents(State(s): State<AppState>) -> Response {
    audited(&s, "list_documents", "health:documents", json!({}), |app| app.query().documents()).await
}

async fn review_list(State(s): State<AppState>, Query(p): Query<ReviewParams>) -> Response {
    let request = json!({ "all": p.all, "document": p.document, "limit": p.limit });
    audited(&s, "review_list", "health:labs", request, move |app| {
        app.query().review_queue(&ReviewQuery { all: p.all.unwrap_or(false), document: p.document, limit: p.limit })
    })
    .await
}

/// Confirms, edits or rejects one extracted value. The decision is a user op:
/// it is kept in the vault and survives `process --rebuild`.
async fn review_decide(State(s): State<AppState>, Path(id): Path<String>, Json(body): Json<ReviewBody>) -> Response {
    let request = json!({ "id": id, "action": body.action });
    audited(&s, "review_decide", "health:write", request, move |app| {
        let decision = match body.action.as_str() {
            "confirm" => Decision::Confirm,
            "edit" => Decision::Edit(body.edit),
            "reject" => Decision::Reject { reason: body.reason },
            other => return Err(invalid(format!("action '{other}' must be confirm, edit or reject"))),
        };
        app.review(&id, decision, ACTOR)
    })
    .await
}

async fn document(State(s): State<AppState>, Path(id): Path<String>, Query(p): Query<DocumentParams>) -> Response {
    tool(&s, "get_document", json!({ "id": id, "include_text": p.include_text.unwrap_or(false) })).await
}

async fn search(State(s): State<AppState>, Query(p): Query<SearchParams>) -> Response {
    let args = json!({ "query": p.q, "from": p.from, "to": p.to, "kinds": split(p.kinds), "limit": p.limit });
    tool(&s, "search_records", args).await
}

async fn transactions(State(s): State<AppState>, Query(p): Query<TransactionParams>) -> Response {
    let args = json!({
        "from": p.from, "to": p.to, "direction": p.direction, "counterparty": p.counterparty,
        "counted_only": p.counted_only, "limit": p.limit, "offset": p.offset,
    });
    tool(&s, "query_transactions", args).await
}

async fn items(State(s): State<AppState>, Query(p): Query<ItemParams>) -> Response {
    let args = json!({
        "from": p.from, "to": p.to, "types": split(p.types), "status": p.status,
        "located_only": p.located_only, "include_text": p.include_text, "limit": p.limit, "offset": p.offset,
    });
    tool(&s, "query_items", args).await
}

async fn entity_links(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    tool(&s, "get_entity", json!({ "name": id })).await
}

async fn sql(State(s): State<AppState>, Json(body): Json<SqlBody>) -> Response {
    tool(&s, "run_sql", json!({ "sql": body.sql })).await
}

async fn entities(State(s): State<AppState>, Query(p): Query<EntityParams>) -> Response {
    let result = blocking(&s, move |app| {
        let result = app.query().entities(p.q.as_deref());
        let request = json!({ "q": p.q });
        let entry = audit::Entry { actor: ACTOR, client_id: CLIENT, action: "list_entities", scope: "health:read", request: &request };
        audit::record(app.conn(), &entry, &result)?;
        result
    })
    .await;
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(e),
    }
}

async fn codes(State(s): State<AppState>) -> Response {
    match blocking(&s, |app| app.query().codes()).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(e),
    }
}

async fn audit(State(s): State<AppState>, Query(p): Query<AuditParams>) -> Response {
    match blocking(&s, move |app| app.query().audit(p.limit)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(e),
    }
}

/// The original file, decrypted from the vault.
async fn raw(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    let result = blocking(&s, move |app| {
        let result = app.raw_original(&id);
        let request = json!({ "id": id });
        let entry = audit::Entry { actor: ACTOR, client_id: CLIENT, action: "get_raw", scope: "health:raw", request: &request };
        let outcome = result.as_ref().map(|_| json!({})).map_err(|e| anyhow::anyhow!("{e:#}"));
        audit::record(app.conn(), &entry, &outcome)?;
        result
    })
    .await;
    match result {
        Ok((meta, bytes)) => {
            let mut resp = bytes.into_response();
            if let Ok(v) = HeaderValue::from_str(&meta.mime) {
                resp.headers_mut().insert(header::CONTENT_TYPE, v);
            }
            let filename = meta.name.unwrap_or_else(|| "original".into()).replace('"', "");
            if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
                resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
            }
            resp
        }
        Err(e) => error_response(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_lan_addresses_are_shown_as_a_reachable_address() {
        let shown = display_addr("0.0.0.0:12501".parse().unwrap());
        assert!(shown.ends_with(":12501"), "{shown}");
        assert!(!shown.starts_with("0.0.0.0"), "{shown}");
        assert_eq!(display_addr("192.168.1.20:12501".parse().unwrap()), "192.168.1.20:12501");
    }
}
