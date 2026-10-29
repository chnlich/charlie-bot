(function() {
  const Chat = globalThis.Chat;
  const isErrorRow = Chat.isErrorRow;

// ---------------------------------------------------------------------------
// The For-you view: the session page's reading layer for web and worker sessions.
//
// For you shows the user's messages, the agent's web replies (web_reply), the
// web reply notices (web_reply_notice) and error rows, and nothing else. It
// reads the store's server order, never the segment order Everything shows:
// the shared span rule (shared.js splitTurnSpan) carries an input that arrives
// after a turn's first assistant message into the next turn, so the segment
// order would put that input below the web replies that followed it. When none
// of them is in the store, one row offers Everything. Everything
// shows the whole record, as the page always has. A web session (a manager
// session that is not a Slack or Discord thread), root or child, and a worker
// session open in For you; a thread session opens in Everything. A worker's For
// you holds its one summary card (chat/worker-summary.js) — nothing else. A
// manual switch holds for that session until the page reloads.
//
// turn-engine.js builds the list from the store. This file holds the rules
// (what shows, the web-session test and the default view), the empty-state row
// and the toggle.
// ---------------------------------------------------------------------------
const FOR_YOU = 'foryou';
const EVERYTHING = 'everything';
const STATUS = 'status';
const SHOWN_ROLES = ['user', 'web_reply', 'web_reply_notice'];
// The manual view choice of each session on this page: a reload empties it, as
// the sidebar's expanded nodes reset.
const manualViews = new Map();
// The profile of the session this page shows, set by every open path
// (session-view.js renderSessionView). The worker's For you holds only its
// summary card, so the row filter and the empty row need to know it.
let currentProfile = null;
// The session this page shows, set beside the profile: the toggle reads it to
// show the Status option for a web session only.
let currentSession = null;
let summaryCardRenderer = null;
// The Status view's show and hide, registered by chat/status-panel.js (the
// registerStatusViewControl entry below). The toggle selects Status; the
// status panel owns the view the selection shows.
let statusViewControl = null;

function setSession(session) {
  currentSession = session;
  currentProfile = session ? (session.profile || null) : null;
}

// A worker session's For you holds its one summary card and nothing else —
// no message rows, no empty-state row.
function isWorker() {
  return currentProfile === 'worker';
}

// A later chat module registers the worker card renderer. The projection calls
// this only for worker sessions, after all page scripts have loaded.
function registerSummaryCardRenderer(renderer) {
  if (summaryCardRenderer) throw new Error('Worker summary card renderer is already registered');
  if (typeof renderer !== 'function') throw new Error('Worker summary card renderer must be a function');
  summaryCardRenderer = renderer;
}

function summaryCard() {
  if (!summaryCardRenderer) throw new Error('Worker summary card renderer is not registered');
  return summaryCardRenderer();
}

function showsInForYou(msg) {
  return SHOWN_ROLES.includes(msg.role) || isErrorRow(msg);
}

// The store's entries that For you shows, in order.
function items(entries) {
  if (isWorker()) return [];
  return entries.filter((entry) => showsInForYou(entry.msg));
}

// The row each completed reply receives in the For-you projection, keyed by its store entry.
function roundButtonRows(entries) {
  const rows = new Map();
  if (isWorker()) return rows;

  let lastReply = null;
  entries.forEach((entry) => {
    const msg = entry.msg;
    if (msg.role === 'web_reply') lastReply = entry;
    if (msg.role !== 'separator' || msg.id == null || msg.id === '') return;
    if (msg.kind !== 'run_end' && msg.event_index != null && lastReply) rows.set(lastReply, msg);
    lastReply = null;
  });
  return rows;
}

// The row an empty For you shows: one button that switches the view to Everything.
function buildEmptyLine() {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'for-you-empty block w-full py-2 text-sm text-slate-400 hover:text-slate-200 transition-colors';
  btn.textContent = 'Nothing for you yet';
  btn.onclick = () => setChatView(EVERYTHING);
  return btn;
}

// A web session: a manager session with no Slack or Discord origin. The status panel
// reads the same test.
function isWebSession(session) {
  return session.profile === 'manager' && session.slack_origin == null && session.discord_origin == null;
}

// A web session, root or child, or a worker session opens in For you; a thread
// session opens in Everything.
function defaultView(session) {
  if (session.profile === 'worker') return FOR_YOU;
  return isWebSession(session) ? FOR_YOU : EVERYTHING;
}

function viewFor(session) {
  return manualViews.get(session.id) || defaultView(session);
}

// The view the turn engine runs: the session's selected view, unless it is
// Status. Status is an overlay over the list, so the engine keeps a message
// view underneath it and the list stays laid out.
function messageViewFor(session) {
  const view = viewFor(session);
  return view === STATUS ? defaultView(session) : view;
}

// The Status view's show and hide, registered by chat/status-panel.js after
// this file loads. A selection before the registration is a wiring bug.
function registerStatusViewControl(control) {
  if (statusViewControl) throw new Error('Status view control is already registered');
  if (!control || typeof control.setShown !== 'function') {
    throw new Error('Status view control needs a setShown function');
  }
  statusViewControl = control;
}

function showStatusView(shown) {
  if (!statusViewControl) throw new Error('Status view control is not registered');
  statusViewControl.setShown(shown);
}

// The toggle's buttons carry aria-pressed, and their styles follow it. The
// Status option shows for a web session only; a thread session and a worker
// session keep the two message views.
function renderToggle(view) {
  const statusShown = currentSession ? isWebSession(currentSession) : false;
  document.querySelectorAll('[data-chat-view-option]').forEach((btn) => {
    btn.setAttribute('aria-pressed', String(btn.dataset.chatViewOption === view));
    if (btn.dataset.chatViewOption === STATUS) btn.classList.toggle('hidden', !statusShown);
  });
}

// The toggle's switch. The choice holds for this session until the page reloads.
// Status leaves the turn engine on its message view: only the overlay changes.
function setChatView(view) {
  if (view === STATUS) {
    showStatusView(true);
    manualViews.set(SESSION_ID, view);
    renderToggle(view);
    return;
  }
  showStatusView(false);
  const engine = Chat.activeTurnEngineFor(document.getElementById('messages'));
  if (!engine) throw new Error('No turn engine is mounted for the chat view');
  engine.setView(view);
  manualViews.set(SESSION_ID, view);
  renderToggle(view);
}

// The turn engine projects these rules, so they register with it; the page reads the same object as
// Chat.ForYou.
const ForYou = {
  FOR_YOU,
  EVERYTHING,
  STATUS,
  setSession,
  items,
  roundButtonRows,
  isWorker,
  registerSummaryCardRenderer,
  summaryCard,
  buildEmptyLine,
  isWebSession,
  defaultView,
  viewFor,
  messageViewFor,
  renderToggle,
};
Chat.registerForYouRules(ForYou);
Chat.wire({setChatView, registerStatusViewControl}, {ForYou});

})();
