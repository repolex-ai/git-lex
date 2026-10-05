//! Build fingerprint: one id for everything built from this source tree.
//!
//! The walk cache must be thrown away whenever the code that produced it
//! changes, and kept when it has not. It used to key on the running
//! executable's path, size and modification time — but `git lex save`
//! runs `git-lex` and gitlexd's sync runs `gitlexd`, two different files,
//! so each threw away the cache the other had just written and every save
//! and every sync re-read the whole repository (stress test, #51). Both
//! binaries are built from the same sources, so a fingerprint of the
//! sources (and the dependency lock) is the identity they share.

use std::path::Path;

fn fnv1a(hash: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *hash ^= u64::from(*b);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn collect(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn main() {
    let mut files = vec![Path::new("Cargo.toml").to_path_buf(), Path::new("Cargo.lock").to_path_buf()];
    collect(Path::new("src"), &mut files);
    files.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for f in &files {
        fnv1a(&mut hash, f.to_string_lossy().as_bytes());
        fnv1a(&mut hash, &std::fs::read(f).unwrap_or_default());
    }
    println!("cargo:rustc-env=GIT_LEX_BUILD_ID={hash:016x}");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
}
