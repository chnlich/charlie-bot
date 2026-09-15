(function() {
// ---------------------------------------------------------------------------
// Context panel — what was / will be loaded, and the local-rule editor
// ---------------------------------------------------------------------------
// "Next run" reads GET /api/sessions/{id}/effective-prompt (the same assembly
// the launch commits); "Current run" reads GET /api/sessions/{id}/runs/{run_id}/context
// (the stored snapshot of record). Source lists, scopes, delivery (full vs
// index), owners and measured char counts come from the server objects — the
// panel never recomposes provenance. The local-rule editor PATCHes
// node_prompt / subtree_prompt; the draft never persists anywhere else.

const panel = {
  sessionId: null,
  gen: 0,
  detail: null,          // session detail (prompt_rules with authoritative bodies)
  preview: null,         // effective-prompt payload
  previewError: null,
  historical: null,      // {runId, payload} when a Run's snapshot is selected
  historicalError: null,
  ruleDraft: null,       // {scope: 'node'|'subtree', text} unsaved
  ruleScope: 'node',
  kind: null,            // preview run kind override
};

// The panel binds to the session it was shown for; every response is dropped
// unless that binding and generation still own the view.
function boundSessionId() {
  return panel.sessionId;
}

function isStale(flight) {
  return flight.gen !== panel.gen || flight.sessionId !== panel.sessionId;
}

async function refresh() {
  const sessionId = boundSessionId();
  if (!sessionId) return;
  const flight = {sessionId, gen: ++panel.gen};
  panel.historical = null;
  panel.historicalError = null;
  try {
    const res = await fetch('/api/sessions/' + sessionId, {cache: 'no-store'});
    if (!res.ok) throw new Error('detail failed: ' + res.status);
    const detail = await res.json();
    if (isStale(flight)) return;
    panel.detail = detail;
    render();
  } catch (err) {
    if (isStale(flight)) return;
    renderError('Failed to load task detail: ' + (err && err.message ? err.message : err));
    return;
  }
  await refreshPreview(flight);
  await refreshCurrentRun(flight);
}

async function refreshPreview(flight) {
  const sessionId = boundSessionId();
  if (!sessionId) return;
  flight = flight || {sessionId, gen: panel.gen};
  const params = panel.kind ? ('?kind=' + encodeURIComponent(panel.kind)) : '';
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/effective-prompt' + params, {cache: 'no-store'});
    const body = await res.json().catch(() => ({}));
    if (isStale(flight)) return;
    if (!res.ok) {
      panel.preview = null;
      panel.previewError = body.detail || ('HTTP ' + res.status);
    } else {
      panel.preview = body;
      panel.previewError = null;
    }
  } catch (err) {
    if (isStale(flight)) return;
    panel.preview = null;
    panel.previewError = String(err);
  }
  render();
}

async function refreshCurrentRun(flight) {
  const sessionId = boundSessionId();
  if (!sessionId) return;
  flight = flight || {sessionId, gen: panel.gen};
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/runs?limit=100', {cache: 'no-store'});
    if (!res.ok) throw new Error('runs failed: ' + res.status);
    const page = await res.json();
    if (isStale(flight)) return;
    const withSnapshot = (page.items || []).filter((r) => r.prompt_snapshot_ref);
    // Current run = the latest run that carries a committed instruction snapshot.
    const current = withSnapshot[withSnapshot.length - 1] || null;
    if (!current) {
      panel.currentRun = null;
      panel.currentSnapshot = null;
      render();
      return;
    }
    const ctxRes = await fetch('/api/sessions/' + sessionId + '/runs/' + encodeURIComponent(current.id) + '/context', {cache: 'no-store'});
    const ctxBody = await ctxRes.json().catch(() => ({}));
    if (isStale(flight)) return;
    if (!ctxRes.ok) {
      panel.currentRun = current;
      panel.currentSnapshot = null;
      panel.currentError = ctxBody.detail || ('HTTP ' + ctxRes.status);
    } else {
      panel.currentRun = current;
      panel.currentSnapshot = ctxBody.snapshot;
      panel.currentError = null;
    }
  } catch (err) {
    if (isStale(flight)) return;
    panel.currentRun = null;
    panel.currentSnapshot = null;
    panel.currentError = String(err);
  }
  render();
}

// -- historical run view ------------------------------------------------

async function showHistoricalRun(runId) {
  const sessionId = boundSessionId();
  if (!sessionId || !runId) return;
  const flight = {sessionId, gen: ++panel.gen};
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/runs/' + encodeURIComponent(runId) + '/context', {cache: 'no-store'});
    const body = await res.json().catch(() => ({}));
    if (isStale(flight)) return;
    if (!res.ok) {
      panel.historical = null;
      panel.historicalError = body.detail || ('HTTP ' + res.status);
    } else {
      panel.historical = {runId, payload: body};
      panel.historicalError = null;
    }
  } catch (err) {
    if (isStale(flight)) return;
    panel.historical = null;
    panel.historicalError = String(err);
  }
  render();
  if (typeof switchTab === 'function') switchTab('task-context');
}

// -- rendering ----------------------------------------------------------

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function renderError(message) {
  const container = document.getElementById('tab-task-context');
  if (!container) return;
  container.textContent = '';
  const box = el('div', 'mx-auto max-w-3xl p-4');
  box.appendChild(el('div', 'rounded-lg bg-red-900/40 border border-red-700/50 text-red-200 text-sm px-4 py-3', message));
  container.appendChild(box);
}

function sectionTitle(text) {
  return el('h3', 'text-xs font-semibold uppercase tracking-wide text-slate-400 mb-2', text);
}

function sourceLine(source, blockDelivery) {
  // One source row: scope, readable ref, owning node (local rules only). The
  // full-vs-index delivery is a BLOCK-level fact (one block = one injection),
  // carried by every source row of that block. Built with textContent — refs
  // are data.
  const row = el('div', 'flex items-center gap-2 flex-wrap text-xs py-0.5');
  const scopeClasses = {
    base: 'border-slate-600 text-slate-300',
    memory: 'border-purple-500/50 text-purple-300',
    subtree: 'border-sky-500/50 text-sky-300',
    node: 'border-emerald-500/50 text-emerald-300',
  };
  row.appendChild(el('span', 'border rounded px-1 py-px uppercase tracking-wide ' + (scopeClasses[source.scope] || 'border-slate-600 text-slate-300'), source.scope));
  row.appendChild(el('span', 'font-mono text-slate-400 break-all', source.source_ref || ''));
  if (source.source_session_id) {
    row.appendChild(el('span', 'text-slate-500', 'owner ' + source.source_session_id.slice(0, 8)));
  }
  if (blockDelivery === 'index') {
    row.appendChild(el('span', 'border border-amber-500/50 text-amber-300 rounded px-1 py-px', 'index only'));
  }
  return row;
}

function blockCard(block, opts) {
  const card = el('div', 'rounded-lg border border-slate-700 bg-slate-800/60 p-3 space-y-1.5');
  const head = el('div', 'flex items-center justify-between gap-2 flex-wrap');
  const sources = el('div', 'space-y-0.5 min-w-0 flex-1');
  for (const s of block.sources) sources.appendChild(sourceLine(s, block.delivery));
  head.appendChild(sources);
  head.appendChild(el('span', 'text-[11px] text-slate-500 whitespace-nowrap', (opts && opts.chars) ? (opts.chars + ' chars') : (block.text.length + ' chars')));
  card.appendChild(head);
  const details = el('details');
  const summary = el('summary', 'text-xs text-blue-400 hover:text-blue-300 cursor-pointer select-none', 'View text');
  details.appendChild(summary);
  const pre = el('pre', 'mt-1 max-h-64 overflow-auto text-xs text-slate-300 whitespace-pre-wrap break-words bg-slate-900 rounded p-2');
  pre.textContent = block.text;
  details.appendChild(pre);
  card.appendChild(details);
  return card;
}

function snapshotSummary(node, snapshot, label) {
  node.appendChild(sectionTitle(label));
  if (!snapshot) {
    node.appendChild(el('p', 'text-xs text-slate-500', 'Not available.'));
    return;
  }
  node.appendChild(el('p', 'text-xs text-slate-400',
    snapshot.char_count + ' chars · hash ' + snapshot.prompt_hash.slice(0, 12) + '…'));
}

function changedSourceSets(currentBlocks, previewBlocks) {
  const key = (s) => s.scope + '|' + (s.source_ref || '') + '|' + (s.source_session_id || '');
  const currentKeys = new Set((currentBlocks || []).flatMap((b) => b.sources.map(key)));
  const previewKeys = new Set((previewBlocks || []).flatMap((b) => b.sources.map(key)));
  return {currentKeys, previewKeys, key};
}

function renderSnapshotInto(container, snapshot, opts) {
  if (!snapshot || !snapshot.blocks || !snapshot.blocks.length) {
    container.appendChild(el('p', 'text-xs text-slate-500', 'No managed instruction blocks.'));
    return;
  }
  const list = el('div', 'space-y-2');
  for (const block of snapshot.blocks) {
    const card = blockCard(block, {});
    if (opts && opts.compare) {
      const {currentKeys, previewKeys, key} = opts.compare;
      const sources = block.sources || [];
      const inCurrent = sources.some((s) => currentKeys.has(key(s)));
      const inPreview = sources.some((s) => previewKeys.has(key(s)));
      if (!inCurrent && inPreview) card.classList.add('ring-1', 'ring-emerald-500/60');
      else if (inCurrent && !inPreview) card.classList.add('ring-1', 'ring-red-500/60');
    }
    list.appendChild(card);
  }
  container.appendChild(list);
}

function render() {
  const container = document.getElementById('tab-task-context');
  if (!container) return;
  const detail = panel.detail;
  if (!detail) return;
  container.textContent = '';
  const wrap = el('div', 'mx-auto max-w-3xl p-4 space-y-4');

  // -- Next run (preview) --------------------------------------------------
  const nextBox = el('div', 'rounded-xl border border-slate-700 bg-slate-800/60 p-4 space-y-3');
  const nextHead = el('div', 'flex items-center gap-2 flex-wrap');
  nextHead.appendChild(sectionTitle('Next run — instructions preview'));
  if (panel.preview) {
    const kindSelect = el('select');
    kindSelect.className = 'bg-slate-900 border border-slate-600 rounded px-2 py-1 text-xs text-slate-300 ml-auto';
    kindSelect.setAttribute('aria-label', 'Preview run kind');
    for (const k of ['manager_turn', 'work', 'review', 'iteration', 'scheduled_step']) {
      const o = el('option', undefined, k);
      o.value = k;
      if ((panel.preview.kind || '') === k) o.selected = true;
      kindSelect.appendChild(o);
    }
    kindSelect.addEventListener('change', () => { panel.kind = kindSelect.value; refreshPreview(); });
    nextHead.appendChild(kindSelect);
  }
  nextBox.appendChild(nextHead);

  if (panel.previewError) {
    const err = el('div', 'rounded-lg bg-red-900/40 border border-red-700/50 text-red-200 text-xs px-3 py-2 space-y-2');
    err.appendChild(el('p', undefined, 'Preview unavailable: ' + panel.previewError));
    const retry = el('button', 'text-xs underline hover:text-red-100', 'Retry');
    retry.addEventListener('click', () => refreshPreview());
    err.appendChild(retry);
    nextBox.appendChild(err);
  } else if (panel.preview) {
    nextBox.appendChild(el('p', 'text-xs text-slate-400',
      panel.preview.char_count + ' chars · hash ' + panel.preview.prompt_hash.slice(0, 12) + '…'));
    if (panel.preview.overlay && panel.preview.overlay.error) {
      nextBox.appendChild(el('p', 'text-xs text-amber-300',
        'Model overlay inactive: ' + panel.preview.overlay.error));
    }
    renderSnapshotInto(nextBox, panel.preview, {compare: panel.currentSnapshot ? changedSourceSets(panel.currentSnapshot.blocks, panel.preview.blocks) : null});
    if (panel.currentSnapshot) {
      nextBox.appendChild(el('p', 'text-[11px] text-slate-500',
        'Green outline: source present in the next run but not the current one. Red: removed since the current run. Both lists are server facts.'));
    }
    const adv = el('details', 'mt-1');
    adv.appendChild(el('summary', 'text-xs text-blue-400 hover:text-blue-300 cursor-pointer select-none', 'Advanced: exact assembled text and hash'));
    const pre = el('pre', 'mt-1 max-h-72 overflow-auto text-xs text-slate-300 whitespace-pre-wrap break-words bg-slate-900 rounded p-2');
    pre.textContent = panel.preview.blocks.map((b) => b.text).join('\n\n');
    adv.appendChild(pre);
    adv.appendChild(el('p', 'mt-1 text-[11px] text-slate-500 font-mono break-all', 'prompt_hash ' + panel.preview.prompt_hash));
    nextBox.appendChild(adv);
    nextBox.appendChild(el('p', 'text-[11px] text-slate-500',
      'This is the managed instruction half only. Task goal, input messages, tool results read later at runtime, and native backend/tool content are separate and are not part of this snapshot.'));
  } else {
    nextBox.appendChild(el('p', 'text-xs text-slate-500', 'Loading preview...'));
  }
  wrap.appendChild(nextBox);

  // -- Current run (historical snapshot) -----------------------------------
  const curBox = el('div', 'rounded-xl border border-slate-700 bg-slate-800/60 p-4 space-y-3');
  curBox.appendChild(sectionTitle('Current run — startup snapshot of record'));
  if (panel.currentRun) {
    curBox.appendChild(el('p', 'text-xs text-slate-400',
      'Run ' + panel.currentRun.id.slice(0, 8) + ' · ' + panel.currentRun.kind));
  }
  if (panel.currentError) {
    const err = el('div', 'rounded-lg bg-red-900/40 border border-red-700/50 text-red-200 text-xs px-3 py-2 space-y-2');
    err.appendChild(el('p', undefined, panel.currentError));
    const retry = el('button', 'text-xs underline hover:text-red-100', 'Retry');
    retry.addEventListener('click', () => refreshCurrentRun());
    err.appendChild(retry);
    curBox.appendChild(err);
  } else if (panel.currentSnapshot) {
    renderSnapshotInto(curBox, panel.currentSnapshot, null);
  } else if (panel.currentRun) {
    curBox.appendChild(el('p', 'text-xs text-slate-500', 'This run has no stored managed-instruction snapshot.'));
  } else {
    curBox.appendChild(el('p', 'text-xs text-slate-500', 'No run has started yet — the next run will commit its snapshot here.'));
  }
  wrap.appendChild(curBox);

  // -- Historical run selected from the Runs tab ---------------------------
  if (panel.historicalError) {
    const err = el('div', 'rounded-lg bg-red-900/40 border border-red-700/50 text-red-200 text-xs px-3 py-2', panel.historicalError);
    wrap.appendChild(err);
  }
  if (panel.historical) {
    const hBox = el('div', 'rounded-xl border border-blue-700/50 bg-slate-800/60 p-4 space-y-3');
    hBox.appendChild(sectionTitle('Historical run ' + panel.historical.runId.slice(0, 8)));
    const payload = panel.historical.payload;
    if (payload.snapshot) {
      hBox.appendChild(el('p', 'text-xs text-slate-400',
        payload.snapshot.char_count + ' chars · hash ' + payload.snapshot.prompt_hash.slice(0, 12) + '…'));
      renderSnapshotInto(hBox, payload.snapshot, null);
    }
    if (payload.legacy_prompt) {
      hBox.appendChild(el('p', 'text-xs text-amber-300', 'Limited historical evidence: ' + payload.legacy_prompt.note));
      const details = el('details');
      details.appendChild(el('summary', 'text-xs text-blue-400 cursor-pointer', 'View raw launch text'));
      const pre = el('pre', 'mt-1 max-h-64 overflow-auto text-xs text-slate-300 whitespace-pre-wrap bg-slate-900 rounded p-2');
      fetch('/files/' + encodePathSegments(payload.legacy_prompt.ref))
        .then((r) => (r.ok ? r.text() : Promise.reject(new Error(String(r.status)))))
        .then((text) => { pre.textContent = text; })
        .catch((err) => { pre.textContent = '(raw launch text unavailable: ' + err.message + ')'; });
      details.appendChild(pre);
      hBox.appendChild(details);
    }
    if (!payload.snapshot && !payload.legacy_prompt) {
      hBox.appendChild(el('p', 'text-xs text-slate-500', 'No stored instruction evidence for this run.'));
    }
    const close = el('button', 'text-xs text-slate-400 hover:text-slate-200 underline', 'Close historical view');
    close.addEventListener('click', () => { panel.historical = null; render(); });
    hBox.appendChild(close);
    wrap.appendChild(hBox);
  }

  // -- Local rule editor ---------------------------------------------------
  wrap.appendChild(renderRuleEditor(detail));
  container.appendChild(wrap);
}

function encodePathSegments(path) {
  return String(path || '').split('/').filter(Boolean).map(encodeURIComponent).join('/');
}

function renderRuleEditor(detail) {
  const rules = detail.prompt_rules || {node: {}, subtree: {}, affected_descendants: 0};
  const box = el('div', 'rounded-xl border border-slate-700 bg-slate-800/60 p-4 space-y-3');
  box.appendChild(sectionTitle('Local rules (optional)'));

  const scopeRow = el('div', 'flex items-center gap-3 flex-wrap');
  scopeRow.setAttribute('role', 'radiogroup');
  scopeRow.setAttribute('aria-label', 'Rule scope');
  for (const opt of [['node', 'This task'], ['subtree', 'This task and descendants']]) {
    const label = el('label', 'flex items-center gap-1.5 text-xs text-slate-300 cursor-pointer select-none');
    const radio = el('input');
    radio.type = 'radio';
    radio.name = 'task-rule-scope';
    radio.value = opt[0];
    radio.className = 'accent-blue-500';
    radio.checked = panel.ruleScope === opt[0];
    radio.setAttribute('aria-label', opt[1]);
    radio.addEventListener('change', () => {
      panel.ruleScope = opt[0];
      panel.ruleDraft = null;
      render();
    });
    label.appendChild(radio);
    label.appendChild(el('span', undefined, opt[1]));
    scopeRow.appendChild(label);
  }
  box.appendChild(scopeRow);

  const scope = panel.ruleScope;
  const rule = rules[scope] || {};
  const draft = panel.ruleDraft;
  const currentText = draft ? draft.text : (rule.text || '');

  const textarea = el('textarea');
  textarea.id = 'task-rule-editor';
  textarea.className = 'w-full bg-slate-900 border border-slate-600 rounded-lg px-3 py-2 text-sm text-slate-200 placeholder-slate-500 focus:outline-none focus:border-blue-500 resize-y font-mono';
  textarea.rows = 5;
  textarea.placeholder = 'Empty by default. Free text applies to ' + (scope === 'node' ? 'this task only.' : 'this task and every descendant task.');
  textarea.value = currentText;
  textarea.setAttribute('aria-label', scope === 'node' ? 'This task rule' : 'This task and descendants rule');
  textarea.addEventListener('input', () => {
    panel.ruleDraft = {scope, text: textarea.value};
    updateRuleFooter(rule, textarea.value);
  });
  box.appendChild(textarea);

  const footer = el('div', 'flex items-center gap-3 flex-wrap text-xs');
  footer.id = 'task-rule-footer';
  const save = el('button', 'px-3 py-1.5 rounded-lg bg-blue-600 hover:bg-blue-500 text-white text-xs font-medium', 'Save ' + (scope === 'node' ? 'this-task rule' : 'subtree rule'));
  save.addEventListener('click', () => saveRule(scope, textarea.value));
  const clear = el('button', 'px-3 py-1.5 rounded-lg border border-red-600/60 text-red-300 hover:bg-red-900/30 text-xs', 'Clear rule');
  clear.addEventListener('click', () => saveRule(scope, null));
  footer.appendChild(save);
  footer.appendChild(clear);
  footer.appendChild(el('span', 'text-slate-500', 'Saved through PATCH; applies to the next run only — an active Run keeps its original snapshot.'));
  box.appendChild(footer);

  const info = el('p', 'text-xs text-slate-500');
  if (scope === 'subtree') {
    info.textContent = 'Applies to this task and ' + (rules.affected_descendants || 0) + ' descendant task(s). This task is included.';
  } else {
    info.textContent = 'Applies to this task only; descendants never inherit it.';
  }
  box.appendChild(info);
  if (rule.ref) {
    box.appendChild(el('p', 'text-[11px] text-slate-600 font-mono break-all', 'current ref prompt_bodies/' + rule.ref + '.md · ' + (rule.chars || 0) + ' chars'));
  }
  return box;
}

function updateRuleFooter(rule, value) {
  // The unsaved draft is flagged in the editor's own footer; the draft lives
  // only in panel memory until Save/Clear PATCHes it.
  let note = document.getElementById('task-rule-draft-note');
  const footer = document.getElementById('task-rule-footer');
  if (!footer) return;
  if (!note) {
    note = el('span', 'text-amber-300');
    note.id = 'task-rule-draft-note';
    footer.appendChild(note);
  }
  const differs = value !== (rule.text || '');
  note.textContent = differs ? 'Unsaved draft' : '';
}

async function saveRule(scope, body) {
  const sessionId = boundSessionId();
  if (!sessionId) return;
  const payload = scope === 'node' ? {node_prompt: body} : {subtree_prompt: body};
  const res = await fetch('/api/sessions/' + sessionId, {
    method: 'PATCH', headers: JSON_HEADERS, body: JSON.stringify(payload),
  });
  if (res.ok) {
    panel.ruleDraft = null;
    showToast(scope === 'node' ? 'This-task rule saved.' : 'Subtree rule saved.', false);
    if (Sidebar.SessionTree) Sidebar.SessionTree.invalidateAll();
    await refresh();
  } else {
    const errBody = await res.json().catch(() => ({}));
    const message = (errBody.detail && errBody.detail.message) || errBody.detail || ('HTTP ' + res.status);
    showToast('Rule save refused: ' + message, true);
    console.error('rule save failed:', res.status, message);
  }
}

// -- lifecycle -----------------------------------------------------------

function onSessionChanged(session) {
  panel.detail = null;
  panel.preview = null;
  panel.previewError = null;
  panel.historical = null;
  panel.historicalError = null;
  panel.currentRun = null;
  panel.currentSnapshot = null;
  panel.currentError = null;
  panel.ruleDraft = null;
  if (!session || !session.profile) {
    panel.sessionId = null;
    return;
  }
  panel.sessionId = session.id;
  refresh();
}

function onTreeChanged(sessionIds) {
  if (!panel.sessionId) return;
  if (sessionIds.includes(panel.sessionId)) refresh();
}

function onTabShown() {
  if (panel.sessionId === boundSessionId() && panel.detail) return;
  refresh();
}

globalThis.TaskContextPanel = {
  onSessionChanged,
  onTabShown,
  onTreeChanged,
  showHistoricalRun,
  refresh,
};
})();
