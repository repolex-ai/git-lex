# gitlexd

`gitlexd` is the git-lex service. One runs per machine. It holds the synced
store of every git-lex repository the machine knows, keeps each store in
step with its repository's commits, and answers SPARQL over HTTP on
localhost. `git lex query` is its client.

```bash
gitlexd start     # stop any other gitlexd on this machine, then run in this terminal
gitlexd stop      # stop every gitlexd on this machine
gitlexd status    # is one running, and what does it hold
```

No flags and no configuration file. The repositories come from
`~/.lex/repos.json`, which every git-lex command writes for the repository
it runs in; the port is 7880; the log is `~/.lex/logs/gitlexd.log`. A
terminal starts gitlexd and owns it; there is no launchd job.

## What it does

Each repository is a *soul*, keyed by its first commit hash (the same value
`.lex/repo.yml` records as `genesis_sha`). The path is where the soul
happens to be; a clone or a move keeps the identity.

- **Owns the store.** gitlexd opens each soul's `.lex/_ignore/oxigraph`
  with the write lock and is the only writer while it runs.
- **Syncs when HEAD moves.** `git lex save` tells gitlexd after its commit
  (one request, no wait), and gitlexd also reads each repository's HEAD
  every two seconds, so a plain `git commit`, a `git pull`, or a save made
  while gitlexd was down all reach the graph. Each sync runs as a worker
  process of gitlexd with the soul as its working directory; souls sync in
  parallel, one sync per soul at a time. The daemon closes its handle on
  the store while the worker writes and reopens it after.
- **Queries wait for a sync in flight.** A query never reads a half-written
  store. A soul that has never been synced is synced first.
- **`git lex sync` hands over.** When gitlexd is running, `git lex sync`
  asks it to sync and waits for the result. Without gitlexd, the sync runs
  in that process as before.

## Which soul a query goes to

`git lex query` is tied to the repository the session started in. It reads
nothing from a flag, a setting or the shell's current directory: under
Claude Code it takes the harness process's working directory (`CLAUDE_PID`);
at a plain terminal, the terminal's. From there: the git root, then the
first commit hash. `git lex direct`, by contrast, follows the shell's
current directory and needs no gitlexd.

A repository initialized after gitlexd started is not held until
`gitlexd restart`; `git lex sync` says so and syncs in process meanwhile.

## HTTP interface

All under `http://127.0.0.1:7880`. `<genesis>` is the first commit hash,
full or an unambiguous prefix.

| Method and path | What it does |
|:---|:---|
| `GET` / `POST /soul/<genesis>/sparql` | The W3C SPARQL 1.1 protocol over that soul's store (SPARQL 1.2 triple terms included). `application/sparql-query`, form `query=`, or JSON `{"query": …}`. SELECT/ASK answer `application/sparql-results+json`; CONSTRUCT/DESCRIBE `application/n-triples`. A pattern with no GRAPH clause sees every graph. |
| `GET /soul/<genesis>/info` | Synced-to commit, whether a sync is in flight, quad count, the graph inventory, kits. |
| `POST /soul/<genesis>/sync` | Ask for a sync. Returns `202` at once; with `?wait=1` returns when the sync is done, with the soul's state. |
| `GET /souls` | Every soul gitlexd holds: genesis, path, name, synced-to commit, errors. |
| `GET /health` | gitlexd is up; uptime, pid, number of souls. |

```bash
curl -X POST http://127.0.0.1:7880/soul/495d8c70/sparql \
  -H "Content-Type: application/sparql-query" \
  --data "SELECT * WHERE { ?s ?p ?o } LIMIT 10"
```

gitlexd answers for its own stores only. A query that names a `SERVICE`
gets an error; a question across stores (pan, ravel) is for syrinxd.
