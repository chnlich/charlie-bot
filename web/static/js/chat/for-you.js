(function() {
  const Chat = globalThis.Chat;
  const isErrorRow = Chat.isErrorRow;

// ---------------------------------------------------------------------------
// The For-you view: the session page's reading layer for web and worker sessions.
//
// For you shows the user's messages, the agent's web replies (web_reply) and
// the web reply notices (web_reply_notice). It also shows draft cards waiting
// for the user's click (draft), error rows, and nothing else. It reads the
// store's server order, never the segment order that Everything shows.
// The shared span rule (shared.js splitTurnSpan) carries an input that arrives
// after a turn's first assistant message into the next turn. The segment order
// would put that input below the web replies that followed it. When none of
// them is in the store, one row offers Everything, which shows the whole
// record as the page always has.
//
// A web session (a manager session that is not a Slack or Discord thread)
// opens in For you, root or child; so does a worker session. A thread session
// opens in Everything. A worker's For you holds its one summary card
// (chat/worker-summary.js) — nothing else. A manual switch holds for that
// session until the page reloads.
//
// turn-engine.js builds the list from the store. This file holds the rules
// (what shows, the web-session test and the default view), the empty-state row,
// the toggle and the one function that applies a session's view choice.
// ---------------------------------------------------------------------------
const FOR_YOU = 'foryou';
const EVERYTHING = 'everything';
const STATUS = 'status';
const SHOWN_ROLES = ['user', 'web_reply', 'web_reply_notice', 'draft'];
// The manual view choice of each session on this page: the message view the
// engine mounts, and whether the Status view is selected on top of it. A reload
// empties the map, as the sidebar's expanded nodes reset.
const manualViews = new Map();
// The session this page shows, set by every open path (session-view.js
// renderSessionView). The worker's For you holds only its summary card. The row
// filter and the empty row need to know that, and the toggle's Status option
// shows for a web session only.
let currentSession = null;
let summaryCardRenderer = null;

function setSession(session) {
  currentSession = session;
}

// A worker session's For you holds its one summary card and nothing else —
// no message rows, no empty-state row.
function isWorker() {
  return currentSession != null && currentSession.profile === 'worker';
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

// The round-end message whose button group each completed round's last reply
// carries in its bubble, keyed by the reply's store entry. A round still
// running has no end event, so its reply carries no group; a run-end round is
// nobody's to rate or explain, so its reply carries none either.
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

// A web session: a manager session that no chat thread owns. The server sets
// `thread_session` on every session it returns. The Status view reads the same test.
function isWebSession(session) {
  return session.profile === 'manager' && !session.thread_session;
}

// A web session, root or child, or a worker session opens in For you; a thread
// session opens in Everything.
function defaultView(session) {
  if (session.profile === 'worker') return FOR_YOU;
  return isWebSession(session) ? FOR_YOU : EVERYTHING;
}

// What a session shows: the message view the engine mounts, and whether the
// Status view is selected on top of it. A session with no manual choice reads
// its default.
function shownFor(session) {
  return manualViews.get(session.id) || {view: defaultView(session), status: false};
}

// The message view the engine mounts for a session.
function viewFor(session) {
  return shownFor(session).view;
}

// The toggle's buttons carry aria-pressed, and their styles follow it. The
// Status option shows for a web session only: a thread session and a worker
// session toggle between For you and Everything.
function renderToggle(view, statusShown) {
  const statusAllowed = currentSession != null && isWebSession(currentSession);
  document.querySelectorAll('[data-chat-view-option]').forEach((btn) => {
    const isStatus = btn.dataset.chatViewOption === STATUS;
    btn.classList.toggle('hidden', isStatus && !statusAllowed);
    const selected = isStatus ? statusShown
      : !statusShown && btn.dataset.chatViewOption === view;
    btn.setAttribute('aria-pressed', String(selected));
  });
}

// The one function that applies a session's two view values: the message view
// the engine mounts, and whether the Status view is selected. It sets the
// visibility of the list, the scroll-to-bottom button and the status panel
// together. setChatView and the session-switch path (session-view.js) both go
// through it, so the engine only ever receives a For you or Everything view.
// Selecting the Status view calls no engine.setView, so the list keeps its
// nodes. Its zero height under display:none holds the engine's projection while
// the panel shows, and the return restores the reader's place through the
// engine's hidden-container path.
function applyChatView(sessionId, view, statusShown, engine) {
  const wasStatusShown = Boolean(manualViews.get(sessionId)?.status);
  manualViews.set(sessionId, {view, status: statusShown});
  const messages = document.getElementById('messages');
  const panel = document.getElementById('status-panel');
  const scrollButton = document.getElementById('scroll-to-bottom');
  // The panel and the scroll button are chrome a harness may mount without,
  // as status-panel.js's renderPanel reads them.
  if (panel) panel.classList.toggle('hidden', !statusShown);
  messages.classList.toggle('hidden', statusShown);
  if (statusShown) {
    // The button points at the hidden list; the return restores it from the
    // engine's pin state.
    if (scrollButton) scrollButton.classList.add('hidden');
  } else if (engine) {
    if (engine.view !== view) engine.setView(view);
    else if (wasStatusShown) {
      // The hidden-container return projects on the engine's ResizeObserver,
      // which delivers a frame or two after the unhide. Until it runs, a reader
      // who returns with arrived content already in the list sits at the stale
      // position over the grown list. This unhide is the size recovery, so it
      // runs the same resize projection here, synchronously. A pinned reader is
      // at the bottom in the return's first frame, and a history reader's
      // anchor restores before the first paint.
      engine.handleResize();
    }
    if (scrollButton) scrollButton.classList.toggle('hidden', engine.pinnedIntent);
  }
  renderToggle(view, statusShown);
}

// The session-switch path (session-view.js): the caller mounted the session's
// message view, and this applies the Status selection the session remembers on
// top of it. A re-open of a session left at the Status view shows the panel
// again, before a page reload.
function applySessionChatView(session, view, engine) {
  applyChatView(session.id, view, shownFor(session).status, engine);
}

// The toggle's switch. The choice holds for this session until the page reloads.
function setChatView(view) {
  const engine = Chat.activeTurnEngineFor(document.getElementById('messages'));
  if (!engine) throw new Error('No turn engine is mounted for the chat view');
  // Status rides on the message view the engine already mounts, so a selection
  // of it never remounts the list.
  applyChatView(SESSION_ID, view === STATUS ? engine.view : view, view === STATUS, engine);
}

// The turn engine projects these rules, so they register with it; the page reads the same object as
// Chat.ForYou.
const ForYou = {
  FOR_YOU,
  EVERYTHING,
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
  applySessionChatView,
};
Chat.registerForYouRules(ForYou);
Chat.wire({setChatView}, {ForYou});

})();
