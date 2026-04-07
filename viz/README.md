# git-lex viz UI

This is the embedded web UI for `git lex viz`. It runs in the user's browser when they start the viz server.

## Architecture

```
User runs:  git lex viz
                 ↓
         Rust HTTP server (axum)
                 ↓
   ┌─────────────┴─────────────┐
   │                           │
   ↓                           ↓
GET /             POST /api/query
(serves index.html)  (runs SPARQL, returns JSON)
                              ↓
                    GET /ws (WebSocket)
                    (live push from agents)
```

The Rust side reads from `.lex/oxigraph` (the local SPARQL store). The browser-side talks to `/api/query` for queries and `/ws` for live updates.

## Files

- `index.html` — entry point, served on `GET /`
- `css/` — stylesheets
- `js/` — JavaScript modules
- `assets/` — fonts, icons, static files

All files in this directory are **embedded into the git-lex binary at compile time** via `include_str!` / `include_bytes!`. To add a new file:

1. Add the file here
2. Add a corresponding `include_str!("../viz/path/to/file")` in `src/main.rs`
3. Add a route in the axum router to serve it

## API Reference

### `POST /api/query`

Run a SPARQL SELECT query against the oxigraph store.

Request:
```json
{ "query": "SELECT ?s ?p ?o WHERE { GRAPH ?g { ?s ?p ?o } } LIMIT 10" }
```

Response:
```json
{
  "vars": ["s", "p", "o"],
  "results": [
    { "s": "https://...", "p": "https://...", "o": "..." }
  ]
}
```

### `GET /ws` (WebSocket)

Bidirectional channel. Currently echoes — agents will eventually push CONSTRUCT results here for live viz updates.

## Conventions

For the visualization layer, use the `viz:` namespace in CONSTRUCT queries to attach UI hints to triples:

- `viz:shape` — `circle`, `square`, `hexagon`, etc.
- `viz:color` — hex color
- `viz:label` — display label
- `viz:size` — node size
- `viz:edgeTo` — edge target
- `viz:layout` — `force`, `radial`, `tree`, `timeline`

The frontend reads these properties and renders accordingly. The agent pushes CONSTRUCT results, the frontend draws them.

## Workspace

W3BL0RD is the primary maintainer. Free to add D3, vanilla JS, whatever feels right. Keep it dependency-light if possible — embedded assets bloat the binary.
