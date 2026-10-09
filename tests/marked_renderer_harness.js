// ---------------------------------------------------------------------------
// Marked harness shared by the chat markdown vm tests and the stream
// collectors: loads the page's real marked build and renderer, and owns the
// CDN URL both fetch and the retrying fetch they share — import them instead
// of restating them.
// ---------------------------------------------------------------------------
const vm = require('node:vm');
const https = require('node:https');

const { readStatic } = require('./read_static');
const { buildRendererContext } = require('./renderer_vm_context');

// Fetch the exact marked build the browser serves (web/templates/index.html).
// No version is pinned, so resolving the range today and caching it keeps the
// tests stable while tracking whatever marked ships.
const MARKED_URL = 'https://cdn.jsdelivr.net/npm/marked/marked.min.js';

// A CDN request fails now and then for reasons outside the repo (a dropped TLS
// handshake, a 5xx from an edge node), and one failure fails the whole test
// process. A network error or a 5xx status therefore gets two more attempts;
// any other status is final, and so is the last failed attempt.
const FETCH_ATTEMPTS = 3;
const FETCH_RETRY_DELAY_MS = 500;

// One GET: resolves { status, body } for any response, rejects on a network
// error. A connection cut mid-body emits 'error' on res only when a listener
// exists; without one 'end' never fires and the promise never settles.
function getOnce(url) {
  return new Promise((resolve, reject) => {
    https.get(url, (res) => {
      let body = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => { body += chunk; });
      res.on('end', () => resolve({ status: res.statusCode, body }));
      res.on('error', reject);
    }).on('error', reject);
  });
}

// The body of the 200 response from url. Throws one error that names the url
// and every failed attempt: there is no offline fallback, so an unreachable
// CDN fails the test loudly. A non-200 body is never returned.
async function fetchUrl(url) {
  const failures = [];
  for (let attempt = 1; ; attempt++) {
    let failure;
    let retryable;
    try {
      const { status, body } = await getOnce(url);
      if (status === 200) return body;
      failure = `HTTP ${status}`;
      retryable = status >= 500;
    } catch (err) {
      failure = err.message;
      retryable = true;
    }
    failures.push(`attempt ${attempt}: ${failure}`);
    if (!retryable || attempt === FETCH_ATTEMPTS) break;
    await new Promise((resolve) => setTimeout(resolve, FETCH_RETRY_DELAY_MS));
  }
  throw new Error(`could not fetch ${url}: ${failures.join('; ')}`);
}

// The promise is cached, a rejection included: an outage costs one
// three-attempt cycle per process, not one per test in the file.
let markedSrcPromise = null;
function loadMarkedSrc() {
  if (!markedSrcPromise) markedSrcPromise = fetchUrl(MARKED_URL);
  return markedSrcPromise;
}

// Load the REAL markdown-renderer.js against the REAL marked in a shared vm
// context, mirroring the browser page order: marked.min.js defines the global
// marked first, then markdown-renderer.js registers its renderer + tokenizer
// via marked.use. The renderer-context base stubs the non-marked globals
// (console, hljs, document): markdown-renderer.js touches hljs and document,
// and the marked build logs its error paths through console. A caller-passed
// hljs replaces the stub so tests can count or shape highlight calls.
async function loadRendererContext(hljs) {
  const markedSrc = await loadMarkedSrc();
  const context = buildRendererContext();
  if (hljs) context.hljs = hljs;
  vm.createContext(context);
  vm.runInContext(markedSrc, context, { filename: 'marked.min.js' });
  // The page's load order: math-scanner.js defines the mathSpan global the
  // renderer's math tokenizer reads.
  vm.runInContext(readStatic('math-scanner.js'), context, { filename: 'math-scanner.js' });
  const src = readStatic('markdown-renderer.js');
  vm.runInContext(src, context, { filename: 'markdown-renderer.js' });
  return context;
}

// The marked object of that same load, for tests that only parse.
async function loadRenderer(hljs) {
  return (await loadRendererContext(hljs)).marked;
}

// The same marked build with no repo renderer loaded, so its tokenizer is
// pristine stock: the control for tokenizer comparisons against loadRenderer().
async function loadStockMarked() {
  const markedSrc = await loadMarkedSrc();
  const context = { console };
  vm.createContext(context);
  vm.runInContext(markedSrc, context, { filename: 'marked.min.js' });
  return context.marked;
}

// One streaming paint over a loadRendererContext() context, the exact parse
// window usage.js's paintStreamDraft opens: streamPaintCodeTokens filled by
// parseStreamDraft's own token walk and closed after.
function paint(context, draft) {
  context.streamPaintCodeTokens = [];
  const html = context.parseStreamDraft(context.fixNestedFences(draft));
  context.streamPaintCodeTokens = null;
  return html;
}

// The REAL paintStreamDraft (usage.js) over the real renderer, with the chat
// seam it reads loaded in page order: namespace, then shared
// (thinkingToggleHtml, shouldAutoScroll), then scroll (restoreBottomPin).
// opts.document supplies the streaming-msg / streaming-content / messages
// elements the paint looks up; the caller installs an engine under
// Chat.TurnEngine.activeFor to drive the follow decision.
async function loadStreamPaintContext(opts) {
  const o = opts || {};
  const markedSrc = await loadMarkedSrc();
  const context = buildRendererContext();
  if (o.document) context.document = o.document;
  vm.createContext(context);
  vm.runInContext(markedSrc, context, { filename: 'marked.min.js' });
  vm.runInContext(readStatic('math-scanner.js'), context, { filename: 'math-scanner.js' });
  vm.runInContext(readStatic('markdown-renderer.js'), context, { filename: 'markdown-renderer.js' });
  vm.runInContext(readStatic('chat/namespace.js'), context, { filename: 'chat/namespace.js' });
  vm.runInContext(readStatic('chat/shared.js'), context, { filename: 'chat/shared.js' });
  vm.runInContext(readStatic('chat/scroll.js'), context, { filename: 'chat/scroll.js' });
  vm.runInContext(readStatic('usage.js'), context, { filename: 'usage.js' });
  return context;
}

module.exports = {
  MARKED_URL,
  fetchUrl,
  loadRenderer,
  loadRendererContext,
  loadStockMarked,
  paint,
  loadStreamPaintContext,
};
