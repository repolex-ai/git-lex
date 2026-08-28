# Serve & Visualize

*Last updated for git-lex v0.1.1 (2026-08-27)*

`git-lex` provides two distinct local services to query, explore, and visualize your knowledge graph. Both servers run via the companion binary `git-lex-serve`:

```bash
git lex serve viz      # Interactive graph visualizer UI (default port: 7878)
git lex serve sparql   # Standard W3C SPARQL endpoint & OpenAPI UI (default port: 7880)
```

Both services read from the **synced store**. You must run `git lex sync` to populate the store before launching either server; otherwise, the visualization server will refuse to start, and the SPARQL server will return `503 Service Unavailable` errors.

> [!TIP]
> You can override default ports using the `--port` flag (e.g., `git lex serve viz --port 8080`). If the default visualization port `7878` is already in use, the server will automatically search for the next available port.

---

## 1. The Interactive Visualizer (`serve viz`)

The visualization interface provides a user-friendly graphical exploration of your repository:
* **Interactive Graph Network:** Vertices represent Things (conceptual nodes) and Files, and edges display relationship predicates (e.g., `linksTo`, `relatedToId`, `fileId`).
* **Timeline Replay:** An animated replay control allows you to watch the knowledge graph build and grow step-by-step through every commit in the repository's history.
* **Activity Stream:** A feed panel lists recent document updates, creation dates, and editing substrates.

---

## 2. The SPARQL Endpoint & HTTP API (`serve sparql`)

The SPARQL server exposes a W3C-compliant endpoint alongside several utility paths:

### Core HTTP Endpoints

| Endpoint | Method | Description |
|:---|:---|:---|
| `/sparql` | `GET` / `POST` | The W3C SPARQL protocol endpoint. Accepts standard SPARQL 1.1 Query payloads. |
| `/swagger-ui` | `GET` | Interactive OpenAPI documentation web interface for querying. |
| `/health` | `GET` | Simple health check endpoint. Returns `200 OK` when the store is online. |
| `/info` | `GET` | Metadata about the synced store, including the latest synced commit SHA and total triple counts. |

### Querying over the Network
To run queries against the local database endpoint programmatically, send a request to `/sparql`:

```bash
curl -X POST http://127.0.0.1:7880/sparql \
  -H "Content-Type: application/sparql-query" \
  -H "Accept: application/sparql-results+json" \
  --data "SELECT * WHERE { ?s ?p ?o } LIMIT 10"
```

> [!IMPORTANT]
> Keep in mind the architectural split: `git lex query` queries the **active working tree** (uncommitted edits included). Conversely, `git lex serve` endpoints read from the **synced store** (committed history). For historical query recipes, see the [Querying Documentation](queries.md).

