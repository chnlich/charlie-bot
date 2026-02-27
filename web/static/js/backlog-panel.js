// ---------------------------------------------------------------------------
// Backlog panel
// ---------------------------------------------------------------------------
const backlogPanel = (() => {
  let _items = [];
  let _history = [];
  let _loaded = false;

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
  };

  function _badge(map, key, fallback) {
    return map[key] || fallback || 'bg-gray-700 text-gray-400';
  }

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

    let actions = '';
    if (item.status === 'pending') {
      actions = `
        <button onclick="backlogPanel.updateStatus('${item.id}','approved')"
                class="px-2 py-1 text-xs rounded bg-green-800 hover:bg-green-700 text-green-200 transition-colors">Approve</button>
        <button onclick="backlogPanel.rejectWithReason('${item.id}')"
                class="px-2 py-1 text-xs rounded bg-red-800 hover:bg-red-700 text-red-200 transition-colors">Reject</button>`;
    } else if (item.status === 'approved') {
      actions = `
        <button onclick="backlogPanel.updateStatus('${item.id}','pending')"
                class="px-2 py-1 text-xs rounded bg-gray-700 hover:bg-gray-600 text-gray-300 transition-colors">Revoke</button>`;
    } else if (item.status === 'rejected') {
      actions = `
        <button onclick="backlogPanel.updateStatus('${item.id}','pending')"
                class="px-2 py-1 text-xs rounded bg-gray-700 hover:bg-gray-600 text-gray-300 transition-colors">Reopen</button>`;
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
      ? `<span class="px-1.5 py-0.5 rounded text-xs font-medium bg-indigo-900 text-indigo-300">${_esc(modLabel)}</span>`
      : '';

    const descId = `backlog-desc-${item.id}`;
    return `
      <div class="bg-gray-800 rounded-lg p-3 border border-gray-700 hover:border-gray-600 transition-colors">
        <div class="flex flex-wrap gap-1 mb-1.5">
          <span class="px-1.5 py-0.5 rounded text-xs font-medium ${priorityCls}">${item.priority || 'low'}</span>
          <span class="px-1.5 py-0.5 rounded text-xs font-medium ${categoryCls}">${item.category || ''}</span>
          <span class="px-1.5 py-0.5 rounded text-xs font-medium ${statusCls}">${item.status || ''}</span>
          ${modBadge}
        </div>
        <p class="text-sm font-semibold text-gray-100 mb-1"><span class="text-gray-500 font-mono">#${_esc(item.id)}</span> ${_esc(item.title || '')}</p>
        <p id="${descId}" class="text-xs text-gray-400 line-clamp-2 cursor-pointer select-none"
           onclick="this.classList.toggle('line-clamp-2')">${_esc(item.description || '')}</p>
        ${item.rejected_reason ? `<p class="text-xs text-red-400 mt-1">Rejected${item.rejected_at ? ' ' + _fmtDate(item.rejected_at) : ''}: ${_esc(item.rejected_reason)}</p>` : ''}
        <p class="text-xs text-gray-600 mt-1">${_fmtDate(item.created)}</p>
        ${backtestHtml}
        ${actions ? `<div class="flex gap-2 mt-2">${actions}</div>` : ''}
      </div>`;
  }

  function _esc(s) {
    return String(s)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function _populateModuleFilter() {
    const sel = document.getElementById('backlog-module-filter');
    if (!sel) return;
    const sources = [...new Set(_items.map(i => i._source).filter(Boolean))].sort();
    const prev = sel.value;
    sel.innerHTML = '<option value="all">All modules</option>' +
      sources.map(s => `<option value="${_esc(s)}">${_esc(_moduleLabel(s))}</option>`).join('');
    if (sources.includes(prev)) sel.value = prev;
  }

  async function refresh() {
    const list = document.getElementById('backlog-list');
    if (list) list.innerHTML = '<p class="text-xs text-gray-500">Loading...</p>';
    try {
      const [bResp, hResp] = await Promise.all([
        fetch('/api/backlog'),
        fetch('/api/backlog/history'),
      ]);
      _items   = bResp.ok ? await bResp.json() : [];
      _history = hResp.ok ? await hResp.json() : [];
      _loaded = true;
    } catch (e) {
      console.error('backlog refresh failed:', e);
      _items = [];
      _history = [];
    }
    _populateModuleFilter();
    render();
  }

  function render() {
    const list = document.getElementById('backlog-list');
    if (!list) return;
    const statusFilter = (document.getElementById('backlog-filter') || {}).value || 'pending';
    const moduleFilter = (document.getElementById('backlog-module-filter') || {}).value || 'all';

    let visible = _items;
    if (statusFilter === 'pending')  visible = visible.filter(i => i.status === 'pending');
    else if (statusFilter === 'done') visible = visible.filter(i => i.status === 'done');
    else if (statusFilter === 'rejected') visible = visible.filter(i => i.status === 'rejected');
    // 'all' → no status filter

    if (moduleFilter !== 'all') {
      visible = visible.filter(i => i._source === moduleFilter);
    }

    if (!visible.length) {
      list.innerHTML = '<p class="text-xs text-gray-500">No items.</p>';
      return;
    }
    list.innerHTML = visible.map(_renderCard).join('');
  }

  async function updateStatus(id, newStatus, extra) {
    try {
      const resp = await fetch(`/api/backlog/${id}`, {
        method: 'PATCH',
        headers: {'Content-Type': 'application/json'},
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

  async function rejectWithReason(id) {
    const reason = prompt('Rejection reason (optional):');
    if (reason === null) return;  // user cancelled
    await updateStatus(id, 'rejected', {rejected_reason: reason || null});
  }

  function init() {
    // Resize handle init happens in app.js via initBacklogResize()
  }

  return {init, refresh, render, updateStatus, rejectWithReason};
})();

// ---------------------------------------------------------------------------
// Backlog panel resize (mirrors initLatexResize pattern)
// ---------------------------------------------------------------------------
function initBacklogResize() {
  const handle = document.getElementById('backlog-resize-handle');
  const panel  = document.getElementById('backlog-panel');
  if (!handle || !panel) return;
  const container = panel.parentElement;
  const saved = localStorage.getItem('backlog-panel-pct');
  if (saved) panel.style.width = saved + '%';

  let startX, startW, containerW;
  handle.addEventListener('mousedown', (e) => {
    e.preventDefault();
    startX = e.clientX;
    containerW = container.offsetWidth;
    startW = panel.offsetWidth;
    handle.classList.add('active');
    document.body.classList.add('resizing');

    function onMove(e) {
      const delta = startX - e.clientX;
      const w = Math.min(Math.max(startW + delta, containerW * 0.2), containerW * 0.8);
      panel.style.width = w + 'px';
    }
    function onUp() {
      handle.classList.remove('active');
      document.body.classList.remove('resizing');
      const pct = (panel.offsetWidth / container.offsetWidth * 100).toFixed(1);
      localStorage.setItem('backlog-panel-pct', pct);
      panel.style.width = pct + '%';
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
    }
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  });
}
