(function() {
  const Chat = globalThis.Chat;

// ---------------------------------------------------------------------------
// The worker session's summary card (the For-you view's one row).
//
// The server computes the summary (src/runtime/worker_transcript.py) and sends
// it two ways: as ``worker_summary`` in the bootstrap payload and as
// ``summary`` on every transcript poll. This file paints that one object into
// the card, side by side with the incremental messages — the card never
// blocks the transcript's own append path. A poll that brings the summary the
// card shows writes only the elapsed-time text. A changed summary repaints the
// card, and each closing passage's fold stays as the reader left it.
// session-view.js feeds the bootstrap value (mountWorkerSummary) and each
// poll's (applyWorkerSummary); turn-engine.js asks for the card node when it
// builds the For-you list.
//
// Card states: a delivered task shows its result, close time, landed commit,
// diff link, one row per run and both roles' closing words. A running one
// updates elapsed time, tool counts, the last text and the last tool call in
// place. A stopped or failed one names the cause sentence and the newest stop
// request with its time and origin (read from the request id's prefix).
// ---------------------------------------------------------------------------

const card = document.createElement('div');
card.className = 'worker-summary w-full rounded-xl border border-slate-700 bg-slate-800/70 px-4 py-3 space-y-3 text-sm';
let currentSummary = null;
// The JSON of the summary the card shows: a poll bringing the same JSON
// leaves the card as the reader has it.
let currentSummaryJson = 'null';
// The closing roles ('work', 'review') whose Show all fold the reader opened.
const openFolds = new Set();

// The whole seconds since an ISO time, in the Run duration words.
function elapsedText(startedAt) {
  return Chat.formatRunDuration(Math.max(0, Math.floor((Date.now() - new Date(startedAt).getTime()) / 1000)));
}

// An elapsed time that ticks in place: the span names its start, and each
// poll rewrites its text from that start.
function elapsedHtml(prefix, startedAt) {
  return '<span data-elapsed-since="' + escapeHtmlAttr(String(startedAt)) + '" data-elapsed-prefix="'
    + escapeHtmlAttr(prefix) + '">' + escapeHtml(prefix + elapsedText(startedAt)) + '</span>';
}

function tickElapsed() {
  card.querySelectorAll('[data-elapsed-since]').forEach((span) => {
    if (span.dataset.elapsedPrefix !== 'Running for ') return;
    const text = span.dataset.elapsedPrefix + elapsedText(span.dataset.elapsedSince);
    if (span.textContent !== text) span.textContent = text;
  });
}

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
// The live Run's duration is the active Run's elapsed time.
function runRowHtml(row, activeStartedAt) {
  const state = String(row.state || '');
  let duration = '';
  if (state === 'running' && activeStartedAt) {
    duration = escapeHtml(elapsedText(activeStartedAt));
  } else if (row.started_at && row.ended_at) {
    const seconds = Math.floor((new Date(row.ended_at) - new Date(row.started_at)) / 1000);
    duration = escapeHtml(Chat.formatRunDuration(seconds));
  }
  const parts = [escapeHtml(String(row.kind || 'work') + ' run')];
  if (row.backend_label) parts.push(escapeHtml(String(row.backend_label)));
  if (duration) parts.push(duration);
  parts.push(escapeHtml(state));
  parts.push(escapeHtml(toolCallWords(row.tool_calls)));
  return '<div class="flex items-center gap-1.5 text-xs text-slate-400 whitespace-nowrap">'
    + parts.join('<span class="text-slate-500"> · </span>')
    + '</div>';
}

// The Show all toggle of one closing passage: flips its fold and records the
// role, so a repaint keeps the fold as the reader left it.
function toggleWorkerSummaryFold(button) {
  const fold = button.closest('.worker-summary-fold');
  const opening = fold.dataset.folded === 'true';
  fold.dataset.folded = opening ? 'false' : 'true';
  button.setAttribute('aria-expanded', String(opening));
  if (opening) openFolds.add(fold.dataset.foldKey);
  else openFolds.delete(fold.dataset.foldKey);
}

// A long closing passage (Chat.isLongPassage) folds behind one button, keyed
// by its role; the full text stays in the markup.
function textHtml(text, foldKey) {
  if (!text) return '';
  const body = '<div class="whitespace-pre-wrap break-words text-slate-300">' + escapeHtml(String(text)) + '</div>';
  if (!Chat.isLongPassage(text)) return body;
  const open = openFolds.has(foldKey);
  return '<div class="worker-summary-fold" data-fold-key="' + foldKey + '" data-folded="' + String(!open) + '">'
    + body.replace('whitespace-pre-wrap', 'worker-summary-clamp whitespace-pre-wrap')
    + '<button type="button" class="worker-summary-toggle" aria-expanded="' + String(open)
    + '" onclick="toggleWorkerSummaryFold(this)">'
    + '<span class="worker-summary-more">Show all ▾</span><span class="worker-summary-less">Show less ▴</span></button></div>';
}

// A stop request's origin reads from the request id's generated prefixes. The
// reverse query declined a new field, so the ones the runtime writes name
// their source. Any other id renders as a bare request reference.
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
function closingHtml(label, closing, foldKey) {
  if (!closing || !closing.text) return '';
  return '<div><div class="text-xs font-semibold text-slate-400">' + escapeHtml(label) + '</div>'
    + textHtml(closing.text, foldKey) + '</div>';
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

// The landed commit of the current close, with the delivery link registry's
// Diff link beside it. The server sends landed and delivery from the current
// close only, so both are null while the task is open.
function landedHtml(summary) {
  if (!summary.landed) return '';
  const links = summary.delivery ? Chat.deliveryLinksHtml(summary.delivery, SESSION_ID) : '';
  return '<p class="text-xs text-slate-400">Landed <span class="break-all text-slate-300">'
    + escapeHtml(String(summary.landed)) + '</span> ' + links + '</p>';
}

// The status line's time: the close time once the task is closed, the live
// Run's elapsed time while it runs.
function statusTimeHtml(summary) {
  if (summary.task_state !== 'open') {
    return '<span class="text-xs text-slate-400 whitespace-nowrap" title="' + escapeHtmlAttr(String(summary.closed_at))
      + '">' + escapeHtml('Ended ' + formatBubbleTime(summary.closed_at)) + '</span>';
  }
  if (summary.active && summary.active.started_at) {
    return '<span class="text-xs text-slate-400 whitespace-nowrap">'
      + elapsedHtml('Running for ', summary.active.started_at) + '</span>';
  }
  return '';
}

function render() {
  const summary = currentSummary;
  const status = statusOf(summary);
  const activeStart = summary.active && summary.active.started_at ? summary.active.started_at : null;
  const head = '<div class="flex items-center gap-2">'
    + '<span class="w-2 h-2 rounded-full flex-shrink-0 ' + status.dot + '"></span>'
    + '<span class="font-semibold text-slate-200">' + escapeHtml(status.label) + '</span>'
    + statusTimeHtml(summary)
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
    + closingHtml("Worker's last message", summary.closing && summary.closing.work, 'work')
    + closingHtml('Review verdict', summary.closing && summary.closing.review, 'review')
    + linksRow(summary);
}

// Applies a poll's summary. The summary the card already shows writes only
// the elapsed-time text; a changed one repaints the card, each fold as the
// reader left it. A session that carries no summary (every non-worker)
// empties the card state; its For-you list has no card then.
function applyWorkerSummary(summary) {
  const json = JSON.stringify(summary || null);
  if (json === currentSummaryJson) {
    if (currentSummary) tickElapsed();
    return;
  }
  currentSummaryJson = json;
  currentSummary = summary || null;
  if (currentSummary) render();
}

// A session open paints its bootstrap summary afresh, every fold closed.
function mountWorkerSummary(summary) {
  openFolds.clear();
  currentSummaryJson = 'null';
  currentSummary = null;
  applyWorkerSummary(summary);
}

// The card node for the For-you list, or null while no summary is showing.
function workerSummaryCard() {
  return currentSummary ? card : null;
}

Chat.ForYou.registerSummaryCardRenderer(workerSummaryCard);
Chat.wire({applyWorkerSummary, mountWorkerSummary, workerSummaryCard, toggleWorkerSummaryFold}, {});

})();
