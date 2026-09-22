//! The daemon's state and its sync loop.
//!
//! Each soul's store is held under a read/write lock: queries take it for
//! reading, a sync takes it for writing. A sync runs as a child process of
//! gitlexd (`gitlexd worker <path>`, working directory set to the soul),
//! because the sync engine assumes the process's working directory is the
//! repository — the git-root lookup, the IRI base cache in git.rs and the
//! bare `git` calls all read it. One process per sync keeps every one of
//! those assumptions true and lets souls sync in parallel (goodlux,
//! 2026-09-22: option A). The daemon closes its handle on the store before
//! the worker starts, since RocksDB's lock is per process, and reopens it
//! when the worker exits. Queries for that soul wait on the lock meanwhile;
//! no query ever reads a half-written store.

use oxigraph::store::Store;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{Notify, RwLock, Semaphore, watch};

/// How many souls may sync at once.
const PARALLEL_SYNCS: usize = 3;
/// How often each soul's HEAD is read to notice a commit that came without
/// a nudge (a plain `git commit`, a `git pull`).
const HEAD_POLL: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone, Default, serde::Serialize)]
pub struct SoulStatus {
    /// The commit the store is synced to: the newest commit in its commits
    /// graph. None until the first sync lands.
    pub synced_to: Option<String>,
    pub syncing: bool,
    /// The last sync's failure, if it failed; cleared by the next success.
    pub last_error: Option<String>,
    pub last_sync_ms: Option<u128>,
    pub syncs: u64,
    /// Why the store could not be opened, if it could not.
    pub open_error: Option<String>,
}

pub struct Soul {
    pub genesis: String,
    pub path: PathBuf,
    pub name: Option<String>,
    /// None when the store does not exist yet (never synced) or is closed
    /// for a running sync.
    pub store: Arc<RwLock<Option<Store>>>,
    pub status: Mutex<SoulStatus>,
    wake: Notify,
    requested: AtomicU64,
    completed_tx: watch::Sender<u64>,
    completed_rx: watch::Receiver<u64>,
}

impl Soul {
    fn new(genesis: String, path: PathBuf, name: Option<String>, store: Option<Store>, open_error: Option<String>) -> Soul {
        let (completed_tx, completed_rx) = watch::channel(0);
        let synced_to = store.as_ref().and_then(crate::sync::synced_marker);
        Soul {
            genesis,
            path,
            name,
            store: Arc::new(RwLock::new(store)),
            status: Mutex::new(SoulStatus { synced_to, open_error, ..SoulStatus::default() }),
            wake: Notify::new(),
            requested: AtomicU64::new(0),
            completed_tx,
            completed_rx,
        }
    }

    pub fn short(&self) -> &str {
        &self.genesis[..8.min(self.genesis.len())]
    }

    pub fn status(&self) -> SoulStatus {
        self.status.lock().unwrap().clone()
    }

    /// Ask for a sync. Returns a ticket; `wait_for(ticket)` resolves once a
    /// sync that started after this request has finished. Requests made
    /// while a sync runs are folded into one following sync.
    pub fn request_sync(&self) -> u64 {
        let n = self.requested.fetch_add(1, Ordering::SeqCst) + 1;
        self.wake.notify_one();
        n
    }

    pub async fn wait_for(&self, ticket: u64) {
        let mut rx = self.completed_rx.clone();
        while *rx.borrow() < ticket {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    pub async fn sync_and_wait(&self) {
        let t = self.request_sync();
        self.wait_for(t).await;
    }
}

pub enum FindError {
    NotFound,
    Ambiguous(Vec<String>),
}

pub struct Daemon {
    pub souls: Vec<Arc<Soul>>,
    pub started: Instant,
    /// The binary to run as the sync worker: this executable.
    worker_exe: PathBuf,
    log_file: Mutex<Option<File>>,
    log_path: Option<PathBuf>,
    syncs: Semaphore,
}

impl Daemon {
    /// Read the registry and open every registered soul's store. A store
    /// that fails to open is kept, with the error, so `/souls` shows it.
    pub fn open() -> Result<Daemon, String> {
        let worker_exe = std::env::current_exe().map_err(|e| format!("cannot find my own executable: {e}"))?;
        let log_path = super::log_path();
        let log_file = match &log_path {
            Some(p) => {
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
                }
                Some(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(p)
                        .map_err(|e| format!("cannot open log {}: {e}", p.display()))?,
                )
            }
            None => None,
        };
        let d = Daemon {
            souls: Vec::new(),
            started: Instant::now(),
            worker_exe,
            log_file: Mutex::new(log_file),
            log_path,
            syncs: Semaphore::new(PARALLEL_SYNCS),
        };
        let mut souls: Vec<Arc<Soul>> = Vec::new();
        for entry in crate::registry_entries() {
            let path = entry.path;
            let genesis = match entry.genesis.or_else(|| crate::git::genesis_sha_at(&path)) {
                Some(g) => g,
                None => {
                    d.log(&format!("skip {}: no first commit (nothing committed yet)", path.display()));
                    continue;
                }
            };
            if let Some(dup) = souls.iter().find(|s| s.genesis == genesis) {
                d.log(&format!(
                    "skip {}: same first commit as {} (a clone; one store per soul)",
                    path.display(),
                    dup.path.display()
                ));
                continue;
            }
            let name = crate::RepoYml::load(&path).name;
            let (store, open_error) = match open_existing(&path) {
                Ok(s) => (s, None),
                Err(e) => {
                    d.log(&format!("{} store failed to open: {e}", &genesis[..8]));
                    (None, Some(e))
                }
            };
            souls.push(Arc::new(Soul::new(genesis, path, name, store, open_error)));
        }
        Ok(Daemon { souls, ..d })
    }

    pub fn log_path(&self) -> Option<&Path> {
        self.log_path.as_deref()
    }

    /// One line to the terminal and to the log file, with the time.
    pub fn log(&self, line: &str) {
        let now = crate::clock::now_rfc3339().unwrap_or_default();
        let text = format!("{now} {line}");
        eprintln!("{text}");
        if let Ok(mut f) = self.log_file.lock()
            && let Some(f) = f.as_mut() {
                let _ = writeln!(f, "{text}");
            }
    }

    /// The soul whose genesis hash starts with `prefix`.
    pub fn find(&self, prefix: &str) -> Result<Arc<Soul>, FindError> {
        let hits: Vec<&Arc<Soul>> = self.souls.iter().filter(|s| s.genesis.starts_with(prefix)).collect();
        match hits.as_slice() {
            [one] => Ok(Arc::clone(one)),
            [] => Err(FindError::NotFound),
            many => Err(FindError::Ambiguous(many.iter().map(|s| s.genesis.clone()).collect())),
        }
    }

    /// Start the per-soul sync loops and HEAD watchers. The watcher's first
    /// reading is the catch-up: a store behind its HEAD syncs at start.
    pub fn spawn_loops(self: &Arc<Self>) {
        for soul in &self.souls {
            tokio::spawn(sync_loop(Arc::clone(self), Arc::clone(soul)));
            tokio::spawn(watch_loop(Arc::clone(soul)));
        }
    }

    /// One sync of one soul, as a worker process. Holds the store's write
    /// lock throughout, so no query reads during it.
    async fn sync_once(&self, soul: &Soul) {
        let _permit = self.syncs.acquire().await;
        let mut guard = Arc::clone(&soul.store).write_owned().await;
        soul.status.lock().unwrap().syncing = true;
        *guard = None; // closes the store: RocksDB's lock is per process
        let started = Instant::now();
        self.log(&format!("{} sync starting: {}", soul.short(), soul.path.display()));
        let exe = self.worker_exe.clone();
        let path = soul.path.clone();
        let out = tokio::task::spawn_blocking(move || {
            std::process::Command::new(exe)
                .arg("worker")
                .arg(&path)
                .current_dir(&path)
                .output()
        })
        .await;
        let mut error: Option<String> = None;
        match out {
            Ok(Ok(o)) => {
                for line in String::from_utf8_lossy(&o.stdout).lines().chain(String::from_utf8_lossy(&o.stderr).lines()) {
                    if !line.trim().is_empty() {
                        self.log(&format!("{}   {line}", soul.short()));
                    }
                }
                if !o.status.success() {
                    let last = String::from_utf8_lossy(&o.stderr)
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("worker exited with {}", o.status));
                    error = Some(last);
                }
            }
            Ok(Err(e)) => error = Some(format!("could not start the sync worker: {e}")),
            Err(e) => error = Some(format!("sync worker task failed: {e}")),
        }
        let (store, open_error) = match open_existing(&soul.path) {
            Ok(s) => (s, None),
            Err(e) => (None, Some(e)),
        };
        let synced_to = store.as_ref().and_then(crate::sync::synced_marker);
        *guard = store;
        let ms = started.elapsed().as_millis();
        {
            let mut st = soul.status.lock().unwrap();
            st.syncing = false;
            st.syncs += 1;
            st.last_sync_ms = Some(ms);
            st.last_error = error.clone();
            st.open_error = open_error.clone();
            st.synced_to = synced_to.clone();
        }
        match (&error, &open_error) {
            (None, None) => self.log(&format!(
                "{} synced to {} in {ms} ms",
                soul.short(),
                synced_to.as_deref().map(|s| &s[..8.min(s.len())]).unwrap_or("(nothing)")
            )),
            (Some(e), _) => self.log(&format!("{} sync FAILED in {ms} ms: {e}", soul.short())),
            (None, Some(e)) => self.log(&format!("{} store failed to reopen after sync: {e}", soul.short())),
        }
    }
}

/// Open a soul's store if it exists. A missing store is not an error — the
/// soul has never been synced, and the first sync creates it.
fn open_existing(root: &Path) -> Result<Option<Store>, String> {
    let p = crate::store_path_at(root);
    if !p.exists() {
        return Ok(None);
    }
    Store::open(&p).map(Some).map_err(|e| format!("{}: {e}", p.display()))
}

async fn sync_loop(d: Arc<Daemon>, soul: Arc<Soul>) {
    loop {
        soul.wake.notified().await;
        loop {
            let target = soul.requested.load(Ordering::SeqCst);
            d.sync_once(&soul).await;
            let _ = soul.completed_tx.send(target);
            if soul.requested.load(Ordering::SeqCst) <= target {
                break;
            }
        }
    }
}

/// HEAD of the repository at `root`, as a hash, if there is one.
fn head_sha(root: &Path) -> Option<String> {
    let repo = git2::Repository::open(root).ok()?;
    let head = repo.head().ok()?;
    head.target().map(|oid| oid.to_string())
}

/// Notice HEAD moving without a nudge: a plain `git commit`, a `git pull`,
/// a save made while gitlexd was down. A sync is asked for when HEAD is
/// not what the store is synced to; so the first reading at start is the
/// catch-up, and a HEAD the nudge already synced asks for nothing.
async fn watch_loop(soul: Arc<Soul>) {
    let mut last: Option<String> = None;
    loop {
        let now = head_sha(&soul.path);
        if now != last {
            last = now.clone();
            let synced_to = soul.status.lock().unwrap().synced_to.clone();
            if now.is_some() && now != synced_to {
                soul.request_sync();
            }
        }
        tokio::time::sleep(HEAD_POLL).await;
    }
}
