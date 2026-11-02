'use strict';
// M106 collector — switch-during-stream repaint. Times the synchronous paint a
// session switch performs on a mid-stream pending draft: hideStreaming (the
// teardown path) then showStreaming past the coalesce window, the draft grown
// by one delta as it is between two switches. The largest on-disk assistant
// draft is the corpus; CHECKOUT picks the code under test; live state read-only.
const crypto = require('node:crypto');

const {
  fetchUrl,
  largestAssistantDraft,
  REPLAY_DELTA_BYTES,
} = require('./stream_collector_common');
const { MARKED_URL } = require('./marked_renderer_harness');
const { buildStreamHarness } = require('./stream_render_harness');

const ROUNDS = 7;
const SWITCH_GAP_MS = 250; // past the 200 ms coalesce window: the synchronous-paint branch

(async () => {
  const text = largestAssistantDraft(() => true);
  if (!text) throw new Error('no on-disk assistant draft');
  const digest = crypto.createHash('sha1').update(text).digest('hex').slice(0, 12);
  const markedSrc = await fetchUrl(MARKED_URL);
  const h = buildStreamHarness(markedSrc);
  h.flushIdle();
  h.showStreaming({ content: text }); // the streamed turn's standing paint; not timed
  const times = [];
  let draft = text;
  let lastFrame = '';
  for (let i = 0; i < ROUNDS; i++) {
    draft += 'x'.repeat(REPLAY_DELTA_BYTES); // one delta lands between the switches
    h.context.hideStreaming();
    h.advance(SWITCH_GAP_MS);
    const before = h.stats().paintMs;
    h.showStreaming({ content: draft });
    times.push(h.stats().paintMs - before);
    const frames = h.stats().frames;
    lastFrame = frames[frames.length - 1];
  }
  times.sort((a, b) => a - b);
  const probe = buildStreamHarness(markedSrc);
  const reference = probe.context.marked.parse(probe.context.fixNestedFences(draft));
  const parity = lastFrame === reference;
  console.log(
    `${(text.length / 1024).toFixed(1)} KB draft (sha1 ${digest}), +${REPLAY_DELTA_BYTES} B delta per switch, ` +
    `${ROUNDS} hide+re-show rounds; switch repaint median ${times[(ROUNDS - 1) >> 1].toFixed(2)} ms, ` +
    `max ${times[ROUNDS - 1].toFixed(2)} ms; frame parity ${parity}`
  );
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
