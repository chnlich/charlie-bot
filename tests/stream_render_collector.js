'use strict';
// M33 collector — assistant-stream draft render replay. Wall-clocks one full-turn
// replay through the checkout's usage.js/markdown-renderer.js and the page-pinned
// marked build: the largest on-disk assistant draft growing in 200-byte deltas at a
// 40 ms virtual cadence. CHECKOUT picks the code under test; live state read-only.
const fs = require('node:fs');
const https = require('node:https');
const path = require('node:path');
const crypto = require('node:crypto');

const { buildStreamHarness } = require('./stream_render_harness');

const MARKED_URL = 'https://cdn.jsdelivr.net/npm/marked/marked.min.js';
function fetchMarked() {
  return new Promise((resolve, reject) => {
    https.get(MARKED_URL, (res) => {
      let data = '';
      res.setEncoding('utf8');
      res.on('data', (c) => { data += c; });
      res.on('end', () => resolve(data));
    }).on('error', reject);
  });
}

// The largest single assistant text block across live chat files; a block cannot
// exceed its file's size, so files no larger than the current best are skipped.
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
      for (const block of ev.message.content || []) {
        if (block && block.type === 'text' && typeof block.text === 'string' && block.text.length > best.length) {
          best = block.text;
        }
      }
    }
  }
  return best;
}

// One replay: 200-byte deltas at a 40 ms virtual cadence; timers drain against
// the virtual clock, so wall time covers only render work (the marked re-parse
// per paint dominates). The final flush mirrors the browser's trailing paint.
function replay(markedSrc, text) {
  const h = buildStreamHarness(markedSrc);
  const deltas = Math.ceil(text.length / 200);
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
  const markedSrc = await fetchMarked();

  replay(markedSrc, text); // cold pass, as at the first streamed turn after a page load; not timed
  const times = [];
  let renders = 0, finalHtml = '';
  for (let r = 0; r < 5; r++) {
    const s = replay(markedSrc, text);
    times.push(s.paintMs);
    renders = s.frames.length;
    finalHtml = s.frames[s.frames.length - 1];
  }
  times.sort((a, b) => a - b);

  // Parity: the last painted frame must equal a direct full-draft render.
  const probe = buildStreamHarness(markedSrc);
  const reference = probe.context.marked.parse(probe.context.fixNestedFences(text));
  console.log(
    `${(text.length / 1024).toFixed(1)} KB draft (sha1 ${digest}), ${Math.ceil(text.length / 200)} deltas ` +
    `at 40 ms virtual cadence, ${renders} paints; replay wall median ${(times[2] / 1000).toFixed(3)} s, ` +
    `max ${(times[4] / 1000).toFixed(3)} s; final-frame parity ${finalHtml === reference}`
  );
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
