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
// The wait before a copy is read again. A worker that just started can match before the cache's disk index has
// settled, so a copy's first match may miss although the copy is stored; one wait cap later the match answers. A
// first screen's network race re-reads at each cap, and a sidebar list re-reads once before it asks the network.
const WAIT_CAP_MS = 400;

const SHELL_STATIC_REF = /<(?:link|script)\b[^>]*?\b(?:href|src)="(\/static\/[^"]+)"/g;

self.addEventListener('install', (event) => {
  event.waitUntil(self.skipWaiting());
});

self.addEventListener('activate', (event) => {
  event.waitUntil(WORKER_CONFIG.enabled ? activateCurrent() : retire());
});

async function activateCurrent() {
  // With navigation preload on, the navigation's network request carries Service-Worker-Navigation-Preload. The chat page
  // answers that request with no body (routes/pages.rs): this worker answers each stored-shell navigation from its copy
  // and never reads the preload response.
  await self.registration.navigationPreload.enable();
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
    event.respondWith(answerFile(event, request));
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

// The navigation's shell: the stored one, or the network's full page on the first load (no shell stored
// yet). A worker that just started can match before the cache's disk index has settled, while the same
// cache answers a moment later, so a miss re-reads twice before it falls back to the network.
async function answerShell(request) {
  const cache = await caches.open(COPY_CACHE);
  for (let attempt = 0; attempt < 3; attempt++) {
    const shell = await cache.match(SHELL_KEY);
    if (shell) return shell;
    if (attempt < 2) await waitCap(WAIT_CAP_MS);
  }
  return fetch(request);
}

// A same-origin file of the shell: the stored file, or an ETag-backed network answer that the next open can reuse.
async function answerFile(event, request) {
  const cache = await caches.open(FILE_CACHE);
  const stored = await cache.match(request);
  if (stored) return stored;
  const response = await fetch(request);
  if (response.ok && response.headers.get('ETag')) event.waitUntil(cache.put(request, response.clone()));
  return response;
}

// A first screen or a sidebar list. A stored copy answers at once and sends no network request: the copy's event
// count is where the page's catch-up starts, and the network's answer replaces the copy at the catch-up's end
// (queueCopyRefresh). A first screen whose first match misses races the network against the copy's re-read at each
// wait cap: the network answers if it gives a usable 200 first; otherwise the copy answers and the network fetch is
// aborted there. A sidebar list is not raced. Its copy is re-read once after the wait cap, and the network answers
// only when that read misses too: a list the copy would beat is a fetch aborted mid-body, and the bytes it already
// put on the link delay the requests that follow it. Without a stored copy the network answers.
async function answerCopy(event, request) {
  const cache = await caches.open(COPY_CACHE);
  if (request.headers.get(NETWORK_ONLY_HEADER) === '1') {
    const response = await networkAnswer(event, request);
    if (response.ok || response.status === 401) refreshQueue.delete(request.url);
    return response;
  }
  if (!isFirstScreenPath(new URL(request.url).pathname)) {
    const copy = await cache.match(request.url) ?? await rereadAfterWaitCap(cache, request.url);
    if (copy) {
      queueCopyRefresh(request.url);
      return copy;
    }
    return networkAnswer(event, request);
  }
  const copy = await cache.match(request.url);
  if (copy) {
    queueCopyRefresh(request.url);
    return copy;
  }
  const controller = new AbortController();
  let networkSettled = false;
  const settled = observe(fetch(request, {signal: controller.signal}).finally(() => { networkSettled = true; }));
  for (;;) {
    const answered = await Promise.race([settled, waitCap(WAIT_CAP_MS)]);
    // A 304 names no body: the network's state matches what the browser's HTTP cache held, which is
    // what the stored copy carries. The page reads the copy.
    if (answered.response && answered.response.status < 500 && answered.response.status !== 304) {
      event.waitUntil(settled.then((result) => afterNetworkAnswer(request, result)));
      return answered.response;
    }
    const copy = await cache.match(request.url);
    if (copy) {
      controller.abort();
      queueCopyRefresh(request.url);
      return copy;
    }
    if (networkSettled) {
      return networkAnswer(event, request, await settled);
    }
  }
}

async function rereadAfterWaitCap(cache, url) {
  await waitCap(WAIT_CAP_MS);
  return cache.match(url);
}

// The abortable fetch of the first screen's race keeps its controller in answerCopy, where the race can stop it.

// One network answer for a request the worker sends to the network: a bodyless 304 — the browser's HTTP
// cache held the entry and the revalidation matched, but a request the worker answers carries no cached
// body the page could read — is fetched once more past the cache and without the conditional header, a
// 200 is stored in the background, and a 401 drops every copy.
async function networkAnswer(event, request, settledAnswer = null) {
  let answer = settledAnswer ?? await observe(fetch(request));
  if (!answer.error && answer.response.status === 304) {
    const headers = new Headers(request.headers);
    headers.delete('If-None-Match');
    answer = await observe(fetch(request.url, {headers, cache: 'no-store'}));
  }
  if (answer.error) throw answer.error;
  event.waitUntil(afterNetworkAnswer(request, answer));
  return answer.response;
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
    await storeCopy(request.url, answer.stored);
  }
}

// The copies the opens answered from, whose network refresh waits for the page's catch-up to complete.
// The page reports that with the catchup-complete message (offline-shell.js); the refresh then shares
// the link with nothing the page still needs.
const refreshQueue = new Set();

function queueCopyRefresh(url) {
  refreshQueue.add(url);
}

// Refreshes the queued copies from the network and drops every copy on a 401. An answer that carries no
// body (a 304) or fails leaves its copy as it is; an error also keeps the queue, so the copies the page
// read while offline are refreshed after the next catch-up.
async function refreshCopies() {
  for (const url of refreshQueue) {
    const cache = await caches.open(COPY_CACHE);
    if (await cache.match(url) === undefined) continue;
    const answer = await observe(fetch(url, {credentials: 'same-origin'}));
    if (answer.error) return;
    if (answer.response.status === 401) {
      refreshQueue.clear();
      await dropCopies();
      return;
    }
    if (answer.stored) await storeCopy(url, answer.stored);
  }
  refreshQueue.clear();
}

// ---------------------------------------------------------------------------
// Stored copies
// ---------------------------------------------------------------------------

function sessionIdOf(url) {
  const match = /^\/api\/sessions\/([^/]+)\/bootstrap$/.exec(url.pathname);
  return match ? decodeURIComponent(match[1]) : null;
}

async function storeCopy(url, response) {
  const body = await response.arrayBuffer();
  const sessionId = sessionIdOf(new URL(url));
  const cache = await caches.open(COPY_CACHE);
  for (;;) {
    const headers = new Headers(response.headers);
    headers.set(STORED_AT_HEADER, new Date().toISOString());
    const entry = new Response(body, {status: response.status, statusText: response.statusText, headers});
    try {
      await cache.put(url, entry);
      break;
    } catch (error) {
      if (error.name !== 'QuotaExceededError') throw error;
      if (!await evictOldestSession(cache, sessionId)) throw error;
    }
  }
  if (sessionId !== null) await touchSession(cache, sessionId);
}

async function storePageCopies(copies) {
  for (const copy of copies) {
    const response = new Response(JSON.stringify(copy.body), {
      headers: {'Content-Type': 'application/json'},
    });
    await storeCopy(copy.url, response);
  }
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
// A refresh leaves the current shell in place until the replacement and its files are ready.
async function warmShell(refresh = false) {
  const cache = await caches.open(COPY_CACHE);
  if (!refresh && await cache.match(SHELL_KEY)) return {stored: true};
  const response = await fetch('/?session=', {cache: 'no-store', credentials: 'same-origin'});
  if (response.status === 401) {
    if (refresh) await dropShell();
    await dropCopies();
    return refresh ? {stored: false, unauthorized: true} : {stored: false};
  }
  if (!response.ok) throw new Error('shell fetch failed: ' + response.status);
  const html = await response.text();
  const refs = shellStaticRefs(html);
  const files = await caches.open(FILE_CACHE);
  const fetchedFiles = [];
  for (const ref of refs) {
    if (await files.match(ref)) continue;
    // The page loaded these files a moment ago, so the browser's HTTP cache holds their bodies. only-if-cached reads
    // the cache and never asks the network, so the warm costs the link nothing and opens no connection during a
    // switch. A file the cache does not hold fails the request with a TypeError, and is not stored.
    let file;
    try {
      file = await fetch(ref, {credentials: 'same-origin', cache: 'only-if-cached', mode: 'same-origin'});
    } catch (error) {
      if (error.name === 'TypeError') continue;
      throw error;
    }
    if (file.status === 401) {
      if (refresh) await dropShell();
      await dropCopies();
      return refresh ? {stored: false, unauthorized: true} : {stored: false};
    }
    if (!file.ok) throw new Error('shell file fetch failed: ' + ref + ' ' + file.status);
    if (file.headers.get('ETag')) fetchedFiles.push([ref, file]);
  }
  for (const [ref, file] of fetchedFiles) await files.put(ref, file);
  for (const request of await files.keys()) {
    const path = new URL(request.url).pathname + new URL(request.url).search;
    if (!refs.includes(path)) await files.delete(request);
  }
  const index = await readIndex(cache);
  index.files = refs;
  await writeIndex(cache, index);
  await cache.put(SHELL_KEY, new Response(html, {headers: {'Content-Type': 'text/html; charset=utf-8'}}));
  return {stored: true};
}

// Called only after a refresh answers 401, so navigation reaches the server instead of using stale HTML.
async function dropShell() {
  const cache = await caches.open(COPY_CACHE);
  await cache.delete(SHELL_KEY);
}

self.addEventListener('message', (event) => {
  const data = event.data || {};
  if (!WORKER_CONFIG.enabled) return;
  if (data.type === 'warm') {
    const warming = data.refresh === true
      ? warmShell(true)
      : warmShell().then(async (result) => {
        if (result.stored) await storePageCopies(data.copies);
        return result;
      });
    event.waitUntil(warming.then(
        (result) => replyTo(event, result),
        (error) => replyTo(event, {stored: false, error: String(error)})));
  } else if (data.type === 'catchup-complete') {
    event.waitUntil(refreshCopies());
  } else if (data.type === 'drop-copies') {
    event.waitUntil(dropCopies());
  } else {
    throw new Error('unknown worker message: ' + data.type);
  }
});

function replyTo(event, message) {
  if (event.ports && event.ports[0]) event.ports[0].postMessage(message);
}
