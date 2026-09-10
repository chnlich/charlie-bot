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
const {
  fetchUrl,
  largestAssistantDraft,
  timedReplays,
  finalFrameParity,
  REPLAY_DELTA_BYTES,
  REPLAY_TICK_MS,
} = require('./stream_collector_common');

const MARKED_URL = 'https://cdn.jsdelivr.net/npm/marked/marked.min.js';
const HLJS_URL = 'https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/highlight.min.js';
const BARE_FENCE_RE = /^ {0,3}(`{3,}|~{3,})[ \t]*$/m;

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

  const { times, renders, finalHtml } = timedReplays(markedSrc, text, { harnessOptions, leadEmptyPaint: true });
  console.log(
    `${(text.length / 1024).toFixed(1)} KB draft (sha1 ${digest}), ${Math.ceil(text.length / REPLAY_DELTA_BYTES)} deltas ` +
    `at ${REPLAY_TICK_MS} ms virtual cadence, ${renders} paints, hljs 11.9.0 common build (${languages} languages); ` +
    `paint-work median ${(times[2] / 1000).toFixed(3)} s, max ${(times[4] / 1000).toFixed(3)} s; ` +
    `final-frame parity ${finalFrameParity(markedSrc, text, finalHtml, { harnessOptions })}`
  );
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
