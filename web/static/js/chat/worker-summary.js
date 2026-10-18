(function() {
  const Chat = globalThis.Chat;

// ---------------------------------------------------------------------------
// The worker session's summary card (the For-you view's one row).
//
// The server computes the summary (src/runtime/worker_transcript.py) and sends
// it two ways: as ``worker_summary`` in the bootstrap payload and as
// ``summary`` on every transcript poll. This file paints that one object into
// the card and replaces the whole card on every poll side by side with the
// incremental messages — the card never blocks the transcript's own append
// path. session-view.js feeds the bootstrap value, the poll feeds the rest;
// turn-engine.js asks for the card node when it builds the For-you list.
//
// Card states: a delivered task shows its result, close time, landed commit and
// diff link, one row per run and both roles' closing words; a running one
// updates elapsed time, tool counts, the last text and the last tool call in
// place; a stopped or failed one names the cause sentence and the newest stop
// request with its time and origin (read from the request id's prefix).
// ---------------------------------------------------------------------------

const card = document.createElement('div');
card.className = 'worker-summary w-full rounded-xl border border-slate-700 bg-slate-800/70 px-4 py-3 space-y-3 text-sm';
let currentSummary = null;

// A run's event time reads as a bare clock (the card shows HH:MM:SS on the
// current-step rows); the closing and stop lines use the full bubble time.
function formatClock(ts) {
  if (!ts) return '';
  const d = new Date(ts);
  return String(d.getHours()).padStart(2, '0') + ':' + String(d.getMinutes()).padStart(2, '0') + ':'
    + String(d.getSeconds()).padStart(2, '0');
}

function toolCallWords(count) {
  const n = Number(count) || 0;
  return n + (n === 1 ? ' tool call' : ' tool calls');
}

// The status line: the task state when closed, the newest run's state while it
// is open. A live run reads Running/Queued; any other open run reads its own
// terminal state.
function statusOf(summary) {
  const state = String(summary.task_state || 'open');
  const runs = summary.runs || [];
  const newest = runs.length ? runs[runs.length - 1] : null;
  if (state === 'completed') return {label: 'Delivered', dot: 'bg-green-500'};
  if (state !== 'open') return {label: 'Task ' + state, dot: 'bg-slate-500'};
  if (newest && newest.state === 'running') return {label: 'Running', dot: 'bg-blue-500 animate-pulse-dot'};
  if (newest && newest.state === 'queued') return {label: 'Queued', dot: 'bg-amber-400'};
  if (newest) return {label: 'Run ' + String(newest.state || ''), dot: 'bg-slate-500'};
  return {label: 'Open', dot: 'bg-slate-500'};
}

// One line per Run in the card: kind, backend, state · duration, tool calls.
function runRowHtml(row, elapsedStartedAt) {
  let state = String(row.state || '');
  let duration = '';
  if (state === 'running' && elapsedStartedAt) {
    duration = Chat.formatTurnDuration(Math.max(0, Math.floor((Date.now() - elapsedStartedAt) / 1000)));
  } else if (row.started_at && row.ended_at) {
    const seconds = Math.floor((new Date(row.ended_at) - new Date(row.started_at)) / 1000);
    duration = Chat.formatTurnDuration(seconds);
  }
  const parts = [String(row.kind || 'work') + ' run'];
  if (row.backend_label) parts.push(String(row.backend_label));
  if (duration) parts.push(duration);
  parts.push(state);
  parts.push(toolCallWords(row.tool_calls));
  return '<div class="flex items-center gap-1.5 text-xs text-slate-400 whitespace-nowrap">'
    + parts.map((part) => escapeHtml(part)).join('<span class="text-slate-500"> · </span>')
    + '</div>';
}

// A closing passage folds behind one button past three lines or a weight of
// 360 (a wide code point counts 2) — the child-report card's rule. The full
// text stays in the markup.
function textHtml(text) {
  if (!text) return '';
  const body = '<div class="whitespace-pre-wrap break-words text-slate-300">' + escapeHtml(String(text)) + '</div>';
  let weight = 0;
  for (const ch of String(text)) weight += wc2chIsWide(ch.codePointAt(0)) ? 2 : 1;
  const long = String(text).split('\n').length > 3 || weight > 360;
  if (!long) return body;
  return '<div class="worker-summary-fold" data-folded="true">' + body.replace('whitespace-pre-wrap', 'worker-summary-clamp whitespace-pre-wrap')
    + '<button type="button" class="worker-summary-toggle" aria-expanded="false" onclick="var c=this.closest(\'.worker-summary-fold\');var o=c.dataset.folded===\'true\';c.dataset.folded=o?\'false\':\'true\';this.setAttribute(\'aria-expanded\',String(o))">'
    + '<span class="worker-summary-more">Show all ▾</span><span class="worker-summary-less">Show less ▴</span></button></div>';
}

// A stop request's origin reads from the request id's generated prefixes; the
// reverse query declined a new field, so the ones the runtime writes name
// their source and any other id renders as a bare request reference.
function stopOrigin(requestId) {
  const id = String(requestId || '');
  if (id.indexOf('closed-task:') === 0) return 'system, on task close';
  if (id.indexOf('ui-stop-') === 0) return 'stop button on this page';
  if (id.indexOf('chat-cancel:') === 0) return 'session cancel API';
  if (id.indexOf('improve:') === 0) return 'improve loop close';
  return 'request ' + id;
}

// The two live rows of the Current step block: the newest text's first line and
// the newest tool call, each behind its own event clock.
function currentStepHtml(active) {
  if (!active) return '';
  const rows = [];
  if (active.last_text && active.last_text.text) {
    const firstLine = String(active.last_text.text).split('\n')[0];
    rows.push('<div class="flex items-center gap-2 text-xs">'
      + '<span class="text-slate-500 whitespace-nowrap">' + escapeHtml(formatClock(active.last_text.timestamp)) + '</span>'
      + '<span class="text-slate-300 truncate min-w-0">' + escapeHtml(firstLine) + '</span></div>');
  }
  if (active.last_tool && active.last_tool.name) {
    const summary = Chat.toolInputSummary(active.last_tool.name, active.last_tool.input);
    rows.push('<div class="flex items-center gap-2 text-xs">'
      + '<span class="text-slate-500 whitespace-nowrap">' + escapeHtml(formatClock(active.last_tool.timestamp)) + '</span>'
      + toolNameChipHtml(String(active.last_tool.name))
      + '<span class="text-slate-400 truncate min-w-0">' + escapeHtml(String(summary.text || '')) + '</span></div>');
  }
  if (!rows.length) return '';
  return '<div class="space-y-1"><div class="text-xs font-semibold text-slate-400">Current step</div>'
    + rows.join('') + '</div>';
}

// A role's closing block, only once the Run that carried it exists.
function closingHtml(label, closing) {
  if (!closing || !closing.text) return '';
  return '<div><div class="text-xs font-semibold text-slate-400">' + escapeHtml(label) + '</div>'
    + textHtml(closing.text) + '</div>';
}

function linksRow(summary) {
  if (!summary.links) return '';
  const links = [
    Chat.evidenceLinkHtml('Task spec', Chat.evidenceHref(summary.links.task_spec_ref)),
    Chat.evidenceLinkHtml('Launch prompt', Chat.evidenceHref(summary.links.launch_prompt_ref)),
    Chat.evidenceLinkHtml('Raw log', Chat.evidenceHref(summary.links.raw_log_ref)),
    Chat.evidenceLinkHtml('Events', Chat.evidenceHref(summary.links.events_ref)),
  ];
  return '<p class="text-xs flex flex-wrap gap-3">' + links.join('') + '</p>';
}

// The landed commit with the delivery link registry's Diff link beside it, only
// once the task recorded a landed ref.
function landedHtml(summary) {
  if (!summary.landed) return '';
  const links = (summary.delivery && typeof Chat.deliveryLinksHtml === 'function')
    ? Chat.deliveryLinksHtml(summary.delivery, typeof SESSION_ID !== 'undefined' ? SESSION_ID : '')
    : '';
  return '<p class="text-xs text-slate-400">Landed <span class="break-all text-slate-300">'
    + escapeHtml(String(summary.landed)) + '</span> ' + links + '</p>';
}

function render() {
  const summary = currentSummary;
  if (!summary) return;
  const status = statusOf(summary);
  const activeStart = summary.active && summary.active.started_at ? new Date(summary.active.started_at).getTime() : null;
  let suffix = '';
  if (summary.closed_at) {
    suffix = 'Ended ' + formatBubbleTime(summary.closed_at);
  } else if (activeStart != null) {
    suffix = 'Running for ' + Chat.formatTurnDuration(Math.max(0, Math.floor((Date.now() - activeStart) / 1000)));
  }
  const head = '<div class="flex items-center gap-2">'
    + '<span class="w-2 h-2 rounded-full flex-shrink-0 ' + status.dot + '"></span>'
    + '<span class="font-semibold text-slate-200">' + escapeHtml(status.label) + '</span>'
    + (suffix
      ? '<span class="text-xs text-slate-400 whitespace-nowrap"'
        + (summary.closed_at ? ' title="' + escapeHtmlAttr(String(summary.closed_at)) + '"' : '')
        + '>' + escapeHtml(suffix) + '</span>'
      : '')
    + '</div>';
  const causeHtml = summary.cause
    ? '<div class="text-xs text-red-400 break-words">' + escapeHtml('Cause: ' + summary.cause.text) + '</div>'
    : '';
  const stopHtml = summary.stop
    ? '<div class="text-xs text-slate-400">'
      + escapeHtml('Stop requested ' + formatBubbleTime(summary.stop.timestamp) + ' by '
        + stopOrigin(summary.stop.request_id)) + '</div>'
    : '';
  const runsHtml = (summary.runs || []).map((row) => runRowHtml(row, activeStart)).join('');
  card.innerHTML = head
    + landedHtml(summary)
    + (summary.runs && summary.runs.length
      ? '<div class="pt-1 border-t border-slate-700/60 space-y-1">' + runsHtml + '</div>'
      : '')
    + currentStepHtml(summary.active)
    + causeHtml + stopHtml
    + closingHtml("Worker's last message", summary.closing && summary.closing.work)
    + closingHtml('Review verdict', summary.closing && summary.closing.review)
    + linksRow(summary);
}

// Applies the newest summary (bootstrap or poll) and repaints the card. A
// session that carries no summary (every non-worker) empties the card state;
// its For-you list has no card then.
function applyWorkerSummary(summary) {
  currentSummary = summary || null;
  if (currentSummary) render();
}

// The card node for the For-you list, or null while no summary is showing.
function workerSummaryCard() {
  return currentSummary ? card : null;
}

Chat.ForYou.registerSummaryCardRenderer(workerSummaryCard);
Chat.wire({applyWorkerSummary, workerSummaryCard}, {});

})();
