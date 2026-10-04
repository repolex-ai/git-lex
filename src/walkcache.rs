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
//! **The total gates (spec §4.3), enforced as one context hash:**
//!   - the installed ontology (every byte under `.lex/ontology/`) — a kit
//!     change can alter every document's output without touching any
//!     document;
//!   - the git-lex binary itself — an upgrade can change every fragment,
//!     and a cache written by the old binary would otherwise keep serving
//!     the old output (and skip rewriting the sidecars history is built
//!     from). Identified by the executable's path, size and modification
//!     time: any install changes it, and reading it costs one stat.
//!
//! Either gate changes → the context hash changes → the whole cache is
//! invalid → full walk, exactly the uncached behavior.
//!
//! **The document existence set is a partial gate.** A link fact exists
//! only while its target exists, so an add, delete or rename changes OTHER
//! files' output. It used to be a total gate, and since almost every save
//! on a large soul adds a file, the cache almost never loaded: lUX re-read
//! all 13,000 documents on every save. Now the cache keeps the file list
//! it was built against (`files.tsv`, its hash in the manifest header), and
//! a file that came or went invalidates only the documents whose bytes
//! mention its name. Every way a document can reach another one by path —
//! a markdown link (relative or root-relative, with or without `.md`,
//! percent-encoded or not) or a frontmatter path value — spells out the
//! target's file name, so a document that does not contain that name
//! cannot resolve differently. The check is on the longest run of
//! URL-safe characters in the name, which every encoding of it keeps
//! verbatim.
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
pub struct CacheEntry {
    /// git blob hash of the file's working-tree bytes at cache time.
    pub bytes_hash: String,
    /// git blob hash the INDEX held for this path at cache time
    /// (empty = untracked then).
    pub index_hash: String,
    /// .md.spo link lines this file contributed (the walk's link total).
    pub links: usize,
    /// Quad lines in the fragment (the sync report's fact count), so a
    /// caller that only needs the count never reads the fragment.
    pub quads: usize,
    /// The walk that produced this entry also wrote the file's sidecars.
    /// `git lex query` extracts without writing them (#39): its entry must
    /// not let a later save take "cached" for "sidecar already right".
    pub sidecars: bool,
}

pub struct WalkCache {
    /// Hash over the binary identity + the ontology bytes. A mismatch
    /// invalidates every entry at once.
    pub ctx_hash: String,
    pub entries: HashMap<String, CacheEntry>,
    /// This run's document list (repo-relative, sorted), written back as
    /// the list the next run compares against.
    files: Vec<String>,
    /// For each document added or removed since the cache was written: the
    /// part of its name a reference to it must contain. A cached document
    /// whose bytes contain any of these is re-extracted.
    changed_names: Vec<String>,
    dir: PathBuf,
    /// Entries proven or refreshed this run — written back on save.
    fresh: HashMap<String, CacheEntry>,
}

fn cache_dir(root: &Path) -> PathBuf {
    crate::layout::walkcache_dir(root)
}

/// git's own blob hash of a byte string — the ONE content-identity
/// primitive this cache uses (never a home-grown digest).
pub fn blob_hash_of(bytes: &[u8]) -> String {
    git2::Oid::hash_object(git2::ObjectType::Blob, bytes)
        .map(|o| o.to_string())
        .unwrap_or_default()
}

/// The running binary's identity: executable path, size and mtime. An
/// install replaces the file, so any upgrade (or downgrade) changes it.
fn binary_identity() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return String::new();
    };
    let Ok(meta) = fs::metadata(&exe) else {
        return exe.to_string_lossy().to_string();
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}\t{}\t{}", exe.to_string_lossy(), meta.len(), mtime)
}

/// The context hash: binary identity + ontology bytes. Anything that can
/// change EVERY document's output without its bytes changing must be in
/// here; when in doubt, include it — the cost of inclusion is a full walk,
/// the cost of omission is silently stale derived state. (The document
/// list is the partial gate: see [`WalkCache::load`].)
pub fn context_hash(root: &Path) -> String {
    let mut acc = Vec::new();
    acc.extend_from_slice(binary_identity().as_bytes());
    acc.push(b'\n');
    // Every byte of the installed kits' ontology, path-sorted. Installed
    // means listed in repo.yml, so removing a kit changes this hash even
    // when its folder is still on disk.
    let ont_files = crate::installed_ontology_files(root);
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

/// The repo-relative, sorted document list.
pub fn relative_files(root: &Path, files: &[PathBuf]) -> Vec<String> {
    let mut rels: Vec<String> = files
        .iter()
        .filter_map(|p| p.strip_prefix(root).ok())
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    rels.sort();
    rels
}

/// The part of a document's name that every reference to it by path must
/// contain: the longest run of URL-safe characters in its file name with
/// any extension dropped (a link may leave `.md` off, and percent-encoding
/// leaves URL-safe characters as they are). None when the name has no such
/// run, and so no safe way to tell who might mention it.
fn reference_needle(rel: &str) -> Option<String> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let stem = match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    };
    stem.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~')))
        .max_by_key(|run| run.len())
        .filter(|run| !run.is_empty())
        .map(str::to_string)
}

impl WalkCache {
    /// Load the cache for this context. None = no usable cache (absent,
    /// unreadable, or built under a different context) — the caller runs
    /// a full walk and a fresh cache is written at the end either way.
    ///
    /// `files` is this run's document list ([`relative_files`]). Documents
    /// added or removed since the cache was written are compared by name:
    /// see the module notes and [`WalkCache::mentions_changed_file`].
    pub fn load(root: &Path, ctx_hash: &str, files: &[String]) -> Option<WalkCache> {
        let dir = cache_dir(root);
        let manifest = fs::read_to_string(dir.join("manifest.tsv")).ok()?;
        let mut lines = manifest.lines();
        let head = lines.next()?;
        let (stored_ctx, stored_files_hash) = head.strip_prefix("CTX\t")?.split_once("\tFILES\t")?;
        if stored_ctx != ctx_hash {
            return None;
        }
        // The list the manifest was written against, proven by its hash: a
        // list from any other run would compute the wrong difference.
        let stored_files = fs::read(dir.join("files.tsv")).ok()?;
        if blob_hash_of(&stored_files) != stored_files_hash {
            return None;
        }
        let stored_files = String::from_utf8(stored_files).ok()?;
        let before: std::collections::HashSet<&str> = stored_files.lines().collect();
        let now: std::collections::HashSet<&str> = files.iter().map(String::as_str).collect();
        let mut changed_names = Vec::new();
        for rel in before.symmetric_difference(&now) {
            changed_names.push(reference_needle(rel)?);
        }
        changed_names.sort();
        changed_names.dedup();
        let mut entries = HashMap::new();
        for line in lines {
            let mut cols = line.split('\t');
            let (Some(rel), Some(bh), Some(ih), Some(links), Some(quads), Some(sidecars)) =
                (cols.next(), cols.next(), cols.next(), cols.next(), cols.next(), cols.next())
            else {
                return None; // torn (or older-format) manifest — distrust the whole thing
            };
            let links: usize = links.parse().ok()?;
            let quads: usize = quads.parse().ok()?;
            let sidecars = match sidecars {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            entries.insert(
                rel.to_string(),
                CacheEntry {
                    bytes_hash: bh.to_string(),
                    index_hash: ih.to_string(),
                    links,
                    quads,
                    sidecars,
                },
            );
        }
        Some(WalkCache {
            ctx_hash: ctx_hash.to_string(),
            entries,
            files: files.to_vec(),
            changed_names,
            dir,
            fresh: HashMap::new(),
        })
    }

    /// An empty cache that will be populated by this run (full-walk path).
    pub fn empty(root: &Path, ctx_hash: &str, files: &[String]) -> WalkCache {
        WalkCache {
            ctx_hash: ctx_hash.to_string(),
            entries: HashMap::new(),
            files: files.to_vec(),
            changed_names: Vec::new(),
            dir: cache_dir(root),
            fresh: HashMap::new(),
        }
    }

    /// The names of documents added or removed since the cache was written.
    pub fn changed_names(&self) -> &[String] {
        &self.changed_names
    }

    /// Does this document's text mention a document that came or went? Its
    /// cached output may then be wrong (a link that now resolves, or no
    /// longer does), so it must be extracted again.
    pub fn mentions_changed_file(changed_names: &[String], content: &str) -> bool {
        changed_names.iter().any(|n| content.contains(n.as_str()))
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
    pub fn hit(
        &mut self,
        relpath: &str,
        bytes_hash: &str,
        index_hash: &str,
        read_fragment: bool,
    ) -> Option<(String, CacheEntry)> {
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
        self.fresh.insert(relpath.to_string(), entry.clone());
        Some((frag, entry))
    }

    /// Record a freshly-extracted file. Errors>0 files are the caller's
    /// responsibility to NOT store (loud-every-run contract). `sidecars`
    /// says whether this walk wrote the file's sidecars as well.
    pub fn store(
        &mut self,
        relpath: &str,
        bytes_hash: &str,
        index_hash: &str,
        fragment: &str,
        links: usize,
        sidecars: bool,
    ) {
        let p = self.frag_path(relpath);
        if let Some(parent) = p.parent()
            && fs::create_dir_all(parent).is_err() {
                return;
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
                quads: fragment.lines().filter(|l| !l.is_empty()).count(),
                sidecars,
            },
        );
    }

    /// Write the manifest of everything proven or produced THIS run —
    /// entries for vanished files fall away here (self-pruning), and a
    /// half-written manifest is impossible to trust-load because the CTX
    /// header is written first and torn rows fail the parse.
    pub fn save(&self) {
        if fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        // The document list first, then the manifest that names its hash: a
        // run killed in between leaves a manifest whose hash no longer
        // matches, and the next run walks in full.
        let mut list = String::new();
        for f in &self.files {
            list.push_str(f);
            list.push('\n');
        }
        if fs::write(self.dir.join("files.tsv"), &list).is_err() {
            return;
        }
        let mut out = format!("CTX\t{}\tFILES\t{}\n", self.ctx_hash, blob_hash_of(list.as_bytes()));
        let mut rels: Vec<&String> = self.fresh.keys().collect();
        rels.sort();
        for rel in rels {
            let e = &self.fresh[rel];
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                rel, e.bytes_hash, e.index_hash, e.links, e.quads, u8::from(e.sidecars)
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
        fs::create_dir_all(crate::layout::kit_ontology_dir(&dir, "t")).unwrap();
        fs::write(crate::layout::repo_yml(&dir), "name: x\nkit: repolex-ai/git-lex-kit-t\n").unwrap();
        dir
    }

    /// #17: a folder repo.yml does not list is not part of the context.
    #[test]
    fn uninstalled_kit_folder_is_not_in_context() {
        let root = tmp_root("ghost");
        let before = context_hash(&root);
        fs::create_dir_all(root.join(".lex/ontology/ghost")).unwrap();
        fs::write(root.join(".lex/ontology/ghost/ghost.ttl"), "ghost:anything").unwrap();
        assert_eq!(before, context_hash(&root));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn roundtrip_hit_after_save() {
        let root = tmp_root("roundtrip");
        let ctx = context_hash(&root);
        let files = vec!["a.md".to_string()];
        let mut c = WalkCache::empty(&root, &ctx, &files);
        c.store("a.md", "bh1", "ih1", "<s> <p> <o> <g> .\n", 2, true);
        c.save();

        let mut loaded = WalkCache::load(&root, &ctx, &files).expect("cache loads");
        assert!(loaded.changed_names().is_empty());
        let (frag, entry) = loaded.hit("a.md", "bh1", "ih1", true).expect("hit");
        assert_eq!(frag, "<s> <p> <o> <g> .\n");
        assert_eq!(entry.links, 2);
        assert_eq!(entry.quads, 1);
        // Either hash off → miss.
        assert!(loaded.hit("a.md", "bhX", "ih1", true).is_none());
        assert!(loaded.hit("a.md", "bh1", "ihX", true).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn context_mismatch_refuses_to_load() {
        let root = tmp_root("ctx");
        let ctx = context_hash(&root);
        let c = WalkCache::empty(&root, &ctx, &[]);
        c.save();
        assert!(WalkCache::load(&root, "different", &[]).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ontology_byte_change_changes_context() {
        let root = tmp_root("ont");
        let before = context_hash(&root);
        fs::write(root.join(".lex/ontology/t/t.ttl"), "t:changed").unwrap();
        assert_ne!(before, context_hash(&root), "gate 2: ontology bytes");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unproven_entries_prune_on_save() {
        let root = tmp_root("prune");
        let ctx = context_hash(&root);
        let mut c = WalkCache::empty(&root, &ctx, &[]);
        c.store("keep.md", "b", "i", "x\n", 0, true);
        c.save();
        // Next run proves nothing, stores one new file.
        let mut c2 = WalkCache::load(&root, &ctx, &[]).unwrap();
        c2.store("only.md", "b", "i", "y\n", 0, true);
        c2.save();
        let c3 = WalkCache::load(&root, &ctx, &[]).unwrap();
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
        fs::create_dir_all(crate::layout::kit_ontology_dir(&root, "t")).unwrap();
        let ctx = context_hash(&root);

        // Run 1: two documents seen, two fragments written.
        let mut c = WalkCache::empty(&root, &ctx, &[]);
        c.store("a.md", "bh-a", "ih-a", "<x> <y> <z> .\n", 0, true);
        c.store("b.md", "bh-b", "ih-b", "<p> <q> <r> .\n", 0, true);
        c.save();
        assert!(root.join(".lex/_ignore/walkcache/frag/a.md.nq").exists());
        assert!(root.join(".lex/_ignore/walkcache/frag/b.md.nq").exists());

        // Run 2: b.md is gone from disk, so the walk never sees it.
        let mut c2 = WalkCache::empty(&root, &ctx, &[]);
        c2.store("a.md", "bh-a", "ih-a", "<x> <y> <z> .\n", 0, true);
        c2.save();

        assert!(root.join(".lex/_ignore/walkcache/frag/a.md.nq").exists(),
            "a surviving document keeps its fragment");
        assert!(!root.join(".lex/_ignore/walkcache/frag/b.md.nq").exists(),
            "a DELETED document must not keep answering queries — this fragment outliving its \
             source is what made a reference to a deleted document read as perfectly resolved, \
             defeating the only dangling-reference check the fleet had");

        let _ = fs::remove_dir_all(&root);
    }

    /// #39: an entry written by a walk that did not write sidecars says so,
    /// and says so again after a round trip through the manifest.
    #[test]
    fn sidecar_flag_survives_the_manifest() {
        let root = tmp_root("sidecar-flag");
        let mut c = WalkCache::empty(&root, "ctx", &[]);
        c.store("q.md", "b", "i", "<s> <p> <o> <g> .\n", 0, false);
        c.store("s.md", "b", "i", "<s> <p> <o> <g> .\n", 0, true);
        c.save();

        let mut loaded = WalkCache::load(&root, "ctx", &[]).expect("manifest loads");
        assert!(!loaded.hit("q.md", "b", "i", false).unwrap().1.sidecars);
        assert!(loaded.hit("s.md", "b", "i", false).unwrap().1.sidecars);
        let _ = fs::remove_dir_all(&root);
    }

    /// A document that came or went no longer throws the cache away: the
    /// cache loads, and names what changed, so only the documents that
    /// mention it are extracted again.
    #[test]
    fn added_and_removed_files_name_what_changed() {
        let root = tmp_root("existence");
        let ctx = context_hash(&root);
        let before = vec!["Soul/Note/a.md".to_string(), "Soul/Note/gone.md".to_string()];
        let mut c = WalkCache::empty(&root, &ctx, &before);
        c.store("Soul/Note/a.md", "bh1", "ih1", "", 0, true);
        c.save();

        let now = vec!["Soul/Note/a.md".to_string(), "Soul/Journal/day-9.md".to_string()];
        let mut loaded = WalkCache::load(&root, &ctx, &now).expect("an added file keeps the cache");
        assert_eq!(loaded.changed_names(), ["day-9".to_string(), "gone".to_string()]);
        assert!(loaded.hit("Soul/Note/a.md", "bh1", "ih1", false).is_some());

        let names = loaded.changed_names().to_vec();
        // Every way a document can point at the new one spells its name.
        for text in [
            "see [day 9](/Soul/Journal/day-9.md)",
            "see [day 9](../Journal/day-9)",
            "related: Soul/Journal/day-9.md",
            "[gone](gone.md#top)",
        ] {
            assert!(WalkCache::mentions_changed_file(&names, text), "{text}");
        }
        assert!(!WalkCache::mentions_changed_file(&names, "see [a](/Soul/Note/a.md)"));
        let _ = fs::remove_dir_all(&root);
    }

    /// A manifest whose file list does not match the hash it recorded (a
    /// run killed between the two writes) is not trusted at all.
    #[test]
    fn mismatched_file_list_is_a_full_walk() {
        let root = tmp_root("torn");
        let ctx = context_hash(&root);
        let files = vec!["a.md".to_string()];
        let c = WalkCache::empty(&root, &ctx, &files);
        c.save();
        fs::write(cache_dir(&root).join("files.tsv"), "a.md\nb.md\n").unwrap();
        assert!(WalkCache::load(&root, &ctx, &files).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reference_needles() {
        assert_eq!(reference_needle("Soul/Note/day-9.md").as_deref(), Some("day-9"));
        assert_eq!(reference_needle("img/photo.png").as_deref(), Some("photo"));
        // Percent-encoding keeps URL-safe runs verbatim: the longest one.
        assert_eq!(reference_needle("Notes/my long title.md").as_deref(), Some("title"));
        assert_eq!(reference_needle("README").as_deref(), Some("README"));
        assert_eq!(reference_needle("Notes/日本.md"), None);
    }
}
