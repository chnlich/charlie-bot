// ---------------------------------------------------------------------------
// Explain (btw-style): per-divider async explanation with a chosen backend
// ---------------------------------------------------------------------------
(function() {
  const Chat = globalThis.Chat;
  const escapeHtmlAttr = Chat.escapeHtmlAttr;
  const escapeJsSingleQuoted = Chat.escapeJsSingleQuoted;

// The lightbulb icon and the pending spinner: the four button states (none /
// pending / ready / error) differ in icon and color class only, so the state
// applier swaps the button's inner SVG and never rebuilds the button itself.
const EXPLAIN_BULB_SVG = '<svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">'
  + '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" '
  + 'd="M9.663 17h4.673M12 3v1m6.364 1.636l-.707.707M21 12h-1M4 12H3m3.343-5.657l-.707-.707'
  + 'm2.828 9.9a5 5 0 117.072 0l-.548.547A3.374 3.374 0 0014 18.469V19a2 2 0 11-4 0v-.531c0'
  + '-.895-.356-1.754-.988-2.386l-.548-.547z"/></svg>';
const EXPLAIN_SPINNER_SVG = '<svg class="w-3.5 h-3.5 animate-spin" fill="none" viewBox="0 0 24 24">'
  + '<circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>'
  + '<path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8v4a4 4 0 00-4 4H4z"></path></svg>';
const EXPLAIN_TITLE = 'Explain this round with a chosen model';
const EXPLAIN_STATE_COLORS = {none: 'text-slate-500', pending: 'text-amber-400', ready: 'text-amber-400', error: 'text-red-400'};

// The persisted truth of this session's explain entries, as the /explain/status
// summary shape ({upto: {state, backend, generated_at}}). Read at render time by
// renderExplainButton, exactly like the round ratings above — turns the engine
// materializes after the status load still land with their real state.
let activeExplainStatuses = {};

function setActiveExplainStatuses(statuses) {
  activeExplainStatuses = statuses || {};
}

function getExplainStatus(eventIndex) {
  return activeExplainStatuses[String(eventIndex)] || null;
}

function explainButtonState(eventIndex) {
  const status = getExplainStatus(eventIndex);
  return status ? status.state : 'none';
}

function applyExplainButtonVisual(btn, state) {
  btn.dataset.explainState = state;
  btn.classList.remove('text-slate-500', 'text-amber-400', 'text-red-400');
  btn.classList.add(EXPLAIN_STATE_COLORS[state] || EXPLAIN_STATE_COLORS.none);
  btn.innerHTML = state === 'pending' ? EXPLAIN_SPINNER_SVG : EXPLAIN_BULB_SVG;
}

function renderExplainButton(sessionId, eventIndex) {
  const state = explainButtonState(eventIndex);
  const sessionArg = escapeJsSingleQuoted(sessionId);
  return '<button type="button" data-explain-state="' + state + '"'
    + ' onclick="explainSession(\'' + sessionArg + '\', ' + eventIndex + ')"'
    + ' class="explain-toggle p-0.5 ' + (EXPLAIN_STATE_COLORS[state] || EXPLAIN_STATE_COLORS.none) + '"'
    + ' title="' + EXPLAIN_TITLE + '">'
    + (state === 'pending' ? EXPLAIN_SPINNER_SVG : EXPLAIN_BULB_SVG)
    + '</button>';
}

// Paint every rendered divider's button from the active status map. Separators
// the turn engine materializes later read the map at render time; this covers
// the ones already in the DOM when a status load or WS frame lands.
function applyExplainStatusesToDom(root) {
  root.querySelectorAll('.explain-toggle[data-explain-state]').forEach((btn) => {
    const sep = btn.closest('.separator-line');
    const eventIndex = sep && sep.dataset.eventIndex;
    if (eventIndex != null) applyExplainButtonVisual(btn, explainButtonState(eventIndex));
  });
}

function explainPanelFor(eventIndex) {
  const sep = document.querySelector('.separator-line[data-event-index="' + eventIndex + '"]');
  const next = sep && sep.nextElementSibling;
  return next && next.classList.contains('explain-panel') ? next : null;
}

function toggleExplainPanel(btn, sessionId, eventIndex) {
  const sep = btn.closest('.separator-line');
  if (!sep) return;
  const next = sep.nextElementSibling;
  if (next && next.classList.contains('explain-panel')) {
    next.remove();
    return;
  }
  openExplainPanelOn(sep, sessionId, eventIndex);
}

function openExplainPanelOn(sep, sessionId, eventIndex) {
  const panel = document.createElement('div');
  panel.className = 'explain-panel mx-4 my-1 px-3 py-2 bg-slate-800/70 border border-slate-700/60 rounded-lg';
  panel.dataset.sessionId = sessionId;
  panel.dataset.eventIndex = eventIndex;
  panel.innerHTML = '<div class="explain-body text-slate-500 text-xs">Loading explanation…</div>';
  sep.parentNode.insertBefore(panel, sep.nextSibling);
  loadExplainPanel(sessionId, eventIndex, panel);
}

// The one opener for the panel outside a button click: the modal confirm path.
// Opening rides toggleExplainPanelTracked, the tracked toggle the turn engine
// returned at registration, so the engine records the open state and restores
// the panel on re-materialization; an already-open panel just re-fetches in place.
function openExplainPanel(sessionId, eventIndex) {
  const sep = document.querySelector('.separator-line[data-event-index="' + eventIndex + '"]');
  const btn = sep && sep.querySelector('.explain-toggle');
  if (!btn) return;
  if (explainPanelFor(eventIndex)) loadExplainPanel(sessionId, eventIndex, explainPanelFor(eventIndex));
  else toggleExplainPanelTracked(btn, sessionId, eventIndex);
}

async function loadExplainPanel(sessionId, eventIndex, panel) {
  const body = panel.querySelector('.explain-body');
  let entry;
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/explain?upto=' + eventIndex);
    if (!res.ok) throw new Error('HTTP ' + res.status);
    entry = await res.json();
  } catch (err) {
    console.error('Load explanation failed:', err);
    body.innerHTML = '<div class="text-red-400 text-xs">Failed to load explanation</div>';
    return;
  }
  renderExplainEntry(body, entry);
}

function explainHeader(entry, pending) {
  const label = BACKEND_OPTIONS[entry.backend] || entry.backend;
  if (pending) return 'Explaining with ' + escapeHtml(label) + '…';
  const when = entry.generated_at ? ' &middot; <span data-explain-ts="' + escapeHtmlAttr(entry.generated_at) + '"></span>' : '';
  return 'Explained by ' + escapeHtml(label) + when;
}

function renderExplainEntry(body, entry) {
  const pending = entry.state === 'pending';
  let html = '<div class="text-[11px] uppercase tracking-wide text-slate-500 mb-1">' + explainHeader(entry, pending) + '</div>';
  if (pending) {
    html += '<div class="text-slate-500 text-xs">Generating explanation…</div>';
  } else if (entry.state === 'error') {
    html += '<div class="text-red-400 text-xs whitespace-pre-wrap">' + escapeHtml(entry.error || 'Explanation failed') + '</div>';
  } else {
    html += mdDiv(entry.answer);
  }
  body.classList.remove('text-slate-500', 'text-xs');
  body.innerHTML = html;
  globalThis.postProcessRenderedMessages(body);
  body.querySelectorAll('[data-explain-ts]').forEach((el) => { el.textContent = formatBubbleTime(el.dataset.explainTs); });
}

// The explain_status WS frame: repaint the divider's button from the frame, and
// refresh the panel content only when that divider's panel is open (the frame
// carries no body; the single-entry GET is what the panel renders from).
function updateExplainStatus(sessionId, upto, state, backend) {
  const status = getExplainStatus(upto) || {};
  activeExplainStatuses[String(upto)] = {state, backend, generated_at: status.generated_at || null};
  const sep = document.querySelector('.separator-line[data-event-index="' + upto + '"]');
  if (sep) {
    const btn = sep.querySelector('.explain-toggle');
    if (btn) applyExplainButtonVisual(btn, state);
  }
  const panel = explainPanelFor(upto);
  if (panel) loadExplainPanel(sessionId, upto, panel);
}

// The modal confirm path: mark the button pending and open the panel skeleton.
function markExplainPending(sessionId, eventIndex, backend) {
  activeExplainStatuses[String(eventIndex)] = {state: 'pending', backend, generated_at: null};
  const sep = document.querySelector('.separator-line[data-event-index="' + eventIndex + '"]');
  const btn = sep && sep.querySelector('.explain-toggle');
  if (btn) applyExplainButtonVisual(btn, 'pending');
  openExplainPanel(sessionId, eventIndex);
}

function explainSession(sessionId, eventIndex) {
  openSessionActionModal({
    action: 'explain',
    sessionId,
    eventIndex,
    title: 'Explain This Round',
    bodyText: 'Choose the model that explains this round. The explanation is kept with the session; nothing is written to the chat history.',
    confirmLabel: 'Explain',
    failureLabel: 'Explain',
    // Explain stays on this session: POST registers the async generation, the
    // button goes pending, and the panel skeleton opens under the divider.
    onConfirm: async (backend) => {
      const res = await fetch('/api/sessions/' + sessionId + '/explain', {
        method: 'POST',
        headers: JSON_HEADERS,
        body: JSON.stringify({event_index: eventIndex, backend}),
      });
      if (!res.ok) throw new Error(await res.text());
      await res.json();
      markExplainPending(sessionId, eventIndex, backend);
    },
  });
}

// One explain/status GET per session load/switch paints every divider's
// explain button from persisted truth; turns the engine materializes later
// read the map at render time, so no per-divider request is ever needed. Only
// the session and switch generation that own the view when the response lands
// may install the map (detail.isCurrent).
async function loadExplainStatuses({sessionId, isCurrent}) {
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/explain/status');
    if (!res.ok) throw new Error('HTTP ' + res.status);
    const statuses = await res.json();
    if (!isCurrent()) return;
    setActiveExplainStatuses(statuses);
    applyExplainStatusesToDom(document);
  } catch (err) {
    console.error('Load explain statuses failed:', err);
  }
}

// The explain button sits on every separator line, and its panel opens under it.
Chat.registerSeparatorButton(renderExplainButton, applyExplainStatusesToDom);
const toggleExplainPanelTracked = Chat.registerSeparatorPanel({
  panelClass: 'explain-panel',
  toggleSelector: '.explain-toggle',
  toggle: toggleExplainPanel,
});
// Explain (btw-style) state change for one divider: repaint the button from the
// frame; a frame carries no body, so an open panel re-fetches the single entry.
registerWsMessageHandler('explain_status', (ev) => updateExplainStatus(SESSION_ID, ev.upto, ev.state, ev.backend));
document.addEventListener('charliebot:session-data-load', (event) => loadExplainStatuses(event.detail));

const GLOBALS = {
  explainSession,
};
Chat.wire(GLOBALS, {});

})();
