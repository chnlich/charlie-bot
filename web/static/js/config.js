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
