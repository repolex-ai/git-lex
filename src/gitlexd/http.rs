//! gitlexd's HTTP interface. Everything is under localhost:PORT.
//!
//! | `GET`/`POST /soul/<genesis>/sparql` | W3C SPARQL protocol over that soul's store |
//! | `GET /soul/<genesis>/info`          | synced-to commit, sync in flight, counts, graphs |
//! | `POST /soul/<genesis>/sync`         | nudge; `?wait=1` returns when the sync is done |
//! | `GET /souls`                        | every soul held, with its state |
//! | `GET /health`                       | up, uptime, souls open |
//!
//! `<genesis>` is the soul's first-commit hash, full or an unambiguous
//! prefix. A query for a soul whose sync is in flight waits for it.
//!
//! Queries see the union of every named graph unless they say GRAPH
//! themselves (`w3c_query_union_at`): the one door has no empty default
//! graph to fall into.

use super::daemon::{Daemon, FindError, Soul};
use axum::extract::{Path as AxPath, Query as AxQuery, Request, State};
use axum::middleware::{self, Next};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use std::collections::HashMap;
use std::sync::Arc;

fn err(status: StatusCode, msg: String) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// The soul a request names. A miss looks at the registry once more
/// before it is a miss: a repository initialized after gitlexd started
/// joins here, on the first request that names it.
fn find(d: &Arc<Daemon>, genesis: &str) -> Result<Arc<Soul>, Box<Response>> {
    let found = match d.find(genesis) {
        Err(FindError::NotFound) if d.refresh() > 0 => d.find(genesis),
        other => other,
    };
    found.map_err(|e| Box::new(match e {
        FindError::NotFound => err(
            StatusCode::NOT_FOUND,
            format!("no soul with first commit {genesis} — GET /souls lists the ones gitlexd holds"),
        ),
        FindError::Ambiguous(all) => err(
            StatusCode::CONFLICT,
            format!("{genesis} matches more than one soul: {}", all.join(", ")),
        ),
    }))
}

/// Run one query against a soul's store, waiting for a sync in flight, and
/// starting one when the soul has never been synced.
async fn run_query(d: Arc<Daemon>, genesis: String, query: String) -> Response {
    let soul = match find(&d, &genesis) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let mut synced_once = false;
    loop {
        let guard = Arc::clone(&soul.store).read_owned().await;
        if guard.is_none() {
            drop(guard);
            if synced_once {
                let st = soul.status();
                let why = st
                    .open_error
                    .or(st.last_error)
                    .unwrap_or_else(|| "the store does not exist and the sync did not create it".to_string());
                return err(StatusCode::SERVICE_UNAVAILABLE, format!("{}: no store: {why}", soul.short()));
            }
            soul.sync_and_wait().await;
            synced_once = true;
            continue;
        }
        let root = soul.path.clone();
        let q = query.clone();
        let out = tokio::task::spawn_blocking(move || {
            let store = guard.as_ref().expect("checked above");
            crate::w3c_query_union_at(Some(&root), store, &q)
        })
        .await;
        return match out {
            Ok(Ok(crate::W3cQueryOutcome::Solutions(v))) | Ok(Ok(crate::W3cQueryOutcome::Boolean(v))) => {
                ([(header::CONTENT_TYPE, "application/sparql-results+json")], v.to_string()).into_response()
            }
            Ok(Ok(crate::W3cQueryOutcome::Graph(nt))) => {
                ([(header::CONTENT_TYPE, "application/n-triples")], nt).into_response()
            }
            Ok(Err(crate::W3cQueryError::Parse(e))) => err(StatusCode::BAD_REQUEST, format!("SPARQL parse error: {e}")),
            Ok(Err(crate::W3cQueryError::Eval(e))) => {
                err(StatusCode::INTERNAL_SERVER_ERROR, format!("SPARQL evaluation error: {e}"))
            }
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("query task failed: {e}")),
        };
    }
}

async fn sparql_get(
    State(d): State<Arc<Daemon>>,
    AxPath(genesis): AxPath<String>,
    AxQuery(params): AxQuery<HashMap<String, String>>,
) -> Response {
    match params.get("query") {
        Some(q) => run_query(d, genesis, q.clone()).await,
        None => err(StatusCode::BAD_REQUEST, "missing ?query= parameter".to_string()),
    }
}

async fn sparql_post(
    State(d): State<Arc<Daemon>>,
    AxPath(genesis): AxPath<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let text = String::from_utf8_lossy(&body).to_string();
    let query = if ct.starts_with("application/sparql-query") {
        text
    } else if ct.starts_with("application/x-www-form-urlencoded") {
        match form_urlencoded::parse(body.as_ref()).find(|(k, _)| k == "query") {
            Some((_, q)) => q.to_string(),
            None => return err(StatusCode::BAD_REQUEST, "missing query= form field".to_string()),
        }
    } else if ct.starts_with("application/json") {
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => match v.get("query").and_then(|q| q.as_str()) {
                Some(q) => q.to_string(),
                None => return err(StatusCode::BAD_REQUEST, "JSON body needs a \"query\" field".to_string()),
            },
            Err(e) => return err(StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")),
        }
    } else {
        text
    };
    run_query(d, genesis, query).await
}

fn soul_json(s: &Soul) -> serde_json::Value {
    let st = s.status();
    serde_json::json!({
        "genesis": s.genesis,
        "path": s.path.display().to_string(),
        "name": s.name,
        "synced_to": st.synced_to,
        "syncing": st.syncing,
        "last_error": st.last_error,
        "open_error": st.open_error,
        "last_sync_ms": st.last_sync_ms,
        "syncs": st.syncs,
    })
}

async fn info(State(d): State<Arc<Daemon>>, AxPath(genesis): AxPath<String>) -> Response {
    let soul = match find(&d, &genesis) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let mut out = soul_json(&soul);
    let repo_yml = crate::layout::repo_yml(&soul.path);
    let y = crate::RepoYml::load(&soul.path);
    out["kit"] = serde_json::json!(y.kit);
    out["optional_kits"] = serde_json::json!(crate::read_repo_yml_optional_kits(&repo_yml));
    out["version"] = serde_json::json!(env!("CARGO_PKG_VERSION"));
    let guard = Arc::clone(&soul.store).read_owned().await;
    if guard.is_some() {
        let counts = tokio::task::spawn_blocking(move || {
            let store = guard.as_ref().expect("checked above");
            let quads = store.len().ok();
            let q = "SELECT ?g (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g ORDER BY DESC(?n)";
            let graphs = match crate::w3c_query(store, q) {
                Ok(crate::W3cQueryOutcome::Solutions(v)) => v,
                _ => serde_json::Value::Null,
            };
            (quads, graphs)
        })
        .await;
        if let Ok((quads, graphs)) = counts {
            out["quads"] = serde_json::json!(quads);
            out["graphs"] = graphs;
        }
    }
    Json(out).into_response()
}

async fn sync(
    State(d): State<Arc<Daemon>>,
    AxPath(genesis): AxPath<String>,
    AxQuery(params): AxQuery<HashMap<String, String>>,
) -> Response {
    let soul = match find(&d, &genesis) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let ticket = soul.request_sync();
    let wait = matches!(params.get("wait").map(|s| s.as_str()), Some("1") | Some("true"));
    if !wait {
        return (StatusCode::ACCEPTED, Json(serde_json::json!({ "accepted": true, "genesis": soul.genesis }))).into_response();
    }
    soul.wait_for(ticket).await;
    let st = soul.status();
    let status = if st.last_error.is_some() || st.open_error.is_some() {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    };
    (status, Json(soul_json(&soul))).into_response()
}

/// `DELETE /soul/{genesis}`: drop the soul. `git lex nuke` sends this
/// before it deletes `.lex/`, so the daemon closes the store first and
/// never syncs the repository again.
async fn forget(State(d): State<Arc<Daemon>>, AxPath(genesis): AxPath<String>) -> Response {
    let soul = match d.find(&genesis) {
        Ok(s) => s,
        Err(FindError::NotFound) => {
            return (StatusCode::OK, Json(serde_json::json!({ "forgotten": false, "reason": "not held" }))).into_response()
        }
        Err(FindError::Ambiguous(all)) => {
            return err(StatusCode::CONFLICT, format!("{genesis} matches more than one soul: {}", all.join(", ")))
        }
    };
    d.forget(&soul.genesis).await;
    (StatusCode::OK, Json(serde_json::json!({ "forgotten": true, "genesis": soul.genesis }))).into_response()
}

async fn souls(State(d): State<Arc<Daemon>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "souls": d.souls().iter().map(|s| soul_json(s)).collect::<Vec<_>>() }))
}

async fn health(State(d): State<Arc<Daemon>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": d.started.elapsed().as_secs(),
        "port": super::PORT,
        "souls": d.souls().len(),
        "syncing": d.souls().iter().filter(|s| s.status().syncing).count(),
    }))
}

pub fn router(d: Arc<Daemon>) -> Router {
    Router::new()
        .route("/soul/{genesis}/sparql", get(sparql_get).post(sparql_post))
        .route("/soul/{genesis}/info", get(info))
        .route("/soul/{genesis}/sync", post(sync))
        .route("/soul/{genesis}", delete(forget))
        .route("/souls", get(souls))
        .route("/health", get(health))
        .with_state(d)
        .layer(middleware::from_fn(local_only))
}

/// The Host values gitlexd answers to: its own address, by any of the three
/// names this machine gives itself. Anything else is refused, which stops
/// DNS rebinding: an outside website that points its own domain name at
/// 127.0.0.1 still sends its own name as the Host, and gets nothing.
fn is_local_host(host: &str) -> bool {
    let port = super::PORT;
    [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")]
        .iter()
        .any(|h| h == host)
}

/// A web page served from this machine: `http` or `https`, host `localhost`,
/// `127.0.0.1` or `[::1]`, any port or none. `null` (a page opened straight
/// from disk, or a sandboxed frame on any site) is not local.
fn is_local_origin(origin: &str) -> bool {
    let Some(rest) = origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://")) else {
        return false;
    };
    let (host, port) = match rest.strip_prefix("[::1]") {
        Some(after) => ("[::1]", after),
        None => match rest.split_once(':') {
            Some((h, p)) => (h, &rest[h.len()..][..1 + p.len()]),
            None => (rest, ""),
        },
    };
    let port_ok = port.is_empty()
        || (port.len() > 1 && port.starts_with(':') && port[1..].bytes().all(|b| b.is_ascii_digit()));
    port_ok && matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// In front of every route: gitlexd serves this machine and nothing else.
///
/// - A request must be addressed to gitlexd itself (see `is_local_host`).
/// - A request with no `Origin` (curl, `git lex query`, another program such
///   as git-lex-ui's server) passes as before.
/// - A web page served from localhost on any port may read the answers: the
///   reply carries `Access-Control-Allow-Origin` for it, and the browser's
///   preflight (`OPTIONS`) is answered here. This is what lets one shared UI
///   layer (git-lex-ui, Pan's and Ravel's views) query gitlexd straight from
///   the browser (goodlux, 2026-10-03).
/// - Any other website is refused outright, before any route runs, so a page
///   on the internet cannot even trigger a sync through the user's browser.
async fn local_only(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !is_local_host(&host) {
        let port = super::PORT;
        return (
            StatusCode::FORBIDDEN,
            format!(
                "gitlexd answers only requests addressed to 127.0.0.1:{port} or localhost:{port}; \
                 this one was addressed to {host:?}. Use http://127.0.0.1:{port}/ as the address.\n"
            ),
        )
            .into_response();
    }
    let Some(origin) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()).map(str::to_string)
    else {
        return next.run(req).await;
    };
    if !is_local_origin(&origin) {
        return (
            StatusCode::FORBIDDEN,
            format!(
                "gitlexd answers web pages served from this machine only. This page's origin is {origin:?}. \
                 Serve the page from http://localhost:<port> (for example `python3 -m http.server`) \
                 and load it from there.\n"
            ),
        )
            .into_response();
    }
    let allow = HeaderValue::from_str(&origin).unwrap_or_else(|_| HeaderValue::from_static("null"));
    let mut resp = if req.method() == Method::OPTIONS {
        let mut r = StatusCode::NO_CONTENT.into_response();
        let h = r.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST"));
        h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("Content-Type, Accept"));
        h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
        r
    } else {
        next.run(req).await
    };
    let h = resp.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, allow);
    h.insert(header::VARY, HeaderValue::from_static("Origin"));
    resp
}

/// Bind and serve until Ctrl-C or SIGTERM.
/// Serve on a port already bound by the caller. The bind happens in the
/// binary before any store is opened, because the port is the machine-wide
/// lock: only one process can listen on it, so binding first is what keeps
/// two gitlexd from ever holding stores at once (goodlux, 2026-09-22).
pub async fn serve(d: Arc<Daemon>, listener: std::net::TcpListener) -> Result<(), String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("cannot make the listener non-blocking: {e}"))?;
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|e| format!("cannot hand the listener to the runtime: {e}"))?;
    d.log(&format!("gitlexd listening on {} with {} soul(s)", super::base_url(), d.souls().len()));
    d.spawn_loops();
    let app = router(Arc::clone(&d));
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| format!("server error: {e}"))?;
    d.log("gitlexd stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}


#[cfg(test)]
mod local_only_tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/soul/{g}/sparql", get(|| async { "rows" }).post(|| async { "rows" }))
            .layer(middleware::from_fn(local_only))
    }

    async fn send(method: &str, path: &str, host: Option<&str>, origin: Option<&str>) -> Response {
        let mut b = axum::http::Request::builder().method(method).uri(path);
        if let Some(h) = host {
            b = b.header(header::HOST, h);
        }
        if let Some(o) = origin {
            b = b.header(header::ORIGIN, o);
        }
        app().oneshot(b.body(Body::empty()).unwrap()).await.unwrap()
    }

    fn acao(r: &Response) -> Option<&str> {
        r.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn local_origins_and_hosts() {
        for o in ["http://localhost:5173", "http://127.0.0.1:8080", "https://localhost", "http://[::1]:3000", "http://localhost"] {
            assert!(is_local_origin(o), "{o} should be local");
        }
        for o in ["null", "http://evil.com", "http://localhost.evil.com", "http://127.0.0.1.nip.io:80", "http://localhost:", "http://localhost:80x", "file://", "http://[::1]x", "ftp://localhost"] {
            assert!(!is_local_origin(o), "{o} should not be local");
        }
        assert!(is_local_host("127.0.0.1:7880") && is_local_host("localhost:7880") && is_local_host("[::1]:7880"));
        assert!(!is_local_host("evil.com:7880") && !is_local_host("127.0.0.1") && !is_local_host(""));
    }

    #[tokio::test]
    async fn programs_without_an_origin_pass_unchanged() {
        let r = send("GET", "/health", Some("127.0.0.1:7880"), None).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert!(acao(&r).is_none());
    }

    #[tokio::test]
    async fn a_localhost_page_may_read() {
        let r = send("GET", "/health", Some("127.0.0.1:7880"), Some("http://localhost:5173")).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(acao(&r), Some("http://localhost:5173"));
    }

    #[tokio::test]
    async fn the_browser_preflight_is_answered() {
        let r = send("OPTIONS", "/soul/abc/sparql", Some("localhost:7880"), Some("http://127.0.0.1:8000")).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert_eq!(acao(&r), Some("http://127.0.0.1:8000"));
        assert_eq!(r.headers().get(header::ACCESS_CONTROL_ALLOW_METHODS).unwrap(), "GET, POST");
    }

    #[tokio::test]
    async fn other_websites_are_refused_before_any_route_runs() {
        for o in ["http://evil.com", "null"] {
            let r = send("POST", "/soul/abc/sparql", Some("127.0.0.1:7880"), Some(o)).await;
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "{o}");
            assert!(acao(&r).is_none());
        }
    }

    #[tokio::test]
    async fn a_request_addressed_elsewhere_is_refused() {
        // DNS rebinding: an outside domain pointed at 127.0.0.1 keeps its own Host.
        let r = send("GET", "/health", Some("rebind.evil.com:7880"), Some("http://rebind.evil.com:7880")).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = send("GET", "/health", None, None).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
}
