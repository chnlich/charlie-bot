'use strict';
// Worker summary card (chat/worker-summary.js): loads the module in a node vm
// against the Chat surface it resolves at runtime (rendering.js exports and
// shared.js globals) and asserts the card's states off its painted HTML.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const test = require('node:test');
const assert = require('node:assert/strict');

const ROOT = path.join(__dirname, '..');
const read = (name) => fs.readFileSync(path.join(ROOT, 'web/static/js/chat', name), 'utf8');

function mkEl(tag) {
  const el = {tagName: tag, className: '', sourceHtml: ''};
  Object.defineProperty(el, 'innerHTML', {
    set(v) { el.sourceHtml = v; },
    get() { return el.sourceHtml; },
  });
  el.classList = {
    add() {}, remove() {},
    contains() { return false; },
  };
  return el;
}

function loadCard() {
  const elements = [];
  const context = {
    document: {
      createElement(tag) {
        const el = mkEl(tag);
        elements.push(el);
        return el;
      },
    },
    escapeHtml: (s) => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;'),
    escapeHtmlAttr: (s) => String(s).replace(/"/g, '&quot;'),
    formatBubbleTime: (iso) => 'T(' + iso + ')',
    toolNameChipHtml: (name) => '<toolchip>' + name + '</toolchip>',
    wc2chIsWide: (cp) => cp > 0x2e00,
    Chat: {
      ForYou: {
        registerSummaryCardRenderer(renderer) { this.summaryCard = renderer; },
      },
      formatTurnDuration: (seconds) => seconds + 's',
      toolInputSummary: (name, input) => ({text: 'input of ' + name + ':' + input.command, limit: 0}),
      evidenceLinkHtml: (label, href) =>
        href ? '<a class="ok">' + label + '</a>' : '<span class="placeholder">' + label + '</span>',
      evidenceHref: (ref) => (ref ? '/evidence/' + encodeURIComponent(ref) : ''),
      deliveryLinksHtml: () => '<a class="ok">Diff</a>',
    },
  };
  context.Chat.wire = function(globals, chatOnly) {
    Object.assign(this, globals, chatOnly || {});
  };
  vm.createContext(context);
  vm.runInContext(read('worker-summary.js'), context, {filename: 'worker-summary.js'});
  return {
    context,
    elements,
    html: () => context.Chat.workerSummaryCard().innerHTML,
  };
}

const DELIVERED = {
  task_state: 'completed',
  closed_at: '2026-10-17T10:00:00+00:00',
  landed: 'origin/main@abc123',
  delivery: {raw_log_ref: 'log', events_ref: 'ev', result_ref: 'res'},
  runs: [
    {run_id: 'r1', kind: 'work', state: 'completed', backend_label: 'claude-haiku',
     started_at: '2026-10-17T09:00:00+00:00', ended_at: '2026-10-17T09:10:00.900+00:00', tool_calls: 161},
    {run_id: 'r2', kind: 'review', state: 'completed', backend_label: 'op-deepseek',
     started_at: '2026-10-17T09:12:00+00:00', ended_at: null, tool_calls: 9},
  ],
  closing: {work: {run_id: 'r1', text: 'done work'}, review: {run_id: 'r2', text: 'ok review'}},
  links: {task_spec_ref: 'spec', launch_prompt_ref: 'lp', raw_log_ref: 'log', events_ref: 'ev'},
};

test('a delivered task shows the Delivered status, ended time, landed commit, run rows, closing words and links', () => {
  const rig = loadCard();
  rig.context.Chat.applyWorkerSummary(DELIVERED);
  const html = rig.html();
  assert.match(html, /Delivered/);
  assert.match(html, /Ended T\(2026-10-17T10:00:00\+00:00\)/);
  assert.match(html, /Landed <span[^>]*>origin\/main@abc123<\/span>/);
  assert.match(html, /<a class="ok">Diff<\/a>/);
  // Each run one row: kind, backend, duration, state and tool calls, joined by
  // the separator dot — the review run carries no ended_at, so it has no
  // duration field.
  assert.match(html, /work run/);
  assert.match(html, /review run/);
  assert.match(html, /161 tool calls/);
  assert.match(html, /9 tool calls/);
  assert.match(html, /600s/);
  assert.match(html, /completed/);
  assert.match(html, /<span class="text-slate-500"> · <\/span>/);
  // Both roles' closing passages.
  assert.match(html, /Worker&#39;s last message|Worker's last message/);
  assert.match(html, /done work/);
  assert.match(html, /Review verdict/);
  assert.match(html, /ok review/);
  // The links row with every ref present resolves to links.
  assert.match(html, /<a class="ok">Task spec<\/a>/);
  assert.match(html, /<a class="ok">Launch prompt<\/a>/);
  assert.match(html, /<a class="ok">Raw log<\/a>/);
  assert.match(html, /<a class="ok">Events<\/a>/);
  assert.doesNotMatch(html, /placeholder/);
  assert.doesNotMatch(html, /Current step/);
});

test('a single tool call reads in the singular', () => {
  const rig = loadCard();
  rig.context.Chat.applyWorkerSummary({
    ...DELIVERED,
    runs: [{...DELIVERED.runs[0], tool_calls: 1}],
    closing: {work: null, review: null},
  });
  const html = rig.html();
  assert.match(html, /1 tool call/);
  assert.doesNotMatch(html, /1 tool calls/);
});

test('a running task shows the live state, elapsed time, current step and tool count', () => {
  const rig = loadCard();
  rig.context.Chat.applyWorkerSummary({
    task_state: 'open',
    active: {
      run_id: 'r-live', started_at: '2026-10-17T09:00:00+00:00', tool_calls: 5,
      last_text: {timestamp: '2026-10-17T09:05:00+00:00', text: 'thinking about it\nmore detail'},
      last_tool: {timestamp: '2026-10-17T09:06:00+00:00', name: 'Bash', input: {command: 'uv run pytest'}},
    },
    runs: [
      {run_id: 'r-live', kind: 'work', state: 'running', backend_label: 'claude-haiku',
       started_at: '2026-10-17T09:00:00+00:00', ended_at: null, tool_calls: 5},
    ],
    closing: {work: null, review: null},
    links: {},
  });
  const html = rig.html();
  assert.match(html, /Running/);
  assert.match(html, /animate-pulse-dot/);
  assert.match(html, /Running for \d+s/);
  // The finished-state extras are absent while the run is live.
  assert.doesNotMatch(html, /Landed /);
  assert.doesNotMatch(html, /Worker's last message/);
  assert.match(html, /Current step/);
  assert.match(html, /5 tool calls/);
  assert.match(html, /<toolchip>Bash<\/toolchip>/);
  assert.match(html, /input of Bash:uv run pytest/);
  // Only the first line of the newest text paints.
  assert.match(html, /thinking about it/);
  assert.doesNotMatch(html, /more detail/);
  assert.match(html, /work run/);
});

test('a queued run reads Queued, another open run reads its own state', () => {
  const rig = loadCard();
  rig.context.Chat.applyWorkerSummary({
    task_state: 'open',
    runs: [{run_id: 'r', kind: 'work', state: 'queued', tool_calls: 0}],
    closing: {work: null, review: null}, links: {},
  });
  assert.match(rig.html(), /Queued/);
  rig.context.Chat.applyWorkerSummary({
    task_state: 'open',
    runs: [{run_id: 'r', kind: 'work', state: 'interrupted', tool_calls: 0}],
    closing: {work: null, review: null}, links: {},
  });
  assert.match(rig.html(), /Run interrupted/);
});

test('a closed task names its task state, its cause and the stop request with time and origin', () => {
  const rig = loadCard();
  rig.context.Chat.applyWorkerSummary({
    task_state: 'archived',
    cause: {run_id: 'r-dead', timestamp: '2026-10-17T08:50:00+00:00', text: 'peer closed connection'},
    stop: {run_id: 'r-dead', request_id: 'ui-stop-179228780811', timestamp: '2026-10-17T08:51:00+00:00'},
    runs: [
      {run_id: 'r-dead', kind: 'work', state: 'interrupted', backend_label: 'op-deepseek-flash-free',
       started_at: '2026-10-17T08:40:00+00:00', ended_at: '2026-10-17T08:51:00+00:00', tool_calls: 3},
    ],
    closing: {work: null, review: null},
    links: {task_spec_ref: null, launch_prompt_ref: null, raw_log_ref: 'log', events_ref: null},
  });
  const html = rig.html();
  assert.match(html, /Task archived/);
  assert.match(html, /text-red-400/);
  assert.match(html, /Cause: peer closed connection/);
  assert.match(html, /Stop requested T\(2026-10-17T08:51:00\+00:00\) by stop button on this page/);
  // Missing refs stay gray placeholders.
  assert.match(html, /<span class="placeholder">Task spec<\/span>/);
  assert.match(html, /<span class="placeholder">Launch prompt<\/span>/);
  assert.match(html, /<span class="placeholder">Events<\/span>/);
  assert.match(html, /<a class="ok">Raw log<\/a>/);
});

test('stop origins read from the request id prefixes and fall back to the id itself', () => {
  const rig = loadCard();
  const apply = (requestId) => {
    rig.context.Chat.applyWorkerSummary({
      task_state: 'open',
      stop: {run_id: 'r', request_id: requestId, timestamp: '2026-10-17T08:51:00+00:00'},
      runs: [], closing: {work: null, review: null}, links: {},
    });
    return rig.html();
  };
  assert.match(apply('closed-task:123'), /by system, on task close/);
  assert.match(apply('ui-stop-179228780811'), /by stop button on this page/);
  assert.match(apply('chat-cancel:abc'), /by session cancel API/);
  assert.match(apply('improve:loop-1'), /by improve loop close/);
  assert.match(apply('someone-else'), /by request someone-else/);
});

test('a session with no summary empties the card', () => {
  const rig = loadCard();
  rig.context.Chat.applyWorkerSummary(DELIVERED);
  rig.context.Chat.applyWorkerSummary(null);
  assert.equal(rig.context.Chat.workerSummaryCard(), null);
});

test('a long closing passage folds, a short one does not', () => {
  const rig = loadCard();
  rig.context.Chat.applyWorkerSummary({
    ...DELIVERED,
    closing: {work: {run_id: 'r1', text: 'x'.repeat(400)}, review: null},
  });
  const html = rig.html();
  assert.match(html, /worker-summary-fold" data-folded="true"/);
  assert.match(html, /Show all/);
  assert.doesNotMatch(html, /ok review/);
});

test('the repoll replaces the painted card', () => {
  const rig = loadCard();
  rig.context.Chat.applyWorkerSummary(DELIVERED);
  assert.match(rig.html(), /Delivered/);
  const card = rig.context.Chat.workerSummaryCard();
  rig.context.Chat.applyWorkerSummary({
    ...DELIVERED, task_state: 'open', closed_at: null, landed: null, delivery: null,
  });
  assert.equal(rig.context.Chat.workerSummaryCard(), card);
  assert.doesNotMatch(rig.html(), /Delivered/);
  assert.match(rig.html(), /Run completed/);
});
