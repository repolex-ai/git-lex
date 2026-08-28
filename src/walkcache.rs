//! The working-tree walk cache — the incremental half of sync (§4.3 of the
//! incremental-sync spec, Rob-approved 2026-08-26 "yes for the 10th time").
//!
//! Every walk used to read, YAML-parse, tree-sitter-parse and quad-emit
//! EVERY document in the repo. Measured at two scales before building
//! (the ratio lesson): on W4R3Z parse dominates 23:1, on lUX parse and
//! emit split nearly 1:1 — so a cache that skipped only parsing would
//! capture half the win at exactly the scale that matters. This cache
//! therefore stores the finished product: each file's exact N-Quads
//! fragment, plus its sidecar-relevant counters.
//!
//! **Where it lives:** `.lex/_ignore/walkcache/` — the machine-local
//! pocket. Never committed, safe to delete at any time (the only cost is
//! one full walk to rebuild it). `manifest.tsv` maps each document to the
//! identity of what produced its fragment; `frag/<relpath>.nq` holds the
//! fragment bytes.
//!
//! **Cache validity is content identity, not process history.** A file's
//! entry is trusted only when BOTH match:
//!   - the git blob hash of its current working-tree BYTES (catches every
//!     edit, and the revert-after-sync case that a status/resume-marker
//!     design silently gets wrong), and
//!   - the blob hash git's INDEX holds for it (the emitted `git/blobHash`
//!     quad reads the index, so an index move — add, commit — must miss).
//!
//! **The two total gates (spec §4.3), enforced as one context hash:**
//!   - the installed ontology (every byte under `.lex/ontology/`) — a kit
//!     change can alter every document's output without touching any
//!     document;
//!   - the document existence set (the sorted file list) — a link fact
//!     exists only while its target exists, so an add/delete/rename
//!     changes OTHER files' output. Either changes → the context hash
//!     changes → the whole cache is invalid → full walk, exactly today's
//!     behavior.
//!
//! **What is never cached:** a file whose extraction produced errors.
//! Errors must stay loud on every run; caching one would let a broken
//! document read as clean forever. (Warnings are different: an unchanged
//! file's warnings go quiet until it is next edited — deliberate; the
//! warning fires at the save that writes the key and at every edit after.)
//!
//! **Escape hatch:** `GIT_LEX_FULL_WALK=1` forces the full walk and
//! rebuilds the cache — also the receipt instrument: full-vs-cached output
//! must be byte-identical.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// One cached document: the identity of what produced its fragment, and
/// the counters the walk must report without re-doing the work.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CacheEntry {
    /// git blob hash of the file's working-tree bytes at cache time.
    pub bytes_hash: String,
    /// git blob hash the INDEX held for this path at cache time
    /// (empty = untracked then).
    pub index_hash: String,
    /// .md.spo link lines this file contributed (the walk's link total).
    pub links: usize,
}

pub(crate) struct WalkCache {
    /// Hash over the ontology bytes + the document existence set. A
    /// mismatch invalidates every entry at once.
    pub ctx_hash: String,
    pub entries: HashMap<String, CacheEntry>,
    dir: PathBuf,
    /// Entries proven or refreshed this run — written back on save.
    fresh: HashMap<String, CacheEntry>,
}

fn cache_dir(root: &Path) -> PathBuf {
    root.join(".lex").join("_ignore").join("walkcache")
}

/// git's own blob hash of a byte string — the ONE content-identity
/// primitive this cache uses (never a home-grown digest).
pub(crate) fn blob_hash_of(bytes: &[u8]) -> String {
    git2::Oid::hash_object(git2::ObjectType::Blob, bytes)
        .map(|o| o.to_string())
        .unwrap_or_default()
}

/// The context hash: ontology bytes + sorted document list. Anything that
/// can change a document's output WITHOUT its bytes changing must be in
/// here; when in doubt, include it — the cost of inclusion is a full walk,
/// the cost of omission is silently stale derived state.
pub(crate) fn context_hash(root: &Path, files: &[PathBuf]) -> String {
    let mut acc = Vec::new();
    // Document existence set, sorted for determinism.
    let mut rels: Vec<String> = files
        .iter()
        .filter_map(|p| p.strip_prefix(root).ok())
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    rels.sort();
    for r in &rels {
        acc.extend_from_slice(r.as_bytes());
        acc.push(b'\n');
    }
    // Every byte of the installed ontology, path-sorted.
    let ont = root.join(".lex").join("ontology");
    let mut ont_files: Vec<PathBuf> = Vec::new();
    collect_files(&ont, &mut ont_files);
    ont_files.sort();
    for f in &ont_files {
        acc.extend_from_slice(f.to_string_lossy().as_bytes());
        acc.push(b'\n');
        acc.extend_from_slice(&fs::read(f).unwrap_or_default());
    }
    blob_hash_of(&acc)
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                collect_files(&p, out);
            } else {
                out.push(p);
            }
        }
    }
}

impl WalkCache {
    /// Load the cache for this context. None = no usable cache (absent,
    /// unreadable, or built under a different context) — the caller runs
    /// a full walk and a fresh cache is written at the end either way.
    pub(crate) fn load(root: &Path, ctx_hash: &str) -> Option<WalkCache> {
        let dir = cache_dir(root);
        let manifest = fs::read_to_string(dir.join("manifest.tsv")).ok()?;
        let mut lines = manifest.lines();
        let head = lines.next()?;
        let stored_ctx = head.strip_prefix("CTX\t")?;
        if stored_ctx != ctx_hash {
            return None;
        }
        let mut entries = HashMap::new();
        for line in lines {
            let mut cols = line.split('\t');
            let (Some(rel), Some(bh), Some(ih), Some(links)) =
                (cols.next(), cols.next(), cols.next(), cols.next())
            else {
                return None; // torn manifest — distrust the whole thing
            };
            let links: usize = links.parse().ok()?;
            entries.insert(
                rel.to_string(),
                CacheEntry {
                    bytes_hash: bh.to_string(),
                    index_hash: ih.to_string(),
                    links,
                },
            );
        }
        Some(WalkCache {
            ctx_hash: ctx_hash.to_string(),
            entries,
            dir,
            fresh: HashMap::new(),
        })
    }

    /// An empty cache that will be populated by this run (full-walk path).
    pub(crate) fn empty(root: &Path, ctx_hash: &str) -> WalkCache {
        WalkCache {
            ctx_hash: ctx_hash.to_string(),
            entries: HashMap::new(),
            dir: cache_dir(root),
            fresh: HashMap::new(),
        }
    }

    fn frag_path(&self, relpath: &str) -> PathBuf {
        self.dir.join("frag").join(format!("{}.nq", relpath))
    }

    /// Cache hit test + fragment read, in one move. Some only when both
    /// identity hashes match — and, when the caller needs the quads
    /// (`read_fragment`), the fragment is readable too. Callers that only
    /// write sidecars (the hook path) skip thousands of fragment reads; a
    /// fragment lost from disk simply misses on the next quad-building run
    /// and is re-extracted — self-healing, never trusted blind.
    pub(crate) fn hit(
        &mut self,
        relpath: &str,
        bytes_hash: &str,
        index_hash: &str,
        read_fragment: bool,
    ) -> Option<(String, usize)> {
        let e = self.entries.get(relpath)?;
        if e.bytes_hash != bytes_hash || e.index_hash != index_hash {
            return None;
        }
        let frag = if read_fragment {
            fs::read_to_string(self.frag_path(relpath)).ok()?
        } else {
            String::new()
        };
        let entry = e.clone();
        let links = entry.links;
        self.fresh.insert(relpath.to_string(), entry);
        Some((frag, links))
    }

    /// Record a freshly-extracted file. Errors>0 files are the caller's
    /// responsibility to NOT store (loud-every-run contract).
    pub(crate) fn store(
        &mut self,
        relpath: &str,
        bytes_hash: &str,
        index_hash: &str,
        fragment: &str,
        links: usize,
    ) {
        let p = self.frag_path(relpath);
        if let Some(parent) = p.parent() {
            if fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        if fs::write(&p, fragment).is_err() {
            return;
        }
        self.fresh.insert(
            relpath.to_string(),
            CacheEntry {
                bytes_hash: bytes_hash.to_string(),
                index_hash: index_hash.to_string(),
                links,
            },
        );
    }

    /// Write the manifest of everything proven or produced THIS run —
    /// entries for vanished files fall away here (self-pruning), and a
    /// half-written manifest is impossible to trust-load because the CTX
    /// header is written first and torn rows fail the parse.
    pub(crate) fn save(&self) {
        if fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        let mut out = format!("CTX\t{}\n", self.ctx_hash);
        let mut rels: Vec<&String> = self.fresh.keys().collect();
        rels.sort();
        for rel in rels {
            let e = &self.fresh[rel];
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\n",
                rel, e.bytes_hash, e.index_hash, e.links
            ));
        }
        let _ = fs::write(self.dir.join("manifest.tsv"), out);
        self.prune_orphan_fragments();
    }

    /// Delete fragment files whose document was not seen this run.
    ///
    /// THE GHOST BUG (@w4r3z-pool, @spacegoat, @w4r3z-pan and @nug3 all found
    /// it within an hour, 2026-08-27). Deleting a document removed its source
    /// and its `.lex/extract/` sidecar, but its fragment under
    /// `walkcache/frag/` survived — and the deleted document went on answering
    /// queries. Reproduced in isolation: fragment present, file absent -> 6
    /// triples for a document that does not exist; move the fragment aside ->
    /// 0; put it back -> 6 again.
    ///
    /// Why it mattered more than tidiness: it defeated the ONLY available
    /// dangling-reference check, and in the reassuring direction. A reference
    /// pointing at a DELETED document read as perfectly resolved, so the one
    /// workaround the fleet had for the missing existence check quietly lied.
    ///
    /// `self.fresh` is every document this run saw — `hit()` and `store()` both
    /// record into it — so anything on disk and not in it is a document that no
    /// longer exists. That holds because the walk is always whole-repo
    /// (`walk_repo_docs` reads the directory tree); a future partial walk would
    /// have to stop calling this or it would prune live fragments.
    fn prune_orphan_fragments(&self) {
        let frag_root = self.dir.join("frag");
        let mut stale: Vec<PathBuf> = Vec::new();
        collect_files(&frag_root, &mut stale);
        for f in stale {
            let Ok(rel) = f.strip_prefix(&frag_root) else { continue };
            let rel = rel.to_string_lossy();
            let Some(doc) = rel.strip_suffix(".nq") else { continue };
            if !self.fresh.contains_key(doc) {
                let _ = fs::remove_file(&f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("glx-walkcache-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".lex").join("ontology").join("t")).unwrap();
        dir
    }

    #[test]
    fn roundtrip_hit_after_save() {
        let root = tmp_root("roundtrip");
        let ctx = context_hash(&root, &[root.join("a.md")]);
        let mut c = WalkCache::empty(&root, &ctx);
        c.store("a.md", "bh1", "ih1", "<s> <p> <o> <g> .\n", 2);
        c.save();

        let mut loaded = WalkCache::load(&root, &ctx).expect("cache loads");
        let (frag, links) = loaded.hit("a.md", "bh1", "ih1", true).expect("hit");
        assert_eq!(frag, "<s> <p> <o> <g> .\n");
        assert_eq!(links, 2);
        // Either hash off → miss.
        assert!(loaded.hit("a.md", "bhX", "ih1", true).is_none());
        assert!(loaded.hit("a.md", "bh1", "ihX", true).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn context_mismatch_refuses_to_load() {
        let root = tmp_root("ctx");
        let ctx = context_hash(&root, &[root.join("a.md")]);
        let c = WalkCache::empty(&root, &ctx);
        c.save();
        assert!(WalkCache::load(&root, "different").is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ontology_byte_change_changes_context() {
        let root = tmp_root("ont");
        let files = vec![root.join("a.md")];
        let before = context_hash(&root, &files);
        fs::write(root.join(".lex/ontology/t/t.ttl"), "t:changed").unwrap();
        assert_ne!(before, context_hash(&root, &files), "gate 2: ontology bytes");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn existence_set_change_changes_context() {
        let root = tmp_root("exist");
        let one = context_hash(&root, &[root.join("a.md")]);
        let two = context_hash(&root, &[root.join("a.md"), root.join("b.md")]);
        assert_ne!(one, two, "gate 3: the document existence set");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unproven_entries_prune_on_save() {
        let root = tmp_root("prune");
        let ctx = context_hash(&root, &[]);
        let mut c = WalkCache::empty(&root, &ctx);
        c.store("keep.md", "b", "i", "x\n", 0);
        c.save();
        // Next run proves nothing, stores one new file.
        let mut c2 = WalkCache::load(&root, &ctx).unwrap();
        c2.store("only.md", "b", "i", "y\n", 0);
        c2.save();
        let c3 = WalkCache::load(&root, &ctx).unwrap();
        assert!(c3.entries.contains_key("only.md"));
        assert!(!c3.entries.contains_key("keep.md"), "vanished files fall away");
        let _ = fs::remove_dir_all(&root);
    }

    /// THE GHOST BUG. A document's fragment must not outlive the document.
    ///
    /// Found within an hour by @w4r3z-pool, @spacegoat, @w4r3z-pan and @nug3,
    /// four seats, four routes. @nug3's reduction was the cleanest: delete a
    /// COMMITTED, clean file and the query returns the identical triple count —
    /// no save involved. The live view was additive-only. Additions propagated
    /// immediately; removals never did.
    #[test]
    fn deleting_a_document_removes_its_fragment() {
        // Its own root, uniquified by clock as well as pid: the shared helper
        // keys only on process id, and under the full suite this test collided
        // with a sibling and died in create_dir_all. A test that passes alone
        // and fails in the suite is not a flaky test, it is a shared-state bug
        // in the fixture.
        let root = std::env::temp_dir().join(format!(
            "glx-walkcache-prune-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".lex").join("ontology").join("t")).unwrap();
        let ctx = context_hash(&root, &[root.join("a.md")]);

        // Run 1: two documents seen, two fragments written.
        let mut c = WalkCache::empty(&root, &ctx);
        c.store("a.md", "bh-a", "ih-a", "<x> <y> <z> .\n", 0);
        c.store("b.md", "bh-b", "ih-b", "<p> <q> <r> .\n", 0);
        c.save();
        assert!(root.join(".lex/_ignore/walkcache/frag/a.md.nq").exists());
        assert!(root.join(".lex/_ignore/walkcache/frag/b.md.nq").exists());

        // Run 2: b.md is gone from disk, so the walk never sees it.
        let mut c2 = WalkCache::empty(&root, &ctx);
        c2.store("a.md", "bh-a", "ih-a", "<x> <y> <z> .\n", 0);
        c2.save();

        assert!(root.join(".lex/_ignore/walkcache/frag/a.md.nq").exists(),
            "a surviving document keeps its fragment");
        assert!(!root.join(".lex/_ignore/walkcache/frag/b.md.nq").exists(),
            "a DELETED document must not keep answering queries — this fragment outliving its \
             source is what made a reference to a deleted document read as perfectly resolved, \
             defeating the only dangling-reference check the fleet had");

        let _ = fs::remove_dir_all(&root);
    }
}
