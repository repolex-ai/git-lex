"""Disposable save-gate contract: python tests/hooks_e2e.py BINARY OUTPUT_DIR.
No installs, daemon startup, direct git commits or shared config mutations.
All subprocess argv/cwd/status/output and output hashes are retained.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

binary = Path(sys.argv[1]).resolve()
base = Path(sys.argv[2]).resolve()
base.mkdir(parents=True, exist_ok=False)
env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
home = base / 'home'
home.mkdir()
bin_dir = base / 'bin'
bin_dir.mkdir()
(bin_dir / 'git-lex').symlink_to(binary)
env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / 'config'), GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null', PATH=str(bin_dir) + ':' + os.environ['PATH'], GIT_AUTHOR_NAME='Gate Test', GIT_AUTHOR_EMAIL='gate@example.invalid', GIT_COMMITTER_NAME='Gate Test', GIT_COMMITTER_EMAIL='gate@example.invalid')
count = 0


def run(cwd, argv, ok=True, extra=None):
    global count
    count += 1
    result = subprocess.run([str(a) for a in argv], cwd=cwd, env=env | (extra or {}), stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    output = result.stdout
    log = base / f'{count:03}.log'
    log.write_bytes(output)
    receipt = dict(argv=[str(a) for a in argv], cwd=str(cwd), exit=result.returncode, log=str(log), sha256=hashlib.sha256(output).hexdigest())
    with (base / 'commands.jsonl').open('a') as f:
        f.write(json.dumps(receipt) + '\n')
    print(json.dumps(receipt), flush=True)
    assert (result.returncode == 0) == ok, output.decode(errors='replace')
    return output.decode(errors='replace').strip()


def write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)


def fixture(repo):
    # A tiny local domain kit, no remote kit or registry dependency.
    write(repo / '.lex/repo.yml', 'name: fixture\nkit: repolex-ai/git-lex-kit-test\n')
    write(repo / '.gitignore', '.lex/_ignore/\n')
    write(repo / '.lex/ontology/test/test.ttl', '''@prefix test: <https://example.invalid/test/> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
test:Note a owl:Class .
test:noteId a owl:DatatypeProperty ; rdfs:domain test:Note .
test:title a owl:DatatypeProperty ; rdfs:domain test:Note .
''')
    write(repo / '.lex/ontology/test/test-shapes.ttl', '''@prefix test: <https://example.invalid/test/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
test:NoteShape a sh:NodeShape ; sh:targetClass test:Note ;
 sh:property [ sh:path test:title ; sh:minCount 1 ] ;
 sh:property [ sh:path test:noteId ; sh:minCount 1 ] .
''')
    write(repo / 'note.md', '---\ntest.Note.noteId: one\ntest.Note.title: Valid\n---\nHello\n')


ordinary = base / 'ordinary'
ordinary.mkdir()
run(ordinary, ['git', 'init', '-q'])
linked = base / 'linked'
run(ordinary, ['git', 'worktree', 'add', '--orphan', '-b', 'linked', linked])
assert (linked / '.git').is_file()
separate = base / 'separate'
separate.mkdir()
run(separate, ['git', 'init', '-q', '--separate-git-dir', base / 'separate.git'])
for repo in (ordinary, linked, separate):
    fixture(repo)
    nested = repo / 'nested/deep'
    nested.mkdir(parents=True)
    for custom in (None, 'relative hooks', str(base / ('absolute hooks ' + repo.name))):
        if custom:
            run(repo, ['git', 'config', 'core.hooksPath', custom])
        hooks_path = Path(run(repo, ['git', 'rev-parse', '--git-path', 'hooks']))
        if not hooks_path.is_absolute():
            hooks_path = repo / hooks_path
        hook = hooks_path / 'pre-commit'
        write(hook, '#!/bin/sh\nprintf user-hook-ran\\n >&2\n')
        out = run(nested, [binary, 'save', '--dry-run'])
        assert 'all save gates pass' in out and 'Validated 1 file' in out, out
        installed = hook.read_text()
        assert 'printf user-hook-ran' in installed and 'hook pre-commit || exit 1' in installed
        assert hook.stat().st_mode & 0o111
        # Use root for real saves: the upstream save stages cwd-relative paths.
        write(repo / 'change.txt', str(custom))
        out = run(repo, [binary, 'save', '--no-restamp', 'fixture validated save'])
        assert 'user-hook-ran' in out and 'Validated 1 file' in out, out
        head = run(repo, ['git', 'rev-parse', 'HEAD'])
        assert run(repo, ['git', 'show', '-s', '--format=%an <%ae>']) == 'Gate Test <gate@example.invalid>'
        # The identity gate must reject a duplicate Thing, not merely run on
        # an empty set of identities.
        write(repo / 'duplicate.md', (repo / 'note.md').read_text())
        out = run(repo, [binary, 'save', '--no-restamp', 'duplicate identity refuses'], ok=False)
        assert 'both claim the Thing' in out, out
        assert run(repo, ['git', 'rev-parse', 'HEAD']) == head
        (repo / 'duplicate.md').unlink()
        # Validation refusal must leave history unchanged.
        write(repo / 'note.md', '---\ntest.Note.noteId: one\n---\nMissing required title\n')
        out = run(repo, [binary, 'save', '--no-restamp', 'must refuse invalid document'], ok=False)
        assert 'violation' in out.lower(), out
        assert run(repo, ['git', 'rev-parse', 'HEAD']) == head
        write(repo / 'note.md', '---\ntest.Note.noteId: one\ntest.Note.title: Valid\n---\nHello\n')
        # Existing foreign refusal remains authoritative after convergence.
        write(hook, '#!/bin/sh\necho foreign-refusal >&2\nexit 23\n')
        write(repo / 'change.txt', 'foreign hook must block')
        out = run(repo, [binary, 'save', 'foreign hook refuses'], ok=False)
        assert 'foreign-refusal' in out and 'exit 23' in hook.read_text()
        assert run(repo, ['git', 'rev-parse', 'HEAD']) == head
        # Damaged markers may not be rewritten or ignored.
        damaged = '#!/bin/sh\n# --- git-lex managed (do not edit this section) ---\nforeign\n'
        write(hook, damaged)
        out = run(repo, [binary, 'save', 'damaged gate refuses'], ok=False)
        assert 'damaged' in out and 'NO validation' in out
        assert hook.read_text() == damaged
        assert run(repo, ['git', 'rev-parse', 'HEAD']) == head
        write(hook, installed)
        # Cleanup still retracts a removed document's sidecars on real save.
        sidecar = repo / '.lex/extract/note.md.fm.spo'
        assert sidecar.exists()
        (repo / 'note.md').unlink()
        run(repo, [binary, 'save', '--no-restamp', 'remove document with cleanup'])
        assert not sidecar.exists()
        write(repo / 'note.md', '---\ntest.Note.noteId: one\ntest.Note.title: Valid\n---\nHello\n')
        if custom:
            run(repo, ['git', 'config', '--unset', 'core.hooksPath'])
    print(f'PASS topology + three hooksPath variants: {repo.name}', flush=True)

# Failed Git discovery must not fall back to .git/hooks, including malformed
# successful responses. The proxy delegates every command except discovery.
real_git = shutil.which('git', path=os.environ['PATH'])
assert real_git is not None, 'git must be on PATH'
proxy = base / 'proxy'
proxy.mkdir()
for label, body in [('failed', 'echo discovery-failed >&2; exit 42'), ('empty', 'exit 0'), ('multiline', "printf 'hooks\\nforeign\\n'"), ('nul', "printf 'hooks\\000bad\\n'"), ('nonutf8', "printf '\\377\\n'")]:
    write(proxy / 'git', '#!/bin/sh\nif [ "$1 $2 $3" = "rev-parse --git-path hooks" ]; then\n' + body + '\nelse\nexec ' + real_git + ' "$@"\nfi\n')
    (proxy / 'git').chmod(0o755)
    head = run(ordinary, ['git', 'rev-parse', 'HEAD'])
    out = run(ordinary, [binary, 'save', 'discovery must fail closed'], ok=False, extra={'PATH': str(proxy) + ':' + env['PATH']})
    assert 'cannot install the pre-commit save gate' in out, (label, out)
    assert run(ordinary, ['git', 'rev-parse', 'HEAD']) == head
    print('PASS discovery refusal: ' + label, flush=True)
print(f'PASS: {count} subprocesses; all raw command receipts in {base}', flush=True)
