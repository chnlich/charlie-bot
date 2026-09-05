'use strict';
// M54 collector — stream-draft paint work with the page's real highlight.js
// build. The M33 collector stubs hljs, so the standing replay metric never sees
// highlightAuto's cost; this one loads the highlight.js build index.html pins
// (11.9.0 common, cdnjs) into the same harness and replays the largest on-disk
// assistant draft that contains a bare code fence — the corpus shape whose
// paint cost highlightAuto dominates. CHECKOUT picks the code under test;
// live state read-only.
const fs = require('node:fs');
const https = require('node:https');
const path = require('node:path');
const crypto = require('node:crypto');

const { buildStreamHarness } = require('./stream_render_harness');

const MARKED_URL = 'https://cdn.jsdelivr.net/npm/marked/marked.min.js';
const HLJS_URL = 'https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/highlight.min.js';
const BARE_FENCE_RE = /^ {0,3}(`{3,}|~{3,})[ \t]*$/m;

function fetch(url) {
  return new Promise((resolve, reject) => {
    https.get(url, (res) => {
      let data = '';
      res.setEncoding('utf8');
      res.on('data', (c) => { data += c; });
      res.on('end', () => resolve(data));
    }).on('error', reject);
  });
}

// The largest single assistant text block across live chat files that contains
// a bare (info-less) code fence — the blocks whose render runs highlightAuto.
function resolveCorpus() {
  const root = path.join(process.env.HOME, '.charliebot', 'sessions');
  const files = [];
  for (const d of fs.readdirSync(root)) {
    const p = path.join(root, d, 'data', 'chat_events.jsonl');
    try { files.push({ p, size: fs.statSync(p).size }); } catch { continue; }
  }
  files.sort((a, b) => a.size - b.size);
  let best = '';
  for (const f of files) {
    if (f.size <= best.length) continue;
    for (const line of fs.readFileSync(f.p, 'utf8').split('\n')) {
      if (!line || line.indexOf('"assistant"') === -1) continue;
      let ev;
      try { ev = JSON.parse(line); } catch { continue; }
      if (ev.type !== 'assistant' || !ev.message) continue;
      const blocks = Array.isArray(ev.message.content) ? ev.message.content : [];
      for (const block of blocks) {
        if (block && block.type === 'text' && typeof block.text === 'string'
            && block.text.length > best.length && BARE_FENCE_RE.test(block.text)) {
          best = block.text;
        }
      }
    }
  }
  if (!best) throw new Error('no on-disk assistant draft with a bare code fence');
  return best;
}

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
  const text = resolveCorpus();
  const digest = crypto.createHash('sha1').update(text).digest('hex').slice(0, 12);
  const [markedSrc, hljsSrc] = await Promise.all([fetch(MARKED_URL), fetch(HLJS_URL)]);
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
