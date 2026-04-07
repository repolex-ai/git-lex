// git-lex viz — main entry point
//
// Connects to the Rust backend at /api/query and /ws.
// W3BL0RD: this is the place to start. Replace the basic query UI
// with a real D3 visualization. Use viz: namespace properties from
// CONSTRUCT queries for shape/color/layout hints.

const ws = new WebSocket('ws://' + location.host + '/ws');
const status = document.getElementById('status');

ws.onopen = () => {
    status.textContent = 'WebSocket connected';
    status.className = 'status connected';
};

ws.onclose = () => {
    status.textContent = 'WebSocket disconnected';
    status.className = 'status error';
};

ws.onmessage = (e) => {
    // Future: agents push CONSTRUCT results here for live viz updates
    console.log('ws:', e.data);
};

async function runQuery() {
    const query = document.getElementById('query').value;
    const results = document.getElementById('results');
    results.textContent = '// Running...';
    try {
        const r = await fetch('/api/query', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ query })
        });
        const data = await r.json();
        results.textContent = JSON.stringify(data, null, 2);
    } catch (e) {
        results.textContent = '// Error: ' + e.message;
    }
}

// Expose for inline onclick
window.runQuery = runQuery;
