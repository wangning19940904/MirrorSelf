//! Phone upload: pairing, upload-only sessions, `POST /v1/upload`, the LAN
//! router and the audit log.

use std::net::SocketAddr;
use std::time::Duration;

use mirrorself_core::config::Home;
use mirrorself_core::App;
use mirrorself_server::rest::Hub;
use mirrorself_server::upload::UploadSettings;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const FIXTURE: &[u8] = include_bytes!("../../../fixtures/apple-health/export.xml");
const PDF: &[u8] = b"%PDF-1.7\n% synthetic checkup report\n";

struct Server {
    _dir: tempfile::TempDir,
    home: Home,
    token: String,
    hub: Hub,
    /// The loopback router: read API plus upload routes.
    api: SocketAddr,
    /// The LAN router: upload page, pairing and upload only.
    lan: SocketAddr,
}

impl Server {
    /// Reopens the home to read what the server wrote.
    fn app(&self) -> App {
        App::open(self.home.clone()).unwrap()
    }

    fn audit(&self) -> Vec<Value> {
        let audit = self.app().query().audit(Some(100)).unwrap();
        audit["items"].as_array().unwrap().clone()
    }
}

async fn start(settings: UploadSettings) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    App::init(&home, "server", None).unwrap();
    let app = App::open(home.clone()).unwrap();
    let token = app.api_token().unwrap();
    let hub = Hub::with_settings(app, settings).unwrap();
    // Ephemeral loopback ports for the test only; the service itself uses
    // the registry's fixed ports (12500, and 12501 for the LAN page).
    let api = serve(hub.router()).await;
    let lan = serve(hub.lan_router()).await;
    Server { _dir: dir, home, token, hub, api, lan }
}

async fn serve(router: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = router.into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    addr
}

struct Reply {
    status: u16,
    head: String,
    body: Value,
    text: String,
}

async fn send(addr: SocketAddr, head: String, body: &[u8]) -> Reply {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    Reply {
        status: head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        head: head.to_ascii_lowercase(),
        body: serde_json::from_str(body).unwrap_or(Value::Null),
        text: body.to_string(),
    }
}

async fn upload(addr: SocketAddr, credential: Option<&str>, name: &str, content: &[u8]) -> Reply {
    let auth = credential.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    let head = format!(
        "POST /v1/upload HTTP/1.1\r\nHost: localhost\r\n{auth}X-File-Name: {name}\r\n\
         Content-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        content.len()
    );
    send(addr, head, content).await
}

async fn pair(addr: SocketAddr, code: &str) -> Reply {
    let body = json!({ "code": code }).to_string();
    let head = format!(
        "POST /v1/pair HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    send(addr, head, body.as_bytes()).await
}

async fn get(addr: SocketAddr, path: &str, credential: Option<&str>) -> Reply {
    let auth = credential.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    send(addr, format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Connection: close\r\n\r\n"), b"").await
}

async fn paired_session(s: &Server) -> String {
    let r = pair(s.lan, &s.hub.pairing_code()).await;
    assert_eq!(r.status, 200, "{}", r.text);
    r.body["session"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn bearer_token_upload_recognises_sources_and_reports_duplicates() {
    let s = start(UploadSettings::default()).await;

    let r = upload(s.api, Some(&s.token), "export.xml", FIXTURE).await;
    assert_eq!(r.status, 201, "{}", r.text);
    assert_eq!(r.body["created"], true);
    assert_eq!(r.body["source"], "apple_health");
    assert_eq!(r.body["kind"], "apple_health_export");
    assert_eq!(r.body["bytes"], FIXTURE.len() as u64);
    assert_eq!(r.body["name"], "export.xml");
    assert_eq!(r.body["errors"], json!([]));
    let raw_id = r.body["raw_id"].as_str().unwrap().to_string();

    // The pipeline ran: the fixture's steps are queryable.
    let count: i64 = s
        .app()
        .conn()
        .query_row("SELECT count(*) FROM observations WHERE raw_id = ?1", [&raw_id], |r| r.get(0))
        .unwrap();
    assert!(count > 0);

    let again = upload(s.api, Some(&s.token), "export-copy.xml", FIXTURE).await;
    assert_eq!(again.status, 200, "{}", again.text);
    assert_eq!(again.body["created"], false);
    assert_eq!(again.body["raw_id"], raw_id.as_str());

    // A percent-encoded UTF-8 name, and PDF magic without a known source.
    let pdf = upload(s.api, Some(&s.token), "%E4%BD%93%E6%A3%80.pdf", PDF).await;
    assert_eq!(pdf.status, 201, "{}", pdf.text);
    assert_eq!((pdf.body["source"].as_str(), pdf.body["kind"].as_str()), (Some("file"), Some("file_pdf")));
    assert_eq!(pdf.body["name"], "体检.pdf");

    let raw = s.app().raw_list().unwrap();
    assert_eq!(raw["count"], 2);

    let audit = s.audit();
    let uploads: Vec<&Value> = audit.iter().filter(|e| e["action"] == "upload").collect();
    assert_eq!(uploads.len(), 3);
    assert!(uploads.iter().all(|e| e["actor"] == "rest" && e["client_id"] == "bearer-token"));
    assert!(uploads.iter().all(|e| e["outcome"] == "ok" && e["scope"] == "upload:write"));
    let mut sizes: Vec<i64> = uploads.iter().map(|e| e["rows"].as_i64().unwrap()).collect();
    sizes.sort();
    assert_eq!(sizes, vec![PDF.len() as i64, FIXTURE.len() as i64, FIXTURE.len() as i64]);
    // File names are personal: the log holds a digest, never the name.
    assert!(!serde_json::to_string(&audit).unwrap().contains("体检"));

    // Without a credential nothing is stored.
    let r = upload(s.api, None, "x.pdf", b"%PDF-1.4 other").await;
    assert_eq!(r.status, 401);
    assert_eq!(r.body["code"], "pair_required");
    assert_eq!(s.app().raw_list().unwrap()["count"], 2);
}

#[tokio::test]
async fn pairing_gives_an_upload_session_on_the_lan_router() {
    let s = start(UploadSettings::default()).await;
    let r = pair(s.lan, &s.hub.pairing_code()).await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert!(r.head.contains("cache-control: no-store"));
    let session = r.body["session"].as_str().unwrap();
    assert_eq!(session.len(), 64);
    assert_ne!(session, s.token);
    assert_eq!(r.body["expires_in"], 12 * 3600);
    assert_eq!(r.body["max_bytes"], 256 * 1024 * 1024);

    let r = upload(s.lan, Some(session), "checkup.pdf", PDF).await;
    assert_eq!(r.status, 201, "{}", r.text);
    assert_eq!(r.body["kind"], "file_pdf");
    // The same session works on loopback too.
    let r = upload(s.api, Some(session), "export.xml", FIXTURE).await;
    assert_eq!(r.status, 201, "{}", r.text);
    assert_eq!(r.body["kind"], "apple_health_export");

    let audit = s.audit();
    let pairing = audit.iter().find(|e| e["action"] == "pair").unwrap();
    assert_eq!(pairing["actor"], "upload-page");
    assert_eq!(pairing["outcome"], "ok");
    let client = pairing["client_id"].as_str().unwrap();
    assert!(client.starts_with("session-") && client.ends_with("@127.0.0.1"), "{client}");
    let uploads: Vec<&Value> = audit.iter().filter(|e| e["action"] == "upload").collect();
    assert_eq!(uploads.len(), 2);
    assert!(uploads.iter().all(|e| e["actor"] == "upload-page" && e["client_id"] == client && e["outcome"] == "ok"));

    let r = get(s.lan, "/v1/session", Some(session)).await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(r.body["max_bytes"], 256 * 1024 * 1024);
    assert!(r.body["expires_in"].as_u64().unwrap() > 12 * 3600 - 60);

    // Unpairing ends the session.
    let head = format!(
        "POST /v1/unpair HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {session}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(send(s.lan, head, b"").await.status, 200);
    let r = upload(s.lan, Some(session), "other.pdf", b"%PDF-1.4 other").await;
    assert_eq!(r.status, 401);
    assert!(s.audit().iter().any(|e| e["action"] == "unpair" && e["client_id"] == client));
    assert_eq!(get(s.lan, "/v1/session", Some(session)).await.status, 401);
    assert_eq!(get(s.lan, "/v1/session", Some(&s.token)).await.status, 401, "the API token is not a session");
}

#[tokio::test]
async fn wrong_and_reused_pairing_codes_are_refused() {
    let s = start(UploadSettings::default()).await;
    let code = s.hub.pairing_code();
    let wrong = if code.starts_with('0') { "1111-1111" } else { "0000-0000" };
    let r = pair(s.lan, wrong).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.body["code"], "wrong_code");
    let r = pair(s.lan, "").await;
    assert_eq!(r.status, 401);

    // The right code works once, with or without its dash.
    let r = pair(s.lan, &code.replace('-', "")).await;
    assert_eq!(r.status, 200, "{}", r.text);
    let r = pair(s.lan, &code).await;
    assert_eq!(r.status, 401, "a code works once");
    assert_ne!(s.hub.pairing_code(), code);

    let head = "POST /v1/pair HTTP/1.1\r\nHost: localhost\r\nContent-Length: 9\r\nConnection: close\r\n\r\n".to_string();
    assert_eq!(send(s.lan, head, b"{not json").await.status, 400);

    let audit = s.audit();
    let pairs: Vec<&str> = audit
        .iter()
        .filter(|e| e["action"] == "pair")
        .map(|e| e["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(pairs.iter().filter(|o| **o == "denied").count(), 3);
    assert_eq!(pairs.iter().filter(|o| **o == "ok").count(), 1);
    assert!(audit
        .iter()
        .filter(|e| e["action"] == "pair" && e["outcome"] == "denied")
        .all(|e| e["client_id"] == "unpaired@127.0.0.1"));
}

#[tokio::test]
async fn pairing_attempts_are_rate_limited_and_wrong_guesses_replace_the_code() {
    let s = start(UploadSettings::default()).await;
    let first = s.hub.pairing_code();
    for _ in 0..5 {
        assert_eq!(pair(s.lan, "abc").await.status, 401);
    }
    // Five wrong guesses retire the code.
    let second = s.hub.pairing_code();
    assert_ne!(second, first);
    assert_eq!(pair(s.lan, &first).await.status, 401);
    for _ in 0..4 {
        assert_eq!(pair(s.lan, "abc").await.status, 401);
    }
    // Ten attempts a minute, from everyone together; even the right code waits.
    let r = pair(s.lan, &s.hub.pairing_code()).await;
    assert_eq!(r.status, 429, "{}", r.text);
    assert_eq!(r.body["code"], "rate_limited");
    assert!(r.head.contains("retry-after: "));
}

#[tokio::test]
async fn unused_codes_and_sessions_expire() {
    let s = start(UploadSettings {
        session_ttl: Duration::from_millis(300),
        code_ttl: Duration::from_millis(300),
        ..UploadSettings::default()
    })
    .await;
    let session = paired_session(&s).await;
    assert_eq!(upload(s.lan, Some(&session), "a.pdf", PDF).await.status, 201);
    let code = s.hub.pairing_code();
    tokio::time::sleep(Duration::from_millis(400)).await;

    let r = upload(s.lan, Some(&session), "b.pdf", b"%PDF-1.4 later").await;
    assert_eq!(r.status, 401, "{}", r.text);
    assert_eq!(r.body["code"], "pair_required");
    assert!(r.body["error"].as_str().unwrap().contains("expired"));

    let r = pair(s.lan, &code).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.body["code"], "code_expired");
    // A fresh code was issued in its place and works.
    assert_ne!(s.hub.pairing_code(), code);
    paired_session(&s).await;
}

#[tokio::test]
async fn the_size_limit_is_enforced_without_reading_past_it() {
    let s = start(UploadSettings { max_bytes: 1024, ..UploadSettings::default() }).await;
    let big = vec![b'x'; 1500];

    // Declared too large: refused before the body is read (none is sent).
    let head = format!(
        "POST /v1/upload HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nX-File-Name: big.bin\r\n\
         Content-Length: 5000000\r\nConnection: close\r\n\r\n",
        s.token
    );
    let r = send(s.api, head, b"").await;
    assert_eq!(r.status, 413, "{}", r.text);
    assert_eq!(r.body["code"], "too_large");
    assert!(r.body["error"].as_str().unwrap().contains("1024 bytes"));

    // No length declared: the stream is cut off at the limit.
    let mut chunked = format!("{:x}\r\n", big.len()).into_bytes();
    chunked.extend_from_slice(&big);
    chunked.extend_from_slice(b"\r\n0\r\n\r\n");
    let head = format!(
        "POST /v1/upload HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nX-File-Name: big.bin\r\n\
         Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        s.token
    );
    let r = send(s.api, head, &chunked).await;
    assert_eq!(r.status, 413, "{}", r.text);

    // At the limit is fine; empty is not.
    let r = upload(s.api, Some(&s.token), "fits.txt", &big[..1024]).await;
    assert_eq!(r.status, 201, "{}", r.text);
    assert_eq!(r.body["kind"], "file_text");
    let r = upload(s.api, Some(&s.token), "empty.txt", b"").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.body["code"], "empty");

    assert_eq!(s.app().raw_list().unwrap()["count"], 1);
    let outcomes: Vec<(String, i64)> = s
        .audit()
        .iter()
        .filter(|e| e["action"] == "upload")
        .map(|e| (e["outcome"].as_str().unwrap().to_string(), e["rows"].as_i64().unwrap()))
        .collect();
    assert_eq!(outcomes.len(), 4);
    assert_eq!(outcomes.iter().filter(|(o, _)| o == "error").count(), 3);
    assert!(outcomes.contains(&("ok".to_string(), 1024)));
}

#[tokio::test]
async fn upload_sessions_cannot_read_and_the_lan_router_only_uploads() {
    let s = start(UploadSettings::default()).await;
    let session = paired_session(&s).await;

    // Loopback read API: the session is recognised and refused.
    let r = get(s.api, "/v1/overview", Some(&session)).await;
    assert_eq!(r.status, 403, "{}", r.text);
    assert_eq!(r.body["error"], "this credential can only upload files");
    let r = get(s.api, "/v1/audit", Some(&session)).await;
    assert_eq!(r.status, 403);
    assert_eq!(get(s.api, "/v1/overview", Some(&s.token)).await.status, 200);

    // LAN: no read routes at all, and the API token is not accepted.
    assert_eq!(get(s.lan, "/v1/overview", Some(&s.token)).await.status, 404);
    assert_eq!(get(s.lan, "/v1/raw/x", Some(&s.token)).await.status, 404);
    assert_eq!(get(s.lan, "/v1/health", None).await.status, 404);
    let r = upload(s.lan, Some(&s.token), "a.pdf", PDF).await;
    assert_eq!(r.status, 403, "{}", r.text);
    assert_eq!(r.body["code"], "token_not_allowed");
    assert_eq!(s.app().raw_list().unwrap()["count"], 0);

    // The page itself is public, self-contained and locked down.
    let r = get(s.lan, "/upload", None).await;
    assert_eq!(r.status, 200);
    assert!(r.head.contains("content-type: text/html"));
    assert!(r.head.contains("content-security-policy: default-src 'none'"));
    assert!(r.text.contains("Pairing code"));
    assert!(!r.text.contains("http://") && !r.text.contains("https://"), "the page makes no external requests");
    let r = get(s.lan, "/", None).await;
    assert_eq!(r.status, 303);
    assert!(r.head.contains("location: /upload"));
    assert_eq!(get(s.api, "/upload", None).await.status, 200);
}
