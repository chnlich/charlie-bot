(function() {
  const Chat = globalThis.Chat;

// ---------------------------------------------------------------------------
// The For-you view: the session page's reading layer for a web session.
//
// For you shows the user's messages, the agent's web replies (web_reply), the
// web reply notices (web_reply_notice) and error rows, in order, and nothing
// else. When none of them is in the store, one row offers Everything. Everything
// shows the whole record, as the page always has. A web session (a manager
// session that is not a Slack or Discord thread), root or child, opens in For
// you; a thread session and a worker session open in Everything. A manual switch
// holds for that session until the page reloads.
//
// turn-engine.js builds the list from the store. This file holds the rules
// (what shows, the web-session test and the default view), the empty-state row
// and the toggle.
// ---------------------------------------------------------------------------
const FOR_YOU = 'foryou';
const EVERYTHING = 'everything';
const SHOWN_ROLES = ['user', 'web_reply', 'web_reply_notice'];
// The aggregator writes a failed turn's event as a system row whose text starts
// with this prefix (message_aggregator.py: ASSISTANT_ERROR and ERROR).
const ERROR_PREFIX = 'Error: ';
// The manual view choice of each session on this page: a reload empties it, as
// the sidebar's expanded nodes reset.
const manualViews = new Map();

function isErrorMessage(msg) {
  return msg.role === 'system' && String(msg.content || '').startsWith(ERROR_PREFIX);
}

function showsInForYou(msg) {
  return SHOWN_ROLES.includes(msg.role) || isErrorMessage(msg);
}

// The store's entries that For you shows, in order.
function items(entries) {
  return entries.filter((entry) => showsInForYou(entry.msg));
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

// A web session, root or child, opens in For you; a thread session and a worker
// session open in Everything.
function defaultView(session) {
  return isWebSession(session) ? FOR_YOU : EVERYTHING;
}

function viewFor(session) {
  return manualViews.get(session.id) || defaultView(session);
}

// The toggle's buttons carry aria-pressed, and their styles follow it.
function renderToggle(view) {
  document.querySelectorAll('[data-chat-view-option]').forEach((btn) => {
    btn.setAttribute('aria-pressed', String(btn.dataset.chatViewOption === view));
  });
}

// The toggle's switch. The choice holds for this session until the page reloads.
function setChatView(view) {
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
  items,
  buildEmptyLine,
  isWebSession,
  defaultView,
  viewFor,
  renderToggle,
};
Chat.registerForYouRules(ForYou);
Chat.wire({setChatView}, {ForYou});

})();
