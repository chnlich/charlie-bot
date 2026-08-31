// ---------------------------------------------------------------------------
// Auth: global fetch wrapper to attach Bearer token and handle 401s
// ---------------------------------------------------------------------------
const _origFetch = window.fetch;
window.fetch = function(url, opts = {}) {
  const key = localStorage.getItem('charliebot_access_key');
  if (key) {
    opts.headers = { ...(opts.headers || {}), 'Authorization': 'Bearer ' + key };
  }
  return _origFetch.call(window, url, opts).then(res => {
    if (res.status === 401) { showAuthOverlay(); }
    return res;
  });
};

function showAuthOverlay() {
  const el = document.getElementById('auth-overlay');
  if (el) el.style.display = 'flex';
}

// Shared Content-Type for fetches that send a JSON body through the wrapper
// above. index.html loads this file before every consumer; diff.html and the
// artifact iframe pages do not, so their copies stay local there.
const JSON_HEADERS = { 'Content-Type': 'application/json' };

// WebSocket endpoints take the access key as a 'token' query param: the
// browser WebSocket API exposes no header channel for the fetch wrapper's
// Bearer header. Loads before the websocket/voice/terminal connectors.
function withAccessToken(url) {
  const key = localStorage.getItem('charliebot_access_key');
  return key ? url + '?token=' + encodeURIComponent(key) : url;
}

// The socket scheme must track the page scheme: an https page cannot open a
// plain ws: socket (browser mixed-content rule).
function wsUrlWithToken(path) {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return withAccessToken(`${proto}//${location.host}${path}`);
}

function hideAuthOverlay() {
  const el = document.getElementById('auth-overlay');
  if (el) el.style.display = 'none';
}

// Mirror the localStorage key into a cookie so top-level browser navigations
// (which carry no Authorization header) authenticate. localStorage stays the
// source of truth for the fetch wrapper / WS; the cookie only serves navigations.
// SameSite=Strict closes the CSRF surface cookie auth would otherwise open;
// Secure is appropriate since the server is reached only over HTTPS (Tailscale).
function writeAccessCookie(key) {
  document.cookie = 'charliebot_access_key=' + key + '; path=/; SameSite=Strict; Secure';
}

function submitAccessKey() {
  const input = document.getElementById('auth-key-input');
  const key = (input && input.value || '').trim();
  if (!key) return;
  localStorage.setItem('charliebot_access_key', key);
  writeAccessCookie(key);
  // Reload so all connections use the new key. If invalid, 401 will re-show the overlay.
  hideAuthOverlay();
  location.reload();
}

function initAuth() {
  if (typeof AUTH_ENABLED === 'undefined' || !AUTH_ENABLED) return;
  const key = localStorage.getItem('charliebot_access_key');
  if (key) {
    // Already-authenticated users get the cookie automatically so navigations start passing.
    writeAccessCookie(key);
  } else {
    showAuthOverlay();
  }
}

// ---------------------------------------------------------------------------
// Config (non-Jinja2 parts; SESSION_ID, DRAFT_KEY, THINKING_SINCE,
// eventCursor, BACKEND_OPTIONS are injected inline by index.html)
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
