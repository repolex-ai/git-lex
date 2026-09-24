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

/// Make sure a gitlexd is answering, starting one if nothing is. The
/// daemon is started detached (its own process group, no terminal, no
/// output but its own log), so it outlives the git-lex command that started it and
/// belongs to no terminal; `gitlexd stop` ends it. Two commands starting
/// one at the same moment cannot produce two daemons: gitlexd binds its
/// port before it does anything else, and the copy that loses the bind
/// exits. Both callers then wait here for the one that won
/// (goodlux, 2026-09-22: `git lex query` starts gitlexd when it must, and
/// there is never more than one).
pub fn ensure_running() -> Result<(), String> {
    if running() {
        return Ok(());
    }
    let exe = gitlexd_exe().ok_or_else(|| {
        "gitlexd is not running and no gitlexd binary was found next to git-lex or on PATH. \
         Install git-lex again (`cargo install --path . --force --locked` from its repo)."
            .to_string()
    })?;
    let log = super::log_path();
    // The daemon writes its own log; giving it this log as stderr too would
    // write every line twice (it did, on the first install). Its stderr is
    // dropped: a copy that lost the port says so to nobody, and that is fine.
    let mut cmd = std::process::Command::new(&exe);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", exe.display()))?;
    // The child is not waited on. If it lost the port to another copy it
    // exits at once; the daemon that holds the port answers below.
    drop(child);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        if running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Err(format!(
        "started {} but nothing answered on {} within 60 s; its log is {}",
        exe.display(),
        super::base_url(),
        log.map(|p| p.display().to_string()).unwrap_or_else(|| "~/.lex/logs/gitlexd.log".into())
    ))
}

/// The gitlexd binary: the one installed next to this git-lex (cargo
/// install puts both in the same directory, and so does a build tree),
/// else whatever PATH has.
fn gitlexd_exe() -> Option<std::path::PathBuf> {
    let name = if cfg!(windows) { "gitlexd.exe" } else { "gitlexd" };
    if let Some(sibling) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(name)))
        && sibling.is_file() {
            return Some(sibling);
        }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
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

/// Tell gitlexd to drop this soul: close its store and stop watching the
/// repository. Waits (briefly) for the answer, because the caller is about
/// to delete the store's directory. A gitlexd that is not running is fine.
pub fn forget(genesis: &str) {
    let _ = agent(Duration::from_secs(10))
        .delete(format!("{}/soul/{genesis}", super::base_url()))
        .call();
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
