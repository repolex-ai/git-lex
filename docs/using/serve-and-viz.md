# Serve & visualize

*Last updated for git-lex v0.1.0 (2026-08-12)*

git-lex ships two local servers (in the `git-lex-serve` binary; `git lex
serve` starts one per invocation):

```bash
git lex serve viz      # graph visualizer web UI (port 7878, opens your browser)
git lex serve sparql   # W3C SPARQL endpoint with a Swagger UI (port 7880)
```

Both serve the **synced store** — run `git lex sync` first, or they refuse to
start (viz) / answer 503 (sparql). Override the port with `--port`; the viz
also walks up to the next free port if its default is taken.

The viz shows recent activity, an interactive graph of your documents and
their links, and an animated replay of your knowledge graph growing
commit by commit.

The sparql server answers at `/sparql` (GET and POST, standard W3C SPARQL
protocol), with interactive docs at `/swagger-ui` and `/health` + `/info`
endpoints.

Note the split: `git lex query` does **not** read the synced store — it
queries a fresh view of your working tree as it is right now. History
questions ("when did this change?") go to the synced store via
`git lex serve sparql`; the ready-made history query is in
[Querying](queries.md).

<!-- TODO(additive): screenshots; the HTTP API -->
