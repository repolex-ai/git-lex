# Hook path regression checks

The pre-commit directory is resolved by `git rev-parse --git-path hooks`
from the worktree root. This delegates linked-worktree, separate-git-dir,
and `core.hooksPath` resolution to Git. A relative result is anchored at
that root, where Git runs pre-commit, not at the CLI caller's nested cwd.
No config is changed by discovery. Spaces in paths are preserved. Discovery
errors, empty output, non-UTF-8 output and embedded control characters refuse
installation; no guessed `.git/hooks` fallback is used. This deliberately
fails closed for otherwise legal non-UTF-8/control-containing POSIX paths.
Convergence resolves once, then reads and installs at that same directory.

Run the normal upstream CI checks:

```
cargo build --locked
cargo test --locked
cargo clippy --locked --all-targets
cargo install --path . --locked --root <disposable-install>
<disposable-install>/bin/git-lex --version
```

The focused Rust test uses actual Git repositories and the production hook
module (no alternate implementation):

```
cargo test --locked --test hooks_paths -- --nocapture
rustfmt --edition 2024 --check src/hooks.rs tests/hooks_paths.rs
```

For actual save/commit controls (Python stdlib only), pass a built binary and
a new disposable output directory:

```
python tests/hooks_e2e.py target/debug/git-lex <new-disposable-output>
```

This creates ordinary, linked (`.git` file), and separate-git-dir repositories;
for each it tests default, relative and absolute hook directories (including
spaces), nested-cwd convergence, executable mode, existing hook preservation,
SHACL success and refusal, duplicate Thing identity refusal, author identity,
sidecar cleanup, foreign-hook refusal and damaged managed markers. A Git proxy
also tests failed, empty, multiline, NUL and invalid UTF-8 discovery responses.
Every subprocess records argv, cwd, exit and a raw-output SHA256; failures
assert that HEAD is unchanged. Only `git lex save` creates fixture commits.
No daemon, remote kit, shared Git configuration or installation is required.
The test needs Git with `worktree add --orphan` support (tested with 2.56.0).

When all temporary state must stay beneath a checkout, set `TMPDIR` to a
checkout-local disposable directory. Some pre-existing tests explicitly need
non-repository directories: prevent upward discovery with
`GIT_CEILING_DIRECTORIES=$TMPDIR` for the Git CLI and an invalid `.git` gitfile
at `$TMPDIR/.git` for libgit2 (pointing to an absent path inside that disposable
directory). Child fixture repositories with their own valid `.git` still work.
Keep HOME/XDG_CONFIG_HOME and Cargo install root disposable as well. Use an
isolated CARGO_HOME if the shared Cargo cache must not be written.

`cargo fmt --all -- --check` currently reports pre-existing repository-wide
formatting differences on upstream 5576a1ac; the two touched Rust files pass
the focused rustfmt check. This change does not reformat unrelated files.
The five upstream ignored golden-receipt tests remain ignored. These checks
were run on Linux, not macOS or Windows; hook management remains POSIX-only.
