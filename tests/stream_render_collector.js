'use strict';
// M33 collector — assistant-stream draft render replay. Wall-clocks one full-turn
// replay through the checkout's usage.js/markdown-renderer.js and the page-pinned
// marked build: the largest on-disk assistant draft growing in 200-byte deltas at a
// 40 ms virtual cadence. CHECKOUT picks the code under test; live state read-only.
const crypto = require('node:crypto');

const {
  fetchUrl,
  largestAssistantDraft,
  timedReplays,
  finalFrameParity,
  REPLAY_DELTA_BYTES,
  REPLAY_TICK_MS,
} = require('./stream_collector_common');

const MARKED_URL = 'https://cdn.jsdelivr.net/npm/marked/marked.min.js';

(async () => {
  const text = largestAssistantDraft(() => true);
  const digest = crypto.createHash('sha1').update(text).digest('hex').slice(0, 12);
  const markedSrc = await fetchUrl(MARKED_URL);

  const { times, renders, finalHtml } = timedReplays(markedSrc, text);
  console.log(
    `${(text.length / 1024).toFixed(1)} KB draft (sha1 ${digest}), ${Math.ceil(text.length / REPLAY_DELTA_BYTES)} deltas ` +
    `at ${REPLAY_TICK_MS} ms virtual cadence, ${renders} paints; replay wall median ${(times[2] / 1000).toFixed(3)} s, ` +
    `max ${(times[4] / 1000).toFixed(3)} s; final-frame parity ${finalFrameParity(markedSrc, text, finalHtml)}`
  );
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
