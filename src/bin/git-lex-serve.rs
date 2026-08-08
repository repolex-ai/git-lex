//! git lex-serve — HTTP server for git-lex knowledge graphs.
//!
//! Two subcommands: the viz server (D3 graph explorer) and the W3C SPARQL
//! endpoint. Runs as `git lex-serve` via git's subcommand discovery
//! (`git <foo>` finds `git-<foo>` on PATH).

use clap::{Parser, Subcommand};
use git_lex::{add_prefixes, find_git_root, open_store_read_only};
use oxigraph::model::Term;
use oxigraph::store::Store;
use std::fs;
use std::path::PathBuf;
use std::process::exit;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "git-lex-serve", about = "Servers for git-lex knowledge graphs")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the visualization server (HTTP on localhost)
    Viz {
        /// Port to listen on
        #[arg(long, default_value = "7878")]
        port: u16,
    },
    /// Start the W3C SPARQL protocol endpoint (+ Swagger UI) over the synced store
    Sparql {
        /// Port to listen on
        #[arg(long, default_value = "7880")]
        port: u16,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Viz { port } => cmd_viz(port),
        Commands::Sparql { port } => cmd_sparql_server(port),
    }
}

// ─── viz server ─────────────────────────────────────────────────

/// Resolve the www directory. Reads from `.lex/www/` in the repo root
/// (installed by the base kit). Assets are served from disk so they can
/// be edited without rebuilding the binary.
fn resolve_www_dir() -> PathBuf {
    find_git_root()
        .map(|r| r.join(".lex").join("www"))
        .unwrap_or_else(|| PathBuf::from(".lex/www"))
}

fn read_viz_asset(www_dir: &PathBuf, rel: &str) -> Option<String> {
    fs::read_to_string(www_dir.join(rel)).ok()
}

#[derive(Clone)]
struct VizState {
    store: Arc<Store>,
    repo_root: Arc<PathBuf>,
}

fn run_sparql_to_json(store: &Store, query: &str) -> serde_json::Value {
    let prefixed = add_prefixes(query);
    let mut parsed = match oxigraph::sparql::SparqlEvaluator::new().parse_query(&prefixed) {
        Ok(p) => p,
        Err(e) => return serde_json::json!({"error": format!("parse error: {}", e)}),
    };
    parsed.dataset_mut().set_default_graph_as_union();

    let results = match parsed.on_store(store).execute() {
        Ok(r) => r,
        Err(e) => return serde_json::json!({"error": format!("query error: {}", e)}),
    };

    match results {
        oxigraph::sparql::QueryResults::Solutions(sols) => {
            let vars: Vec<String> = sols.variables().iter().map(|v| v.as_str().to_string()).collect();
            let mut rows = Vec::new();
            for sol in sols.flatten() {
                let mut row = serde_json::Map::new();
                for var in &vars {
                    if let Some(t) = sol.get(var.as_str()) {
                        let val = match t {
                            Term::NamedNode(n) => n.as_str().to_string(),
                            Term::Literal(l) => l.value().to_string(),
                            Term::BlankNode(b) => format!("_:{}", b.as_str()),
                            Term::Triple(t) => format!("<<{} {} {}>>", t.subject, t.predicate, t.object),
                        };
                        row.insert(var.clone(), serde_json::Value::String(val));
                    }
                }
                rows.push(serde_json::Value::Object(row));
            }
            serde_json::json!({"type": "select", "vars": vars, "results": rows})
        }
        oxigraph::sparql::QueryResults::Boolean(b) => {
            serde_json::json!({"type": "ask", "boolean": b})
        }
        oxigraph::sparql::QueryResults::Graph(triples) => {
            let mut emitted = Vec::new();
            for t in triples.flatten() {
                let s = match t.subject {
                    oxigraph::model::NamedOrBlankNode::NamedNode(n) => n.as_str().to_string(),
                    oxigraph::model::NamedOrBlankNode::BlankNode(b) => format!("_:{}", b.as_str()),
                };
                let p = t.predicate.as_str().to_string();
                let (o_val, o_type, o_datatype) = match t.object {
                    Term::NamedNode(n) => (n.as_str().to_string(), "iri", None),
                    Term::Literal(l) => (l.value().to_string(), "literal", Some(l.datatype().as_str().to_string())),
                    Term::BlankNode(b) => (format!("_:{}", b.as_str()), "bnode", None),
                    Term::Triple(t) => (format!("<<{} {} {}>>", t.subject, t.predicate, t.object), "triple", None),
                };
                let mut triple = serde_json::Map::new();
                triple.insert("subject".to_string(), serde_json::Value::String(s));
                triple.insert("predicate".to_string(), serde_json::Value::String(p));
                let mut obj = serde_json::Map::new();
                obj.insert("value".to_string(), serde_json::Value::String(o_val));
                obj.insert("type".to_string(), serde_json::Value::String(o_type.to_string()));
                if let Some(dt) = o_datatype {
                    obj.insert("datatype".to_string(), serde_json::Value::String(dt));
                }
                triple.insert("object".to_string(), serde_json::Value::Object(obj));
                emitted.push(serde_json::Value::Object(triple));
            }
            serde_json::json!({"type": "construct", "triples": emitted})
        }
    }
}

fn api_file_for_uri(state: &VizState, uri: Option<&str>) -> serde_json::Value {
    let uri = match uri {
        Some(u) if !u.is_empty() => u,
        _ => return serde_json::json!({"error": "missing 'uri' query parameter"}),
    };

    // The doc IRI is a deterministic function of the file path
    // (resource_uri: percent-encode, strip the scaffold "Soul/" prefix) —
    // so the path is DERIVED back from the IRI, no store query. The old
    // implementation asked the retired git: layer for git:path, which died
    // at the git2 cutover (w3bl0rd's live-confirmed bug, 2026-07-21).
    let base = format!("{}/", git_lex::resource_base_at(&state.repo_root));
    let tail = match uri.strip_prefix(base.as_str()) {
        Some(t) if !t.is_empty() => t,
        _ => return serde_json::json!({"error": format!("not a document IRI (expected {base}<path>)"), "uri": uri}),
    };
    // Percent-decode the IRI tail back to a filesystem path.
    let decoded = {
        let mut out = Vec::new();
        let b = tail.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                if let Ok(byte) = u8::from_str_radix(&tail[i + 1..i + 3], 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).to_string()
    };
    // The scaffold root doesn't repeat in the IRI: try the bare path, then
    // under Soul/ (the same normalization resource_uri applies forward).
    let rel = if state.repo_root.join(&decoded).exists() {
        decoded
    } else {
        let scaffolded = format!("Soul/{}", decoded);
        if state.repo_root.join(&scaffolded).exists() {
            scaffolded
        } else {
            return serde_json::json!({"error": "no file for this IRI", "uri": uri, "tried": [decoded, format!("Soul/…")]});
        }
    };

    let abs = state.repo_root.join(&rel);
    let canon_root = state.repo_root.canonicalize().unwrap_or_else(|_| (*state.repo_root).clone());
    let canon_abs = match abs.canonicalize() {
        Ok(p) => p,
        Err(e) => return serde_json::json!({"error": format!("file not found: {}", e), "path": rel}),
    };
    if !canon_abs.starts_with(&canon_root) {
        return serde_json::json!({"error": "path escapes repo root", "path": rel});
    }
    let raw = match std::fs::read_to_string(&canon_abs) {
        Ok(s) => s,
        Err(e) => return serde_json::json!({"error": format!("read failed: {}", e), "path": rel}),
    };

    let (frontmatter, body) = if raw.starts_with("---\n") {
        if let Some(end) = raw[4..].find("\n---\n") {
            let fm_text = &raw[4..4 + end];
            let body_text = &raw[4 + end + 5..];
            (Some(fm_text.to_string()), body_text.to_string())
        } else if let Some(end) = raw[4..].find("\n---") {
            let fm_text = &raw[4..4 + end];
            let body_text = raw.get(4 + end + 4..).unwrap_or("");
            (Some(fm_text.to_string()), body_text.to_string())
        } else {
            (None, raw.clone())
        }
    } else {
        (None, raw.clone())
    };

    serde_json::json!({
        "uri": uri,
        "path": rel,
        "frontmatter": frontmatter,
        "content": body,
    })
}

fn cmd_viz(port: u16) {
    if open_store_read_only().is_none() {
        eprintln!("No knowledge graph store found.");
        eprintln!("Run 'git lex sync' first to build the store.");
        exit(1);
    }
    let www_dir = resolve_www_dir();
    if !www_dir.exists() {
        eprintln!("No www directory found at {}", www_dir.display());
        eprintln!("Run 'git lex init' to install the base kit.");
        exit(1);
    }
    run_viz_server(port, www_dir);
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn run_viz_server(port: u16, www_dir: PathBuf) {
    use axum::{
        Router,
        routing::{get, post},
        response::{Html, Json},
    };

    let store = Arc::new(
        open_store_read_only().expect("failed to open store read-only — run `git lex sync` first"),
    );

    let repo_root = Arc::new(find_git_root().unwrap_or_else(|| std::env::current_dir().unwrap()));
    let state = VizState { store, repo_root };
    let www_dir = Arc::new(www_dir);

    let app = Router::new()
        .route("/", get({
            let www_dir = www_dir.clone();
            move || {
                let www_dir = www_dir.clone();
                async move {
                    match read_viz_asset(&www_dir, "index.html") {
                        Some(body) => Html(body),
                        None => Html("<h1>index.html not found in .lex/www/</h1>".to_string()),
                    }
                }
            }
        }))
        .route("/css/main.css", get({
            let www_dir = www_dir.clone();
            move || {
                let www_dir = www_dir.clone();
                async move {
                    let body = read_viz_asset(&www_dir, "css/main.css").unwrap_or_default();
                    ([("content-type", "text/css"), ("cache-control", "no-store")], body)
                }
            }
        }))
        .route("/js/main.js", get({
            let www_dir = www_dir.clone();
            move || {
                let www_dir = www_dir.clone();
                async move {
                    let body = read_viz_asset(&www_dir, "js/main.js").unwrap_or_default();
                    ([("content-type", "application/javascript"), ("cache-control", "no-store")], body)
                }
            }
        }))
        .route("/api/query", post({
            let state = state.clone();
            move |Json(payload): Json<serde_json::Value>| {
                let state = state.clone();
                async move {
                    let query = payload.get("query")
                        .and_then(|v| v.as_str())
                        .unwrap_or("SELECT ?s ?p ?o WHERE { ?s ?p ?o } LIMIT 10");
                    Json(run_sparql_to_json(&state.store, query))
                }
            }
        }))
        .route("/api/viz/nodes", get({
            let state = state.clone();
            move || {
                let state = state.clone();
                async move {
                    // Render-ready node rows: every typed document in the now
                    // view (INCLUDING orphans nothing links to), with a
                    // computed display label — gl:name when present, else the
                    // IRI tail. No client-side joining or label munging.
                    let q = "PREFIX gl: <https://repolex.ai/ontology/git-lex/> \
                        SELECT ?id ?type ?label WHERE { \
                          GRAPH <https://repolex.ai/git-lex/NamedGraph/now> { \
                            ?id a ?type . \
                            OPTIONAL { ?id gl:name ?n } \
                            BIND(COALESCE(?n, REPLACE(STR(?id), \"^.*/\", \"\")) AS ?label) \
                          } }";
                    Json(run_sparql_to_json(&state.store, q))
                }
            }
        }))
        .route("/api/viz/edges", get({
            let state = state.clone();
            move || {
                let state = state.clone();
                async move {
                    // Render-ready edge rows: one row per link, uniform
                    // columns + a `predicate` column for per-predicate
                    // coloring (w3bl0rd's contract, 2026-07-21). Covers
                    // md:linksTo AND every kit object-property whose object
                    // is doc-shaped or a slug — anything connecting
                    // documents. `target` is always a STRING; `resolved` is
                    // a boolean — the JS branches on columns, never on RDF
                    // term kinds.
                    // Kit predicates live under ontology/<kit>/ (the
                    // kit/ tier is retired but old stores may carry it —
                    // match both); machinery vocab (ontology/git-lex/) is
                    // excluded except md:linksTo above. The a-box base is
                    // DERIVED per repo, never hardcoded.
                    let q = format!("PREFIX md: <https://repolex.ai/ontology/git-lex/md/> \
                        SELECT ?from ?predicate ?target ?resolved WHERE {{ \
                          GRAPH <https://repolex.ai/git-lex/NamedGraph/now> {{ \
                            {{ ?from md:linksTo ?to . BIND(STR(md:linksTo) AS ?predicate) }} \
                            UNION \
                            {{ ?from ?p ?to . \
                              FILTER(STRSTARTS(STR(?p), \"https://repolex.ai/ontology/\")) \
                              FILTER(!STRSTARTS(STR(?p), \"https://repolex.ai/ontology/git-lex/\")) \
                              FILTER(isIRI(?to) && STRSTARTS(STR(?to), \"{abox}/\")) \
                              BIND(STR(?p) AS ?predicate) }} \
                            BIND(STR(?to) AS ?target) \
                            BIND(ISIRI(?to) AS ?resolved) \
                          }} }}",
                        abox = git_lex::resource_base_at(&state.repo_root));
                    Json(run_sparql_to_json(&state.store, &q))
                }
            }
        }))
        .route("/api/store-info", get({
            let state = state.clone();
            move || {
                let state = state.clone();
                async move {
                    // Snapshot of the store's graph inventory. The post-Part-5
                    // store shape: the one graph (statement history + current
                    // base facts), the git2 machinery graphs, the ontology
                    // graph, and the derived now view.
                    let q = "SELECT ?g (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } } \
                             GROUP BY ?g ORDER BY DESC(?n)";
                    let graphs = run_sparql_to_json(&state.store, q);
                    Json(serde_json::json!({
                        "graphs": graphs,
                        "one_graph": "https://repolex.ai/git-lex/LexHistoryGraph",
                        "now_view": "https://repolex.ai/git-lex/NamedGraph/now",
                        "model": "one-graph (RDF 1.2 SpoEvents; now = materialized view of the base layer)",
                    }))
                }
            }
        }))
        .route("/api/file", get({
            let state = state.clone();
            move |axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>| {
                let state = state.clone();
                async move {
                    Json(api_file_for_uri(&state, params.get("uri").map(|s| s.as_str())))
                }
            }
        }))
        ;

    let mut chosen_port = port;
    let mut listener = None;
    for candidate in port..port.saturating_add(20) {
        let addr = format!("127.0.0.1:{}", candidate);
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => {
                chosen_port = candidate;
                listener = Some(l);
                break;
            }
            Err(_) => continue,
        }
    }
    let listener = match listener {
        Some(l) => l,
        None => {
            eprintln!("Failed to bind: ports {}..{} all in use", port, port.saturating_add(20));
            return;
        }
    };

    let addr = format!("127.0.0.1:{}", chosen_port);
    if chosen_port != port {
        println!("Port {} was taken, using {} instead", port, chosen_port);
    }
    let url = format!("http://{}", addr);
    println!("git-lex-serve viz listening on {}", url);
    println!("Serving assets from {}", www_dir.display());
    println!("Press Ctrl+C to stop, or: kill {}", std::process::id());

    let _ = open::that_detached(&url);

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("Server error: {}", e);
    }
}

// ─── W3C SPARQL protocol endpoint (Task 2 Part B) ───────────────────────────
//
// A real SPARQL 1.1 Protocol surface over the SYNCED persistent store:
//   GET  /sparql?query=…                  (protocol §2.1.1)
//   POST /sparql   application/sparql-query (raw)      (§2.1.3)
//   POST /sparql   application/x-www-form-urlencoded query=… (§2.1.2)
//   POST /sparql   application/json {"query": …}        (convenience)
// SELECT/ASK → application/sparql-results+json; CONSTRUCT/DESCRIBE →
// application/n-triples; malformed query → 400 with the parse error;
// evaluation failure → 500. The store is opened read-only PER REQUEST so
// every query sees the latest `git lex sync` and never blocks a writer.
// This is what Pan-in-git-lex-mode and Syrinx speak to. Swagger at /swagger-ui.

mod query_server {
    use axum::extract::Query as AxQuery;
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::{Json, Router};
    use git_lex::{
        find_git_root, open_store_read_only, read_repo_yml_optional_kits, w3c_query,
        W3cQueryError, W3cQueryOutcome,
    };
    use std::collections::HashMap;
    use utoipa::{OpenApi, ToSchema};
    use utoipa_swagger_ui::SwaggerUi;

    #[derive(serde::Serialize, ToSchema)]
    pub struct HealthResponse {
        pub ok: bool,
        pub store: bool,
        pub version: String,
    }

    #[derive(serde::Serialize, ToSchema)]
    pub struct InfoResponse {
        /// Repo root this endpoint serves.
        pub root: String,
        /// The base kit (repo.yml `kit:`), if any.
        pub kit: Option<String>,
        /// Installed optional kits (repo.yml `optional_kits:`).
        pub optional_kits: Vec<String>,
        pub version: String,
    }

    #[derive(serde::Deserialize, ToSchema)]
    pub struct QueryBody {
        /// SPARQL text. Standard prefixes (rdf/rdfs/owl/xsd, git:/lex:/fm: +
        /// the installed kit's prefix) are pre-declared.
        pub query: String,
    }

    #[derive(serde::Serialize, ToSchema)]
    pub struct ErrorBody {
        pub error: String,
    }

    fn err(status: StatusCode, msg: String) -> Response {
        (status, Json(ErrorBody { error: msg })).into_response()
    }

    fn run(query: &str) -> Response {
        let Some(store) = open_store_read_only() else {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "no synced store found — run `git lex sync` first".to_string(),
            );
        };
        match w3c_query(&store, query) {
            Ok(W3cQueryOutcome::Solutions(v)) | Ok(W3cQueryOutcome::Boolean(v)) => (
                [(header::CONTENT_TYPE, "application/sparql-results+json")],
                v.to_string(),
            )
                .into_response(),
            Ok(W3cQueryOutcome::Graph(nt)) => {
                ([(header::CONTENT_TYPE, "application/n-triples")], nt).into_response()
            }
            Err(W3cQueryError::Parse(e)) => err(StatusCode::BAD_REQUEST, format!("SPARQL parse error: {e}")),
            Err(W3cQueryError::Eval(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("SPARQL evaluation error: {e}")),
        }
    }

    #[utoipa::path(get, path = "/sparql", tag = "sparql",
        params(("query" = String, Query, description = "SPARQL query text")),
        responses(
            (status = 200, description = "W3C application/sparql-results+json (SELECT/ASK) or application/n-triples (CONSTRUCT/DESCRIBE)"),
            (status = 400, body = ErrorBody, description = "Malformed query"),
            (status = 503, body = ErrorBody, description = "No synced store")))]
    async fn sparql_get(AxQuery(params): AxQuery<HashMap<String, String>>) -> Response {
        match params.get("query") {
            Some(q) => run(q),
            None => err(StatusCode::BAD_REQUEST, "missing ?query= parameter".to_string()),
        }
    }

    #[utoipa::path(post, path = "/sparql", tag = "sparql",
        request_body(content = QueryBody,
            description = "application/sparql-query (raw), application/x-www-form-urlencoded (query=…), or application/json {\"query\": …}"),
        responses(
            (status = 200, description = "W3C application/sparql-results+json (SELECT/ASK) or application/n-triples (CONSTRUCT/DESCRIBE)"),
            (status = 400, body = ErrorBody, description = "Malformed query"),
            (status = 503, body = ErrorBody, description = "No synced store")))]
    async fn sparql_post(headers: HeaderMap, body: axum::body::Bytes) -> Response {
        let ct = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let text = String::from_utf8_lossy(&body).to_string();
        if ct.starts_with("application/sparql-query") {
            run(&text)
        } else if ct.starts_with("application/x-www-form-urlencoded") {
            match form_urlencoded::parse(body.as_ref()).find(|(k, _)| k == "query") {
                Some((_, q)) => run(&q),
                None => err(StatusCode::BAD_REQUEST, "missing query= form field".to_string()),
            }
        } else if ct.starts_with("application/json") {
            match serde_json::from_str::<QueryBody>(&text) {
                Ok(b) => run(&b.query),
                Err(e) => err(StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")),
            }
        } else {
            // Bare POST body as query text — pragmatic default.
            run(&text)
        }
    }

    #[utoipa::path(get, path = "/health", tag = "meta",
        responses((status = 200, body = HealthResponse)))]
    async fn health() -> Json<HealthResponse> {
        Json(HealthResponse {
            ok: true,
            store: open_store_read_only().is_some(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        })
    }

    #[utoipa::path(get, path = "/info", tag = "meta",
        responses((status = 200, body = InfoResponse), (status = 503, body = ErrorBody)))]
    async fn info() -> Response {
        let Some(root) = find_git_root() else {
            return err(StatusCode::SERVICE_UNAVAILABLE, "not in a git repo".to_string());
        };
        let repo_yml = root.join(".lex").join("repo.yml");
        Json(InfoResponse {
            root: root.display().to_string(),
            kit: git_lex::get_kit(),
            optional_kits: read_repo_yml_optional_kits(&repo_yml),
            version: env!("CARGO_PKG_VERSION").to_string(),
        })
        .into_response()
    }

    #[derive(OpenApi)]
    #[openapi(
        info(
            title = "git-lex SPARQL endpoint",
            description = "W3C SPARQL endpoint over a git-lex soul store. Query language: SPARQL 1.2 (oxigraph rdf-12 — RDF 1.2 triple terms, <<( s p o )>> syntax, verified live) carried over the standard SPARQL 1.1 protocol + results format (1.2 changes the language, not the wire). Queries run against the SYNCED store (run `git lex sync` to refresh). The ONE GRAPH <https://repolex.ai/git-lex/LexHistoryGraph> holds current base facts alongside their full statement history (git-lex:SpoEvent, assertedIn/retractedIn -> git2:Commit); <https://repolex.ai/git-lex/NamedGraph/now> is the derived current-state view (plain triples, no event machinery); the vocabulary self-describes in GRAPH <https://repolex.ai/git-lex/NamedGraph/repo-ontology>.",
        ),
        paths(sparql_get, sparql_post, health, info),
        components(schemas(HealthResponse, InfoResponse, QueryBody, ErrorBody)),
        tags(
            (name = "sparql", description = "SPARQL 1.2 queries over the standard protocol"),
            (name = "meta", description = "Endpoint identity + kit discovery")
        )
    )]
    pub struct ApiDoc;

    pub fn router() -> Router {
        Router::new()
            .route("/sparql", get(sparql_get).post(sparql_post))
            .route("/health", get(health))
            .route("/info", get(info))
            .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
    }
}

fn cmd_sparql_server(port: u16) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async move {
        let app = query_server::router();
        let addr = format!("127.0.0.1:{port}");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .unwrap_or_else(|e| { eprintln!("bind {addr}: {e}"); exit(1) });
        println!("git-lex SPARQL endpoint on http://{addr}/sparql (swagger at /swagger-ui)");
        axum::serve(listener, app).await.expect("server error");
    });
}
