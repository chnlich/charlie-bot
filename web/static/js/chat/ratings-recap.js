
(function() {
  const Chat = globalThis.Chat;
  const escapeHtmlAttr = Chat.escapeHtmlAttr;
  const escapeJsSingleQuoted = Chat.escapeJsSingleQuoted;

let activeRoundRatings = {};

function setActiveRoundRatings(roundRatings) {
  activeRoundRatings = roundRatings || {};
}

function getRoundRating(roundId) {
  return activeRoundRatings[String(roundId)] || null;
}

function roundRatingButtonClass(buttonRating, activeRating) {
  const active = buttonRating === activeRating;
  const activeClass = buttonRating === 'thumbs_up' ? 'text-green-400' : 'text-red-400';
  const hoverClass = buttonRating === 'thumbs_up' ? 'hover:text-green-400' : 'hover:text-red-400';
  return 'round-rating-button p-0.5 text-sm leading-none transition-colors duration-150 ' + (active ? activeClass : 'text-slate-500 ' + hoverClass);
}

function renderRoundRatingButtons(sessionId, roundId) {
  const activeRating = getRoundRating(roundId);
  const sessionArg = escapeJsSingleQuoted(sessionId);
  const roundKey = String(roundId);
  const roundArg = escapeJsSingleQuoted(roundKey);
  const sharedAttrs = ' data-round-rating-session="' + escapeHtmlAttr(sessionId) + '"'
    + ' data-round-rating-event="' + escapeHtmlAttr(roundKey) + '"';
  return '<button type="button" data-round-rating="thumbs_up"' + sharedAttrs
    + ' aria-pressed="' + String(activeRating === 'thumbs_up') + '"'
    + ' onclick="rateRound(\'' + sessionArg + '\', \'' + roundArg + '\', \'thumbs_up\')"'
    + ' class="' + roundRatingButtonClass('thumbs_up', activeRating) + '" title="Thumbs up">'
    + '<svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6.633 10.5c.806 0 1.533-.446 2.031-1.08a9.041 9.041 0 0 1 2.861-2.4c.723-.384 1.35-.956 1.653-1.715a4.498 4.498 0 0 0 .322-1.672V2.75A.75.75 0 0 1 14.25 2a2.25 2.25 0 0 1 2.25 2.25c0 1.152-.26 2.243-.723 3.218-.266.558.107 1.282.725 1.282h3.126c1.026 0 1.945.694 2.054 1.715.045.422.068.85.068 1.285a11.95 11.95 0 0 1-2.649 7.521c-.388.482-.987.729-1.605.729H13.48c-.483 0-.964-.078-1.423-.23l-3.114-1.04a4.501 4.501 0 0 0-1.423-.23H5.904M14.25 9h2.25M5.904 18.75c.083.205.173.405.27.602.197.4-.078.898-.523.898h-.908c-.889 0-1.713-.518-1.972-1.368a12 12 0 0 1-.521-3.507c0-1.553.295-3.036.831-4.398C3.387 10.203 4.167 9.75 5 9.75h1.053c.472 0 .745.556.5.96a8.958 8.958 0 0 0-1.302 4.665c0 1.194.232 2.333.654 3.375Z"/></svg></button>'
    + '<button type="button" data-round-rating="thumbs_down"' + sharedAttrs
    + ' aria-pressed="' + String(activeRating === 'thumbs_down') + '"'
    + ' onclick="rateRound(\'' + sessionArg + '\', \'' + roundArg + '\', \'thumbs_down\')"'
    + ' class="' + roundRatingButtonClass('thumbs_down', activeRating) + '" title="Thumbs down">'
    + '<svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M7.5 15h2.25m8.024-9.75c.011.05.028.1.052.148.591 1.2.924 2.55.924 3.977a8.96 8.96 0 0 1-.999 4.125m.023-8.25c-.076-.365.183-.75.575-.75h.908c.889 0 1.713.518 1.972 1.368.339 1.11.521 2.287.521 3.507 0 1.553-.295 3.036-.831 4.398C20.613 14.547 19.833 15 19 15h-1.053c-.472 0-.745-.556-.5-.96a8.95 8.95 0 0 0 .303-.54m.023-8.25H16.48a4.5 4.5 0 0 1-1.423-.23l-3.114-1.04a4.5 4.5 0 0 0-1.423-.23H6.504c-.618 0-1.217.247-1.605.729A11.95 11.95 0 0 0 2.25 12c0 .434.023.863.068 1.285C2.427 14.306 3.346 15 4.372 15h3.126c.618 0 .991.724.725 1.282A7.471 7.471 0 0 0 7.5 19.5a2.25 2.25 0 0 0 2.25 2.25.75.75 0 0 0 .75-.75v-.633c0-.573.11-1.14.322-1.672.304-.76.93-1.33 1.653-1.715a9.04 9.04 0 0 0 2.86-2.4c.498-.634 1.226-1.08 2.032-1.08h.384"/></svg></button>';
}

function applyRoundRatingButtonState(btn, activeRating) {
  const buttonRating = btn.dataset.roundRating;
  const isActive = buttonRating === activeRating;
  btn.classList.toggle('text-slate-500', !isActive);
  btn.classList.toggle('text-green-400', buttonRating === 'thumbs_up' && isActive);
  btn.classList.toggle('text-red-400', buttonRating === 'thumbs_down' && isActive);
  btn.classList.toggle('hover:text-green-400', buttonRating === 'thumbs_up' && !isActive);
  btn.classList.toggle('hover:text-red-400', buttonRating === 'thumbs_down' && !isActive);
  btn.setAttribute('aria-pressed', String(isActive));
}

function updateRoundRatingButtons(sessionId, roundId, activeRating) {
  const roundKey = String(roundId);
  document.querySelectorAll('[data-round-rating]').forEach(btn => {
    if (btn.dataset.roundRatingSession === String(sessionId) && btn.dataset.roundRatingEvent === roundKey) {
      applyRoundRatingButtonState(btn, activeRating);
    }
  });
}

function refreshAllRoundRatingButtons() {
  document.querySelectorAll('[data-round-rating]').forEach(btn => {
    applyRoundRatingButtonState(btn, getRoundRating(btn.dataset.roundRatingEvent));
  });
}

async function initializeRoundRatings() {
  if (!SESSION_ID) return;
  const sessionId = SESSION_ID;
  try {
    const res = await fetch('/api/sessions/' + sessionId);
    if (!res.ok) throw new Error(`Load round ratings failed: ${res.status}`);
    const session = await res.json();
    if (SESSION_ID !== sessionId) return;
    setActiveRoundRatings(session.round_ratings || {});
    refreshAllRoundRatingButtons();
  } catch (err) {
    console.error('Load round ratings failed:', err);
  }
}

async function rateRound(sessionId, roundId, rating) {
  const roundKey = String(roundId);
  const nextRating = activeRoundRatings[roundKey] === rating ? null : rating;
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/rounds/' + encodeURIComponent(roundKey) + '/rate', {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify({rating: nextRating}),
    });
    if (!res.ok) throw new Error(`Rate round failed: ${res.status}`);
    const session = await res.json();
    if (SESSION_ID !== sessionId) return;
    setActiveRoundRatings(session.round_ratings || {});
    updateRoundRatingButtons(sessionId, roundKey, getRoundRating(roundKey));
  } catch (err) {
    console.error('Rate round failed:', err);
  }
}

// ---------------------------------------------------------------------------
// In-session recap: zero-token extraction + opt-in cached Haiku summary
// ---------------------------------------------------------------------------
const RECAP_ASK_CAP = 6;

// The one small action-button style for the recap panel's text buttons:
// renderRecapAsks's show-all toggle and recapRerunButton's re-summarize.
const RECAP_ACTION_CLASS = 'mt-1 text-[11px] text-sky-400 hover:text-sky-300';

function toggleRecapPanel(btn, sessionId, eventIndex) {
  const sep = btn.closest('.separator-line');
  if (!sep) return;
  const next = sep.nextElementSibling;
  if (next && next.classList.contains('recap-panel')) {
    next.remove();
    btn.classList.remove('text-sky-400');
    return;
  }
  btn.classList.add('text-sky-400');
  const panel = document.createElement('div');
  panel.className = 'recap-panel mx-4 my-1 px-3 py-2 bg-slate-800/70 border border-slate-700/60 rounded-lg';
  panel.dataset.sessionId = sessionId;
  panel.dataset.eventIndex = eventIndex;
  panel.innerHTML = '<div class="recap-body text-slate-500 text-xs">Loading recap…</div>';
  sep.parentNode.insertBefore(panel, sep.nextSibling);
  loadRecap(sessionId, eventIndex, panel);
}

async function loadRecap(sessionId, eventIndex, panel) {
  const body = panel.querySelector('.recap-body');
  let data;
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/recap?upto=' + eventIndex);
    if (!res.ok) throw new Error('HTTP ' + res.status);
    data = await res.json();
  } catch (err) {
    console.error('Load recap failed:', err);
    body.innerHTML = '<div class="text-red-400 text-xs">Failed to load recap</div>';
    return;
  }
  body.classList.remove('text-slate-500', 'text-xs');
  body.innerHTML =
    recapSectionLabel('What was discussed')
    + renderRecapAsks(data.asks)
    + '<div class="recap-summary mt-2 pt-2 border-t border-slate-700/60"></div>';
  applyRecapSummary(panel, sessionId, eventIndex, data);
}

function recapSectionLabel(text) {
  return '<div class="text-[11px] uppercase tracking-wide text-slate-500 mb-1">' + escapeHtml(text) + '</div>';
}

function renderRecapAsks(asks) {
  if (!asks || !asks.length) return '<div class="text-slate-500 text-xs">(none)</div>';
  const items = asks.map((ask, i) =>
    '<li class="' + (i >= RECAP_ASK_CAP ? 'recap-ask-extra hidden' : '') + '">' + escapeHtml(ask) + '</li>'
  ).join('');
  let html = '<ul class="list-disc pl-5 space-y-0.5 text-xs text-slate-300">' + items + '</ul>';
  if (asks.length > RECAP_ASK_CAP) {
    html += '<button class="' + RECAP_ACTION_CLASS + '" onclick="toggleRecapAsks(this)">'
      + 'Show all (' + asks.length + ')</button>';
  }
  return html;
}

function toggleRecapAsks(btn) {
  const panel = btn.closest('.recap-panel');
  const extras = panel.querySelectorAll('.recap-ask-extra');
  const collapsed = extras.length && extras[0].classList.contains('hidden');
  extras.forEach((el) => el.classList.toggle('hidden', !collapsed));
  btn.textContent = collapsed ? 'Collapse' : 'Show all (' + (RECAP_ASK_CAP + extras.length) + ')';
}

function applyRecapSummary(panel, sessionId, eventIndex, data) {
  const sumEl = panel.querySelector('.recap-summary');
  if (data.summary && !data.summary_stale) {
    sumEl.innerHTML = recapSectionLabel('Summary') + recapSummaryText(data.summary);
    return;
  }
  if (data.summary && data.summary_stale) {
    sumEl.innerHTML = recapSectionLabel('Summary (stale)') + recapSummaryText(data.summary) + recapRerunButton();
    return;
  }
  // No summary yet for any point up to here -> the explicit recap-button click generates one.
  fetchRecapSummary(sessionId, eventIndex, panel);
}

function recapSummaryText(text) {
  return '<div class="text-xs text-slate-300 whitespace-pre-wrap">' + escapeHtml(text || '') + '</div>';
}

function recapRerunButton() {
  return '<button class="' + RECAP_ACTION_CLASS + '" onclick="rerunRecapSummary(this)">↻ Re-summarize</button>';
}

function rerunRecapSummary(btn) {
  const panel = btn.closest('.recap-panel');
  fetchRecapSummary(panel.dataset.sessionId, panel.dataset.eventIndex, panel);
}

async function fetchRecapSummary(sessionId, eventIndex, panel) {
  const sumEl = panel.querySelector('.recap-summary');
  sumEl.innerHTML = recapSectionLabel('Summary') + '<div class="text-slate-500 text-xs">Summarizing…</div>';
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/recap/summarize?upto=' + eventIndex, {method: 'POST'});
    if (!res.ok) throw new Error('HTTP ' + res.status);
    const data = await res.json();
    sumEl.innerHTML = recapSectionLabel('Summary') + recapSummaryText(data.summary);
  } catch (err) {
    console.error('Summarize recap failed:', err);
    sumEl.innerHTML = recapSectionLabel('Summary') + '<div class="text-red-400 text-xs">Failed to summarize</div>' + recapRerunButton();
  }
}

// ---------------------------------------------------------------------------
// Explain (btw-style): per-divider async explanation with a chosen backend
// ---------------------------------------------------------------------------
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
function applyExplainStatusesToDom() {
  document.querySelectorAll('.explain-toggle[data-explain-state]').forEach((btn) => {
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
// Opening rides globalThis.toggleExplainPanel — the turn engine's tracked
// wrapper — so the engine records the open state and restores the panel on
// re-materialization; an already-open panel just re-fetches in place.
function openExplainPanel(sessionId, eventIndex) {
  const sep = document.querySelector('.separator-line[data-event-index="' + eventIndex + '"]');
  const btn = sep && sep.querySelector('.explain-toggle');
  if (!btn) return;
  if (explainPanelFor(eventIndex)) loadExplainPanel(sessionId, eventIndex, explainPanelFor(eventIndex));
  else globalThis.toggleExplainPanel(btn, sessionId, eventIndex);
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

const GLOBALS = {
  setActiveRoundRatings,
  rateRound,
  toggleRecapPanel,
  toggleRecapAsks,
  rerunRecapSummary,
  setActiveExplainStatuses,
  toggleExplainPanel,
  markExplainPending,
  applyExplainStatusesToDom,
  updateExplainStatus,
};
const CHAT_ONLY = {
  renderRoundRatingButtons,
  initializeRoundRatings,
  renderExplainButton,
};
Chat.wire(GLOBALS, CHAT_ONLY);

})();
