'use strict';
// M81 collector — chat math-walk (KaTeX auto-render) delimiter gate. The walk
// scans every prose text node for the four delimiters on every message
// re-render and every coalesced streamed paint even when the message carries
// no math — the walk M33's replay and M60's repeat-page metric stub away. This
// collector wall-clocks the walk through the checkout's real renderer code and
// the page's CDN-pinned katex build over a jsdom DOM, over the worst
// message page's assistant text blocks (a subset of M60's page corpus, which
// adds user/worker_summary/plan bodies) and the largest math-free streamed
// draft, live corpora read-only; CHECKOUT picks the code under test. jsdom
// stays off the repo's dependency tree: one-time scratch install, resolved
// through JSDOM_HOME (default /tmp/node_modules) with a loud preflight failure.
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const {
  fetchUrl,
  HLJS_URL,
  largestAssistantDraft,
  worstPageCorpus,
  assistantTexts,
} = require('./stream_collector_common');
const { buildStreamHarness } = require('./stream_render_harness');
const { MARKED_URL } = require('./marked_renderer_harness');

const CHECKOUT = process.env.CHECKOUT || path.join(__dirname, '..');
// The katex build the checkout's chat page serves (web/templates/index.html):
// the walk under test is that page's walk, so the build reads from the same
// page instead of a second pin here. No match is a loud preflight failure.
const INDEX_HTML = fs.readFileSync(path.join(CHECKOUT, 'web', 'templates', 'index.html'), 'utf8');
const katexScript = /<script defer src="(https:\/\/cdn\.jsdelivr\.net\/npm\/katex@[^"]+\/dist\/katex\.min\.js)"><\/script>/.exec(INDEX_HTML);
if (!katexScript) {
  throw new Error('web/templates/index.html carries no katex.min.js script tag; the collector lost its build pin');
}
const KATEX_URL = katexScript[1];
const AUTO_URL = KATEX_URL.replace('katex.min.js', 'contrib/auto-render.min.js');
const KATEX_LABEL = /katex@([^/]+)\//.exec(KATEX_URL)[1];
const PAGE_MESSAGES = 40;

let JSDOM;
try {
  JSDOM = require(path.join(process.env.JSDOM_HOME || '/tmp/node_modules', 'jsdom')).JSDOM;
} catch {
  console.error('jsdom not found; one-time scratch install: npm i --prefix /tmp jsdom@24');
  process.exit(1);
}

(async () => {
  const [markedSrc, hljsSrc, katexSrc, autoSrc] = await Promise.all([
    fetchUrl(MARKED_URL),
    fetchUrl(HLJS_URL),
    fetchUrl(KATEX_URL),
    fetchUrl(AUTO_URL),
  ]);
  // One jsdom window carries the page's builds; two containers isolate the
  // page and streamed shapes. stage() parses a fragment (untimed harness
  // floor — the browser's innerHTML parse is native), the walk walls cover
  // only the renderMathInElement call the page code makes.
  const dom = new JSDOM('<!doctype html><body><div id="c"></div><div id="s"></div></body>', {
    runScripts: 'outside-only',
  });
  const w = dom.window;
  w.eval(katexSrc);
  w.eval(autoSrc);
  w.eval(hljsSrc);
  w.eval(markedSrc);
  const rawWalk = w.renderMathInElement;

  // Page shape: the checkout's renderer, then its postProcess step
  // (querySelectorAll('.prose-msg') -> renderChatMath). The checkout's page
  // load order: math-scanner.js defines the mathSpan global the renderer's
  // math tokenizer reads. Both ride the jsdom vm context with script
  // semantics: w.eval would be an indirect eval, and math-scanner.js's
  // 'use strict' confines a strict eval's function declarations to the eval
  // instead of the window global the renderer reads.
  w.platform = {};
  const domContext = dom.getInternalVMContext();
  vm.runInContext(fs.readFileSync(path.join(CHECKOUT, 'web/static/js/math-scanner.js'), 'utf8'), domContext,
      { filename: 'math-scanner.js' });
  vm.runInContext(fs.readFileSync(path.join(CHECKOUT, 'web/static/js/markdown-renderer.js'), 'utf8'), domContext,
      { filename: 'markdown-renderer.js' });
  let pageWalks = 0;
  w.renderMathInElement = (el, opts) => {
    pageWalks += 1;
    rawWalk(el, opts);
  };

  const { file, fileSize, page } = worstPageCorpus(assistantTexts, PAGE_MESSAGES);
  const pageBytes = page.reduce((sum, t) => sum + t.length, 0);
  const digest = crypto.createHash('sha1').update(page.join('\u0000')).digest('hex').slice(0, 12);
  // The corpus filter rides the checkout's own gate predicate when it exists,
  // so the A/B's math-free corpus is the gate's own math-free definition.
  const mathFree = typeof w.hasMathDelimiter === 'function'
    ? (t) => !w.hasMathDelimiter(t)
    : (t) => !t.includes('$') && !t.includes('\\(') && !t.includes('\\[');
  const nFree = page.filter(mathFree).length;
  // renderMessage's mdDiv attribute: escapeHtml's serializer (& < >) plus the
  // quote escape; the HTML parser decodes it back.
  const dataRaw = (t) => t.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
  w.document.getElementById('c').innerHTML =
    page.map((t) => `<div class="prose-msg" data-raw="${dataRaw(t)}">${w.renderProseMarkdown(t)}</div>`).join('');
  const pageRoot = w.document.getElementById('c');
  const before = pageRoot.innerHTML;
  const proseMsgs = pageRoot.querySelectorAll('.prose-msg');
  const t0 = performance.now();
  proseMsgs.forEach(w.renderChatMath);
  const pageWall = performance.now() - t0;
  const pageParity = nFree === page.length ? pageRoot.innerHTML === before : pageRoot.innerHTML.includes('katex');
  if (!pageParity) throw new Error('page walk changed a math-free page');

  // Repeat shape: the same page rebuilt into fresh elements — the parse memo
  // serves the identical pre-walk HTML — so this is the walk cost a session
  // re-entry pays. A walked-bytes memo serves the first render's output;
  // byte-identity against the cold walk is the parity contract.
  const coldWalked = pageRoot.innerHTML;
  pageRoot.innerHTML =
    page.map((t) => `<div class="prose-msg" data-raw="${dataRaw(t)}">${w.renderProseMarkdown(t)}</div>`).join('');
  const t1 = performance.now();
  pageRoot.querySelectorAll('.prose-msg').forEach(w.renderChatMath);
  const repeatWall = performance.now() - t1;
  const repeatParity = pageRoot.innerHTML === coldWalked;
  if (!repeatParity) throw new Error('repeat render diverged from the cold walk');

  // Streamed shape: the largest math-free draft through the checkout's real
  // paint path; the wrapper walks each painted frame (the gate-less arm).
  const text = largestAssistantDraft(mathFree);
  if (!text) throw new Error('no math-free assistant draft on disk');
  const draftDigest = crypto.createHash('sha1').update(text).digest('hex').slice(0, 12);
  const h = buildStreamHarness(markedSrc);
  let walkMs = 0;
  let walkCalls = 0;
  h.context.renderMathInElement = (el, opts) => {
    w.document.getElementById('s').innerHTML = el.innerHTML;
    const tw = performance.now();
    rawWalk(w.document.getElementById('s'), opts);
    walkMs += performance.now() - tw;
    walkCalls += 1;
  };
  const deltas = Math.ceil(text.length / 200);
  for (let i = 1; i <= deltas; i++) {
    h.showStreaming({ content: text.slice(0, i * 200) });
    h.advance(40);
  }
  h.advance(200);

  console.log(
    `${page.length} bodies (${nFree} math-free, ${(pageBytes / 1024).toFixed(1)} KB, corpus sha1 ${digest}) ` +
    `of a ${(fileSize / 1e6).toFixed(1)} MB live chat file, katex ${KATEX_LABEL} walk over jsdom; ` +
    `page re-render wall ${pageWall.toFixed(2)} ms, ${pageWalks} walks, parity ${pageParity}; ` +
    `repeat re-render wall ${repeatWall.toFixed(2)} ms, parity ${repeatParity}; ` +
    `${(text.length / 1024).toFixed(1)} KB math-free draft (sha1 ${draftDigest}), ${h.stats().frames.length} paints: ` +
    `walk wall ${walkMs.toFixed(2)} ms (${walkCalls} walks)`
  );
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
