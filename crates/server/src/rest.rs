//! REST API (`/v1`). Loopback by default, bearer-token authenticated.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
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

const ACTOR: &str = "rest";
const CLIENT: &str = "bearer-token";

#[derive(Clone)]
struct AppState {
    app: Arc<Mutex<App>>,
    token: Arc<String>,
}

pub fn router(app: App) -> Result<Router> {
    let token = app.api_token()?;
    let state = AppState {
        app: Arc::new(Mutex::new(app)),
        token: Arc::new(token),
    };
    Ok(Router::new()
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
        .route("/v1/query/sql", post(sql))
        .route("/v1/audit", get(audit))
        .route("/v1/raw/{id}", get(raw))
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .route("/v1/health", get(|| async { Json(json!({ "ok": true })) }))
        .with_state(state))
}

/// Binds `addr` (failing on a conflict rather than picking another port) and serves.
pub async fn serve(app: App, addr: SocketAddr) -> Result<()> {
    if !addr.ip().is_loopback() {
        eprintln!("warning: binding {addr}; MirrorSelf should listen on loopback and be exposed only through a tunnel");
    }
    let router = router(app)?;
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            conflict(format!("{addr} is already in use; stop the other process or pass --addr"))
        } else {
            anyhow::Error::new(e).context(format!("binding {addr}"))
        }
    })?;
    eprintln!("mirrorself REST API listening on http://{addr}/v1");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("serving the REST API")
}

async fn require_token(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(t) if constant_time_eq(t.as_bytes(), state.token.as_bytes()) => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            Json(json!({ "error": "missing or invalid bearer token" })),
        )
            .into_response(),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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
