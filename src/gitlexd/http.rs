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
use axum::extract::{Path as AxPath, Query as AxQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::collections::HashMap;
use std::sync::Arc;

fn err(status: StatusCode, msg: String) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

fn find(d: &Daemon, genesis: &str) -> Result<Arc<Soul>, Box<Response>> {
    d.find(genesis).map_err(|e| Box::new(match e {
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

async fn souls(State(d): State<Arc<Daemon>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "souls": d.souls.iter().map(|s| soul_json(s)).collect::<Vec<_>>() }))
}

async fn health(State(d): State<Arc<Daemon>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": d.started.elapsed().as_secs(),
        "port": super::PORT,
        "souls": d.souls.len(),
        "syncing": d.souls.iter().filter(|s| s.status().syncing).count(),
    }))
}

pub fn router(d: Arc<Daemon>) -> Router {
    Router::new()
        .route("/soul/{genesis}/sparql", get(sparql_get).post(sparql_post))
        .route("/soul/{genesis}/info", get(info))
        .route("/soul/{genesis}/sync", post(sync))
        .route("/souls", get(souls))
        .route("/health", get(health))
        .with_state(d)
}

/// Bind and serve until Ctrl-C or SIGTERM.
pub async fn serve(d: Arc<Daemon>) -> Result<(), String> {
    let addr = format!("127.0.0.1:{}", super::PORT);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("cannot listen on {addr}: {e} (is another gitlexd, or an old git-lex-serve, still running?)"))?;
    d.log(&format!("gitlexd listening on http://{addr} with {} soul(s)", d.souls.len()));
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
