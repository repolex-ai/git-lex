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

    const [repoInfo, recentCommits] = await Promise.all([
        loadRepoInfo(),
        loadRecentCommits(30),
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

    // Build class palette from canonical types only.
    const classMap = {};
    canonical.forEach(n => {
        if (!classMap[n.type]) {
            classMap[n.type] = {
                uri: n.type,
                name: shortName(n.type),
                color: colorForClass(Object.keys(classMap).length),
                enabled: true,
            };
        }
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

    // Size by degree — visual difference bumped so it actually reads.
    graphState.nodes.forEach(n => {
        n.size = 6 + Math.sqrt(n.degree) * 5;
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
    REPULSION: 4000,
    EDGE_REST: 140,
    SPRING_K: 0.04,
    CENTERING: 0.0008,
    DAMPING: 0.5,
    STEP: 0.4,
};

// Run one physics step over the currently-visible nodes/edges.
function stepForceLayout() {
    const enabled = new Set(graphState.classes.filter(c => c.enabled).map(c => c.uri));
    const nodes = graphState.nodes.filter(n => enabled.has(n.type));
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
            const force = LAYOUT.REPULSION / dist2;
            fx += (dx / dist) * force;
            fy += (dy / dist) * force;
        }
        fx -= a.x * LAYOUT.CENTERING;
        fy -= a.y * LAYOUT.CENTERING;
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
const ENERGY_FLOOR = 0.05;

function animateLayout() {
    _layoutRAF = null;
    const ke = stepForceLayout();
    _layoutEnergy = _layoutEnergy * 0.9 + ke * 0.1;
    drawGraph();
    if (_layoutEnergy > ENERGY_FLOOR) {
        _layoutRAF = requestAnimationFrame(animateLayout);
    }
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
    for (let i = 0; i < 80; i++) stepForceLayout();
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
    const visibleNodes = graphState.nodes.filter(n => enabled.has(n.type));
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

    detail.innerHTML = `
        <button class="close">×</button>
        <h3>${escapeHtml(node.label)}</h3>
        <div class="detail-meta">
            <span class="node-dot" style="background:${node.color}"></span>
            ${escapeHtml(node.typeName)} · ${node.degree} connection${node.degree === 1 ? '' : 's'}
        </div>
        ${renderEdgeGroup(out, 'Outgoing')}
        ${renderEdgeGroup(inc, 'Incoming')}
        <div class="detail-uri"><code>${escapeHtml(node.id)}</code></div>
    `;
    detail.querySelector('.close').addEventListener('click', () => {
        detail.hidden = true;
        graphState.selected = null;
        drawGraph();
    });
    // Click any neighbor link in the detail panel → jump selection to it.
    detail.querySelectorAll('a[data-id]').forEach(a => {
        a.addEventListener('click', e => {
            e.preventDefault();
            const id = a.dataset.id;
            const target = graphState.nodes.find(n => n.id === id);
            if (target) {
                graphState.selected = id;
                showNodeDetail(target);
                drawGraph();
            }
        });
    });
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

    canvas.addEventListener('wheel', e => {
        e.preventDefault();
        const factor = e.deltaY > 0 ? 0.9 : 1.1;
        graphState.zoom = Math.max(0.2, Math.min(4, graphState.zoom * factor));
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
    const subjects = {};
    let displayType = 'graph';
    let layout = 'force';
    let title = '';

    triples.forEach(t => {
        const s = t.subject;
        const p = t.predicate;
        const o = t.object;
        if (!subjects[s]) subjects[s] = { id: s, props: {}, edges: [] };

        if (p === VIZ_NS + 'displayType') displayType = (o.value || '').toLowerCase();
        if (p === VIZ_NS + 'layout') layout = (o.value || '').toLowerCase();
        if (p === VIZ_NS + 'title') title = o.value || '';
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
        // Default: render as graph
        const nodes = Object.values(subjects).map(s => ({
            id: s.id,
            label: s.props[VIZ_NS + 'label'] || shortName(s.id),
            color: s.props[VIZ_NS + 'color'] || '#1f4e8a',
            size: parseFloat(s.props[VIZ_NS + 'size']) || 8,
            edges: s.edges,
        }));
        html += `<div style="color:#888">${nodes.length} nodes ready. (full graph render coming next iteration)</div>`;
        html += '<ul style="margin-top:1rem;font-size:0.85rem">';
        nodes.slice(0, 20).forEach(n => {
            html += `<li><span style="display:inline-block;width:.7em;height:.7em;background:${n.color};margin-right:.5em"></span>${escapeHtml(n.label)} → ${n.edges.length} edges</li>`;
        });
        html += '</ul>';
    }

    container.innerHTML = html;
}

// ════════════════════════════════════════════
// INIT
// ════════════════════════════════════════════

document.addEventListener('DOMContentLoaded', () => {
    initRouting();
    initGraphInput();
    connectWS();
    // Resize graph on window changes
    window.addEventListener('resize', () => {
        if (currentMode === 'graph') resizeGraph();
    });
});
