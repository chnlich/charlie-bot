'use strict';
// Shared plumbing for the stream-render collectors: the CDN fetch, the
// live-corpus scan, and the streamed-draft replay scaffolding the M33 and M54
// metrics share (one cadence, one replay loop, one timed fleet, one parity
// check, so their numbers stay comparable). Read-only over
// ~/.charliebot/sessions; the collectors keep their own metric definitions
// (corpus filter, harness options, reporting).
const fs = require('node:fs');
const https = require('node:https');
const path = require('node:path');

const { buildStreamHarness } = require('./stream_render_harness');

// The highlight.js build the chat page serves (web/templates/index.html); the
// collectors' numbers stay comparable only while they highlight through the
// browser's build, so bump it with the template.
const HLJS_URL = 'https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/highlight.min.js';

function fetchUrl(url) {
  return new Promise((resolve, reject) => {
    https.get(url, (res) => {
      let data = '';
      res.setEncoding('utf8');
      res.on('data', (c) => { data += c; });
      res.on('end', () => resolve(data));
    }).on('error', reject);
  });
}

// The live-corpus root liveChatFiles walks. Exported so a bridge-registered
// suite can skip hosts that carry no live corpus (CI, fresh checkouts); the
// collectors themselves fail loud on it.
const LIVE_CHAT_ROOT = path.join(process.env.HOME, '.charliebot', 'sessions');

// Every live session's chat file as { p, size }, skipping the sessions whose
// file is absent or unreadable. The one census both corpora below walk.
function liveChatFiles() {
  const files = [];
  for (const d of fs.readdirSync(LIVE_CHAT_ROOT)) {
    const p = path.join(LIVE_CHAT_ROOT, d, 'data', 'chat_events.jsonl');
    try { files.push({ p, size: fs.statSync(p).size }); } catch { continue; }
  }
  return files;
}

// The largest single assistant text block across live chat files that accept()
// passes; '' when none does. A block cannot exceed its file's size, so files no
// larger than the current best are skipped.
function largestAssistantDraft(accept) {
  const files = liveChatFiles().sort((a, b) => a.size - b.size);
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
            && block.text.length > best.length && accept(block.text)) {
          best = block.text;
        }
      }
    }
  }
  return best;
}

// An assistant event's text-block bodies: the non-empty strings of its message
// content blocks of type text.
function assistantTexts(ev) {
  if (ev.type !== 'assistant' || !ev.message) return [];
  const blocks = Array.isArray(ev.message.content) ? ev.message.content : [];
  return blocks.flatMap((b) => (b?.type === 'text' && typeof b.text === 'string' && b.text ? [b.text] : []));
}

// The live chat file carrying the most bytes, as { p, size }; null when the
// live tree holds no chat file.
function worstChatFile() {
  let worst = null;
  for (const f of liveChatFiles()) {
    if (worst === null || f.size > worst.size) worst = f;
  }
  return worst;
}

// The worst live chat file's message page: the pageMessages largest bodies its
// events carry, as { file, fileSize, page }; throws when the live tree holds no
// chat file or the worst file carries no body. bodyTexts(event) returns the
// bodies one event contributes (possibly none), so each collector pins the
// event shapes its metric walks.
function worstPageCorpus(bodyTexts, pageMessages) {
  const worst = worstChatFile();
  if (!worst) throw new Error('no on-disk live chat file');
  const { p: best, size: bestSize } = worst;
  const texts = [];
  for (const line of fs.readFileSync(best, 'utf8').split('\n')) {
    if (!line) continue;
    let ev;
    try {
      ev = JSON.parse(line);
    } catch {
      continue;
    }
    for (const text of bodyTexts(ev)) texts.push(text);
  }
  if (!texts.length) throw new Error('no message bodies in the worst live chat file');
  texts.sort((a, b) => b.length - a.length);
  return { file: best, fileSize: bestSize, page: texts.slice(0, pageMessages) };
}

// The streamed-draft replay cadence both metrics report against.
const REPLAY_DELTA_BYTES = 200;
const REPLAY_TICK_MS = 40;
const TIMED_REPLAYS = 5;

// One replay: the draft grows REPLAY_DELTA_BYTES bytes per paint, one
// REPLAY_TICK_MS virtual tick per delta, then one trailing flush mirrors the
// browser's trailing paint. The harness's timers drain against the virtual
// clock, so wall time covers only render work. options.leadEmptyPaint opens
// with one empty paint, which consumes the harness's immediate first paint
// (usage.js paints a stream's first call outright), so delta one paints from
// the coalesced flush instead — M54's call sequence; M33 has no leading paint.
function replayDraft(markedSrc, text, { harnessOptions, leadEmptyPaint = false } = {}) {
  const h = buildStreamHarness(markedSrc, harnessOptions);
  const deltas = Math.ceil(text.length / REPLAY_DELTA_BYTES);
  if (leadEmptyPaint) h.showStreaming({ content: '' });
  for (let i = 1; i <= deltas; i++) {
    h.showStreaming({ content: text.slice(0, i * REPLAY_DELTA_BYTES) });
    h.advance(REPLAY_TICK_MS);
  }
  h.advance(REPLAY_DELTA_BYTES);
  return h.stats();
}

// One untimed cold pass (the first streamed turn after a page load), then
// TIMED_REPLAYS timed replays; the reported fleet is the sorted paint walls
// plus the last replay's paint count and final frame.
function timedReplays(markedSrc, text, options = {}) {
  replayDraft(markedSrc, text, options);
  const times = [];
  let renders = 0, finalHtml = '';
  for (let r = 0; r < TIMED_REPLAYS; r++) {
    const s = replayDraft(markedSrc, text, options);
    times.push(s.paintMs);
    renders = s.frames.length;
    finalHtml = s.frames[s.frames.length - 1];
  }
  times.sort((a, b) => a - b);
  return { times, renders, finalHtml };
}

// Final-frame parity: the last painted frame must equal a direct full-draft
// render through the same harness context — the same real-hljs options
// included, so a highlight cache hit's bytes are pinned to a cold render's.
function finalFrameParity(markedSrc, text, finalHtml, { harnessOptions } = {}) {
  const probe = buildStreamHarness(markedSrc, harnessOptions);
  const reference = probe.context.marked.parse(probe.context.fixNestedFences(text));
  return finalHtml === reference;
}

module.exports = {
  HLJS_URL,
  LIVE_CHAT_ROOT,
  fetchUrl,
  largestAssistantDraft,
  assistantTexts,
  worstPageCorpus,
  REPLAY_DELTA_BYTES,
  REPLAY_TICK_MS,
  replayDraft,
  timedReplays,
  finalFrameParity,
};
