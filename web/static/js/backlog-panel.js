// ---------------------------------------------------------------------------
// Backlog panel
// ---------------------------------------------------------------------------
const backlogPanel = (() => {
  let _items = [];
  let _history = [];
  let _loaded = false;
  let _repos = [];
  let _currentRepo = null;  // null = default (first configured repo)

  const PRIORITY_BADGE = {
    high:   'bg-red-900 text-red-300',
    medium: 'bg-yellow-900 text-yellow-300',
    low:    'bg-gray-700 text-gray-400',
  };

  const CATEGORY_BADGE = {
    feature:  'bg-blue-900 text-blue-300',
    strategy: 'bg-purple-900 text-purple-300',
    data:     'bg-green-900 text-green-300',
    infra:    'bg-orange-900 text-orange-300',
    backtest: 'bg-cyan-900 text-cyan-300',
  };

  const STATUS_BADGE = {
    pending:     'bg-gray-700 text-gray-300',
    approved:    'bg-green-900 text-green-300',
    in_progress: 'bg-blue-900 text-blue-300',
    done:        'bg-green-800 text-green-200',
    rejected:    'bg-red-900 text-red-300',
    failed:      'bg-orange-900 text-orange-300',
    revision_requested: 'bg-yellow-900 text-yellow-300',
  };

  function _badge(map, key, fallback) {
    return map[key] || fallback || 'bg-gray-700 text-gray-400';
  }

  // Full literal class strings, like the badge maps: Tailwind's content scan
  // only generates classes it sees as complete tokens, so nothing here may
  // switch to composing `bg-${color}-800`-style fragments.
  const ACTION_BTN_CLASS = {
    green:  'px-2 py-1 text-xs rounded bg-green-800 hover:bg-green-700 text-green-200 transition-colors',
    yellow: 'px-2 py-1 text-xs rounded bg-yellow-800 hover:bg-yellow-700 text-yellow-200 transition-colors',
    red:    'px-2 py-1 text-xs rounded bg-red-800 hover:bg-red-700 text-red-200 transition-colors',
    gray:   'px-2 py-1 text-xs rounded bg-gray-700 hover:bg-gray-600 text-gray-300 transition-colors',
    orange: 'px-2 py-1 text-xs rounded bg-orange-800 hover:bg-orange-700 text-orange-200 transition-colors',
  };

  function _moduleLabel(source) {
    if (!source) return '';
    return source.replace(/^alpha-lab-/, '');
  }

  function _fmtDate(raw) {
    if (!raw) return '';
    const d = new Date(raw);
    if (isNaN(d)) return raw;
    return d.toLocaleString('en-US', {
      month: 'short', day: 'numeric',
      hour: 'numeric', minute: '2-digit',
    });
  }

  function _historyFor(id) {
    return _history.find(h => String(h.idea_id) === String(id));
  }

  function _renderCard(item) {
    const hist = _historyFor(item.id);
    const priorityCls = _badge(PRIORITY_BADGE, item.priority);
    const categoryCls = _badge(CATEGORY_BADGE, item.category);
    const statusCls   = _badge(STATUS_BADGE, item.status);
    const modLabel    = _moduleLabel(item._source);
    const repo = _currentRepo || '';

    // updateStatus takes the target status between id and the identity triple
    // (id, source, repo) that every other action handler takes.
    const identityArgs = [item.id, item._source || '', repo];
    const actionBtn = (handler, color, label, status) => {
      const args = status ? [item.id, status, item._source || '', repo] : identityArgs;
      const onclick = `backlogPanel.${handler}(${args.map(a => `'${a}'`).join(',')})`;
      return `<button onclick="${onclick}" class="${ACTION_BTN_CLASS[color]}">${label}</button>`;
    };

    let actions = '';
    if (item.status === 'pending') {
      actions = actionBtn('updateStatus', 'green', 'Approve', 'approved')
        + ' ' + actionBtn('requestRevision', 'yellow', 'Revise')
        + ' ' + actionBtn('rejectWithReason', 'red', 'Reject');
    } else if (item.status === 'revision_requested') {
      actions = actionBtn('updateStatus', 'green', 'Approve', 'approved')
        + ' ' + actionBtn('rejectWithReason', 'red', 'Reject');
    } else if (item.status === 'approved') {
      actions = actionBtn('updateStatus', 'gray', 'Revoke', 'pending');
    } else if (item.status === 'rejected') {
      actions = actionBtn('updateStatus', 'gray', 'Reopen', 'pending');
    } else if (item.status === 'failed') {
      actions = actionBtn('retryItem', 'orange', 'Retry');
    }

    let backtestHtml = '';
    if (item.status === 'done' && hist && hist.backtest_result) {
      const br = hist.backtest_result;
      if (br.before && br.after && typeof br.before === 'object' && typeof br.after === 'object') {
        const metrics = [...new Set([...Object.keys(br.before), ...Object.keys(br.after)])];
        const rows = metrics.map(m => {
          const bv = br.before[m] ?? '—';
          const av = br.after[m] ?? '—';
          return `<span class="text-gray-400">${m}:</span> <span class="text-gray-200">${bv}</span>` +
            `<span class="text-gray-500">→</span><span class="text-gray-200">${av}</span>`;
        }).join(' &middot; ');
        backtestHtml = `<div class="mt-2 text-xs font-mono text-gray-400 bg-gray-900 rounded px-2 py-1">${rows}</div>`;
      } else {
        const pairs = Object.entries(br).map(([k, v]) =>
          `<span class="text-gray-400">${k}:</span> <span class="text-gray-200">${typeof v === 'object' ? JSON.stringify(v) : v}</span>`
        ).join(' &middot; ');
        backtestHtml = `<div class="mt-2 text-xs font-mono text-gray-400 bg-gray-900 rounded px-2 py-1">${pairs}</div>`;
      }
    }

    const modBadge = modLabel
      ? `<span class="px-1.5 py-0.5 rounded text-xs font-medium bg-indigo-900 text-indigo-300">${escapeHtmlAttr(modLabel)}</span>`
      : '';

    const descId = `backlog-desc-${item.id}`;
    return `
      <div class="bg-gray-800 rounded-lg p-3 border border-gray-700 hover:border-gray-600 transition-colors">
        <div class="flex flex-wrap gap-1 mb-1.5">
          <span class="px-1.5 py-0.5 rounded text-xs font-medium ${priorityCls}">${item.priority || 'low'}</span>
          ${item.category ? `<span class="px-1.5 py-0.5 rounded text-xs font-medium ${categoryCls}">${item.category}</span>` : ''}
          ${item.status ? `<span class="px-1.5 py-0.5 rounded text-xs font-medium ${statusCls}">${item.status}</span>` : ''}
          ${modBadge}
        </div>
        <p class="text-sm font-semibold text-gray-100 mb-1"><span class="text-gray-500 font-mono">#${escapeHtmlAttr(item.id)}</span> ${escapeHtmlAttr(item.title || '')}</p>
        <p id="${descId}" class="text-xs text-gray-400 line-clamp-2 cursor-pointer select-none"
           onclick="this.classList.toggle('line-clamp-2')">${escapeHtmlAttr(item.description || '')}</p>
        ${item.rejected_reason ? `<p class="text-xs text-red-400 mt-1">Rejected${item.rejected_at ? ' ' + _fmtDate(item.rejected_at) : ''}: ${escapeHtmlAttr(item.rejected_reason)}</p>` : ''}
        ${item.failed_reason ? `<p class="text-xs text-orange-400 mt-1">Failed${item.failed_at ? ' ' + _fmtDate(item.failed_at) : ''}${item.failed_count > 1 ? ' (' + item.failed_count + 'x)' : ''}: ${escapeHtmlAttr(item.failed_reason)}</p>` : ''}
        ${item.revision_feedback ? `<p class="text-xs text-yellow-400 mt-1">Revision requested${item.revision_requested_at ? ' ' + _fmtDate(item.revision_requested_at) : ''}: ${escapeHtmlAttr(item.revision_feedback)}</p>` : ''}
        <p class="text-xs text-gray-600 mt-1">${_fmtDate(item.created)}</p>
        ${backtestHtml}
        ${actions ? `<div class="flex gap-2 mt-2">${actions}</div>` : ''}
      </div>`;
  }

  function _populateModuleFilter() {
    const sel = document.getElementById('backlog-module-filter');
    if (!sel) return;
    const sources = [...new Set(_items.map(i => i._source).filter(Boolean))].sort();
    const prev = sel.value;
    sel.innerHTML = '<option value="all">All modules</option>' +
      sources.map(s => `<option value="${escapeHtmlAttr(s)}">${escapeHtmlAttr(_moduleLabel(s))}</option>`).join('');
    if (sources.includes(prev)) sel.value = prev;
  }

  function _repoQs() {
    return _currentRepo ? '?repo=' + encodeURIComponent(_currentRepo) : '';
  }

  function _populateRepoSelector() {
    const sel = document.getElementById('backlog-repo-selector');
    if (!sel || !_repos.length) return;
    sel.innerHTML = _repos.map(r =>
      `<option value="${escapeHtmlAttr(r.path)}">${escapeHtmlAttr(r.label)}</option>`
    ).join('');
    if (_currentRepo) sel.value = _currentRepo;
    else sel.value = _repos[0].path;
  }

  async function refresh() {
    const list = document.getElementById('backlog-list');
    if (list) list.innerHTML = '<p class="text-xs text-gray-500">Loading...</p>';
    let error = null;
    try {
      // Fetch repos list on first load
      if (!_repos.length) {
        const rResp = await fetch('/api/backlog/repos');
        _repos = rResp.ok ? await rResp.json() : [];
        _populateRepoSelector();
      }
      const qs = _repoQs();
      const [bResp, hResp] = await Promise.all([
        fetch('/api/backlog' + qs),
        fetch('/api/backlog/history' + qs),
      ]);
      if (!bResp.ok) {
        _items = [];
        _history = [];
        error = `Failed to load backlog (status ${bResp.status})`;
      } else {
        _items   = await bResp.json();
        _history = hResp.ok ? await hResp.json() : [];
        _loaded = true;
      }
    } catch (e) {
      console.error('backlog refresh failed:', e);
      _items = [];
      _history = [];
      error = `Failed to load backlog: ${e.message || e}`;
    }
    if (error) {
      if (list) list.innerHTML = `<p class="text-xs text-red-400">${escapeHtmlAttr(error)}</p>`;
      return;
    }
    _populateModuleFilter();
    render();
  }

  function switchRepo(path) {
    _currentRepo = path;
    refresh();
  }

  function render() {
    const list = document.getElementById('backlog-list');
    if (!list) return;
    const statusFilter = (document.getElementById('backlog-filter') || {}).value || 'pending';
    const moduleFilter = (document.getElementById('backlog-module-filter') || {}).value || 'all';

    let visible = _items;
    if (statusFilter === 'pending')  visible = visible.filter(i => i.status === 'pending' || i.status === 'in_progress' || i.status === 'failed' || i.status === 'revision_requested');
    else if (statusFilter === 'done') visible = visible.filter(i => i.status === 'done');
    else if (statusFilter === 'rejected') visible = visible.filter(i => i.status === 'rejected');
    else if (statusFilter === 'failed') visible = visible.filter(i => i.status === 'failed');
    // 'all' → no status filter

    if (moduleFilter !== 'all') {
      visible = visible.filter(i => i._source === moduleFilter);
    }

    const priorityWeight = {high: 0, medium: 1, low: 2};
    visible.sort((a, b) => (priorityWeight[a.priority] ?? 3) - (priorityWeight[b.priority] ?? 3));

    if (!visible.length) {
      list.innerHTML = '<p class="text-xs text-gray-500">No items.</p>';
      return;
    }
    list.innerHTML = visible.map(_renderCard).join('');
  }

  async function updateStatus(id, newStatus, source, repo, extra) {
    const params = new URLSearchParams();
    if (source) params.set('source', source);
    if (repo) params.set('repo', repo);
    const qs = params.toString() ? '?' + params.toString() : '';
    try {
      const resp = await fetch(`/api/backlog/${id}${qs}`, {
        method: 'PATCH',
        headers: JSON_HEADERS,
        body: JSON.stringify({status: newStatus, ...extra}),
      });
      if (!resp.ok) {
        console.error('backlog PATCH failed:', await resp.text());
        return;
      }
    } catch (e) {
      console.error('backlog updateStatus failed:', e);
      return;
    }
    await refresh();
  }

  async function rejectWithReason(id, source, repo) {
    const reason = prompt('Rejection reason (optional):');
    if (reason === null) return;  // user cancelled
    await updateStatus(id, 'rejected', source, repo, {rejected_reason: reason || null});
  }

  async function requestRevision(id, source, repo) {
    const text = prompt('Revision feedback (required):');
    if (!text) return;  // cancelled or empty
    await updateStatus(id, 'revision_requested', source, repo, {revision_feedback: text});
  }

  async function retryItem(id, source, repo) {
    await updateStatus(id, 'approved', source, repo);
  }

  function init() {
    // Resize handle init happens in app.js via initBacklogResize()
  }

  return {init, refresh, render, updateStatus, rejectWithReason, requestRevision, retryItem, switchRepo};
})();

// ---------------------------------------------------------------------------
// Backlog panel resize
// ---------------------------------------------------------------------------
function initBacklogResize() {
  initPanelResize({
    handleId: 'backlog-resize-handle',
    panelId: 'backlog-panel',
    storageKey: 'backlog-panel-pct',
    onDragStart: () => {},
    onDragEnd: () => {},
  });
}
