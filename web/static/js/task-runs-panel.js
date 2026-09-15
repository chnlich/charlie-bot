(function() {
// ---------------------------------------------------------------------------
// Runs panel — one node's actual executions (the v2 replacement for Workers)
// ---------------------------------------------------------------------------
// Reads the paged Run API (fact-derived state per row), offers explicit Stop
// (one run's identity-aware endpoint) and Retry (a new run of an eligible
// finished run), links evidence through the file server, and lists this node's
// direct child tasks. Everything re-renders from server facts; duplicate
// events cannot duplicate rows.

const panel = {
  sessionId: null,
  gen: 0,
  items: [],
  nextCursor: null,
  children: [],
  loading: false,
};

// The panel binds to the session it was shown for; responses for a prior node
// are dropped.
function boundSessionId() {
  return panel.sessionId;
}

function isStale(flight) {
  return flight.gen !== panel.gen || flight.sessionId !== panel.sessionId;
}

function reset() {
  panel.items = [];
  panel.nextCursor = null;
  panel.children = [];
  panel.loading = false;
}

async function refresh() {
  const sessionId = boundSessionId();
  if (!sessionId) return;
  const flight = {sessionId, gen: ++panel.gen};
  reset();
  await loadPage(flight, null);
  loadChildren(flight);
}

async function loadPage(flight, cursor) {
  // Concurrent loads are safe: results are dropped when their flight is stale
  // (a rapid node switch must never be blocked by the prior node's fetch), and
  // rows dedupe by id.
  panel.loading = true;
  renderLoadingNotice();
  try {
    const params = new URLSearchParams({limit: '50'});
    if (cursor) params.set('cursor', cursor);
    const res = await fetch('/api/sessions/' + boundSessionId() + '/runs?' + params.toString());
    if (!res.ok) throw new Error('runs failed: ' + res.status);
    const page = await res.json();
    if (isStale(flight)) { panel.loading = false; return; }
    const known = new Set(panel.items.map((r) => r.id));
    for (const run of page.items || []) {
      if (!known.has(run.id)) panel.items.push(run);
    }
    panel.nextCursor = page.next_cursor || null;
  } catch (err) {
    panel.loading = false;
    if (!isStale(flight)) renderError('Failed to load runs: ' + (err && err.message ? err.message : err));
    return;
  }
  panel.loading = false;
  if (!isStale(flight)) render();
}

async function loadMore() {
  if (!panel.nextCursor || panel.loading) return;
  const flight = {sessionId: boundSessionId(), gen: panel.gen};
  await loadPage(flight, panel.nextCursor);
}

async function loadChildren(flight) {
  try {
    const res = await fetch('/api/sessions/tree?parent_id=' + encodeURIComponent(boundSessionId()) + '&include_archived=true&limit=100');
    if (!res.ok) throw new Error(String(res.status));
    const page = await res.json();
    if (isStale(flight)) return;
    panel.children = page.items || [];
  } catch (err) {
    if (!isStale(flight)) console.error('children fetch failed:', err);
    return;
  }
  if (!isStale(flight)) render();
}

// -- actions -----------------------------------------------------------

async function stopRun(runId) {
  const sessionId = boundSessionId();
  if (!sessionId) return;
  const requestId = (crypto.randomUUID ? crypto.randomUUID() : String(Date.now()));
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/runs/' + encodeURIComponent(runId) + '/cancel', {
      method: 'POST', headers: JSON_HEADERS, body: JSON.stringify({request_id: requestId}),
    });
    if (!res.ok) {
      const body = await res.json().catch(() => ({}));
      const message = (body.detail && body.detail.message) || body.detail || ('HTTP ' + res.status);
      showToast('Stop refused: ' + message, true);
      return;
    }
    const payload = await res.json();
    showToast(payload.outcome ? ('Run stopped (outcome ' + payload.outcome + ').') : 'Stop requested; the exit is observed on the run.', false);
    await refresh();
  } catch (err) {
    console.error('stopRun failed:', err);
    showToast('Stop failed: ' + err.message, true);
  }
}

async function retryRun(runId) {
  const sessionId = boundSessionId();
  if (!sessionId) return;
  const requestId = (crypto.randomUUID ? crypto.randomUUID() : String(Date.now()));
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/retry', {
      method: 'POST', headers: JSON_HEADERS, body: JSON.stringify({request_id: requestId, run_id: runId}),
    });
    if (!res.ok) {
      const body = await res.json().catch(() => ({}));
      const blockers = (body.detail && body.detail.blockers) || [body.detail?.message || ('HTTP ' + res.status)];
      showToast('Retry refused: ' + blockers.join(' '), true);
      return;
    }
    showToast('Retry queued.', false);
    await refresh();
  } catch (err) {
    console.error('retryRun failed:', err);
    showToast('Retry failed: ' + err.message, true);
  }
}

// -- rendering ---------------------------------------------------------

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function stateBadgeClass(state) {
  if (state === 'running') return 'border-blue-500/50 text-blue-300 bg-blue-500/10';
  if (state === 'attention') return 'border-red-500/50 text-red-300 bg-red-500/10';
  if (state === 'queued') return 'border-amber-500/50 text-amber-300 bg-amber-500/10';
  if (state === 'success') return 'border-green-500/50 text-green-300 bg-green-500/10';
  if (state === 'failed' || state === 'interrupted') return 'border-red-500/50 text-red-300 bg-red-500/10';
  return 'border-slate-600 text-slate-400';
}

function formatDuration(run) {
  if (!run.started_at || !run.ended_at) return null;
  const secs = Math.max(0, Math.round((new Date(run.ended_at) - new Date(run.started_at)) / 1000));
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return m + 'm' + s + 's';
}

function evidenceLink(label, ref) {
  if (!ref) return null;
  const a = el('a', 'text-blue-400 hover:text-blue-300 underline', label);
  a.href = '/files/' + String(ref).split('/').filter(Boolean).map(encodeURIComponent).join('/');
  a.target = '_blank';
  a.rel = 'noopener';
  return a;
}

function runCard(run) {
  const card = el('div', 'rounded-xl border border-slate-700 bg-slate-800/60 p-3 space-y-2');
  const head = el('div', 'flex items-center gap-2 flex-wrap');
  head.appendChild(el('span', 'text-xs font-mono text-slate-400', run.id.slice(0, 8)));
  head.appendChild(el('span', 'text-[11px] border rounded px-1.5 py-px border-slate-600 text-slate-300', run.kind));
  head.appendChild(el('span', 'text-[11px] border rounded px-1.5 py-px ' + stateBadgeClass(run.state), run.state));
  if (run.stop_requested && run.state !== 'running') {
    head.appendChild(el('span', 'text-[11px] text-slate-500', 'stop requested'));
  }
  if (run.review_of_run_id) head.appendChild(el('span', 'text-[11px] text-purple-300', 'review of ' + run.review_of_run_id.slice(0, 8)));
  if (run.retry_of_run_id) head.appendChild(el('span', 'text-[11px] text-sky-300', 'retry of ' + run.retry_of_run_id.slice(0, 8)));
  head.appendChild(el('span', 'ml-auto text-[11px] text-slate-500',
    (run.model || 'N/A') + ' · inputs ' + ((run.input_event_ids || []).length)));
  card.appendChild(head);

  const timing = el('div', 'text-[11px] text-slate-500 flex flex-wrap gap-x-3');
  timing.appendChild(el('span', undefined, 'started ' + (run.started_at ? new Date(run.started_at).toLocaleString() : 'N/A')));
  timing.appendChild(el('span', undefined, 'ended ' + (run.ended_at ? new Date(run.ended_at).toLocaleString() : 'N/A')));
  const dur = formatDuration(run);
  timing.appendChild(el('span', undefined, 'duration ' + (dur || 'N/A')));
  timing.appendChild(el('span', undefined, 'exit ' + (run.exit_code === null || run.exit_code === undefined ? 'N/A' : run.exit_code)));
  card.appendChild(timing);

  const foot = el('div', 'flex items-center gap-2 flex-wrap text-xs');
  const links = [];
  const raw = evidenceLink('raw log', run.raw_log_ref);
  if (raw) links.push(raw);
  const events = evidenceLink('events', run.events_ref);
  if (events) links.push(events);
  const result = evidenceLink('result', run.result_ref);
  if (result) links.push(result);
  if (!links.length) foot.appendChild(el('span', 'text-slate-600', 'No evidence files recorded'));
  for (const link of links) {
    foot.appendChild(link);
    foot.appendChild(el('span', 'text-slate-600', '·'));
  }
  const ctxBtn = el('button', 'text-xs text-blue-400 hover:text-blue-300 underline', 'Context');
  ctxBtn.setAttribute('aria-label', 'View startup context of run ' + run.id);
  ctxBtn.addEventListener('click', () => {
    if (globalThis.TaskContextPanel) globalThis.TaskContextPanel.showHistoricalRun(run.id);
  });
  foot.appendChild(ctxBtn);

  if (run.state === 'running' || run.state === 'queued' || run.state === 'attention') {
    const stopBtn = el('button', 'ml-auto text-xs px-2 py-1 rounded border border-red-600/60 text-red-300 hover:bg-red-900/30', 'Stop');
    stopBtn.setAttribute('aria-label', 'Stop run ' + run.id);
    stopBtn.addEventListener('click', () => stopRun(run.id));
    foot.appendChild(stopBtn);
  }
  if (run.state === 'failed' || run.state === 'interrupted') {
    const retryBtn = el('button', 'ml-auto text-xs px-2 py-1 rounded border border-sky-600/60 text-sky-300 hover:bg-sky-900/30', 'Retry');
    retryBtn.setAttribute('aria-label', 'Retry run ' + run.id);
    retryBtn.addEventListener('click', () => retryRun(run.id));
    foot.appendChild(retryBtn);
  }
  card.appendChild(foot);
  return card;
}

function renderError(message) {
  const container = document.getElementById('tab-runs');
  if (!container) return;
  container.textContent = '';
  container.appendChild(el('div', 'mx-auto max-w-3xl p-4', '')
    .appendChild(el('div', 'rounded-lg bg-red-900/40 border border-red-700/50 text-red-200 text-sm px-4 py-3', message))
    .parentNode || document.createDocumentFragment());
}

function renderLoadingNotice() {
  const container = document.getElementById('tab-runs');
  if (!container || panel.items.length) return;
  container.textContent = '';
  container.appendChild(el('p', 'text-slate-500 text-sm p-4', 'Loading runs...'));
}

function render() {
  const container = document.getElementById('tab-runs');
  if (!container) return;
  container.textContent = '';
  const wrap = el('div', 'mx-auto max-w-3xl p-4 space-y-4');

  // Direct child tasks (one click each; the tree remains the hierarchy owner).
  const childSection = el('div', 'rounded-xl border border-slate-700 bg-slate-800/60 p-4 space-y-2');
  childSection.appendChild(el('h3', 'text-xs font-semibold uppercase tracking-wide text-slate-400', 'Direct child tasks'));
  if (panel.children.length) {
    for (const child of panel.children) {
      const row = el('div', 'flex items-center gap-2');
      const link = el('a', 'text-sm text-blue-400 hover:text-blue-300 truncate', child.name);
      link.href = '/?session=' + encodeURIComponent(child.id);
      link.addEventListener('click', (e) => { e.preventDefault(); switchSession(child.id); });
      row.appendChild(link);
      row.appendChild(el('span', 'text-[11px] text-slate-500', child.profile + ' · ' + child.work_state + (child.archived ? ' · archived' : '')));
      childSection.appendChild(row);
    }
  } else {
    childSection.appendChild(el('p', 'text-xs text-slate-500', 'No child tasks.'));
  }
  wrap.appendChild(childSection);

  const runsSection = el('div', 'space-y-2');
  runsSection.appendChild(el('h3', 'text-xs font-semibold uppercase tracking-wide text-slate-400', 'Runs (' + panel.items.length + ')'));
  if (!panel.items.length) {
    runsSection.appendChild(el('p', 'text-sm text-slate-500', 'No runs recorded for this task yet.'));
  }
  for (const run of panel.items) runsSection.appendChild(runCard(run));
  if (panel.nextCursor) {
    const more = el('button', 'w-full text-xs text-blue-400 hover:text-blue-300 border border-slate-700 rounded-lg py-2', 'Load older runs');
    more.addEventListener('click', loadMore);
    runsSection.appendChild(more);
  }
  wrap.appendChild(runsSection);
  container.appendChild(wrap);
}

// -- lifecycle -----------------------------------------------------------

function onSessionChanged(session) {
  reset();
  if (!session || !session.profile) {
    panel.sessionId = null;
    return;
  }
  panel.sessionId = session.id;
}

function onTreeChanged(sessionIds) {
  if (!panel.sessionId) return;
  if (!sessionIds.includes(panel.sessionId)) return;
  // A run fact or child report landed for this node: refresh the visible page.
  refresh();
}

function onTabShown() {
  if (panel.sessionId !== boundSessionId() || !panel.items.length) refresh();
}

globalThis.TaskRunsPanel = {
  onSessionChanged,
  onTabShown,
  onTreeChanged,
  refresh,
};
})();
