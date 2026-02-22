// ---------------------------------------------------------------------------
// Config (non-Jinja2 parts; SESSION_ID, DRAFT_KEY, THINKING_SINCE,
// eventCursor, usageTotalCost, BACKEND_OPTIONS are injected inline by index.html)
// ---------------------------------------------------------------------------
let _draftTimer = null;
function saveDraft() {
  if (!DRAFT_KEY) return;
  clearTimeout(_draftTimer);
  _draftTimer = setTimeout(() => {
    const v = document.getElementById('msg-input').value;
    if (v) localStorage.setItem(DRAFT_KEY, v);
    else localStorage.removeItem(DRAFT_KEY);
  }, 300);
}
let masterThinking = !!THINKING_SINCE;

// ---------------------------------------------------------------------------
// Backend switcher
// ---------------------------------------------------------------------------
async function switchBackend(id) {
  if (!SESSION_ID) return;
  const select = document.getElementById('backend-select');
  const prevId = select ? select.dataset.current : id;
  if (select) select.disabled = true;
  try {
    const res = await fetch(`/api/chat/${SESSION_ID}/backend`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ backend: id }),
    });
    if (!res.ok) {
      const err = await res.json().catch(() => ({}));
      showToast('Switch failed: ' + (err.detail || res.status), true);
      if (select) select.value = prevId;  // revert to previous selection
      return;
    }
    if (select) select.dataset.current = id;
    showToast('Switched to ' + (BACKEND_OPTIONS[id] || id));
  } catch (err) {
    console.error('switchBackend failed:', err);
    showToast('Switch failed: ' + err.message, true);
    if (select) select.value = prevId;
  } finally {
    if (select) select.disabled = masterThinking;
  }
}

function showToast(msg, isError) {
  const existing = document.getElementById('backend-toast');
  if (existing) existing.remove();
  const toast = document.createElement('div');
  toast.id = 'backend-toast';
  toast.textContent = msg;
  toast.className = 'fixed bottom-6 left-1/2 -translate-x-1/2 px-4 py-2 rounded-lg text-xs font-medium shadow-lg z-50 transition-opacity '
    + (isError ? 'bg-red-700 text-red-100' : 'bg-slate-700 text-slate-100');
  document.body.appendChild(toast);
  setTimeout(() => { toast.style.opacity = '0'; setTimeout(() => toast.remove(), 300); }, 2000);
}
