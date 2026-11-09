// ---------------------------------------------------------------------------
// Offline shell worker: the chat page's shell, the same-origin files it names,
// and stored copies of the first screen and the sidebar list.
// ---------------------------------------------------------------------------
// The server prepends `WORKER_CONFIG = {version, enabled}` (routes/pages.rs). With `enabled` false
// this file is the retired form: it empties the worker's caches and unregisters itself.
//
// Interception is limited to the GETs listed below. Everything else (POSTs, the socket, other
// /api/* requests) is not answered here and reaches the network unchanged.
//
// Storage: COPY_CACHE holds the shell under SHELL_KEY, the copies of the first screen and the
// sidebar list, and an index of session ids with their last open time. Its name carries the
// release version, so a new release starts with an empty copy store. FILE_CACHE holds the shell's
// same-origin files under their own versioned paths; a file that did not change keeps its path, so
// a release does not fetch it again.

const COPY_PREFIX = 'last-state-';
const COPY_CACHE = COPY_PREFIX + WORKER_CONFIG.version;
const FILE_CACHE = COPY_PREFIX + 'files';
const SHELL_KEY = self.location.origin + '/shell';
const INDEX_KEY = self.location.origin + '/index';
const STORED_AT_HEADER = 'X-CharlieBot-Stored-At';
const NETWORK_ONLY_HEADER = 'X-CharlieBot-Network-Only';
// How long the network may take before a stored copy answers a first screen or a list. The
// page's first paint budget is 1 s: navigation start to the request, plus rendering the message,
// plus a 0.1 s margin. Until the bench measures those on the reader's device, the cap is 0.25 s.
const WAIT_CAP_MS = 250;

const SHELL_STATIC_REF = /<(?:link|script)\b[^>]*?\b(?:href|src)="(\/static\/[^"]+)"/g;

self.addEventListener('install', (event) => {
  event.waitUntil(self.skipWaiting());
});

self.addEventListener('activate', (event) => {
  event.waitUntil(WORKER_CONFIG.enabled ? activateCurrent() : retire());
});

async function activateCurrent() {
  for (const name of await caches.keys()) {
    if (name.startsWith(COPY_PREFIX) && name !== COPY_CACHE && name !== FILE_CACHE) {
      await caches.delete(name);
    }
  }
  // The pages this load opened stay their own: a worker that claims a page mid-load reroutes the rest of
  // that load through itself. The next navigation in the worker's scope is controlled without a claim.
}

async function retire() {
  for (const name of await caches.keys()) {
    if (name.startsWith(COPY_PREFIX)) await caches.delete(name);
  }
  await self.clients.claim();
  await self.registration.unregister();
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

self.addEventListener('fetch', (event) => {
  if (!WORKER_CONFIG.enabled) return;
  const request = event.request;
  if (request.method !== 'GET') return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  if (url.pathname === '/' && url.searchParams.has('session') && request.mode === 'navigate') {
    event.respondWith(answerShell(request));
  } else if (url.pathname.startsWith('/static/')) {
    event.respondWith(answerFile(request));
  } else if (isFirstScreenPath(url.pathname) || isSidebarListRequest(url)) {
    event.respondWith(answerCopy(event, request));
  }
});

function isFirstScreenPath(pathname) {
  return /^\/api\/sessions\/[^/]+\/bootstrap$/.test(pathname);
}

// The sidebar's full listing, which carries no `ids`: rows fetched by id are a refresh of rows already on
// the page and go to the network.
function isSidebarListRequest(url) {
  return url.pathname === '/api/sessions/' && !url.searchParams.has('ids');
}

// The navigation's shell: the stored one, or the network's full page on the first load (no shell stored yet).
async function answerShell(request) {
  const cache = await caches.open(COPY_CACHE);
  const shell = await cache.match(SHELL_KEY);
  if (shell) return shell;
  return fetch(request);
}

// A same-origin file of the shell: the stored file, or the network when it is not stored.
async function answerFile(request) {
  const cache = await caches.open(FILE_CACHE);
  const stored = await cache.match(request);
  if (stored) return stored;
  return fetch(request);
}

// A first screen or a sidebar list. Without a stored copy the network answers, uncapped. With one, the
// network answers if it gives a 200 within the wait cap; a network error, a 5xx or the cap's end gives the
// copy, and the network's answer still updates the copy in the background.
async function answerCopy(event, request) {
  const cache = await caches.open(COPY_CACHE);
  const networkOnly = request.headers.get(NETWORK_ONLY_HEADER) === '1';
  const copy = networkOnly ? undefined : await cache.match(request.url);
  const settled = observe(fetch(request));
  if (!copy) {
    const answer = await settled;
    if (answer.error) throw answer.error;
    event.waitUntil(afterNetworkAnswer(request, answer));
    return answer.response;
  }
  const answered = await Promise.race([settled, waitCap(WAIT_CAP_MS)]);
  event.waitUntil(settled.then((answer) => afterNetworkAnswer(request, answer)));
  if (answered.response && answered.response.status < 500) return answered.response;
  return copy;
}

// The network's answer as the worker sees it: the response, a clone to store when it is a 200 (cloned here,
// before the page can read the body), or the error of a request that never answered.
function observe(network) {
  return network.then(
      (response) => ({response, stored: response.ok ? response.clone() : null}),
      (error) => ({error}));
}

function waitCap(ms) {
  return new Promise((resolve) => setTimeout(() => resolve({}), ms));
}

// A 200 is stored, a 401 drops every copy, and any other answer leaves the copies as they are.
async function afterNetworkAnswer(request, answer) {
  if (answer.error) return;
  if (answer.response.status === 401) {
    await dropCopies();
  } else if (answer.stored) {
    await storeCopy(request, answer.stored);
  }
}

// ---------------------------------------------------------------------------
// Stored copies
// ---------------------------------------------------------------------------

function sessionIdOf(url) {
  const match = /^\/api\/sessions\/([^/]+)\/bootstrap$/.exec(url.pathname);
  return match ? decodeURIComponent(match[1]) : null;
}

async function storeCopy(request, response) {
  const url = new URL(request.url);
  const body = await response.arrayBuffer();
  const sessionId = sessionIdOf(url);
  const cache = await caches.open(COPY_CACHE);
  for (;;) {
    const headers = new Headers(response.headers);
    headers.set(STORED_AT_HEADER, new Date().toISOString());
    const entry = new Response(body, {status: response.status, statusText: response.statusText, headers});
    try {
      await cache.put(request.url, entry);
      break;
    } catch (error) {
      if (error.name !== 'QuotaExceededError') throw error;
      if (!await evictOldestSession(cache, sessionId)) throw error;
    }
  }
  if (sessionId !== null) await touchSession(cache, sessionId);
}

async function readIndex(cache) {
  const stored = await cache.match(INDEX_KEY);
  return stored ? stored.json() : {sessions: {}, files: []};
}

async function writeIndex(cache, index) {
  await cache.put(INDEX_KEY, new Response(JSON.stringify(index), {headers: {'Content-Type': 'application/json'}}));
}

async function touchSession(cache, sessionId) {
  const index = await readIndex(cache);
  index.sessions[sessionId] = Date.now();
  await writeIndex(cache, index);
}

// Quota is short: the session opened longest ago loses its first screen. The session being stored is not
// evicted. Returns false when no other session is stored.
async function evictOldestSession(cache, keepSessionId) {
  const index = await readIndex(cache);
  const candidates = Object.entries(index.sessions).filter(([id]) => id !== keepSessionId);
  if (candidates.length === 0) return false;
  candidates.sort((a, b) => a[1] - b[1]);
  const [oldestId] = candidates[0];
  await cache.delete(new URL('/api/sessions/' + oldestId + '/bootstrap', self.location.origin).href);
  delete index.sessions[oldestId];
  await writeIndex(cache, index);
  return true;
}

// Every copy goes; the shell stays, since it holds no session's data.
async function dropCopies() {
  const cache = await caches.open(COPY_CACHE);
  for (const request of await cache.keys()) {
    if (request.url !== SHELL_KEY && request.url !== INDEX_KEY) await cache.delete(request);
  }
  await writeIndex(cache, {sessions: {}, files: []});
}

// ---------------------------------------------------------------------------
// The shell, after the page has loaded
// ---------------------------------------------------------------------------

// The shell's same-origin references, as the server rendered them.
function shellStaticRefs(html) {
  const refs = [];
  for (const match of html.matchAll(SHELL_STATIC_REF)) refs.push(match[1]);
  return [...new Set(refs)];
}

// Fetches the shell and the files it names, stores them, and drops the stored files it no longer names.
// A shell this release already stored stays: the fetch it needs happens once per release, on the first
// open after it (the cache name carries the version), and once more after a 4409 drops the shell.
async function warmShell() {
  const cache = await caches.open(COPY_CACHE);
  if (await cache.match(SHELL_KEY)) return;
  const response = await fetch('/?session=', {cache: 'no-store', credentials: 'same-origin'});
  if (response.status === 401) {
    await dropCopies();
    return;
  }
  if (!response.ok) throw new Error('shell fetch failed: ' + response.status);
  const html = await response.text();
  await cache.put(SHELL_KEY, new Response(html, {headers: {'Content-Type': 'text/html; charset=utf-8'}}));
  const refs = shellStaticRefs(html);
  const files = await caches.open(FILE_CACHE);
  for (const ref of refs) {
    if (await files.match(ref)) continue;
    const file = await fetch(ref, {credentials: 'same-origin'});
    if (!file.ok) throw new Error('shell file fetch failed: ' + ref + ' ' + file.status);
    if (file.headers.get('ETag')) await files.put(ref, file);
  }
  for (const request of await files.keys()) {
    const path = new URL(request.url).pathname + new URL(request.url).search;
    if (!refs.includes(path)) await files.delete(request);
  }
  const index = await readIndex(cache);
  index.files = refs;
  await writeIndex(cache, index);
}

// The page's own stored shell goes before its reload after a 4409 close: the reload then reads the
// current release's page from the network.
async function dropShell() {
  const cache = await caches.open(COPY_CACHE);
  await cache.delete(SHELL_KEY);
}

self.addEventListener('message', (event) => {
  const data = event.data || {};
  if (!WORKER_CONFIG.enabled) return;
  if (data.type === 'warm') {
    event.waitUntil(warmShell().then(
        () => replyTo(event, {stored: true}),
        (error) => replyTo(event, {stored: false, error: String(error)})));
  } else if (data.type === 'drop-shell') {
    event.waitUntil(dropShell().then(() => replyTo(event, {done: true})));
  } else if (data.type === 'drop-copies') {
    event.waitUntil(dropCopies());
  } else {
    throw new Error('unknown worker message: ' + data.type);
  }
});

function replyTo(event, message) {
  if (event.ports && event.ports[0]) event.ports[0].postMessage(message);
}
