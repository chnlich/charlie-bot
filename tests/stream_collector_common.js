'use strict';
// Shared plumbing for the stream-render collectors: the CDN fetch and the
// live-corpus scan. Read-only over ~/.charliebot/sessions; the collectors keep
// their own metric definitions (harness options, replay cadence, reporting).
const fs = require('node:fs');
const https = require('node:https');
const path = require('node:path');

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

// The largest single assistant text block across live chat files that accept()
// passes; '' when none does. A block cannot exceed its file's size, so files no
// larger than the current best are skipped.
function largestAssistantDraft(accept) {
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
            && block.text.length > best.length && accept(block.text)) {
          best = block.text;
        }
      }
    }
  }
  return best;
}

module.exports = { fetchUrl, largestAssistantDraft };
