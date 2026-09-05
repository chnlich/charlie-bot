'use strict';
// M54 collector — stream-draft paint work with the page's real highlight.js
// build. The M33 collector stubs hljs, so the standing replay metric never sees
// highlightAuto's cost; this one loads the highlight.js build index.html pins
// (11.9.0 common, cdnjs) into the same harness and replays the largest on-disk
// assistant draft that contains a bare code fence — the corpus shape whose
// paint cost highlightAuto dominates. CHECKOUT picks the code under test;
// live state read-only.
const crypto = require('node:crypto');

const { buildStreamHarness } = require('./stream_render_harness');
const { fetchUrl, largestAssistantDraft } = require('./stream_collector_common');

const MARKED_URL = 'https://cdn.jsdelivr.net/npm/marked/marked.min.js';
const HLJS_URL = 'https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/highlight.min.js';
const BARE_FENCE_RE = /^ {0,3}(`{3,}|~{3,})[ \t]*$/m;

// One replay: 200-byte deltas at a 40 ms virtual cadence; timers drain against
// the virtual clock, so wall time covers only render work.
function replay(markedSrc, text, harnessOptions) {
  const h = buildStreamHarness(markedSrc, harnessOptions);
  const deltas = Math.ceil(text.length / 200);
  h.showStreaming({ content: '' });
  for (let i = 1; i <= deltas; i++) {
    h.showStreaming({ content: text.slice(0, i * 200) });
    h.advance(40);
  }
  h.advance(200);
  return h.stats();
}

(async () => {
  // The corpus is the largest assistant text block that contains a bare
  // (info-less) code fence — the blocks whose render runs highlightAuto; the
  // fail-loud throw keeps an empty live store from silently benchmarking ''.
  const text = largestAssistantDraft((t) => BARE_FENCE_RE.test(t));
  if (!text) throw new Error('no on-disk assistant draft with a bare code fence');
  const digest = crypto.createHash('sha1').update(text).digest('hex').slice(0, 12);
  const [markedSrc, hljsSrc] = await Promise.all([fetchUrl(MARKED_URL), fetchUrl(HLJS_URL)]);
  const harnessOptions = { hljsSource: hljsSrc };
  const languages = (() => {
    const probe = buildStreamHarness(markedSrc, harnessOptions);
    return probe.context.hljs.listLanguages().length;
  })();

  replay(markedSrc, text, harnessOptions); // cold pass, as at the first streamed turn after a page load; not timed
  const times = [];
  let renders = 0, finalHtml = '';
  for (let r = 0; r < 5; r++) {
    const s = replay(markedSrc, text, harnessOptions);
    times.push(s.paintMs);
    renders = s.frames.length;
    finalHtml = s.frames[s.frames.length - 1];
  }
  times.sort((a, b) => a - b);

  // Parity: the last painted frame must equal a direct full-draft render
  // through the same real-hljs context, so a cache hit's bytes are pinned to
  // a cold render's bytes.
  const probe = buildStreamHarness(markedSrc, harnessOptions);
  const reference = probe.context.marked.parse(probe.context.fixNestedFences(text));
  console.log(
    `${(text.length / 1024).toFixed(1)} KB draft (sha1 ${digest}), ${Math.ceil(text.length / 200)} deltas ` +
    `at 40 ms virtual cadence, ${renders} paints, hljs 11.9.0 common build (${languages} languages); ` +
    `paint-work median ${(times[2] / 1000).toFixed(3)} s, max ${(times[4] / 1000).toFixed(3)} s; ` +
    `final-frame parity ${finalHtml === reference}`
  );
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
