//! The `.lex/` folder, spelled once (#36).
//!
//! Every path under a repo's `.lex/` comes from a function here. Nothing
//! else joins `".lex"`, so moving a folder is one edit, and the layout is
//! readable in one screen:
//!
//! ```text
//! .lex/
//!   repo.yml          the repo's kit, identity and genesis (git-lex writes it)
//!   identity.yml      commit identity, when a substrate does not carry one
//!   kit/<org>/<repo>/ each installed kit, as fetched
//!   ontology/<kit>/   each kit's ontology files, as installed
//!   www/              web assets the kits ship
//!   query/            stored queries
//!   extract/          the sidecars: one .fm.spo and .md.spo per document
//!   _ignore/          derived, never committed:
//!     oxigraph/         the store
//!     walkcache/        the walk's fragment cache
//!     spine/            the exported spine
//!     cottas/           where the spine used to be
//! ```
//!
//! The machine registry (`~/.lex/repos`) is not a repo folder and lives in
//! the registry code, not here.

use std::path::{Path, PathBuf};

/// The folder's name at the repo root.
pub const LEX_DIR: &str = ".lex";

/// `.lex/`
pub fn lex_dir(root: &Path) -> PathBuf {
    root.join(LEX_DIR)
}

/// `.lex/repo.yml`
pub fn repo_yml(root: &Path) -> PathBuf {
    lex_dir(root).join("repo.yml")
}

/// `.lex/identity.yml`
pub fn identity_yml(root: &Path) -> PathBuf {
    lex_dir(root).join("identity.yml")
}

/// `.lex/kit/` — every installed kit, as fetched, under `<org>/<repo>/`.
pub fn kits_dir(root: &Path) -> PathBuf {
    lex_dir(root).join("kit")
}

/// `.lex/kit/<org>/<repo>/`
pub fn kit_dir(root: &Path, org: &str, repo: &str) -> PathBuf {
    kits_dir(root).join(org).join(repo)
}

/// `.lex/ontology/` — every installed kit's ontology files, under its short name.
pub fn ontology_dir(root: &Path) -> PathBuf {
    lex_dir(root).join("ontology")
}

/// `.lex/ontology/<kit>/`
pub fn kit_ontology_dir(root: &Path, kit: impl AsRef<Path>) -> PathBuf {
    ontology_dir(root).join(kit)
}

/// `.lex/www/`
pub fn www_dir(root: &Path) -> PathBuf {
    lex_dir(root).join("www")
}

/// `.lex/query/`
pub fn query_dir(root: &Path) -> PathBuf {
    lex_dir(root).join("query")
}

/// `.lex/extract/`
pub fn extract_dir(root: &Path) -> PathBuf {
    lex_dir(root).join("extract")
}

/// `.lex/_ignore/` — derived state, never committed.
pub fn ignore_dir(root: &Path) -> PathBuf {
    lex_dir(root).join("_ignore")
}

/// `.lex/_ignore/oxigraph/` — the store.
pub fn store_dir(root: &Path) -> PathBuf {
    ignore_dir(root).join("oxigraph")
}

/// `.lex/_ignore/walkcache/`
pub fn walkcache_dir(root: &Path) -> PathBuf {
    ignore_dir(root).join("walkcache")
}

/// `.lex/_ignore/spine/`
pub fn spine_dir(root: &Path) -> PathBuf {
    ignore_dir(root).join("spine")
}

/// `.lex/_ignore/cottas/` — the spine's former home, swept when found.
pub fn cottas_dir(root: &Path) -> PathBuf {
    ignore_dir(root).join("cottas")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout as one table, so a change here is a visible diff.
    #[test]
    fn every_path_under_lex() {
        let r = Path::new("/r");
        let rel = |p: PathBuf| p.strip_prefix(r).unwrap().to_string_lossy().replace('\\', "/");
        assert_eq!(rel(lex_dir(r)), ".lex");
        assert_eq!(rel(repo_yml(r)), ".lex/repo.yml");
        assert_eq!(rel(identity_yml(r)), ".lex/identity.yml");
        assert_eq!(rel(kits_dir(r)), ".lex/kit");
        assert_eq!(rel(kit_dir(r, "repolex-ai", "git-lex-kit-soul")), ".lex/kit/repolex-ai/git-lex-kit-soul");
        assert_eq!(rel(ontology_dir(r)), ".lex/ontology");
        assert_eq!(rel(kit_ontology_dir(r, "soul")), ".lex/ontology/soul");
        assert_eq!(rel(www_dir(r)), ".lex/www");
        assert_eq!(rel(query_dir(r)), ".lex/query");
        assert_eq!(rel(extract_dir(r)), ".lex/extract");
        assert_eq!(rel(ignore_dir(r)), ".lex/_ignore");
        assert_eq!(rel(store_dir(r)), ".lex/_ignore/oxigraph");
        assert_eq!(rel(walkcache_dir(r)), ".lex/_ignore/walkcache");
        assert_eq!(rel(spine_dir(r)), ".lex/_ignore/spine");
        assert_eq!(rel(cottas_dir(r)), ".lex/_ignore/cottas");
    }
}
