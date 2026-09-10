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

const { fetchUrl, worstPageCorpus, assistantTexts } = require('./stream_collector_common');
const { buildRendererContext } = require('./renderer_vm_context');

const CHECKOUT = process.env.CHECKOUT || path.join(__dirname, '..');
const MARKED_URL = 'https://cdn.jsdelivr.net/npm/marked/marked.min.js';
const HLJS_URL = 'https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/highlight.min.js';
const PAGE_MESSAGES = 40;

function readJs(name) {
  return fs.readFileSync(path.join(CHECKOUT, 'web/static/js', name), 'utf8');
}

// The bodies one event contributes to the page corpus: assistant text blocks
// plus the string bodies of user/worker_summary/plan events — the page a
// re-entry of the heaviest session re-renders.
function pageBodyTexts(ev) {
  if (ev.type === 'user' || ev.type === 'worker_summary' || ev.type === 'plan') {
    return typeof ev.content === 'string' && ev.content ? [ev.content] : [];
  }
  return assistantTexts(ev);
}

async function loadContext(hljsSource) {
  const context = buildRendererContext({ withTimers: true });
  vm.createContext(context);
  vm.runInContext(hljsSource, context, { filename: 'highlight.min.js' });
  vm.runInContext(await fetchUrl(MARKED_URL), context, { filename: 'marked.min.js' });
  vm.runInContext(readJs('markdown-renderer.js'), context, { filename: 'markdown-renderer.js' });
  return context;
}

(async () => {
  const { file, fileSize, page } = worstPageCorpus(pageBodyTexts, PAGE_MESSAGES);
  const pageBytes = page.reduce((sum, t) => sum + t.length, 0);
  const digest = crypto.createHash('sha1').update(page.join('\u0000')).digest('hex').slice(0, 12);
  const hljsSource = await fetchUrl(HLJS_URL);
  const languages = await (async () => {
    const probe = await loadContext(hljsSource);
    return probe.hljs.listLanguages().length;
  })();

  // The parse the checkout's message body path runs: the memo helper when the
  // checkout has it, the pre-fix raw expression otherwise. The memoized parse
  // defers the code highlight, so a page pass is followed by the scheduled
  // flush that carries the highlight work and settles the memo entries; the
  // 8 ms timebox reschedules, so the driver drains the timer queue to
  // completion.
  const probe = await loadContext(hljsSource);
  const memoized = typeof probe.renderProseMarkdown === 'function';
  const parse = memoized
    ? (t) => probe.renderProseMarkdown(t)
    : (t) => probe.marked.parse(probe.fixNestedFences(t));
  const flush = memoized
    ? () => {
        const t0 = performance.now();
        for (let i = 0; i < 10000 && probe.__timerCount() > 0; i++) probe.__runTimers();
        return performance.now() - t0;
      }
    : () => 0;

  const pass = () => {
    const t0 = performance.now();
    let html = '';
    for (const text of page) html += parse(text);
    return { ms: performance.now() - t0, html };
  };

  const cold = pass(); // cold first paint: the deferred parse; the highlight lands in the flush
  const flushMs = flush();
  const times = [];
  let last = null;
  for (let i = 0; i < 5; i++) {
    last = pass();
    times.push(last.ms);
  }
  times.sort((a, b) => a - b);

  // Parity: after the flush, the settled body bytes equal a direct render's.
  const reference = await loadContext(hljsSource);
  const worst = page[0];
  const direct = reference.marked.parse(reference.fixNestedFences(worst));
  const settledWorst = parse(worst);
  const parity = settledWorst === direct && last.html.includes(settledWorst)
    && !last.html.includes('data-hl');

  console.log(
    `${page.length} largest bodies (${(pageBytes / 1024).toFixed(1)} KB, page-corpus sha1 ${digest}) of a ` +
    `${(fileSize / 1e6).toFixed(1)} MB live chat file, marked + hljs 11.9.0 common build (${languages} languages), ` +
    `${memoized ? 'memoized' : 'pre-fix'} parse; cold first paint ${(cold.ms / 1000).toFixed(3)} s, ` +
    `highlight flush ${(flushMs / 1000).toFixed(3)} s; ` +
    `repeat-page median ${times[2].toFixed(2)} ms, max ${times[4].toFixed(2)} ms; parity ${parity}`
  );
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
