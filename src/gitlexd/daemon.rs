//! The daemon's state and its sync loop.
//!
//! Each soul's store is held under a read/write lock: queries take it for
//! reading, a sync takes it for writing. A sync runs as a child process of
//! gitlexd (`gitlexd worker <path>`, working directory set to the soul),
//! because the sync engine assumes the process's working directory is the
//! repository — the git-root lookup, the IRI base cache in git.rs and the
//! bare `git` calls all read it. One process per sync keeps every one of
//! those assumptions true and lets souls sync in parallel (goodlux,
//! 2026-09-22: option A).
//!
//! RocksDB lets one process open a store, so the worker cannot write the
//! store the daemon is serving. It writes a copy instead (#46): the daemon
//! takes a backup of the store into `.lex/_ignore/oxigraph.next` (hard links
//! on the same disk, so it is cheap), the worker syncs into that, and on
//! success the daemon takes the write lock, closes the store, swaps the two
//! folders and reopens. Queries keep reading the old store for the whole
//! sync and wait only for the swap. A failed sync never touches the store
//! being served. No query ever reads a half-written store.

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
    /// Set when the daemon has dropped this soul (its `.lex` is gone, or
    /// `git lex nuke` said so). The loops exit; nothing syncs it again.
    gone: std::sync::atomic::AtomicBool,
    /// Held for a whole sync, copy to swap. The store's own lock is held
    /// only for the swap, so dropping a soul waits on this instead.
    syncing: tokio::sync::Mutex<()>,
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
            gone: std::sync::atomic::AtomicBool::new(false),
            syncing: tokio::sync::Mutex::new(()),
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
    /// Every soul held, in registry order. Behind a lock because a soul
    /// registered after start joins on the first request that names it.
    souls: std::sync::RwLock<Vec<Arc<Soul>>>,
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
            souls: std::sync::RwLock::new(Vec::new()),
            started: Instant::now(),
            worker_exe,
            log_file: Mutex::new(log_file),
            log_path,
            syncs: Semaphore::new(PARALLEL_SYNCS),
        };
        for entry in crate::registry_entries() {
            d.adopt(entry);
        }
        Ok(d)
    }

    /// Drop a soul: close its store, take it off the list, end its loops.
    /// For `git lex nuke` (which deletes `.lex/` and then commits, a commit
    /// the HEAD watcher would otherwise answer with a sync that recreates
    /// the store in the cleaned repository), and for a `.lex/` that simply
    /// vanished. Returns the soul when it was held.
    pub async fn forget(&self, genesis: &str) -> Option<Arc<Soul>> {
        let soul = {
            let mut souls = self.souls.write().unwrap();
            let i = souls.iter().position(|s| s.genesis == genesis)?;
            souls.remove(i)
        };
        soul.gone.store(true, Ordering::SeqCst);
        // Waits for a running sync to finish, then closes the store.
        let sync = soul.syncing.lock().await;
        let mut guard = Arc::clone(&soul.store).write_owned().await;
        *guard = None;
        drop(guard);
        drop(sync);
        soul.wake.notify_one();
        self.log(&format!("{} {} dropped (its .lex is gone)", soul.short(), soul.path.display()));
        Some(soul)
    }

    /// A snapshot of the souls held right now.
    pub fn souls(&self) -> Vec<Arc<Soul>> {
        self.souls.read().unwrap().clone()
    }

    /// The first commit that keys a registry entry's soul, if it has one.
    fn genesis_of(entry: &crate::RegistryEntry) -> Option<String> {
        entry.genesis.clone().or_else(|| crate::git::genesis_sha_at(&entry.path))
    }

    /// Take one registry entry in: open its store (if it has one) and add
    /// it to the souls held. Says why when it cannot. Returns the soul when
    /// it was added; None when skipped.
    fn adopt(&self, entry: crate::RegistryEntry) -> Option<Arc<Soul>> {
        let path = entry.path.clone();
        let Some(genesis) = Self::genesis_of(&entry) else {
            self.log(&format!("skip {}: no first commit (nothing committed yet)", path.display()));
            return None;
        };
        let mut souls = self.souls.write().unwrap();
        if let Some(dup) = souls.iter().find(|s| s.genesis == genesis) {
            self.log(&format!(
                "skip {}: same first commit as {} (a clone; one store per soul)",
                path.display(),
                dup.path.display()
            ));
            return None;
        }
        let name = crate::RepoYml::load(&path).name;
        let (store, open_error) = match open_existing(&path) {
            Ok(s) => (s, None),
            Err(e) => {
                self.log(&format!("{} store failed to open: {e}", &genesis[..8]));
                (None, Some(e))
            }
        };
        let soul = Arc::new(Soul::new(genesis, path, name, store, open_error));
        souls.push(Arc::clone(&soul));
        Some(soul)
    }

    /// Read the registry again and take in every repository registered
    /// since the last look, starting its loops. Called when a request names
    /// a soul that is not held: `git lex init` registers the repository and
    /// the first save, sync or query after it lands here, so a new
    /// repository needs no restart (goodlux, 2026-09-22). Returns how many
    /// joined.
    pub fn refresh(self: &Arc<Self>) -> usize {
        let held: Vec<String> = self.souls().iter().map(|s| s.genesis.clone()).collect();
        let mut joined = 0;
        for entry in crate::registry_entries() {
            if Self::genesis_of(&entry).is_some_and(|g| held.contains(&g)) {
                continue;
            }
            if let Some(soul) = self.adopt(entry) {
                self.log(&format!("{} {} joined (registered after start)", soul.short(), soul.path.display()));
                self.spawn_soul_loops(&soul);
                joined += 1;
            }
        }
        joined
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
        let souls = self.souls.read().unwrap();
        let hits: Vec<&Arc<Soul>> = souls.iter().filter(|s| s.genesis.starts_with(prefix)).collect();
        match hits.as_slice() {
            [one] => Ok(Arc::clone(one)),
            [] => Err(FindError::NotFound),
            many => Err(FindError::Ambiguous(many.iter().map(|s| s.genesis.clone()).collect())),
        }
    }

    /// Start the per-soul sync loops and HEAD watchers. The watcher's first
    /// reading is the catch-up: a store behind its HEAD syncs at start.
    pub fn spawn_loops(self: &Arc<Self>) {
        for soul in self.souls() {
            self.spawn_soul_loops(&soul);
        }
    }

    fn spawn_soul_loops(self: &Arc<Self>, soul: &Arc<Soul>) {
        tokio::spawn(sync_loop(Arc::clone(self), Arc::clone(soul)));
        tokio::spawn(watch_loop(Arc::clone(self), Arc::clone(soul)));
    }

    /// One sync of one soul, as a worker process writing a copy of the
    /// store, swapped in on success. Queries read the old store meanwhile.
    async fn sync_once(&self, soul: &Soul) {
        if soul.gone.load(Ordering::SeqCst) || !crate::layout::lex_dir(&soul.path).is_dir() {
            // Never run the engine in a repository git-lex has left: the
            // engine would create `.lex/` again. The watcher drops it.
            return;
        }
        let _permit = self.syncs.acquire().await;
        let _sync = soul.syncing.lock().await;
        if soul.gone.load(Ordering::SeqCst) {
            return;
        }
        soul.status.lock().unwrap().syncing = true;
        let started = Instant::now();
        self.log(&format!("{} sync starting: {}", soul.short(), soul.path.display()));

        let next = crate::layout::store_next_dir(&soul.path);
        let mut error: Option<String> = copy_store(&soul.store, &next).await.err();
        if error.is_none() {
            let exe = self.worker_exe.clone();
            let path = soul.path.clone();
            let next_arg = next.clone();
            let out = tokio::task::spawn_blocking(move || {
                std::process::Command::new(exe)
                    .arg("worker")
                    .arg(&path)
                    .arg(&next_arg)
                    .current_dir(&path)
                    .output()
            })
            .await;
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
        }

        // Swap only a finished copy, and never for a soul dropped meanwhile.
        let mut open_error: Option<String> = None;
        let mut synced_to = soul.status.lock().unwrap().synced_to.clone();
        if error.is_none() && !soul.gone.load(Ordering::SeqCst) {
            let swap_started = Instant::now();
            let mut guard = Arc::clone(&soul.store).write_owned().await;
            *guard = None; // closes the store: RocksDB's lock is per process
            let swapped = swap_in(&soul.path);
            let (store, e) = match open_existing(&soul.path) {
                Ok(s) => (s, None),
                Err(e) => (None, Some(e)),
            };
            synced_to = store.as_ref().and_then(crate::sync::synced_marker);
            *guard = store;
            drop(guard);
            open_error = swapped.err().or(e);
            self.log(&format!("{} store swapped in {} ms", soul.short(), swap_started.elapsed().as_millis()));
        }
        // A failed sync leaves its copy behind; it is never served.
        let _ = std::fs::remove_dir_all(&next);
        let _ = std::fs::remove_dir_all(crate::layout::store_prev_dir(&soul.path));

        let ms = started.elapsed().as_millis();
        {
            let mut st = soul.status.lock().unwrap();
            st.syncing = false;
            st.syncs += 1;
            st.last_sync_ms = Some(ms);
            st.last_error = error.clone();
            if error.is_none() {
                st.open_error = open_error.clone();
                st.synced_to = synced_to.clone();
            }
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

/// Copy the served store into `next` for a sync to write. Readers carry on
/// while the backup is taken (it holds the read lock only); a soul that has
/// never synced has nothing to copy, and the worker creates the store.
async fn copy_store(store: &Arc<RwLock<Option<Store>>>, next: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(next);
    let guard = Arc::clone(store).read_owned().await;
    let next = next.to_path_buf();
    tokio::task::spawn_blocking(move || match guard.as_ref() {
        Some(s) => s
            .backup(&next)
            .map_err(|e| format!("could not copy the store for the sync ({}): {e}", next.display())),
        None => Ok(()),
    })
    .await
    .map_err(|e| format!("store copy task failed: {e}"))?
}

/// Move the synced copy in place of the store: the store aside to `prev`,
/// the copy to the store's place. The store must be closed. Undone by
/// [`recover_swap`] if the process dies in between.
fn swap_in(root: &Path) -> Result<(), String> {
    let live = crate::store_path_at(root);
    let next = crate::layout::store_next_dir(root);
    let prev = crate::layout::store_prev_dir(root);
    let _ = std::fs::remove_dir_all(&prev);
    if live.exists() {
        std::fs::rename(&live, &prev).map_err(|e| format!("could not move {} aside: {e}", live.display()))?;
    }
    std::fs::rename(&next, &live).map_err(|e| format!("could not move the synced store into {}: {e}", live.display()))
}

/// Finish or undo a swap the process died in the middle of. The store is
/// missing only between the two renames of [`swap_in`]: with the old store
/// already aside, the copy is a finished sync and goes in; without a copy,
/// the old store goes back. Otherwise leftovers of an interrupted sync are
/// removed.
fn recover_swap(root: &Path) {
    let live = crate::store_path_at(root);
    let next = crate::layout::store_next_dir(root);
    let prev = crate::layout::store_prev_dir(root);
    if !live.exists() && prev.exists() {
        let from = if next.exists() { &next } else { &prev };
        let _ = std::fs::rename(from, &live);
    }
    if live.exists() {
        let _ = std::fs::remove_dir_all(&next);
        let _ = std::fs::remove_dir_all(&prev);
    }
}

/// Open a soul's store if it exists. A missing store is not an error — the
/// soul has never been synced, and the first sync creates it.
fn open_existing(root: &Path) -> Result<Option<Store>, String> {
    recover_swap(root);
    let p = crate::store_path_at(root);
    if !p.exists() {
        return Ok(None);
    }
    Store::open(&p).map(Some).map_err(|e| format!("{}: {e}", p.display()))
}

async fn sync_loop(d: Arc<Daemon>, soul: Arc<Soul>) {
    loop {
        soul.wake.notified().await;
        if soul.gone.load(Ordering::SeqCst) {
            return;
        }
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
async fn watch_loop(d: Arc<Daemon>, soul: Arc<Soul>) {
    let mut last: Option<String> = None;
    loop {
        if soul.gone.load(Ordering::SeqCst) {
            return;
        }
        if !crate::layout::lex_dir(&soul.path).is_dir() {
            d.forget(&soul.genesis).await;
            return;
        }
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

#[cfg(test)]
mod swap_tests {
    use super::*;
    use oxigraph::model::{GraphNameRef, NamedNodeRef, QuadRef};

    fn root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("glx-swap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(crate::layout::ignore_dir(&d)).unwrap();
        d
    }

    fn quad(n: &str) -> QuadRef<'_> {
        QuadRef::new(
            NamedNodeRef::new_unchecked(n),
            NamedNodeRef::new_unchecked("https://ex/p"),
            NamedNodeRef::new_unchecked("https://ex/o"),
            GraphNameRef::DefaultGraph,
        )
    }

    /// The whole cycle: the copy is taken while the store stays readable,
    /// a write to the copy does not reach the served store, and after the
    /// swap the store holds the copy's writes.
    #[tokio::test]
    async fn copy_write_swap() {
        let r = root("cycle");
        let live = crate::store_path_at(&r);
        let next = crate::layout::store_next_dir(&r);
        let s = Store::open(&live).unwrap();
        s.insert(quad("https://ex/old")).unwrap();
        let served = Arc::new(RwLock::new(Some(s)));

        copy_store(&served, &next).await.unwrap();
        {
            let copy = Store::open(&next).unwrap();
            copy.insert(quad("https://ex/new")).unwrap();
        }
        let reader = served.read().await;
        let s = reader.as_ref().unwrap();
        assert!(s.contains(quad("https://ex/old")).unwrap());
        assert!(!s.contains(quad("https://ex/new")).unwrap(), "the served store never sees the sync");
        drop(reader);

        *served.write().await = None;
        swap_in(&r).unwrap();
        let s = open_existing(&r).unwrap().unwrap();
        assert!(s.contains(quad("https://ex/old")).unwrap());
        assert!(s.contains(quad("https://ex/new")).unwrap());
        assert!(!next.exists());
        let _ = std::fs::remove_dir_all(&r);
    }

    /// A soul that has never synced has nothing to copy; the worker creates
    /// the store in the copy's place and the swap moves it in.
    #[tokio::test]
    async fn first_sync_has_nothing_to_copy() {
        let r = root("first");
        let next = crate::layout::store_next_dir(&r);
        copy_store(&Arc::new(RwLock::new(None)), &next).await.unwrap();
        assert!(!next.exists());
        Store::open(&next).unwrap().insert(quad("https://ex/new")).unwrap();
        swap_in(&r).unwrap();
        assert!(open_existing(&r).unwrap().unwrap().contains(quad("https://ex/new")).unwrap());
        let _ = std::fs::remove_dir_all(&r);
    }

    fn marker(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("which"), name).unwrap();
    }

    fn which(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("which")).unwrap()
    }

    /// Died between the two renames, with the synced copy ready: it goes in.
    #[test]
    fn recovery_finishes_a_half_done_swap() {
        let r = root("half");
        marker(&crate::layout::store_prev_dir(&r), "prev");
        marker(&crate::layout::store_next_dir(&r), "next");
        recover_swap(&r);
        assert_eq!(which(&crate::store_path_at(&r)), "next");
        assert!(!crate::layout::store_prev_dir(&r).exists());
        let _ = std::fs::remove_dir_all(&r);
    }

    /// Store aside and no copy: the old store goes back.
    #[test]
    fn recovery_puts_the_old_store_back() {
        let r = root("back");
        marker(&crate::layout::store_prev_dir(&r), "prev");
        recover_swap(&r);
        assert_eq!(which(&crate::store_path_at(&r)), "prev");
        let _ = std::fs::remove_dir_all(&r);
    }

    /// A sync that died before its swap leaves a copy that is never served.
    #[test]
    fn recovery_discards_an_unfinished_copy() {
        let r = root("unfinished");
        marker(&crate::store_path_at(&r), "live");
        marker(&crate::layout::store_next_dir(&r), "next");
        recover_swap(&r);
        assert_eq!(which(&crate::store_path_at(&r)), "live");
        assert!(!crate::layout::store_next_dir(&r).exists());
        let _ = std::fs::remove_dir_all(&r);
    }
}
