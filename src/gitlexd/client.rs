//! What the git-lex command line uses to reach a running gitlexd. Blocking,
//! localhost, short timeouts: a daemon that is not there must cost the
//! caller milliseconds, not seconds.

use std::time::Duration;

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

fn read(mut resp: ureq::http::Response<ureq::Body>) -> (u16, String, String) {
    let status = resp.status().as_u16();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    (status, ct, body)
}

/// The error text a body carries, or the body itself.
fn error_text(status: u16, body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
        .unwrap_or_else(|| format!("HTTP {status}: {body}"))
}

/// Is a gitlexd answering on its port?
pub fn running() -> bool {
    health().is_some()
}

pub fn health() -> Option<serde_json::Value> {
    let resp = agent(Duration::from_millis(800))
        .get(format!("{}/health", super::base_url()))
        .call()
        .ok()?;
    let (status, _, body) = read(resp);
    (status == 200).then(|| serde_json::from_str(&body).ok()).flatten()
}

/// Tell gitlexd this soul's HEAD moved. Fire and forget: the caller does not
/// wait, and a gitlexd that is not running is not an error here.
pub fn nudge(genesis: &str) {
    let _ = agent(Duration::from_millis(800))
        .post(format!("{}/soul/{genesis}/sync", super::base_url()))
        .send_empty();
}

/// Ask gitlexd to sync this soul and wait until it has. Returns the soul's
/// state as gitlexd reports it.
pub fn sync_and_wait(genesis: &str) -> Result<serde_json::Value, String> {
    let resp = agent(Duration::from_secs(3600))
        .post(format!("{}/soul/{genesis}/sync?wait=1", super::base_url()))
        .send_empty()
        .map_err(|e| format!("gitlexd did not answer: {e}"))?;
    let (status, _, body) = read(resp);
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("gitlexd answered with something that is not JSON: {e}"))?;
    if status == 200 {
        Ok(v)
    } else {
        Err(error_text(status, &body))
    }
}

/// The result of one query as gitlexd sent it: the content type and the
/// body (`application/sparql-results+json` or `application/n-triples`).
pub struct QueryResponse {
    pub content_type: String,
    pub body: String,
}

pub fn query(genesis: &str, sparql: &str) -> Result<QueryResponse, String> {
    let resp = agent(Duration::from_secs(3600))
        .post(format!("{}/soul/{genesis}/sparql", super::base_url()))
        .header("content-type", "application/sparql-query")
        .send(sparql)
        .map_err(|e| format!("gitlexd did not answer: {e}"))?;
    let (status, content_type, body) = read(resp);
    if status == 200 {
        Ok(QueryResponse { content_type, body })
    } else {
        Err(error_text(status, &body))
    }
}

pub fn souls() -> Result<serde_json::Value, String> {
    let resp = agent(Duration::from_secs(5))
        .get(format!("{}/souls", super::base_url()))
        .call()
        .map_err(|e| format!("gitlexd did not answer: {e}"))?;
    let (status, _, body) = read(resp);
    if status == 200 {
        serde_json::from_str(&body).map_err(|e| e.to_string())
    } else {
        Err(error_text(status, &body))
    }
}
