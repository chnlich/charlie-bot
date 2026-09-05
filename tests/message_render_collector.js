'use strict';
// M60 collector — chat message-body markdown parse, repeat page render.
//
// Every session switch rebuilds the turn engine, so the same page's message
// bodies re-run the parse (marked + fence fix) on every re-entry and every
// repeat render. This collector loads the checkout's markdown-renderer.js with
// the page's real marked + highlight.js builds, resolves the worst on-disk
// page corpus (the 40 largest message bodies of the live chat file carrying
// the most bytes; live state read-only), and times full page passes: one cold
// pass, as at the first render after a page load, then five timed repeats —
// the session re-entry shape, identical bodies the memo serves without a
// re-parse. CHECKOUT picks the code under test; the pre-fix form (no
// renderProseMarkdown) re-parses every repeat, the post-fix form serves them
// from the memo.
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const { fetchUrl } = require('./stream_collector_common');
const { hljsStub } = require('./hljs_stub');

const CHECKOUT = process.env.CHECKOUT || path.join(__dirname, '..');
const MARKED_URL = 'https://cdn.jsdelivr.net/npm/marked/marked.min.js';
const HLJS_URL = 'https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/highlight.min.js';
const PAGE_MESSAGES = 40;

function readJs(name) {
  return fs.readFileSync(path.join(CHECKOUT, 'web/static/js', name), 'utf8');
}

// Worst page corpus: the live chat file carrying the most bytes; from it the
// largest assistant/user/worker-summary/plan bodies — the page a re-entry of
// the heaviest session re-renders.
function pageCorpus() {
  const root = path.join(process.env.HOME, '.charliebot', 'sessions');
  let best = null;
  let bestSize = -1;
  for (const d of fs.readdirSync(root)) {
    const p = path.join(root, d, 'data', 'chat_events.jsonl');
    let size;
    try {
      size = fs.statSync(p).size;
    } catch {
      continue;
    }
    if (size > bestSize) {
      best = p;
      bestSize = size;
    }
  }
  if (!best) throw new Error('no on-disk live chat file');

  const texts = [];
  for (const line of fs.readFileSync(best, 'utf8').split('\n')) {
    if (!line) continue;
    let ev;
    try {
      ev = JSON.parse(line);
    } catch {
      continue;
    }
    const type = ev.type;
    if (type === 'assistant') {
      const blocks = Array.isArray(ev.message?.content) ? ev.message.content : [];
      for (const block of blocks) {
        if (block?.type === 'text' && typeof block.text === 'string' && block.text) texts.push(block.text);
      }
    } else if ((type === 'user' || type === 'worker_summary' || type === 'plan')
        && typeof ev.content === 'string' && ev.content) {
      texts.push(ev.content);
    }
  }
  if (!texts.length) throw new Error('no message bodies in the worst live chat file');
  texts.sort((a, b) => b.length - a.length);
  return { file: best, fileSize: bestSize, page: texts.slice(0, PAGE_MESSAGES) };
}

async function loadContext(hljsSource) {
  const context = {
    console: { error() {}, warn() {}, log() {} },
    hljs: hljsStub,
    document: { querySelectorAll: () => [] },
    platform: {},
  };
  vm.createContext(context);
  vm.runInContext(hljsSource, context, { filename: 'highlight.min.js' });
  vm.runInContext(await fetchUrl(MARKED_URL), context, { filename: 'marked.min.js' });
  vm.runInContext(readJs('markdown-renderer.js'), context, { filename: 'markdown-renderer.js' });
  return context;
}

(async () => {
  const { file, fileSize, page } = pageCorpus();
  const pageBytes = page.reduce((sum, t) => sum + t.length, 0);
  const digest = crypto.createHash('sha1').update(page.join('\u0000')).digest('hex').slice(0, 12);
  const hljsSource = await fetchUrl(HLJS_URL);
  const languages = await (async () => {
    const probe = await loadContext(hljsSource);
    return probe.hljs.listLanguages().length;
  })();

  // The parse the checkout's message body path runs: the memo helper when the
  // checkout has it, the pre-fix raw expression otherwise.
  const probe = await loadContext(hljsSource);
  const memoized = typeof probe.renderProseMarkdown === 'function';
  const parse = memoized
    ? (t) => probe.renderProseMarkdown(t)
    : (t) => probe.marked.parse(probe.fixNestedFences(t));

  const pass = () => {
    const t0 = performance.now();
    let html = '';
    for (const text of page) html += parse(text);
    return { ms: performance.now() - t0, html };
  };

  const cold = pass(); // cold pass, as at the first render after a page load; not timed
  const times = [];
  let last = null;
  for (let i = 0; i < 5; i++) {
    last = pass();
    times.push(last.ms);
  }
  times.sort((a, b) => a - b);

  // Parity: the memoized body must equal a direct cold render's bytes.
  const reference = await loadContext(hljsSource);
  const worst = page[0];
  const direct = reference.marked.parse(reference.fixNestedFences(worst));
  const parity = parse(worst) === direct && last.html === cold.html;

  console.log(
    `${page.length} largest bodies (${(pageBytes / 1024).toFixed(1)} KB, page-corpus sha1 ${digest}) of a ` +
    `${(fileSize / 1e6).toFixed(1)} MB live chat file, marked + hljs 11.9.0 common build (${languages} languages), ` +
    `${memoized ? 'memoized' : 'pre-fix'} parse; cold pass ${(cold.ms / 1000).toFixed(3)} s; ` +
    `repeat-page median ${times[2].toFixed(2)} ms, max ${times[4].toFixed(2)} ms; parity ${parity}`
  );
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
