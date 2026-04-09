// git-lex viz — main entry point
// Three modes: Overview, Graph, Push
// W3BL0RD's domain. Pod with W4R3Z on the Rust side.

const API = '';
const WS_URL = 'ws://' + location.host + '/ws';

// ════════════════════════════════════════════
// SPARQL helpers
// ════════════════════════════════════════════

async function sparql(query) {
    try {
        const r = await fetch(API + '/api/query', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ query })
        });
        const data = await r.json();
        if (data.error) {
            console.warn('SPARQL error:', data.error, '\nquery:', query);
            return [];
        }
        return data.results || [];
    } catch (e) {
        console.error('SPARQL fetch failed:', e);
        return [];
    }
}

// Strip namespace prefix to get a short label from a URI.
function shortName(uri) {
    if (!uri) return '';
    const hash = uri.lastIndexOf('#');
    if (hash >= 0) return uri.substring(hash + 1);
    const slash = uri.lastIndexOf('/');
    if (slash >= 0) return uri.substring(slash + 1);
    return uri;
}

// Strip extension from a filename
function stripExt(name) {
    const dot = name.lastIndexOf('.');
    return dot > 0 ? name.substring(0, dot) : name;
}

// ════════════════════════════════════════════
// Mode routing
// ════════════════════════════════════════════

const modes = ['activity', 'graph', 'interactive'];
const views = {};
modes.forEach(m => views[m] = document.getElementById('view-' + m));
const sidebarRight = document.getElementById('sidebar-right');

let currentMode = null;
const loaded = new Set();

function setMode(mode) {
    if (!modes.includes(mode)) mode = 'activity';
    currentMode = mode;

    document.querySelectorAll('.mode-link').forEach(a => {
        a.classList.toggle('active', a.dataset.mode === mode);
    });

    modes.forEach(m => {
        views[m].hidden = (m !== mode);
    });

    // Right sidebar (class toggles + stats) only on graph mode
    sidebarRight.hidden = (mode !== 'graph');

    if (!loaded.has(mode)) {
        loaded.add(mode);
        if (mode === 'activity') loadActivity();
        if (mode === 'graph') loadGraph();
    }

    if (mode === 'graph') resizeGraph();
}

function initRouting() {
    document.querySelectorAll('.mode-link').forEach(a => {
        a.addEventListener('click', e => {
            e.preventDefault();
            const mode = a.dataset.mode;
            location.hash = mode;
            setMode(mode);
        });
    });

    window.addEventListener('hashchange', () => {
        const mode = location.hash.replace('#', '') || 'activity';
        setMode(mode);
    });

    const initial = location.hash.replace('#', '') || 'activity';
    setMode(initial);
}

// ════════════════════════════════════════════
// WebSocket — push listener
// ════════════════════════════════════════════

const status = document.getElementById('status');
let ws = null;

function setStatus(text, cls) {
    status.textContent = text;
    status.className = 'status ' + (cls || '');
}

function connectWS() {
    setStatus('connecting…', 'connecting');
    try {
        ws = new WebSocket(WS_URL);
    } catch (e) {
        setStatus('error', 'error');
        setTimeout(connectWS, 3000);
        return;
    }

    ws.onopen = () => setStatus('connected', 'connected');
    ws.onclose = () => {
        setStatus('disconnected', 'error');
        setTimeout(connectWS, 3000);
    };
    ws.onerror = () => setStatus('error', 'error');
    ws.onmessage = (e) => {
        try {
            const msg = JSON.parse(e.data);
            if (msg.type === 'scene') {
                handlePush(msg.data || {});
            }
        } catch {
            // Ignore non-JSON messages
        }
    };
}

// ════════════════════════════════════════════
// RECENT ACTIVITY (landing page)
// ════════════════════════════════════════════

async function loadActivity() {
    const view = views.activity;

    const [repoInfo, recentCommits, timeline] = await Promise.all([
        loadRepoInfo(),
        loadRecentCommits(30),
        loadCommitTimeline(),
    ]);

    let html = '';

    // Repo header
    html += '<div class="repo-header">';
    html += `<h1>${repoInfo.name || 'Repository'}</h1>`;
    html += '<div class="repo-subtitle">';
    if (repoInfo.kit) html += `<span>kit: ${repoInfo.kit}</span>`;
    if (repoInfo.created) html += `<span>since ${repoInfo.created}</span>`;
    if (repoInfo.commits) html += `<span>${repoInfo.commits} commits</span>`;
    if (repoInfo.docs) html += `<span>${repoInfo.docs} documents</span>`;
    if (repoInfo.totalTriples) html += `<span>${repoInfo.totalTriples.toLocaleString()} triples</span>`;
    html += '</div>';
    html += '</div>';

    // History scrubber timeline (Day 7 sketch — slider stub).
    // Wires to W4R3Z's planned /api/scrub?commit={sha} endpoint when it ships.
    if (timeline.length > 1) {
        html += renderTimeline(timeline);
    }

    // Recent activity
    if (recentCommits.length > 0) {
        html += '<div class="section">';
        html += '<div class="section-title">Recent activity</div>';
        html += '<div class="activity-list">';
        recentCommits.forEach(c => {
            html += '<div class="activity-row">';
            html += `<div class="when">${c.when}</div>`;
            html += `<div class="what">${escapeHtml(c.message)}</div>`;
            html += `<div class="changed">${c.changedHint || ''}</div>`;
            html += `<div class="who">${escapeHtml(c.author || '')}</div>`;
            html += '</div>';
        });
        html += '</div>';
        html += '</div>';
    }

    view.innerHTML = html;
    attachTimelineHandlers();
}

async function loadRepoInfo() {
    const info = { name: '', kit: '', version: '', created: '', commits: 0, docs: 0, totalTriples: 0 };

    // Read repo metadata from git:Repo entity
    const meta = await sparql(`
        PREFIX git: <https://repolex.ai/ontology/git-lex/git/>
        SELECT ?repo ?name ?kit ?version ?created WHERE {
            ?repo a git:Repo .
            OPTIONAL { ?repo git:name ?name }
            OPTIONAL { ?repo git:kit ?kit }
            OPTIONAL { ?repo git:version ?version }
            OPTIONAL { ?repo git:created ?created }
        } LIMIT 1
    `);
    if (meta[0]) {
        info.name = meta[0].name || '';
        info.kit = meta[0].kit || '';
        info.version = meta[0].version || '';
        info.created = meta[0].created || '';
        info.repoUri = meta[0].repo || '';
    }

    // Fall back to repo name from commit URI if no Repo entity
    if (!info.name) {
        const sample = await sparql(`
            PREFIX git: <https://repolex.ai/ontology/git-lex/git/>
            SELECT ?c WHERE { ?c a git:Commit } LIMIT 1
        `);
        if (sample[0]) {
            const m = sample[0].c.match(/^https?:\/\/[^/]+\/([^/]+\/[^/]+)\//);
            if (m) info.name = m[1];
        }
    }

    // Count commits
    const commits = await sparql(`
        PREFIX git: <https://repolex.ai/ontology/git-lex/git/>
        SELECT (COUNT(?c) AS ?n) WHERE { ?c a git:Commit }
    `);
    if (commits[0]) info.commits = parseInt(commits[0].n) || 0;

    // Count distinct documents
    const docs = await sparql(`
        PREFIX fm: <https://repolex.ai/ontology/git-lex/fm/>
        SELECT (COUNT(DISTINCT ?d) AS ?n) WHERE { ?d fm:title ?t }
    `);
    if (docs[0]) info.docs = parseInt(docs[0].n) || 0;

    // Total triples
    const total = await sparql(`SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }`);
    if (total[0]) info.totalTriples = parseInt(total[0].n) || 0;

    return info;
}

// Types that exist in the store but should NOT appear as Overview cards.
// - lex-upper/Document is the generic untyped-document fallback
// - RDF/OWL/SHACL meta types are infrastructure
const HIDDEN_TYPE_PREFIXES = [
    'https://repolex.ai/ontology/lex-upper/',
    'https://repolex.ai/ontology/lex-o/',
    'http://www.w3.org/2002/07/owl',
    'http://www.w3.org/2000/01/rdf-schema',
    'http://www.w3.org/1999/02/22-rdf-syntax-ns',
    'http://www.w3.org/ns/shacl',
];

const FM_TITLE = 'https://repolex.ai/ontology/git-lex/fm/title';
const GIT_NS = 'https://repolex.ai/ontology/git-lex/git/';

// Per-type label predicate. Returned in priority order — first one that has
// values for the subject wins. Falls back to fm:title, then shortName(IRI).
const LABEL_PREDICATES = {
    [GIT_NS + 'Commit']:  GIT_NS + 'message',
    [GIT_NS + 'Blob']:    GIT_NS + 'path',
    [GIT_NS + 'Branch']:  GIT_NS + 'shortName',
    [GIT_NS + 'Repo']:    GIT_NS + 'name',
};

function isHiddenType(uri) {
    return HIDDEN_TYPE_PREFIXES.some(p => uri.startsWith(p));
}

async function loadClassCounts() {
    // Walk every type with at least one instance — kit, git layer, anything.
    // Scoped to the frontmatter named graph to avoid the cross-graph union
    // dup that inflates counts elsewhere. Hide infrastructure / placeholder
    // types via HIDDEN_TYPE_PREFIXES.
    const rows = await sparql(`
        SELECT ?type (COUNT(DISTINCT ?s) AS ?count) WHERE {
            GRAPH ?g { ?s a ?type . }
            FILTER(STRENDS(STR(?g), "/now"))
        }
        GROUP BY ?type
        ORDER BY DESC(?count)
    `);

    const classes = [];
    for (const row of rows) {
        const uri = row.type;
        if (!uri || isHiddenType(uri)) continue;

        const count = parseInt(row.count) || 0;
        if (count === 0) continue;

        const labelPred = LABEL_PREDICATES[uri] || FM_TITLE;
        const name = shortName(uri);

        // Sample labels for this class, scoped to /frontmatter so we don't
        // hit the cross-graph dup union.
        const samples = await sparql(`
            SELECT DISTINCT ?label WHERE {
                GRAPH ?g {
                    ?s a <${uri}> ; <${labelPred}> ?label .
                }
                FILTER(STRENDS(STR(?g), "/now"))
            }
            ORDER BY ?label
            LIMIT 6
        `);

        let sampleStrs = samples.map(r => (r.label || '').toString().trim()).filter(Boolean);

        // Commit messages can be multi-line — keep just the first line.
        if (uri === GIT_NS + 'Commit') {
            sampleStrs = sampleStrs.map(s => s.split('\n')[0]);
        }
        // Blob paths can be long — show the basename for the sample list.
        if (uri === GIT_NS + 'Blob') {
            sampleStrs = sampleStrs.map(s => s.split('/').pop());
        }

        classes.push({
            uri,
            name,
            count,
            samples: sampleStrs,
        });
    }

    return classes;
}

// Load every commit (capped) with its file-change count, for the history
// scrubber strip. The change count is a v1 stand-in for true delta magnitude
// — once W4R3Z's retraction-aware sync graphs land, swap COUNT(?changed) for
// the per-sync-graph quad delta.
async function loadCommitTimeline() {
    const rows = await sparql(`
        PREFIX git: <https://repolex.ai/ontology/git-lex/git/>
        SELECT ?c ?date ?msg ?author (COUNT(?changed) AS ?n) WHERE {
            ?c a git:Commit ;
               git:committedDate ?date ;
               git:message ?msg .
            OPTIONAL { ?c git:authorName ?author }
            OPTIONAL { ?c git:changed ?changed }
        }
        GROUP BY ?c ?date ?msg ?author
        ORDER BY ?date
        LIMIT 500
    `);
    return rows.map(r => ({
        sha: (r.c.match(/\/commit\/([a-f0-9]+)/) || [])[1] || '',
        uri: r.c,
        date: r.date,
        msg: (r.msg || '').split('\n')[0],
        author: r.author || '',
        n: parseInt(r.n) || 0,
    }));
}

// Render a horizontal SVG timeline strip with one tick per commit.
// Height ∝ delta magnitude. Hover = tooltip. Click = stub for now (will wire
// to /api/scrub when the endpoint lands).
function renderTimeline(commits) {
    if (commits.length === 0) return '';
    const maxN = Math.max(1, ...commits.map(c => c.n));
    const W = 1000;       // viewBox width — scales to container
    const H = 56;
    const padX = 12;
    const padY = 10;
    const trackY = H - padY;
    const trackW = W - padX * 2;
    const span = trackW / Math.max(1, commits.length - 1);

    let bars = '';
    commits.forEach((c, i) => {
        const x = padX + i * span;
        const ratio = c.n / maxN;
        const h = 4 + ratio * (H - padY * 2 - 4);
        const y = trackY - h;
        const w = Math.max(2, Math.min(span * 0.6, 6));
        bars += `<rect class="tl-tick" x="${x - w / 2}" y="${y}" width="${w}" height="${h}" rx="1" data-sha="${c.sha}" data-i="${i}"></rect>`;
    });
    // HEAD marker = the latest commit
    const lastX = padX + (commits.length - 1) * span;
    bars += `<line class="tl-head" x1="${lastX}" y1="${padY - 2}" x2="${lastX}" y2="${trackY + 2}"></line>`;
    bars += `<text class="tl-head-label" x="${lastX}" y="${padY - 4}" text-anchor="middle">HEAD</text>`;

    // Baseline
    bars += `<line class="tl-base" x1="${padX}" y1="${trackY}" x2="${W - padX}" y2="${trackY}"></line>`;

    // Build the data-* JSON for client-side tooltip lookup
    const dataJson = JSON.stringify(commits.map(c => ({
        sha: c.sha.substring(0, 7),
        msg: c.msg.substring(0, 80),
        author: c.author,
        date: c.date,
        n: c.n,
    })));

    return `
        <div class="section timeline-section">
            <div class="section-title">History · ${commits.length} commits · ticks sized by file change count</div>
            <div class="timeline-wrap">
                <svg class="timeline" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none">
                    ${bars}
                </svg>
                <div class="timeline-tooltip" id="timeline-tooltip" hidden></div>
            </div>
            <div class="timeline-hint">Hover a tick to inspect a commit. Click is a stub — will swap the graph view to that point in time once <code>/api/scrub</code> ships.</div>
            <script type="application/json" id="timeline-data">${escapeHtml(dataJson)}</script>
        </div>
    `;
}

// After loadActivity injects HTML, wire up the timeline interactions.
function attachTimelineHandlers() {
    const dataEl = document.getElementById('timeline-data');
    if (!dataEl) return;
    let commits;
    try { commits = JSON.parse(dataEl.textContent); } catch { return; }
    const tooltip = document.getElementById('timeline-tooltip');
    document.querySelectorAll('.tl-tick').forEach(tick => {
        tick.addEventListener('mouseenter', e => {
            const i = parseInt(tick.dataset.i, 10);
            const c = commits[i];
            if (!c) return;
            tooltip.innerHTML = `
                <div class="tt-msg">${escapeHtml(c.msg)}</div>
                <div class="tt-meta">${escapeHtml(c.author || '')} · ${formatDate(c.date)} · ${c.n} file${c.n === 1 ? '' : 's'} · <code>${c.sha}</code></div>
            `;
            const r = tick.getBoundingClientRect();
            const wrapR = tick.closest('.timeline-wrap').getBoundingClientRect();
            tooltip.style.left = (r.left + r.width / 2 - wrapR.left) + 'px';
            tooltip.style.top = (r.top - wrapR.top - 8) + 'px';
            tooltip.hidden = false;
        });
        tick.addEventListener('mouseleave', () => {
            tooltip.hidden = true;
        });
        tick.addEventListener('click', () => {
            const sha = tick.dataset.sha;
            console.log('[scrub stub] would fetch /api/scrub?commit=' + sha);
            // Visual feedback so the user sees something happen.
            document.querySelectorAll('.tl-tick.active').forEach(t => t.classList.remove('active'));
            tick.classList.add('active');
        });
    });
}

async function loadRecentCommits(limit = 30) {
    // Pull commit-level info first.
    const rows = await sparql(`
        PREFIX git: <https://repolex.ai/ontology/git-lex/git/>
        SELECT ?c ?msg ?author ?date WHERE {
            ?c a git:Commit ;
               git:message ?msg .
            OPTIONAL { ?c git:authorName ?author }
            OPTIONAL { ?c git:committedDate ?date }
        }
        ORDER BY DESC(?date)
        LIMIT ${limit}
    `);

    if (!rows.length) return [];

    // Pull change paths for the same commits in one query and group by commit.
    const commitUris = rows.map(r => `<${r.c}>`).join(' ');
    const changes = await sparql(`
        PREFIX git: <https://repolex.ai/ontology/git-lex/git/>
        SELECT ?c ?changed WHERE {
            VALUES ?c { ${commitUris} }
            ?c git:changed ?changed .
        }
    `);

    // Group changed paths by commit, derive (count, top folder).
    const byCommit = {};
    changes.forEach(row => {
        const c = row.c;
        // git:changed values look like .../changeset/<sha>/path/to/file
        // Strip the changeset prefix to get the on-disk path.
        const m = (row.changed || '').match(/\/changeset\/[a-f0-9]+\/(.+)$/);
        const path = m ? m[1] : (row.changed || '');
        if (!byCommit[c]) byCommit[c] = [];
        byCommit[c].push(path);
    });

    return rows.map(r => {
        const paths = byCommit[r.c] || [];
        const count = paths.length;
        let hint = '';
        if (count > 0) {
            // Find the most common top-level folder among the changed paths.
            // Skip .lex internal noise so user-visible folders win when present.
            const folderCounts = {};
            paths.forEach(p => {
                const top = p.split('/')[0] || p;
                if (!top) return;
                folderCounts[top] = (folderCounts[top] || 0) + 1;
            });
            // Prefer non-".lex" folders even if .lex has more files.
            const entries = Object.entries(folderCounts);
            const userEntries = entries.filter(([k]) => !k.startsWith('.'));
            const pick = (userEntries.length ? userEntries : entries)
                .sort((a, b) => b[1] - a[1])[0];
            const topFolder = pick ? pick[0] : '';
            hint = `+${count} file${count === 1 ? '' : 's'}`;
            if (topFolder) hint += ` · ${topFolder}/`;
        }

        return {
            message: (r.msg || '').split('\n')[0].substring(0, 100),
            author: r.author || '',
            when: formatDate(r.date),
            changedHint: hint,
        };
    });
}

function formatDate(iso) {
    if (!iso) return '';
    try {
        const d = new Date(iso);
        if (isNaN(d.getTime())) return iso.substring(0, 10);
        const now = Date.now();
        const diff = now - d.getTime();
        const day = 86400000;
        if (diff < day) return Math.floor(diff / 3600000) + 'h ago';
        if (diff < 30 * day) return Math.floor(diff / day) + 'd ago';
        return d.toISOString().substring(0, 10);
    } catch {
        return iso.substring(0, 10);
    }
}

function escapeHtml(s) {
    const d = document.createElement('div');
    d.textContent = s;
    return d.innerHTML;
}

// ════════════════════════════════════════════
// GRAPH MODE — auto-detected default graph
// ════════════════════════════════════════════

const canvas = document.getElementById('graph-canvas');
const gctx = canvas ? canvas.getContext('2d') : null;
let GW = 0, GH = 0;
let graphState = {
    nodes: [],          // [{ id, label, type, typeColor, x, y, vx, vy, size, file }]
    edges: [],          // [{ source, target, predicate, predicateName, color }]
    classes: [],        // [{ uri, name, color, enabled }]
    predicates: [],     // [{ uri, name, color }]
    selected: null,
    pan: { x: 0, y: 0 },
    zoom: 1,
    drag: null,
    // Neighborhood-focus mode. When focusedNodeIds is non-null, only nodes
    // whose IRI is in the set are shown — turning the whole graph into a
    // localized k-hop view centered on the user's pick.
    focusedNodeIds: null,
    focusedRoot: null,
    focusedHops: 0,
};

const CLASS_PALETTE = [
    '#1f4e8a', '#bb2200', '#2a8a4a', '#aa5500',
    '#7733aa', '#aa6688', '#445577', '#888822',
    '#cc4488', '#226688', '#cc6622', '#558844',
];

// Edges are drawn in subdued versions of these so they don't compete with
// the node fills but stay distinguishable across predicate types.
const EDGE_PALETTE = [
    '#3a3a3a', '#9b3333', '#2f6a3f', '#7a5a1a',
    '#5a3a7a', '#7a4a5a', '#445566', '#666622',
];

function colorForClass(idx) {
    return CLASS_PALETTE[idx % CLASS_PALETTE.length];
}

function colorForEdge(idx) {
    return EDGE_PALETTE[idx % EDGE_PALETTE.length];
}

// Type IRIs we never want to render in the graph at all — RDF infrastructure.
// (kit/none/* phantom classes used to live here too; removed after the
// folder→class strip in git-lex commit 9bf11e2.)
const GRAPH_HIDDEN_TYPES = [
    'http://www.w3.org/2002/07/owl',
    'http://www.w3.org/2000/01/rdf-schema',
    'http://www.w3.org/1999/02/22-rdf-syntax-ns',
    'http://www.w3.org/ns/shacl',
    'https://repolex.ai/ontology/lex-o/',
];

// When a subject has multiple types, pick the most-specific one. Kit classes
// win over the lex-upper:Document fallback.
function pickCanonicalType(types) {
    const visible = types.filter(t => !GRAPH_HIDDEN_TYPES.some(p => t.startsWith(p)));
    if (visible.length === 0) return null;
    // Prefer non-lex-upper types (i.e. real kit classes) over generic Document.
    const specific = visible.find(t => !t.startsWith('https://repolex.ai/ontology/lex-upper/'));
    return specific || visible[0];
}

async function loadGraph() {
    // Scope queries to <repo>/now — the canonical "current state" graph.
    // Excludes /sync/{sha} and /changeset/{sha} which materialize per-commit
    // deltas and would inflate degree counts via default-graph union.

    const rawNodes = await sparql(`
        PREFIX fm: <https://repolex.ai/ontology/git-lex/fm/>
        SELECT DISTINCT ?s ?type ?title WHERE {
            GRAPH ?g {
                ?s a ?type ; fm:title ?title .
            }
            FILTER(STRENDS(STR(?g), "/now"))
        }
    `);

    // Edges: any predicate whose subject AND object both have an fm:title.
    // Captures lex:mentions / lex:linksTo (body wikilinks) plus any kit
    // owl:ObjectProperty that resolved to an entity IRI (e.g. soul:relatedTo).
    const edges = await sparql(`
        PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
        PREFIX fm: <https://repolex.ai/ontology/git-lex/fm/>
        PREFIX git: <https://repolex.ai/ontology/git-lex/git/>
        SELECT DISTINCT ?s ?p ?o WHERE {
            GRAPH ?g {
                ?s ?p ?o .
                ?s fm:title ?st .
                ?o fm:title ?ot .
                FILTER(?s != ?o)
                FILTER(?p != rdf:type)
                FILTER(!STRSTARTS(STR(?p), STR(fm:)))
                FILTER(!STRSTARTS(STR(?p), STR(git:)))
            }
            FILTER(STRENDS(STR(?g), "/now"))
        }
    `);

    // Group raw rows by subject so we can pick a canonical type.
    const bySubject = {};
    rawNodes.forEach(r => {
        if (!bySubject[r.s]) bySubject[r.s] = { id: r.s, types: [], title: r.title };
        bySubject[r.s].types.push(r.type);
    });

    // Resolve canonical type per subject; drop subjects with no visible type.
    const canonical = [];
    for (const s of Object.values(bySubject)) {
        const type = pickCanonicalType(s.types);
        if (!type) continue;
        canonical.push({ id: s.id, title: s.title, type });
    }

    // Build class palette from canonical types observed in instances.
    const classMap = {};
    canonical.forEach(n => {
        if (!classMap[n.type]) {
            classMap[n.type] = {
                uri: n.type,
                name: shortName(n.type),
                color: colorForClass(Object.keys(classMap).length),
                enabled: true,
                count: 0,
            };
        }
        classMap[n.type].count++;
    });

    // Augment with empty classes from any kit TBox loaded into the store.
    // This is what makes Brief / Pod / Proclamation / Freeform show in the
    // legend even when no instances exist yet — the TBox load (commit 18e5847)
    // puts every kit's owl:Class declarations into a graph we can query.
    // Excludes lex-upper / lex / shacl / rdf / owl meta-classes.
    const tboxClasses = await sparql(`
        PREFIX owl: <http://www.w3.org/2002/07/owl#>
        PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
        SELECT DISTINCT ?cls ?label WHERE {
            GRAPH ?g {
                ?cls a owl:Class .
                OPTIONAL { ?cls rdfs:label ?label }
            }
            FILTER(STRSTARTS(STR(?g), "https://repolex.ai/ontology/kit/"))
        }
    `);
    tboxClasses.forEach(r => {
        if (classMap[r.cls]) return;
        classMap[r.cls] = {
            uri: r.cls,
            name: r.label || shortName(r.cls),
            color: colorForClass(Object.keys(classMap).length),
            enabled: true,
            count: 0,
        };
    });

    graphState.classes = Object.values(classMap).sort((a, b) => a.name.localeCompare(b.name));

    // Build node objects.
    const nodeById = {};
    graphState.nodes = canonical.map(n => {
        const cls = classMap[n.type];
        const node = {
            id: n.id,
            label: n.title || shortName(n.id),
            type: n.type,
            typeName: cls.name,
            color: cls.color,
            x: (Math.random() - 0.5) * 400,
            y: (Math.random() - 0.5) * 400,
            vx: 0, vy: 0,
            size: 6,
            degree: 0,
        };
        nodeById[n.id] = node;
        return node;
    });

    // Build predicate palette from the edges we'll actually keep.
    const predicateMap = {};
    edges.forEach(e => {
        if (!nodeById[e.s] || !nodeById[e.o]) return;
        if (!predicateMap[e.p]) {
            predicateMap[e.p] = {
                uri: e.p,
                name: shortName(e.p),
                color: colorForEdge(Object.keys(predicateMap).length),
            };
        }
    });
    graphState.predicates = Object.values(predicateMap).sort((a, b) => a.name.localeCompare(b.name));

    graphState.edges = edges
        .filter(e => nodeById[e.s] && nodeById[e.o])
        .map(e => {
            nodeById[e.s].degree++;
            nodeById[e.o].degree++;
            const pred = predicateMap[e.p];
            return {
                source: nodeById[e.s],
                target: nodeById[e.o],
                predicate: e.p,
                predicateName: pred.name,
                color: pred.color,
            };
        });

    // Size by degree — log curve so high-degree hubs don't dwarf leaves.
    // Min 8 (readable), top ~26 for very high degree. Compressed range
    // keeps the graph visually balanced; W4R3Z-style hubs no longer eat
    // the screen.
    graphState.nodes.forEach(n => {
        n.size = 8 + Math.log2(n.degree + 1) * 4;
    });

    renderGraphControls();
    settleAndAnimate();
}

function renderGraphControls() {
    const classesEl = document.getElementById('graph-classes');
    classesEl.innerHTML = '';
    graphState.classes.forEach(c => {
        const lbl = document.createElement('label');
        lbl.className = 'class-toggle';
        lbl.innerHTML = `
            <input type="checkbox" ${c.enabled ? 'checked' : ''}>
            <span class="swatch" style="background:${c.color}"></span>
            <span>${c.name}</span>
        `;
        const cb = lbl.querySelector('input');
        cb.addEventListener('change', () => {
            c.enabled = cb.checked;
            // Re-run the simulation so the visible nodes spread to fill the
            // freed space (or compress when a class re-enters). Animated.
            kickSimulation();
        });
        classesEl.appendChild(lbl);
    });

    // Predicate legend — read-only swatches showing edge color → predicate.
    const predEl = document.getElementById('graph-predicates');
    if (predEl) {
        predEl.innerHTML = '';
        graphState.predicates.forEach(p => {
            const row = document.createElement('div');
            row.className = 'pred-row';
            row.innerHTML = `
                <span class="pred-swatch" style="background:${p.color}"></span>
                <span>${p.name}</span>
            `;
            predEl.appendChild(row);
        });
    }

    document.getElementById('graph-meta').textContent =
        `${graphState.nodes.length} nodes · ${graphState.edges.length} edges`;
}

// Force-layout constants — tuned so graphs of 25-150 nodes spread out enough
// for labels to read without becoming sparse and lost in space.
const LAYOUT = {
    REPULSION: 9000,
    EDGE_REST: 150,
    SPRING_K: 0.06,
    CENTERING: 0.0022,
    ORPHAN_PULL: 0.014,    // extra centering force for degree ≤ 1 nodes
    DAMPING: 0.45,
    STEP: 0.4,
};

// Run one physics step over the currently-visible nodes/edges.
function stepForceLayout() {
    const enabled = new Set(graphState.classes.filter(c => c.enabled).map(c => c.uri));
    const focused = graphState.focusedNodeIds;
    const nodes = graphState.nodes.filter(n =>
        enabled.has(n.type) && (!focused || focused.has(n.id))
    );
    if (nodes.length === 0) return 0;
    const visIds = new Set(nodes.map(n => n.id));
    const edges = graphState.edges.filter(e => visIds.has(e.source.id) && visIds.has(e.target.id));

    let totalKE = 0;

    // Repulsion
    for (let i = 0; i < nodes.length; i++) {
        const a = nodes[i];
        let fx = 0, fy = 0;
        for (let j = 0; j < nodes.length; j++) {
            if (i === j) continue;
            const b = nodes[j];
            const dx = a.x - b.x;
            const dy = a.y - b.y;
            const dist2 = Math.max(dx * dx + dy * dy, 1);
            const dist = Math.sqrt(dist2);
            // Bigger nodes push harder so hubs don't stack on each other.
            const sizeBoost = (a.size + b.size) / 24;
            const force = LAYOUT.REPULSION * sizeBoost / dist2;
            fx += (dx / dist) * force;
            fy += (dy / dist) * force;
        }
        // Centering. Low-degree nodes (orphans + one-edge leaves) get a
        // stronger pull so they don't drift off-screen.
        const center = a.degree <= 1 ? LAYOUT.ORPHAN_PULL : LAYOUT.CENTERING;
        fx -= a.x * center;
        fy -= a.y * center;
        a.vx = (a.vx + fx) * LAYOUT.DAMPING;
        a.vy = (a.vy + fy) * LAYOUT.DAMPING;
    }

    // Spring attraction
    edges.forEach(e => {
        const dx = e.target.x - e.source.x;
        const dy = e.target.y - e.source.y;
        const dist = Math.sqrt(dx * dx + dy * dy) || 1;
        const displacement = dist - LAYOUT.EDGE_REST;
        const force = LAYOUT.SPRING_K * displacement;
        const ux = dx / dist;
        const uy = dy / dist;
        e.source.vx += ux * force;
        e.source.vy += uy * force;
        e.target.vx -= ux * force;
        e.target.vy -= uy * force;
    });

    // Integrate. Off-screen nodes still tick so they keep their relative
    // positions when their class is re-enabled.
    graphState.nodes.forEach(n => {
        n.x += n.vx * LAYOUT.STEP;
        n.y += n.vy * LAYOUT.STEP;
        totalKE += n.vx * n.vx + n.vy * n.vy;
    });

    return totalKE;
}

// Continuous animation loop. Steps the simulation each frame as long as the
// system has measurable kinetic energy. Class-toggle changes call kickSimulation()
// to restart the loop.
let _layoutRAF = null;
let _layoutEnergy = 0;
// Higher floor = settles faster, less "cutesy floaty drift". The graph
// locks in once it's good enough rather than forever-jiggling.
const ENERGY_FLOOR = 8;

function animateLayout() {
    _layoutRAF = null;
    const ke = stepForceLayout();
    _layoutEnergy = _layoutEnergy * 0.9 + ke * 0.1;
    drawGraph();
    if (_layoutEnergy > ENERGY_FLOOR) {
        _layoutRAF = requestAnimationFrame(animateLayout);
    } else {
        // Settled — recenter so the graph sits at world origin regardless of
        // any drift during simulation, then auto-fit zoom so the whole
        // graph fills the viewport with margin.
        recenterGraph();
        fitGraphToViewport();
        drawGraph();
    }
}

function recenterGraph() {
    const ns = graphState.nodes;
    if (ns.length === 0) return;
    let sx = 0, sy = 0;
    ns.forEach(n => { sx += n.x; sy += n.y; });
    const cx = sx / ns.length;
    const cy = sy / ns.length;
    ns.forEach(n => { n.x -= cx; n.y -= cy; });
}

// After settle: pick a zoom level that makes the whole graph fit in the
// viewport with comfortable margin. Auto-fits only if user hasn't manually
// zoomed (graphState.userZoomed flag).
function fitGraphToViewport() {
    const ns = graphState.nodes;
    if (ns.length === 0 || !GW || !GH) return;
    if (graphState.userZoomed) return;
    let minX = Infinity, maxX = -Infinity, minY = Infinity, maxY = -Infinity;
    ns.forEach(n => {
        minX = Math.min(minX, n.x - n.size);
        maxX = Math.max(maxX, n.x + n.size);
        minY = Math.min(minY, n.y - n.size);
        maxY = Math.max(maxY, n.y + n.size);
    });
    const w = maxX - minX;
    const h = maxY - minY;
    if (w <= 0 || h <= 0) return;
    // Margin: 10% on each side, plus space for node labels under each node.
    const margin = 0.85;
    const zoomX = (GW * margin) / w;
    const zoomY = (GH * margin) / h;
    graphState.zoom = Math.max(0.2, Math.min(2.5, Math.min(zoomX, zoomY)));
    graphState.pan.x = 0;
    graphState.pan.y = 0;
}

function kickSimulation() {
    _layoutEnergy = 100;            // pretend we're hot so the loop keeps going
    if (_layoutRAF == null) {
        _layoutRAF = requestAnimationFrame(animateLayout);
    }
}

// Initial settle: warm-start by running a chunk of frames synchronously so
// the user doesn't see the graph fly together for too long, then hand off
// to the animator for the final settle.
function settleAndAnimate() {
    // Heavy warm-start: do most of the settling synchronously so the user
    // sees a (mostly) stable graph on first paint instead of watching it
    // crawl into place over a few seconds.
    for (let i = 0; i < 350; i++) stepForceLayout();
    recenterGraph();
    fitGraphToViewport();
    kickSimulation();
}

function resizeGraph() {
    if (!canvas) return;
    const rect = canvas.getBoundingClientRect();
    GW = rect.width;
    GH = rect.height;
    canvas.width = GW * devicePixelRatio;
    canvas.height = GH * devicePixelRatio;
    canvas.style.width = GW + 'px';
    canvas.style.height = GH + 'px';
    gctx.setTransform(devicePixelRatio, 0, 0, devicePixelRatio, 0, 0);
    drawGraph();
}

function drawGraph() {
    if (!gctx || !canvas.width) return;
    gctx.clearRect(0, 0, GW, GH);

    const enabled = new Set(graphState.classes.filter(c => c.enabled).map(c => c.uri));
    const focused = graphState.focusedNodeIds;
    const visibleNodes = graphState.nodes.filter(n =>
        enabled.has(n.type) && (!focused || focused.has(n.id))
    );
    const visibleNodeIds = new Set(visibleNodes.map(n => n.id));

    gctx.save();
    gctx.translate(GW / 2 + graphState.pan.x, GH / 2 + graphState.pan.y);
    gctx.scale(graphState.zoom, graphState.zoom);

    const selId = graphState.selected;
    // When something is selected, edges that don't touch it dim to give
    // focus to the selection's neighborhood.
    const dimOthers = selId != null;

    // Edges — colored by predicate, with a small arrow at the target end.
    const edgeWidth = Math.max(1.2, 1.6 / graphState.zoom);
    graphState.edges.forEach(e => {
        if (!visibleNodeIds.has(e.source.id) || !visibleNodeIds.has(e.target.id)) return;
        const touchesSel = !dimOthers || e.source.id === selId || e.target.id === selId;
        gctx.strokeStyle = touchesSel ? e.color : 'rgba(180,180,180,0.35)';
        gctx.fillStyle = gctx.strokeStyle;
        gctx.lineWidth = edgeWidth;

        // Compute the segment that ends at the target node's edge (not its
        // center) so the arrow head sits cleanly outside the disc.
        const dx = e.target.x - e.source.x;
        const dy = e.target.y - e.source.y;
        const dist = Math.sqrt(dx * dx + dy * dy) || 1;
        const ux = dx / dist;
        const uy = dy / dist;
        const sx = e.source.x + ux * e.source.size;
        const sy = e.source.y + uy * e.source.size;
        const tx = e.target.x - ux * e.target.size;
        const ty = e.target.y - uy * e.target.size;

        gctx.beginPath();
        gctx.moveTo(sx, sy);
        gctx.lineTo(tx, ty);
        gctx.stroke();

        // Arrow head — simple filled triangle pointing along (ux, uy).
        const ah = Math.max(6, 8 / graphState.zoom);
        const aw = ah * 0.55;
        const px = -uy;
        const py = ux;
        gctx.beginPath();
        gctx.moveTo(tx, ty);
        gctx.lineTo(tx - ux * ah + px * aw, ty - uy * ah + py * aw);
        gctx.lineTo(tx - ux * ah - px * aw, ty - uy * ah - py * aw);
        gctx.closePath();
        gctx.fill();
    });

    // Edge labels — predicate names at midpoint, rotated to follow the edge.
    // Only draw when zoomed in enough to read, and skip very short edges so
    // dense clusters don't drown in text.
    if (graphState.zoom > 0.7) {
        // Constant on-screen size: 11px regardless of zoom. The canvas has
        // a `gctx.scale(zoom, zoom)` in effect, so we counter-divide.
        const labelPx = 11 / graphState.zoom;
        gctx.font = `600 ${labelPx}px 'American Typewriter', Courier, monospace`;
        gctx.textAlign = 'center';
        gctx.textBaseline = 'middle';
        graphState.edges.forEach(e => {
            if (!visibleNodeIds.has(e.source.id) || !visibleNodeIds.has(e.target.id)) return;
            const touchesSel = !dimOthers || e.source.id === selId || e.target.id === selId;
            if (dimOthers && !touchesSel) return;
            const dx = e.target.x - e.source.x;
            const dy = e.target.y - e.source.y;
            const dist = Math.sqrt(dx * dx + dy * dy) || 1;
            if (dist < 60) return;
            const mx = (e.source.x + e.target.x) / 2;
            const my = (e.source.y + e.target.y) / 2;
            // Keep text upright: flip if the edge points left-to-right backwards.
            let angle = Math.atan2(dy, dx);
            if (angle > Math.PI / 2) angle -= Math.PI;
            if (angle < -Math.PI / 2) angle += Math.PI;
            gctx.save();
            gctx.translate(mx, my);
            gctx.rotate(angle);
            // Background pad so the label doesn't fight the edge stroke
            const text = e.predicateName || '';
            const tw = gctx.measureText(text).width;
            gctx.fillStyle = 'rgba(255,255,255,0.85)';
            gctx.fillRect(-tw / 2 - 2, -labelPx / 2 - 1, tw + 4, labelPx + 2);
            gctx.fillStyle = e.color;
            gctx.fillText(text, 0, 0);
            gctx.restore();
        });
    }

    // Nodes
    visibleNodes.forEach(n => {
        const isSelected = selId === n.id;
        const isNeighbor = dimOthers && !isSelected && graphState.edges.some(e =>
            (e.source.id === selId && e.target.id === n.id) ||
            (e.target.id === selId && e.source.id === n.id)
        );
        const isFocused = !dimOthers || isSelected || isNeighbor;
        gctx.globalAlpha = isFocused ? 1 : 0.3;
        gctx.beginPath();
        gctx.arc(n.x, n.y, n.size, 0, Math.PI * 2);
        gctx.fillStyle = n.color;
        gctx.fill();
        gctx.strokeStyle = isSelected ? '#000' : '#ffffff';
        gctx.lineWidth = (isSelected ? 2.5 : 1.4) / graphState.zoom;
        gctx.stroke();
    });
    gctx.globalAlpha = 1;

    // Labels — only draw for moderately sized nodes & at zoom > 0.5
    if (graphState.zoom > 0.5) {
        gctx.font = `${11 / graphState.zoom}px 'American Typewriter', Courier, monospace`;
        gctx.fillStyle = '#222';
        gctx.textAlign = 'center';
        gctx.textBaseline = 'top';
        visibleNodes.forEach(n => {
            if (n.size < 6 && graphState.zoom < 1) return;
            const lbl = n.label.length > 22 ? n.label.substring(0, 20) + '…' : n.label;
            gctx.fillText(lbl, n.x, n.y + n.size + 2);
        });
    }

    gctx.restore();
}

function focusClassInGraph(cls) {
    graphState.classes.forEach(c => c.enabled = (c.uri === cls));
    renderGraphControls();
    kickSimulation();
}

// BFS k-hop neighborhood through already-loaded edges. Returns a Set of node
// IRIs reachable within `hops` steps from the root, including the root.
function neighborhoodIds(rootId, hops) {
    const found = new Set([rootId]);
    let frontier = new Set([rootId]);
    for (let h = 0; h < hops; h++) {
        const next = new Set();
        graphState.edges.forEach(e => {
            if (frontier.has(e.source.id) && !found.has(e.target.id)) {
                next.add(e.target.id);
                found.add(e.target.id);
            }
            if (frontier.has(e.target.id) && !found.has(e.source.id)) {
                next.add(e.source.id);
                found.add(e.source.id);
            }
        });
        if (next.size === 0) break;
        frontier = next;
    }
    return found;
}

function focusNeighborhood(rootId, hops) {
    graphState.focusedNodeIds = neighborhoodIds(rootId, hops);
    graphState.focusedRoot = rootId;
    graphState.focusedHops = hops;
    // Refresh detail panel so the focus controls update.
    const root = graphState.nodes.find(n => n.id === rootId);
    if (root) showNodeDetail(root);
    kickSimulation();
}

// ════════════════════════════════════════════
// MARKDOWN VIEWER PANE
// ════════════════════════════════════════════
//
// Double-click any neighbor link in the detail panel (or any node on the
// canvas) → opens a second card to the left of the detail card showing the
// rendered markdown of that node's underlying file.
//
// Sketched against the contract: GET /api/file?uri=<encoded-iri> returns
// { content: string, frontmatter?: string } as JSON. W4R3Z hasn't shipped
// the endpoint yet — until then we render a graceful stub showing the URI
// and the predicted endpoint URL so the user sees the wiring is real.

function openMarkdownViewer(node) {
    const viewer = document.getElementById('graph-md-viewer');
    if (!viewer || !node) return;
    viewer.hidden = false;

    const url = '/api/file?uri=' + encodeURIComponent(node.id);
    viewer.innerHTML = `
        <div class="md-header">
            <h3 class="md-title">${escapeHtml(node.label)}</h3>
            <button class="md-close" aria-label="Close">×</button>
        </div>
        <div class="md-body"><div class="md-loading">loading…</div></div>
    `;
    viewer.querySelector('.md-close').addEventListener('click', closeMarkdownViewer);

    fetch(url)
        .then(r => {
            if (!r.ok) throw new Error('HTTP ' + r.status);
            return r.json();
        })
        .then(data => {
            // /api/file returns 200 with {error: "…"} when the IRI isn't in
            // the store (e.g. "no fm:path for this IRI"). The !r.ok check
            // above doesn't catch that, so we'd silently render an empty
            // body. Surface the error via the same stub path that HTTP
            // errors take.
            if (data && data.error) throw new Error(data.error);
            renderMarkdownInto(viewer, data, node);
        })
        .catch(err => renderMarkdownStub(viewer, node, err));
}

function closeMarkdownViewer() {
    const viewer = document.getElementById('graph-md-viewer');
    if (viewer) viewer.hidden = true;
}

function renderMarkdownInto(viewer, data, node) {
    const body = viewer.querySelector('.md-body');
    const fm = data.frontmatter ? `<div class="md-fm">${escapeHtml(data.frontmatter)}</div>` : '';
    const html = renderMarkdown(data.content || '');
    body.innerHTML = fm + html;
    attachWikilinkHandlers(body);
}

// Resolve a wikilink target string (e.g. "w4r3z", "project/git-lex",
// "pod-3") to a document IRI + display label via SPARQL. Strategy:
//   1. Exact path match on fm:path (handles "project/git-lex")
//   2. Ends-with path match on fm:path (handles bare "w4r3z" → "agent/w4r3z.md")
//   3. Exact title match on fm:title (handles prose-case wikilinks)
// First hit wins. Returns { id, label } or null.
async function resolveWikilink(target) {
    if (!target) return null;
    // Normalize: trim, drop trailing .md if the user wrote one.
    let t = target.trim();
    t = t.replace(/\.md$/i, '');
    // Double-quote-safe literal: the extractor normalizes slugs to ascii
    // but body wikilinks can carry arbitrary prose. Escape any embedded
    // quotes and backslashes before interpolating into the SPARQL literal.
    const esc = (s) => s.replace(/\\/g, '\\\\').replace(/"/g, '\\"');
    const pathWithMd = esc(t + '.md');
    const titleLit = esc(t);

    const query = `
        PREFIX fm: <https://repolex.ai/ontology/git-lex/fm/>
        SELECT ?s ?label WHERE {
            {
                ?s fm:path ?path .
                FILTER(
                    LCASE(STR(?path)) = LCASE("${pathWithMd}") ||
                    STRENDS(LCASE(STR(?path)), LCASE("/${pathWithMd}"))
                )
                OPTIONAL { ?s fm:title ?t }
                BIND(COALESCE(?t, "${titleLit}") AS ?label)
            } UNION {
                ?s fm:title ?title .
                FILTER(LCASE(STR(?title)) = LCASE("${titleLit}"))
                BIND(?title AS ?label)
            }
        } LIMIT 1`;

    try {
        const rows = await sparql(query);
        if (!Array.isArray(rows) || !rows.length) return null;
        const r = rows[0];
        return { id: r.s, label: r.label || target };
    } catch (e) {
        return null;
    }
}

// Wire dblclick handlers on any .wikilink elements in a freshly-rendered
// markdown body. Resolver is async; while it runs we mark the element
// data-wl-state="resolving" so CSS can hint progress; on success we jump
// graph selection (if the target is already in the current graph view)
// and open a new markdown viewer for the resolved document.
function attachWikilinkHandlers(body) {
    const links = body.querySelectorAll('a.wikilink');
    links.forEach(a => {
        a.style.cursor = 'pointer';
        a.title = 'double-click to open';
        a.addEventListener('dblclick', async (e) => {
            e.preventDefault();
            e.stopPropagation();
            const target = a.dataset.wikilink;
            if (!target) return;
            if (a.dataset.wlState === 'resolving') return;
            a.dataset.wlState = 'resolving';
            const resolved = await resolveWikilink(target);
            if (!resolved) {
                a.dataset.wlState = 'unresolved';
                a.title = `could not resolve [[${target}]]`;
                return;
            }
            a.dataset.wlState = 'resolved';
            // If the resolved IRI matches a node in the active graph view,
            // update selection + refresh the detail card + redraw so the
            // focus follows the user into the target document.
            if (typeof graphState !== 'undefined' && Array.isArray(graphState.nodes)) {
                const hit = graphState.nodes.find(n => n.id === resolved.id);
                if (hit) {
                    graphState.selected = hit.id;
                    if (typeof showNodeDetail === 'function') showNodeDetail(hit);
                    if (typeof drawGraph === 'function') drawGraph();
                }
            }
            openMarkdownViewer({ id: resolved.id, label: resolved.label });
        });
    });
}

function renderMarkdownStub(viewer, node, err) {
    const body = viewer.querySelector('.md-body');
    body.innerHTML = `
        <div class="md-error">file viewer endpoint not yet available</div>
        <div class="md-stub-note">
            Double-click jumps to the markdown for this entity.
            <br><br>
            URI: <code>${escapeHtml(node.id)}</code>
            <br><br>
            Wiring expects <code>GET /api/file?uri=&lt;iri&gt;</code> returning
            <code>{ content, frontmatter? }</code>. Endpoint is queued for the
            next sync of this pod's backend work; UI is shipped against the
            contract so it'll light up the moment the server responds.
            <br><br>
            <span style="color:#bbb;font-size:0.6rem">${escapeHtml(err && err.message || '')}</span>
        </div>
    `;
}

// Tiny markdown renderer — handles the subset of CommonMark we actually use
// in git-lex notes (headings, paragraphs, lists, code blocks, inline code,
// bold, italic, links, blockquotes). Not a full parser; the goal is "good
// enough to read your own notes," not "render arbitrary GFM."
function renderMarkdown(src) {
    if (!src) return '';
    // Strip a leading YAML frontmatter block if the server didn't already.
    let body = src;
    const fmMatch = body.match(/^---\n([\s\S]*?)\n---\n?/);
    if (fmMatch) body = body.slice(fmMatch[0].length);

    // Pull out fenced code blocks first so we don't apply inline rules inside.
    const codeBlocks = [];
    body = body.replace(/```([a-z]*)\n([\s\S]*?)```/g, (m, lang, code) => {
        codeBlocks.push(`<pre><code>${escapeHtml(code.replace(/\n$/, ''))}</code></pre>`);
        return `\u0000CODE${codeBlocks.length - 1}\u0000`;
    });

    // Pull out wikilinks before inline/escape processing so the raw target can
    // survive into the attribute. Re-inserted as placeholder tokens that the
    // inline() pipeline passes through untouched, then substituted at the end
    // with a data-wikilink attr carrying the unescaped target for the runtime
    // resolver to use.
    const wikilinks = [];
    body = body.replace(/\[\[([^\]]+)\]\]/g, (m, target) => {
        wikilinks.push(target);
        return `\u0000WL${wikilinks.length - 1}\u0000`;
    });

    const inline = (s) => {
        s = escapeHtml(s);
        // Inline code (after escaping so backticks survive)
        s = s.replace(/`([^`]+)`/g, '<code>$1</code>');
        // Bold then italic (order matters)
        s = s.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
        s = s.replace(/(?<!\*)\*([^*]+)\*(?!\*)/g, '<em>$1</em>');
        s = s.replace(/_([^_]+)_/g, '<em>$1</em>');
        // Markdown links [text](url)
        s = s.replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
        // Restore wikilink placeholders with proper data-wikilink attribute.
        // Target is used both as the visible text and as the resolver key.
        s = s.replace(/\u0000WL(\d+)\u0000/g, (_, idx) => {
            const target = wikilinks[parseInt(idx)];
            const attr = target.replace(/&/g, '&amp;').replace(/"/g, '&quot;');
            return `<a class="wikilink" data-wikilink="${attr}">${escapeHtml(target)}</a>`;
        });
        return s;
    };

    const lines = body.split('\n');
    const out = [];
    let i = 0;
    while (i < lines.length) {
        const line = lines[i];

        // Code block placeholder
        const cbMatch = line.match(/^\u0000CODE(\d+)\u0000$/);
        if (cbMatch) { out.push(codeBlocks[parseInt(cbMatch[1])]); i++; continue; }

        // Headings
        const h = line.match(/^(#{1,6})\s+(.+)$/);
        if (h) {
            const level = Math.min(h[1].length, 3);
            out.push(`<h${level}>${inline(h[2])}</h${level}>`);
            i++; continue;
        }

        // Blockquote
        if (/^>\s?/.test(line)) {
            const block = [];
            while (i < lines.length && /^>\s?/.test(lines[i])) {
                block.push(lines[i].replace(/^>\s?/, ''));
                i++;
            }
            out.push(`<blockquote>${inline(block.join(' '))}</blockquote>`);
            continue;
        }

        // Unordered list
        if (/^\s*[-*]\s+/.test(line)) {
            out.push('<ul>');
            while (i < lines.length && /^\s*[-*]\s+/.test(lines[i])) {
                out.push(`<li>${inline(lines[i].replace(/^\s*[-*]\s+/, ''))}</li>`);
                i++;
            }
            out.push('</ul>');
            continue;
        }

        // Ordered list
        if (/^\s*\d+\.\s+/.test(line)) {
            out.push('<ol>');
            while (i < lines.length && /^\s*\d+\.\s+/.test(lines[i])) {
                out.push(`<li>${inline(lines[i].replace(/^\s*\d+\.\s+/, ''))}</li>`);
                i++;
            }
            out.push('</ol>');
            continue;
        }

        // Blank → paragraph break
        if (line.trim() === '') { i++; continue; }

        // Paragraph: gather contiguous non-blank lines
        const para = [line];
        i++;
        while (i < lines.length && lines[i].trim() !== '' && !/^(#{1,6}\s|\s*[-*]\s|\s*\d+\.\s|>\s?|\u0000CODE\d+\u0000)/.test(lines[i])) {
            para.push(lines[i]);
            i++;
        }
        out.push(`<p>${inline(para.join(' '))}</p>`);
    }
    return out.join('\n');
}

function clearFocus() {
    graphState.focusedNodeIds = null;
    graphState.focusedRoot = null;
    graphState.focusedHops = 0;
    if (graphState.selected) {
        const root = graphState.nodes.find(n => n.id === graphState.selected);
        if (root) showNodeDetail(root);
    }
    kickSimulation();
}

function showNodeDetail(node) {
    const detail = document.getElementById('graph-detail');
    detail.hidden = false;

    // Walk this node's edges, group by predicate, split into outgoing/incoming.
    const out = {}; // predicate -> [{ node, color }]
    const inc = {};
    graphState.edges.forEach(e => {
        if (e.source.id === node.id) {
            (out[e.predicate] = out[e.predicate] || { color: e.color, name: e.predicateName, items: [] }).items.push(e.target);
        }
        if (e.target.id === node.id) {
            (inc[e.predicate] = inc[e.predicate] || { color: e.color, name: e.predicateName, items: [] }).items.push(e.source);
        }
    });

    function renderEdgeGroup(map, heading) {
        const keys = Object.keys(map).sort();
        if (keys.length === 0) return '';
        let h = `<div class="edge-group-heading">${heading}</div>`;
        keys.forEach(p => {
            const g = map[p];
            h += `<div class="edge-group">`;
            h += `<div class="edge-group-pred"><span class="pred-swatch" style="background:${g.color}"></span>${escapeHtml(g.name)}</div>`;
            h += `<ul>`;
            g.items.forEach(target => {
                h += `<li><a href="#" data-id="${escapeHtml(target.id)}">`;
                h += `<span class="node-dot" style="background:${target.color}"></span>`;
                h += `${escapeHtml(target.label)}`;
                h += `</a></li>`;
            });
            h += `</ul></div>`;
        });
        return h;
    }

    const isFocusRoot = graphState.focusedRoot === node.id;
    const focusToolbar = `
        <div class="focus-toolbar">
            ${isFocusRoot
                ? `<span class="focus-status">focused · ${graphState.focusedHops}-hop · ${graphState.focusedNodeIds.size} nodes</span>
                   <button class="focus-btn" data-act="hop+">+1 hop</button>
                   ${graphState.focusedHops > 1 ? `<button class="focus-btn" data-act="hop-">−1 hop</button>` : ''}
                   <button class="focus-btn" data-act="clear">show all</button>`
                : `<button class="focus-btn" data-act="focus1">focus 1-hop</button>
                   <button class="focus-btn" data-act="focus2">focus 2-hop</button>`
            }
        </div>
    `;
    detail.innerHTML = `
        <button class="close">×</button>
        <h3>${escapeHtml(node.label)}</h3>
        <div class="detail-meta">
            <span class="node-dot" style="background:${node.color}"></span>
            ${escapeHtml(node.typeName)} · ${node.degree} connection${node.degree === 1 ? '' : 's'}
        </div>
        ${focusToolbar}
        ${renderEdgeGroup(out, 'Outgoing')}
        ${renderEdgeGroup(inc, 'Incoming')}
        <div class="detail-uri"><code>${escapeHtml(node.id)}</code></div>
    `;
    // Wire the focus toolbar buttons.
    detail.querySelectorAll('.focus-btn').forEach(btn => {
        btn.addEventListener('click', () => {
            const act = btn.dataset.act;
            if (act === 'focus1') focusNeighborhood(node.id, 1);
            else if (act === 'focus2') focusNeighborhood(node.id, 2);
            else if (act === 'hop+') focusNeighborhood(node.id, graphState.focusedHops + 1);
            else if (act === 'hop-') focusNeighborhood(node.id, graphState.focusedHops - 1);
            else if (act === 'clear') clearFocus();
        });
    });
    detail.querySelector('.close').addEventListener('click', () => {
        detail.hidden = true;
        graphState.selected = null;
        drawGraph();
    });
    // Click any neighbor link in the detail panel → jump selection to it
    // AND open the markdown viewer pane for that neighbor. (Previously
    // only dblclick opened the viewer, but Rob wanted single-click to be
    // the read-the-thing gesture since the viewer is now pinned bottom-left
    // and doesn't cover the graph.)
    detail.querySelectorAll('a[data-id]').forEach(a => {
        a.addEventListener('click', e => {
            e.preventDefault();
            const id = a.dataset.id;
            const target = graphState.nodes.find(n => n.id === id);
            if (target) {
                graphState.selected = id;
                showNodeDetail(target);
                drawGraph();
                openMarkdownViewer(target);
            }
        });
    });
    // Also let users double-click the title of the currently-selected node
    // to view its own markdown without having to click a neighbor.
    const titleEl = detail.querySelector('h3');
    if (titleEl) {
        titleEl.style.cursor = 'pointer';
        titleEl.title = 'double-click to view markdown';
        titleEl.addEventListener('dblclick', () => openMarkdownViewer(node));
    }
}

// Graph mouse interaction
function initGraphInput() {
    if (!canvas) return;

    canvas.addEventListener('mousedown', e => {
        const rect = canvas.getBoundingClientRect();
        graphState.drag = {
            x: e.clientX - rect.left,
            y: e.clientY - rect.top,
            startPan: { ...graphState.pan },
        };
    });

    canvas.addEventListener('mousemove', e => {
        if (!graphState.drag) return;
        const rect = canvas.getBoundingClientRect();
        const dx = (e.clientX - rect.left) - graphState.drag.x;
        const dy = (e.clientY - rect.top) - graphState.drag.y;
        graphState.pan.x = graphState.drag.startPan.x + dx;
        graphState.pan.y = graphState.drag.startPan.y + dy;
        drawGraph();
    });

    window.addEventListener('mouseup', e => {
        if (!graphState.drag) return;
        const moved = Math.abs(e.clientX - (graphState.drag.x + canvas.getBoundingClientRect().left)) > 3;
        graphState.drag = null;
        if (moved) return;
        // Click → hit test
        const rect = canvas.getBoundingClientRect();
        const wx = (e.clientX - rect.left - GW / 2 - graphState.pan.x) / graphState.zoom;
        const wy = (e.clientY - rect.top - GH / 2 - graphState.pan.y) / graphState.zoom;
        const hit = graphState.nodes.find(n => {
            const dx = n.x - wx, dy = n.y - wy;
            return dx * dx + dy * dy < (n.size + 4) * (n.size + 4);
        });
        if (hit) {
            graphState.selected = hit.id;
            showNodeDetail(hit);
            drawGraph();
        }
    });

    canvas.addEventListener('dblclick', e => {
        const rect = canvas.getBoundingClientRect();
        const wx = (e.clientX - rect.left - GW / 2 - graphState.pan.x) / graphState.zoom;
        const wy = (e.clientY - rect.top - GH / 2 - graphState.pan.y) / graphState.zoom;
        const hit = graphState.nodes.find(n => {
            const dx = n.x - wx, dy = n.y - wy;
            return dx * dx + dy * dy < (n.size + 4) * (n.size + 4);
        });
        if (hit) openMarkdownViewer(hit);
    });

    canvas.addEventListener('wheel', e => {
        e.preventDefault();
        // Scale step by the actual wheel delta so trackpad scrolls feel
        // smooth instead of stepped, and halve the overall sensitivity vs
        // the old ±10%-per-tick. Clamp per-event delta so a big spin
        // doesn't blow past the view.
        const SENSITIVITY = 0.0025;
        const delta = Math.max(-40, Math.min(40, e.deltaY));
        const factor = Math.exp(-delta * SENSITIVITY);
        graphState.zoom = Math.max(0.2, Math.min(4, graphState.zoom * factor));
        graphState.userZoomed = true;
        drawGraph();
    }, { passive: false });

    window.addEventListener('resize', () => {
        if (currentMode === 'graph') resizeGraph();
    });
}

// ════════════════════════════════════════════
// PUSH MODE
// ════════════════════════════════════════════

function handlePush(data) {
    // data is {query, result} where result is {type:"construct", triples:[...]}
    const view = views.interactive;
    view.querySelector('.push-empty').hidden = true;
    const content = document.getElementById('push-content');
    content.hidden = false;

    document.getElementById('push-query').textContent = data.query || '';
    document.getElementById('push-time').textContent = new Date().toLocaleTimeString();

    const triples = (data.result && data.result.triples) || [];
    const render = document.getElementById('push-render');
    render.innerHTML = '';
    renderPushPayload(render, triples);
}

const VIZ_NS = 'https://repolex.ai/ontology/viz/';

function renderPushPayload(container, triples) {
    if (!triples.length) {
        container.innerHTML = '<div class="view-loading">Empty push payload.</div>';
        return;
    }

    // Group triples by subject. Each subject becomes a "thing".
    // Read viz: properties as rendering hints.
    //
    // Global hint predicates (viz:displayType, viz:title, viz:layout) apply to
    // the scene as a whole — they are NEVER stored on subjects. A CONSTRUCT
    // that carries only hints on a scene IRI (e.g. <urn:scene:foo> viz:title "…")
    // produces zero data subjects from that IRI, so it won't pollute tables with
    // a stray row or the graph view with an orphan node.
    const GLOBAL_HINTS = new Set([
        VIZ_NS + 'displayType',
        VIZ_NS + 'layout',
        VIZ_NS + 'title',
    ]);
    const subjects = {};
    let displayType = 'graph';
    let layout = 'force';
    let title = '';

    triples.forEach(t => {
        const s = t.subject;
        const p = t.predicate;
        const o = t.object;

        if (GLOBAL_HINTS.has(p)) {
            if (p === VIZ_NS + 'displayType') displayType = (o.value || '').toLowerCase();
            else if (p === VIZ_NS + 'layout') layout = (o.value || '').toLowerCase();
            else if (p === VIZ_NS + 'title') title = o.value || '';
            return;
        }

        if (!subjects[s]) subjects[s] = { id: s, props: {}, edges: [] };
        if (p === VIZ_NS + 'edgeTo') {
            subjects[s].edges.push(o.value);
        } else {
            subjects[s].props[p] = o.value;
        }
    });

    // Title bar
    let html = '';
    if (title) html += `<h2 style="font-weight:normal;margin-bottom:1rem">${escapeHtml(title)}</h2>`;
    html += `<div style="font-size:0.7rem;color:#888;margin-bottom:1rem">displayType: ${displayType} · layout: ${layout} · ${triples.length} triples</div>`;

    if (displayType === 'text') {
        const text = Object.values(subjects).map(s => s.props[VIZ_NS + 'text'] || '').join('\n');
        html += `<pre>${escapeHtml(text)}</pre>`;
    } else if (displayType === 'table') {
        const rows = Object.values(subjects);
        const allKeys = new Set();
        rows.forEach(r => Object.keys(r.props).forEach(k => allKeys.add(k)));
        const cols = [...allKeys].filter(k => k.startsWith(VIZ_NS)).map(k => k.replace(VIZ_NS, ''));
        html += '<table class="kv-table"><tr>' + cols.map(c => `<th>${c}</th>`).join('') + '</tr>';
        rows.forEach(r => {
            html += '<tr>' + cols.map(c => `<td>${escapeHtml(r.props[VIZ_NS + c] || '')}</td>`).join('') + '</tr>';
        });
        html += '</table>';
    } else {
        // Default: render as live force-directed graph on a canvas.
        // Reuses the same physics + drawing approach as the main graph view
        // but scoped to its own state so it doesn't fight the squad/repo view.
        const nodes = Object.values(subjects).map(s => ({
            id: s.id,
            label: s.props[VIZ_NS + 'label'] || shortName(s.id),
            color: s.props[VIZ_NS + 'color'] || '#1f4e8a',
            size: parseFloat(s.props[VIZ_NS + 'size']) || 12,
            edges: s.edges,
        }));
        html += `<div style="color:#888;font-size:0.7rem;margin-bottom:0.5rem">${nodes.length} nodes · ${nodes.reduce((a, n) => a + n.edges.length, 0)} edges</div>`;
        html += `<canvas id="push-canvas" style="width:100%;height:560px;background:#fff;border:1px solid #e0e0e0;border-radius:4px;"></canvas>`;
        container.innerHTML = html;
        // Defer until the canvas is in the DOM so we can size it.
        requestAnimationFrame(() => renderPushGraph(nodes));
        return;
    }

    container.innerHTML = html;
}

// Self-contained force-layout + canvas render for push payloads. The push
// view has its own simulation state so it doesn't trample the main graph.
let pushRAF = null;
function renderPushGraph(rawNodes) {
    const canvas = document.getElementById('push-canvas');
    if (!canvas) return;
    if (pushRAF) { cancelAnimationFrame(pushRAF); pushRAF = null; }

    // Build node + edge arrays. Edges reference target ids; resolve to refs.
    const nodeById = {};
    const nodes = rawNodes.map(n => {
        const node = {
            id: n.id,
            label: n.label,
            color: n.color,
            size: n.size,
            x: (Math.random() - 0.5) * 300,
            y: (Math.random() - 0.5) * 300,
            vx: 0,
            vy: 0,
        };
        nodeById[n.id] = node;
        return node;
    });
    const edges = [];
    rawNodes.forEach(n => {
        n.edges.forEach(targetId => {
            const t = nodeById[targetId];
            if (t) edges.push({ source: nodeById[n.id], target: t });
        });
    });

    // Compute degree for orphan-pull centering.
    nodes.forEach(n => { n.degree = 0; });
    edges.forEach(e => { e.source.degree++; e.target.degree++; });

    // Size canvas for retina.
    const rect = canvas.getBoundingClientRect();
    const W = rect.width;
    const H = rect.height;
    canvas.width = W * devicePixelRatio;
    canvas.height = H * devicePixelRatio;
    const ctx = canvas.getContext('2d');
    ctx.setTransform(devicePixelRatio, 0, 0, devicePixelRatio, 0, 0);

    // Use the same LAYOUT constants as the main graph for consistency.
    let zoom = 1;
    let pan = { x: 0, y: 0 };

    function step() {
        // Repulsion + size-aware push
        for (let i = 0; i < nodes.length; i++) {
            const a = nodes[i];
            let fx = 0, fy = 0;
            for (let j = 0; j < nodes.length; j++) {
                if (i === j) continue;
                const b = nodes[j];
                const dx = a.x - b.x;
                const dy = a.y - b.y;
                const d2 = Math.max(dx * dx + dy * dy, 1);
                const d = Math.sqrt(d2);
                const sb = (a.size + b.size) / 24;
                const f = LAYOUT.REPULSION * sb / d2;
                fx += (dx / d) * f;
                fy += (dy / d) * f;
            }
            const c = a.degree <= 1 ? LAYOUT.ORPHAN_PULL : LAYOUT.CENTERING;
            fx -= a.x * c;
            fy -= a.y * c;
            a.vx = (a.vx + fx) * LAYOUT.DAMPING;
            a.vy = (a.vy + fy) * LAYOUT.DAMPING;
        }
        // Springs
        edges.forEach(e => {
            const dx = e.target.x - e.source.x;
            const dy = e.target.y - e.source.y;
            const d = Math.sqrt(dx * dx + dy * dy) || 1;
            const f = LAYOUT.SPRING_K * (d - LAYOUT.EDGE_REST);
            const ux = dx / d, uy = dy / d;
            e.source.vx += ux * f;
            e.source.vy += uy * f;
            e.target.vx -= ux * f;
            e.target.vy -= uy * f;
        });
        let ke = 0;
        nodes.forEach(n => {
            n.x += n.vx * LAYOUT.STEP;
            n.y += n.vy * LAYOUT.STEP;
            ke += n.vx * n.vx + n.vy * n.vy;
        });
        return ke;
    }

    function recenterAndFit() {
        if (nodes.length === 0) return;
        let sx = 0, sy = 0;
        nodes.forEach(n => { sx += n.x; sy += n.y; });
        const cx = sx / nodes.length;
        const cy = sy / nodes.length;
        nodes.forEach(n => { n.x -= cx; n.y -= cy; });
        // Fit
        let minX = Infinity, maxX = -Infinity, minY = Infinity, maxY = -Infinity;
        nodes.forEach(n => {
            minX = Math.min(minX, n.x - n.size);
            maxX = Math.max(maxX, n.x + n.size);
            minY = Math.min(minY, n.y - n.size);
            maxY = Math.max(maxY, n.y + n.size);
        });
        const w = maxX - minX, h = maxY - minY;
        if (w > 0 && h > 0) {
            zoom = Math.max(0.2, Math.min(2.5, Math.min(W * 0.85 / w, H * 0.85 / h)));
        }
    }

    function draw() {
        ctx.clearRect(0, 0, W, H);
        ctx.save();
        ctx.translate(W / 2 + pan.x, H / 2 + pan.y);
        ctx.scale(zoom, zoom);
        // Edges
        ctx.strokeStyle = 'rgba(80,80,80,0.6)';
        ctx.lineWidth = 1.4 / zoom;
        edges.forEach(e => {
            const dx = e.target.x - e.source.x;
            const dy = e.target.y - e.source.y;
            const d = Math.sqrt(dx * dx + dy * dy) || 1;
            const ux = dx / d, uy = dy / d;
            const sx = e.source.x + ux * e.source.size;
            const sy = e.source.y + uy * e.source.size;
            const tx = e.target.x - ux * e.target.size;
            const ty = e.target.y - uy * e.target.size;
            ctx.beginPath();
            ctx.moveTo(sx, sy);
            ctx.lineTo(tx, ty);
            ctx.stroke();
        });
        // Nodes
        nodes.forEach(n => {
            ctx.beginPath();
            ctx.arc(n.x, n.y, n.size, 0, Math.PI * 2);
            ctx.fillStyle = n.color;
            ctx.fill();
            ctx.strokeStyle = '#fff';
            ctx.lineWidth = 1.4 / zoom;
            ctx.stroke();
        });
        // Labels
        if (zoom > 0.5) {
            ctx.font = `${11 / zoom}px 'American Typewriter', Courier, monospace`;
            ctx.fillStyle = '#222';
            ctx.textAlign = 'center';
            ctx.textBaseline = 'top';
            nodes.forEach(n => {
                ctx.fillText(n.label, n.x, n.y + n.size + 2);
            });
        }
        ctx.restore();
    }

    // Heavy warm-start, then animate the final settle.
    for (let i = 0; i < 350; i++) step();
    recenterAndFit();
    draw();

    let energy = 100;
    function loop() {
        pushRAF = null;
        const ke = step();
        energy = energy * 0.9 + ke * 0.1;
        draw();
        if (energy > ENERGY_FLOOR) {
            pushRAF = requestAnimationFrame(loop);
        } else {
            recenterAndFit();
            draw();
        }
    }
    pushRAF = requestAnimationFrame(loop);
}

// ════════════════════════════════════════════
// INIT
// ════════════════════════════════════════════

document.addEventListener('DOMContentLoaded', () => {
    initRouting();
    initGraphInput();
    connectWS();
    updateSnapshotPill();
    // Resize graph on window changes
    window.addEventListener('resize', () => {
        if (currentMode === 'graph') resizeGraph();
    });
});

// Store-snapshot pill — visibility nerf against the silent staleness
// bug documented in brief/2026-04-09-sparql-endpoint-and-live-store.md.
// The viz server opens oxigraph as a read-only snapshot at startup and
// never sees later writes until restart. Until W4R3Z ships the real fix
// (per-query reopen + /api/reload), this pill surfaces the age of the
// snapshot so users know when they're looking at stale data.
//
// Contract (GET /api/store-info): { snapshot_at: "<ISO-8601>" }. Any
// other fields fine, only snapshot_at is required. If the endpoint
// 404s the pill hides itself — no-op until W4R3Z ships the endpoint.
//
// Sketch contributed by @M3RCUR14L (2026-04-09), ported into git-lex-viz
// styling for visual consistency with the typewriter palette.
async function updateSnapshotPill() {
    const el = document.getElementById('store-snapshot');
    const ageEl = document.getElementById('store-snapshot-age');
    if (!el || !ageEl) return;
    try {
        const r = await fetch('/api/store-info');
        if (!r.ok) return;  // leaves pill hidden — graceful degrade
        const info = await r.json();
        if (!info || !info.snapshot_at) return;
        const ts = new Date(info.snapshot_at);
        if (isNaN(ts.getTime())) return;
        el.hidden = false;
        function renderAge() {
            const mins = Math.floor((Date.now() - ts.getTime()) / 60000);
            let label;
            if (mins < 1) label = 'just now';
            else if (mins < 60) label = mins + 'm ago';
            else if (mins < 1440) label = Math.floor(mins / 60) + 'h ago';
            else label = Math.floor(mins / 1440) + 'd ago';
            ageEl.textContent = label;
            el.classList.toggle('stale', mins >= 10);
            el.classList.toggle('very-stale', mins >= 60);
            el.title = 'Data snapshot taken ' + ts.toISOString() +
                '. Any writes since then won\'t show until server reload.';
        }
        renderAge();
        setInterval(renderAge, 30000);
    } catch (e) {
        // Network error or malformed response — leave pill hidden.
    }
}
