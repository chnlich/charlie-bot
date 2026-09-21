// ---------------------------------------------------------------------------
// Auth: global fetch wrapper to attach Bearer token and handle 401s
// ---------------------------------------------------------------------------
// The access key's one browser name: the localStorage key the fetch wrapper and
// the WS token read, and the cookie name a top-level navigation authenticates
// with. The server middleware reads the cookie by the same name
// (src/api/auth.py _ACCESS_KEY_COOKIE); the served login page keeps its own
// literal because it is HTML, not this file.
const ACCESS_KEY_NAME = 'charliebot_access_key';

// The one reader of the access key for request auth: the fetch wrapper below and
// the voice upload's XHR (which no wrapper patches) both send this header.
function accessTokenAuthorization() {
  const key = localStorage.getItem(ACCESS_KEY_NAME);
  return key ? 'Bearer ' + key : null;
}

const _origFetch = window.fetch;
window.fetch = function(url, opts = {}) {
  const authorization = accessTokenAuthorization();
  if (authorization) {
    opts.headers = { ...(opts.headers || {}), 'Authorization': authorization };
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

// Shared fill styling for every horizontal meter bar on index.html (sidebar
// context bar, ext-usage quota bars): one literal keeps the bars painting
// alike. index.html loads this file before both consumers; the static
// #usage-bar markup renders before any script, so its copy stays local there.
const PROGRESS_BAR_FILL_CLASS = 'h-full rounded-full transition-all duration-300';

// WebSocket endpoints take the access key as a 'token' query param: the
// browser WebSocket API exposes no header channel for the fetch wrapper's
// Bearer header. Loads before the websocket/voice/terminal connectors.
function withAccessToken(url) {
  const key = localStorage.getItem(ACCESS_KEY_NAME);
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
  document.cookie = ACCESS_KEY_NAME + '=' + key + '; path=/; SameSite=Strict; Secure';
}

function submitAccessKey() {
  const input = document.getElementById('auth-key-input');
  const key = (input && input.value || '').trim();
  if (!key) return;
    localStorage.setItem(ACCESS_KEY_NAME, key);
  writeAccessCookie(key);
  // Reload so all connections use the new key. If invalid, 401 will re-show the overlay.
  hideAuthOverlay();
  location.reload();
}

function initAuth() {
  if (typeof AUTH_ENABLED === 'undefined' || !AUTH_ENABLED) return;
  const key = localStorage.getItem(ACCESS_KEY_NAME);
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

// The one writer of the msg-input draft: every switch path and the debounced
// input handler flush through here, keyed by the live DRAFT_KEY.
function saveDraftNow() {
  if (!DRAFT_KEY) return;
  const v = document.getElementById('msg-input').value;
  if (v) localStorage.setItem(DRAFT_KEY, v);
  else localStorage.removeItem(DRAFT_KEY);
}

function saveDraft() {
  clearTimeout(_draftTimer);
  _draftTimer = setTimeout(saveDraftNow, 300);
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
