(function() {
  const Sidebar = globalThis.Sidebar;

function formatCardTimestamp(d) {
  const mm = String(d.getMonth() + 1).padStart(2, '0');
  const dd = String(d.getDate()).padStart(2, '0');
  const hh = String(d.getHours()).padStart(2, '0');
  const mi = String(d.getMinutes()).padStart(2, '0');
  return mm + '/' + dd + ' ' + hh + ':' + mi;
}

function formatTriggerTimeLabel(status, fireAt) {
  if (status === 'cancelled') return 'cancelled';
  const prefix = status === 'fired' ? 'fired at ' : 'fires at ';
  return prefix + formatCardTimestamp(new Date(fireAt));
}

// One trigger card gets painted from three sites -- full render, live append,
// and the poll-driven updateTriggerStatus -- and every site derives the card's
// border and icon from the status. One mapping keeps an update from flashing a
// stale palette onto a card painted by another site.
function triggerStatusChrome(status) {
  return {
    border: status === 'pending' ? 'border-amber-500/50 border-dashed'
      : status === 'fired' ? 'border-green-500/50' : 'border-slate-600',
    icon: status === 'pending' ? 'text-amber-400'
      : status === 'fired' ? 'text-green-400' : 'text-slate-500',
  };
}

function workerUuidRow(id) {
  const safe = escapeHtml(id);
  return '<p class="text-xs text-slate-600 font-mono truncate" title="click to copy" data-uuid="' + safe + '" onclick="event.stopPropagation(); navigator.clipboard.writeText(this.dataset.uuid); const el=this; el.textContent=\'copied\'; if(el._copyTimer)clearTimeout(el._copyTimer); el._copyTimer=setTimeout(()=>{el.textContent=el.dataset.uuid;el._copyTimer=null;},800)">' + safe + '</p>';
}

function cancelButtonHtml(jsCall, title, domId) {
  return '<button' + (domId ? ' id="' + domId + '"' : '')
    + ' onclick="event.stopPropagation(); ' + jsCall + '"'
    + ' class="p-1 rounded hover:bg-slate-700 text-slate-500 hover:text-red-400 transition-colors"'
    + ' title="' + title + '"><svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">'
    + '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12"/></svg></button>';
}

// The card class chain is written by all three paint/update sites; the border
// segment comes from triggerStatusChrome(status).
function triggerCardClass(borderClass) {
  return 'bg-slate-800 rounded-xl border ' + borderClass + ' overflow-hidden';
}

function triggerCardBodyHtml(triggerId, status, message, fireAt, sessionId) {
  const chrome = triggerStatusChrome(status);
  const strikeClass = status === 'cancelled' ? ' line-through text-slate-500' : '';
  const cancelBtn = status === 'pending'
    ? cancelButtonHtml("cancelTrigger('" + triggerId + "', '" + sessionId + "')", 'Cancel trigger', '')
    : '';
  return '<div class="flex items-center gap-3 px-4 py-3">'
    + '<svg id="trigger-dot-' + triggerId + '" class="w-4 h-4 flex-shrink-0 ' + chrome.icon + '" fill="none" stroke="currentColor" viewBox="0 0 24 24">'
    + Sidebar.CLOCK_SVG_BODY
    + '</svg>'
    + '<div class="flex-1 min-w-0">'
    + '<p class="text-sm truncate' + strikeClass + '">' + escapeHtml(message || '') + '</p>'
    + '<p id="trigger-status-' + triggerId + '" class="text-xs text-slate-500" data-fire-at="' + escapeHtml(fireAt || '') + '">' + formatTriggerTimeLabel(status, fireAt) + '</p>'
    + '</div>'
    + cancelBtn
    + '</div>';
}

const WORKER_CARD_CLASS = 'bg-slate-800 rounded-xl border border-slate-700 overflow-hidden';

// A thread card gets painted from two sites -- the full renderWorkersTab pass
// and the live addWorkerCard append -- and updateWorkerStatus rewrites the
// dot, status line, and cancel button by id. One body builder keeps both
// painters on the ids and class chains the updater looks up.
// A truncated list row ships only the description prefix (the card paints one
// CSS-truncated line), so its full-text modal fetches the thread row on click
// instead of holding the whole text in an attribute.
function fetchWorkerDescription(threadId, sessionId) {
  return fetch('/api/threads/' + sessionId + '/threads/' + threadId)
    .then(r => {
      if (!r.ok) throw new Error('HTTP ' + r.status);
      return r.json();
    })
    .then(meta => showTextModal('Worker Description', meta.description || ''))
    .catch(err => console.error('fetchWorkerDescription failed:', err));
}

function workerCardBodyHtml(t, sessionId) {
  const dotColor = STATUS_DOT_COLORS[t.status] || 'bg-slate-500';
  const pulse = t.status === 'running' ? ' animate-pulse' : '';
  const created = new Date(t.created_at);
  let duration = '';
  if (t.completed_at) {
    const secs = Math.floor((new Date(t.completed_at) - created) / 1000);
    duration = ' &middot; ' + Math.floor(secs / 60) + 'm' + (secs % 60) + 's';
  }
  const cancelBtn = t.status === 'running'
    ? cancelButtonHtml("cancelThread('" + t.id + "', '" + sessionId + "')", 'Cancel', 'cancel-btn-' + t.id)
    : '';
  const descTruncated = typeof t.description_full_len === 'number';
  const descClick = descTruncated
    ? "fetchWorkerDescription('" + t.id + "', '" + sessionId + "')"
    : "showTextModal('Worker Description', this.dataset.full)";
  const descFullAttr = descTruncated ? '' : ' data-full="' + escapeHtmlAttr(t.description) + '"';
  return '<div class="flex items-center gap-3 px-4 py-3 cursor-pointer hover:bg-slate-750" onclick="toggleThreadDetail(\'' + t.id + '\', \'' + sessionId + '\')">'
    + '<span id="thread-dot-' + t.id + '" class="w-2 h-2 rounded-full flex-shrink-0 ' + dotColor + pulse + '"></span>'
    + '<div class="flex-1 min-w-0">'
    + '<p class="text-sm truncate cursor-pointer hover:text-blue-400 transition-colors" title="Click to view full description" onclick="event.stopPropagation(); ' + descClick + '"' + descFullAttr + '>' + escapeHtml(t.description || '') + '</p>'
    + '<p id="thread-status-' + t.id + '" class="text-xs text-slate-500">' + (t.status || 'idle') + ' &middot; ' + formatCardTimestamp(created) + duration + (t.backend ? ' &middot; ' + (BACKEND_OPTIONS[t.backend] || t.backend) : '') + '</p>'
    + workerUuidRow(t.id)
    + '</div>'
    + cancelBtn
    + '<svg class="w-4 h-4 text-slate-500 transition-transform thread-chevron" id="chevron-' + t.id + '" fill="none" stroke="currentColor" viewBox="0 0 24 24">'
    + Sidebar.CHEVRON_SVG_PATH
    + '</svg>'
    + '</div>'
    + '<div id="thread-detail-' + t.id + '" class="hidden border-t border-slate-700">'
    + '<div id="thread-attach-' + t.id + '" class="px-4 pt-4 hidden"></div>'
    + '<div id="thread-events-' + t.id + '" class="p-4 max-h-96 overflow-y-auto"><p class="text-xs text-slate-500">Loading events...</p></div>'
    + '</div>';
}

function renderWorkersTab(threads, sessionId, triggers) {
  const container = document.getElementById('tab-workers');
  if (!container) return;

  triggers = triggers || [];

  if ((!threads || !threads.length) && !triggers.length) {
    container.innerHTML = '<div id="no-workers-placeholder" class="flex items-center justify-center h-full text-slate-500 text-sm">No worker threads</div>';
    return;
  }

  const threadCards = (threads || []).map(t =>
    '<div class="' + WORKER_CARD_CLASS + '">' + workerCardBodyHtml(t, sessionId) + '</div>');

  const triggerCards = triggers.map(tr => {
    const chrome = triggerStatusChrome(tr.status);
    return '<div id="trigger-card-' + tr.id + '" class="' + triggerCardClass(chrome.border) + '">'
      + triggerCardBodyHtml(tr.id, tr.status, tr.message, tr.fire_at, sessionId)
      + '</div>';
  });

  container.innerHTML = threadCards.join('') + triggerCards.join('');
}

function renderWorkersTabUnknown() {
  const container = document.getElementById('tab-workers');
  if (!container) return;
  container.innerHTML = '<div id="workers-loading-placeholder" class="flex items-center justify-center h-full text-slate-500 text-sm">Loading worker threads...</div>';
  updateWorkersTabBadge();
}

// Poll-based workers tab updates (replaces WS-driven addWorkerCard/updateWorkerStatus)

function renderWorkersListItems(items, sessionId) {
  const threads = [];
  const triggers = [];
  for (const item of items || []) {
    if (item.type === 'trigger') triggers.push(item);
    else threads.push(item);
  }
  renderWorkersTab(threads, sessionId, triggers);
  updateWorkersTabBadge();
}

function restartWorkersPolling() {
  startPageTimer('workers-list', pollWorkers, 3000);
}

async function ensureWorkersLoadedForActiveSession(opts) {
  const force = opts && opts.force;
  const pollSessionId = SESSION_ID;
  if (!pollSessionId) return;
  if (!force && workersLoadedForSession === pollSessionId) return;
  if (workersLoadInflightForSession === pollSessionId) return;
  workersLoadInflightForSession = pollSessionId;
  try {
    const res = await fetch('/api/threads/' + pollSessionId + '/list');
    if (!res.ok) throw new Error(res.status);
    const items = await res.json();
    if (pollSessionId !== SESSION_ID) return;
    renderWorkersListItems(items || [], pollSessionId);
    workersLoadedForSession = pollSessionId;
    restartWorkersPolling();
  } catch (err) {
    console.error('loadWorkers failed:', err);
  } finally {
    if (workersLoadInflightForSession === pollSessionId) workersLoadInflightForSession = null;
  }
}

function pollWorkers() {
  const pollSessionId = SESSION_ID;
  if (!pollSessionId) return;
  if (workersLoadedForSession !== pollSessionId) {
    ensureWorkersLoadedForActiveSession({force: true});
    return;
  }
  fetch('/api/threads/' + pollSessionId + '/list')
    .then(r => r.ok ? r.json() : null)
    .then(items => {
      if (!items || pollSessionId !== SESSION_ID) return;
      for (const item of items) {
        if (item.type === 'trigger') {
          const existing = document.getElementById('trigger-dot-' + item.id);
          if (!existing) {
            addTriggerCard(item.id, item.message, item.fire_at, item.created_at, item.status);
          } else {
            updateTriggerStatus(item.id, item.status);
          }
        } else {
          const existing = document.getElementById('thread-dot-' + item.id);
          if (!existing) {
            addWorkerCard(item.id, item.description, item.created_at, item.backend || '', item.description_full_len);
            if (item.status !== 'running') updateWorkerStatus(item.id, item.status);
          } else {
            updateWorkerStatus(item.id, item.status);
          }
        }
      }
    })
    .catch(err => console.error('pollWorkers failed:', err));
}

function updateWorkersTabBadge() {
  var btn = document.getElementById('btn-workers');
  if (!btn) return;
  var count = document.querySelectorAll('[id^="thread-dot-"]').length
    + document.querySelectorAll('[id^="trigger-dot-"]').length;
  var badge = btn.querySelector('span');
  if (count > 0) {
    if (!badge) {
      badge = document.createElement('span');
      badge.className = 'ml-1 text-xs bg-slate-600 px-1.5 py-0.5 rounded-full';
      btn.appendChild(badge);
    }
    badge.textContent = count;
  } else if (badge) {
    badge.remove();
  }
}

// ---------------------------------------------------------------------------
// Workers tab live updates
// ---------------------------------------------------------------------------
const finalFetchDone = new Set();

const STATUS_DOT_COLORS = {
  running: 'bg-blue-500', completed: 'bg-green-500',
  failed: 'bg-red-500', cancelled: 'bg-slate-500', idle: 'bg-slate-500',
};

function updateWorkerStatus(threadId, status) {
  const dot = document.getElementById('thread-dot-' + threadId);
  const text = document.getElementById('thread-status-' + threadId);
  if (!dot || !text) return;
  dot.className = 'w-2 h-2 rounded-full flex-shrink-0 ' + (STATUS_DOT_COLORS[status] || 'bg-slate-500');
  // preserve timestamp portion if present
  const cur = text.textContent;
  const dotIdx = cur.indexOf(' · ');
  const suffix = dotIdx !== -1 ? cur.substring(dotIdx) : '';
  text.textContent = status + suffix;
  const cancelBtn = document.getElementById('cancel-btn-' + threadId);
  if (cancelBtn) cancelBtn.style.display = status === 'running' ? '' : 'none';

  // If worker transitions back to running, allow a future final fetch
  if (status === 'running') {
    finalFetchDone.delete(threadId);
    return;
  }

  // When worker finishes, stop auto-poll and do one final fetch
  stopThreadPoll(threadId);
  if (!finalFetchDone.has(threadId)) {
    finalFetchDone.add(threadId);
    loadedThreads.delete(threadId);
    loadedEventCounts.delete(threadId);
    // If detail is currently expanded, do a final fetch
    const detail = document.getElementById('thread-detail-' + threadId);
    if (detail && !detail.classList.contains('hidden')) {
      fetchAndRenderEvents(threadId, SESSION_ID).catch(e => console.warn('Final event fetch failed:', e));
    }
  }
}

function addWorkerCard(threadId, description, createdAt, backend, descriptionFullLen) {
  const container = document.getElementById('tab-workers');
  if (!container) return;
  // Remove placeholder if present
  document.getElementById('no-workers-placeholder')?.remove();
  // Don't add duplicate
  if (document.getElementById('thread-dot-' + threadId)) return;
  const card = document.createElement('div');
  card.className = WORKER_CARD_CLASS;
  card.innerHTML = workerCardBodyHtml({
    id: threadId, status: 'running', description: description,
    created_at: createdAt || new Date().toISOString(), backend: backend,
    description_full_len: descriptionFullLen,
  }, SESSION_ID);
  container.prepend(card);
  updateWorkersTabBadge();
}

// ---------------------------------------------------------------------------
// Trigger card functions
// ---------------------------------------------------------------------------

function addTriggerCard(triggerId, message, fireAt, createdAt, status) {
  const container = document.getElementById('tab-workers');
  if (!container) return;
  document.getElementById('no-workers-placeholder')?.remove();
  if (document.getElementById('trigger-dot-' + triggerId)) return;

  const chrome = triggerStatusChrome(status);

  const card = document.createElement('div');
  card.className = triggerCardClass(chrome.border);
  card.id = 'trigger-card-' + triggerId;
  card.innerHTML = triggerCardBodyHtml(triggerId, status, message, fireAt, SESSION_ID);
  container.appendChild(card);
  updateWorkersTabBadge();
}

function updateTriggerStatus(triggerId, status) {
  const icon = document.getElementById('trigger-dot-' + triggerId);
  const text = document.getElementById('trigger-status-' + triggerId);
  const card = document.getElementById('trigger-card-' + triggerId);
  if (!icon) return;

  const chrome = triggerStatusChrome(status);
  const iconClassName = 'w-4 h-4 flex-shrink-0 ' + chrome.icon;
  // SVG: className is a read-only SVGAnimatedString; the string compare and
  // assignment must go through the attribute, or a poll update never paints.
  if (icon.getAttribute('class') !== iconClassName) icon.setAttribute('class', iconClassName);

  if (card) {
    card.className = triggerCardClass(chrome.border);
  }

  if (text) {
    const fireAt = text.dataset.fireAt || '';
    if (status === 'cancelled') {
      text.textContent = 'cancelled';
    } else if (fireAt) {
      text.textContent = formatTriggerTimeLabel(status, fireAt);
    }
  }

  // Hide cancel button for non-pending
  if (status !== 'pending' && card) {
    const cancelBtn = card.querySelector('button[title="Cancel trigger"]');
    if (cancelBtn) cancelBtn.style.display = 'none';
  }

  // Add strikethrough for cancelled
  if (status === 'cancelled' && card) {
    const msg = card.querySelector('p.text-sm');
    if (msg) { msg.classList.add('line-through', 'text-slate-500'); }
  }
}

function cancelTrigger(triggerId, sessionId) {
  fetch('/api/internal/triggers/' + sessionId + '/' + triggerId + '/cancel', {method: 'POST'})
    .then(r => { if (r.ok) updateTriggerStatus(triggerId, 'cancelled'); })
    .catch(err => console.error('Cancel trigger failed:', err));
}


Object.assign(Sidebar, {
  fetchWorkerDescription,
  renderWorkersTab,
  renderWorkersTabUnknown,
  restartWorkersPolling,
  ensureWorkersLoadedForActiveSession,
  pollWorkers,
  updateWorkersTabBadge,
  updateWorkerStatus,
  addWorkerCard,
  updateTriggerStatus,
  cancelTrigger,
});
Sidebar.expose([
  'fetchWorkerDescription',
  'renderWorkersTab',
  'renderWorkersTabUnknown',
  'restartWorkersPolling',
  'ensureWorkersLoadedForActiveSession',
  'pollWorkers',
  'updateWorkersTabBadge',
  'updateWorkerStatus',
  'addWorkerCard',
  'updateTriggerStatus',
  'cancelTrigger',
]);

})();
